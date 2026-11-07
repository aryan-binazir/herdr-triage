//! Everything a command does with Herdr and the store: syncing, coalescing hook requests, acting on
//! threads and carrying out what triage decides, and arming wake timers for what triage accepted.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use chrono::Local;
use serde_json::{Map, Value};

use crate::herdr::{Agent, Herdr, Tab, Workspace};
use crate::picker::Item;
use crate::store::Store;
use crate::triage::{
    self, Action, After, Close, Closing, Observed, Rank, Refusal, ReopenPlan, Row, Section,
    SessionRef, State, Thread, UNTITLED,
};
use crate::wakers;

const DEFAULT_PIN_ICON: &str = "\u{f435}";

type Clock = dyn Fn() -> i64;

pub struct Session {
    herdr: Herdr,
    store: Store,
    clock: Box<Clock>,
    waker: Box<dyn FnMut(i64) -> io::Result<()>>,
}

pub struct View {
    pub items: Vec<Item>,
    pub open_sections: BTreeSet<Section>,
    pub undoable: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Refused(String),
    /// The thread was resumed in a new tab, which Herdr focused.
    Reopened,
    Done {
        /// The thread acted on, under the key Herdr knows it by now.
        key: String,
        notice: Option<String>,
        /// The change left a snooze pending whose wake timer could not be started.
        wake_failed: Option<String>,
    },
}

struct Snapshot {
    agents: Vec<Agent>,
    workspaces: Vec<Workspace>,
    tabs: Vec<Tab>,
    rows: Vec<Row>,
    open_sections: BTreeSet<Section>,
    undoable: bool,
    wake_failed: Option<String>,
}

type Synced<R> = io::Result<(Snapshot, R)>;

impl Session {
    pub fn connect() -> io::Result<Self> {
        let herdr = Herdr::connect()?;
        let store = Store::for_socket(herdr.socket_path());
        Ok(Self::new(
            herdr,
            store,
            || Local::now().timestamp(),
            wakers::spawn,
        ))
    }

    fn new(
        herdr: Herdr,
        store: Store,
        clock: impl Fn() -> i64 + 'static,
        waker: impl FnMut(i64) -> io::Result<()> + 'static,
    ) -> Self {
        Self {
            herdr,
            store,
            clock: Box::new(clock),
            waker: Box::new(waker),
        }
    }

    /// Herdr runs at most 32 plugin commands at once and rejects the rest, so a hook that finds a
    /// sync already running leaves it a request to sync again and exits instead of waiting.
    pub fn sync(&mut self) -> io::Result<()> {
        self.store.request_sync()?;
        self.serve_sync_requests()
    }

    /// After a Herdr restart nothing remembers the snoozes' wake timers, so all of them start again.
    pub fn startup(&mut self) -> io::Result<()> {
        self.store.prune_wake_locks((self.clock)());
        let deadlines: BTreeSet<_> = self
            .store
            .transact(|state| state.pending_wakes())?
            .into_iter()
            .collect();
        for until in deadlines {
            (self.waker)(until)?;
        }
        self.sync()
    }

    /// Focuses the next agent that needs you after the one in `invoked_from` (else the focused one).
    pub fn next(&mut self, invoked_from: Option<&str>) -> io::Result<()> {
        let (snapshot, ()) = self.sync_and_serve(false, |_, _, _| ())?;
        let current = snapshot
            .agents
            .iter()
            .find(|agent| match invoked_from {
                Some(pane) => agent.pane_id == pane,
                None => agent.focused,
            })
            .map(Agent::key);
        let next = triage::next_attention(&snapshot.rows, current.as_deref());
        match next.and_then(|row| row.agent_index) {
            Some(index) => self.herdr.focus(&snapshot.agents[index].pane_id),
            None => self.herdr.notify("Nothing needs you"),
        }
    }

    pub fn view(&mut self) -> io::Result<View> {
        self.view_after(|_, _, _| ())
    }

    pub fn rename(&mut self, key: &str, title: Option<String>) -> io::Result<View> {
        self.view_after(|state, observed, now| {
            state.rename(current_key(observed, key), title, now);
        })
    }

    pub fn toggle_section(&mut self, section: Section) -> io::Result<View> {
        self.view_after(|state, _, _| state.toggle_section(section))
    }

    /// Acts on the thread the picker showed as `key`, which Herdr may have rekeyed since.
    pub fn act(&mut self, key: &str, action: Action) -> io::Result<Outcome> {
        self.carry_out(|state, observed, now| {
            let key = current_key(observed, key);
            let agent = observed.iter().find(|agent| agent.key == key);
            let after = state.apply(key, agent, action, now)?;
            Ok((key.to_owned(), after))
        })
    }

    pub fn undo(&mut self) -> io::Result<Outcome> {
        self.carry_out(|state, _, now| state.undo(now))
    }

    pub fn focus(&mut self, pane: &str) -> io::Result<()> {
        self.herdr.focus(pane)
    }

    fn view_after(&mut self, edit: impl FnOnce(&mut State, &[Observed], i64)) -> io::Result<View> {
        let (snapshot, ()) = self.sync_and_serve(true, edit)?;
        Ok(View {
            items: items(&snapshot, (self.clock)()),
            open_sections: snapshot.open_sections,
            undoable: snapshot.undoable,
        })
    }

    fn carry_out(
        &mut self,
        edit: impl FnOnce(&mut State, &[Observed], i64) -> Result<(String, After), Refusal>,
    ) -> io::Result<Outcome> {
        let (snapshot, after) = self.sync_and_serve(false, edit)?;
        let done = |key, notice| Outcome::Done {
            key,
            notice,
            wake_failed: snapshot.wake_failed.clone(),
        };
        let outcome = match after {
            Err(Refusal(refusal)) => Ok(Outcome::Refused(refusal)),
            Ok((key, After::Nothing)) => Ok(done(key, None)),
            Ok((key, After::ClosePane(close))) => self
                .close_session(&snapshot, &close)
                .map(|notice| done(key, notice)),
            Ok((_, After::Reopen(plan))) => {
                self.reopen(&snapshot, &plan).map(|()| Outcome::Reopened)
            }
        };
        let served = self.serve_sync_requests();
        let outcome = outcome?;
        served?;
        Ok(outcome)
    }

    fn sync_and_serve<R>(
        &mut self,
        fetch_tabs: bool,
        edit: impl FnOnce(&mut State, &[Observed], i64) -> R,
    ) -> Synced<R> {
        let synced = self
            .sync_once(true, fetch_tabs, edit)
            .and_then(|synced| synced.ok_or_else(|| io::Error::other("the store was busy")))
            .and_then(|synced| synced);
        let served = self.serve_sync_requests();
        let synced = synced?;
        served?;
        Ok(synced)
    }

    fn serve_sync_requests(&mut self) -> io::Result<()> {
        let mut failure = None;
        while self.store.sync_requested() {
            match self.sync_once(false, false, |_, _, _| ())? {
                None => break,
                Some(Err(error)) => drop(failure.get_or_insert(error)),
                Some(Ok(_)) => {}
            }
        }
        failure.map_or(Ok(()), Err)
    }

    fn sync_once<R>(
        &mut self,
        wait: bool,
        fetch_tabs: bool,
        edit: impl FnOnce(&mut State, &[Observed], i64) -> R,
    ) -> io::Result<Option<Synced<R>>> {
        let synced = self.transact(wait, |held, state| {
            held.store.take_sync_request();
            sync_locked(held, state, fetch_tabs, edit)
        })?;
        let Some(((exit_checks, synced), wake_failed)) = synced else {
            return Ok(None);
        };
        for at in exit_checks {
            let _ = (self.waker)(at);
        }
        Ok(Some(synced.map(|(snapshot, edited)| {
            let wake_failed = wake_failed.map(|error| error.to_string());
            (
                Snapshot {
                    wake_failed,
                    ..snapshot
                },
                edited,
            )
        })))
    }

    fn locked<T>(&mut self, change: impl FnOnce(&mut Held, &mut State) -> T) -> io::Result<T> {
        match self.transact(true, change)? {
            Some((result, _)) => Ok(result),
            None => Err(io::Error::other("the store was busy")),
        }
    }

    fn transact<T>(
        &mut self,
        wait: bool,
        change: impl FnOnce(&mut Held, &mut State) -> T,
    ) -> io::Result<Option<(T, Option<io::Error>)>> {
        let store = &self.store;
        let mut held = Held {
            herdr: &mut self.herdr,
            store,
            clock: &*self.clock,
        };
        let change = |state: &mut State| {
            let before = state.pending_wakes();
            let result = change(&mut held, state);
            (result, newly_pending(before, state.pending_wakes()))
        };
        let changed = if wait {
            store.transact(change).map(Some)
        } else {
            store.try_transact(change)
        }?;
        let Some((result, due)) = changed else {
            return Ok(None);
        };
        let mut failure = None;
        for until in due {
            if let Err(error) = (self.waker)(until) {
                failure.get_or_insert(error);
            }
        }
        Ok(Some((result, failure)))
    }

    fn close_session(&mut self, snapshot: &Snapshot, close: &Close) -> io::Result<Option<String>> {
        let closing = self.locked(|held, state| {
            let generation = held.herdr.server_generation();
            state.confirm_close(close, &generation, held.now(), || {
                let agents = held.herdr.agents()?;
                Ok(agents
                    .iter()
                    .map(|agent| observe(agent, &snapshot.workspaces))
                    .collect())
            })
        })??;
        match closing {
            Closing::Close(pane) => {
                let closed = self.herdr.close_pane(&pane);
                self.locked(|_, state| state.report_close(close, closed.is_ok()))?;
                closed.map(|()| None)
            }
            Closing::KeepOpen(notice) => Ok(Some(notice)),
            Closing::Skip => Ok(None),
        }
    }

    fn reopen(&mut self, snapshot: &Snapshot, plan: &ReopenPlan) -> io::Result<()> {
        let workspaces: Vec<&str> = snapshot
            .workspaces
            .iter()
            .map(|w| w.workspace_id.as_str())
            .collect();
        let launch = plan.launch(&self.herdr.server_generation(), &workspaces);
        if let Some(pane) = launch.focus
            && self.herdr.focus_pane(pane).is_ok()
        {
            return Ok(());
        }
        let cwd = launch.cwd.filter(|dir| Path::new(dir).is_dir());
        self.locked(|held, state| {
            let herdr = &mut *held.herdr;
            let pane = herdr.create_tab(launch.workspace, cwd)?;
            let launched = match launch.codex_session {
                Some(session) => herdr.report_codex_session(&pane, session),
                None => Ok(()),
            }
            .and_then(|()| herdr.run_command(&pane, &shell_command(launch.argv)));
            if let Err(error) = launched {
                let _ = herdr.close_pane(&pane);
                return Err(error);
            }
            let now = (held.clock)();
            state.report_reopen(plan, &pane, &herdr.server_generation(), now);
            Ok(())
        })?
    }
}

struct Held<'a> {
    herdr: &'a mut Herdr,
    store: &'a Store,
    clock: &'a Clock,
}

impl Held<'_> {
    fn now(&self) -> i64 {
        (self.clock)()
    }
}

fn newly_pending(before: Vec<i64>, after: Vec<i64>) -> Vec<i64> {
    let mut counts: BTreeMap<i64, isize> = BTreeMap::new();
    for until in before {
        *counts.entry(until).or_default() -= 1;
    }
    for until in after {
        *counts.entry(until).or_default() += 1;
    }
    counts
        .into_iter()
        .filter_map(|(until, grew)| (grew > 0).then_some(until))
        .collect()
}

fn sync_locked<R>(
    held: &mut Held,
    state: &mut State,
    fetch_tabs: bool,
    edit: impl FnOnce(&mut State, &[Observed], i64) -> R,
) -> (Vec<i64>, Synced<R>) {
    let HerdrView {
        generation,
        agents,
        workspaces,
        tabs,
    } = match read_herdr(held.herdr, fetch_tabs) {
        Ok(view) => view,
        Err(error) => return (Vec::new(), Err(error)),
    };
    let observed: Vec<Observed> = agents.iter().map(|a| observe(a, &workspaces)).collect();
    let now = held.now();
    let exit_checks = state.reconcile(&generation, &observed, now);
    let edited = edit(state, &observed, now);
    let rows = state.ordered(&observed);
    let published = publish(held.herdr, &agents, &rows, now);
    let snapshot = Snapshot {
        agents,
        workspaces,
        tabs,
        rows,
        open_sections: state.open_sections.clone(),
        undoable: state.undo.is_some(),
        wake_failed: None,
    };
    (exit_checks, published.map(|()| (snapshot, edited)))
}

struct HerdrView {
    generation: String,
    agents: Vec<Agent>,
    workspaces: Vec<Workspace>,
    tabs: Vec<Tab>,
}

fn read_herdr(herdr: &mut Herdr, fetch_tabs: bool) -> io::Result<HerdrView> {
    for _ in 0..2 {
        let generation = herdr.server_generation();
        let view = HerdrView {
            agents: herdr.agents()?,
            workspaces: herdr.workspaces()?,
            tabs: if fetch_tabs {
                herdr.tabs()?
            } else {
                Vec::new()
            },
            generation: String::new(),
        };
        if herdr.server_generation() == generation {
            return Ok(HerdrView { generation, ..view });
        }
    }
    Err(io::Error::other("herdr restarted while it was being read"))
}

/// Herdr rekeys a thread once it learns its session, possibly after the picker last looked.
fn current_key<'a>(observed: &'a [Observed], key: &'a str) -> &'a str {
    observed
        .iter()
        .find(|agent| agent.pre_session_key.as_deref() == Some(key))
        .map_or(key, |agent| agent.key.as_str())
}

fn observe(agent: &Agent, workspaces: &[Workspace]) -> Observed {
    let session = agent.session();
    let workspace = workspaces
        .iter()
        .find(|w| w.workspace_id == agent.workspace_id);
    Observed {
        key: agent.key(),
        pre_session_key: session.map(|_| agent.terminal_key()),
        pane_id: agent.pane_id.clone(),
        terminal_id: agent.terminal_id.clone(),
        workspace_panes: workspace.map_or(0, |w| w.pane_count),
        status: agent.agent_status,
        seq: agent.state_change_seq,
        focused: agent.focused,
        thread: Thread {
            label: thread_label(agent),
            agent: session
                .map(|s| s.agent.clone())
                .or_else(|| agent.agent.clone()),
            cwd: agent.cwd.clone(),
            workspace_id: Some(agent.workspace_id.clone()),
            workspace_label: workspace.map(|w| w.label.clone()),
            session: session.map(|s| SessionRef {
                source: s.source.clone(),
                kind: s.kind.clone(),
                value: s.value.clone(),
            }),
        },
    }
}

fn publish(herdr: &mut Herdr, agents: &[Agent], rows: &[Row], now: i64) -> io::Result<()> {
    let pin_icon = pin_icon();
    let mut failure = None;
    for row in rows {
        let Some(index) = row.agent_index else {
            continue;
        };
        let agent = &agents[index];
        let rank = row.rank().token().to_owned();
        let label = row.sidebar_label(&pin_icon, now, &Local);
        if agent.tokens.get("triage_rank") == Some(&rank)
            && agent.tokens.get("triage") == label.as_ref()
        {
            continue;
        }
        let mut tokens = Map::new();
        tokens.insert("triage_rank".into(), Value::String(rank));
        tokens.insert("triage".into(), label.map_or(Value::Null, Value::String));
        // A pane can close between the snapshot and here; the others still need their tokens.
        if let Err(error) = herdr.report_tokens(&agent.pane_id, &tokens) {
            failure.get_or_insert(error);
        }
    }
    let view = herdr.set_view(&view_label(rows), &Rank::hidden_tokens());
    failure.map_or(view, Err)
}

fn view_label(rows: &[Row]) -> String {
    let count = |rank| rows.iter().filter(|row| row.rank() == rank).count();
    let (snoozed, settled) = (count(Rank::Snoozed), count(Rank::Settled));
    // Herdr right-aligns the label after its own "agents" title, so it has to stay short.
    match (snoozed, settled) {
        (0, 0) => "triage".into(),
        (n, 0) => format!("{n} snoozed"),
        (0, n) => format!("{n} settled"),
        (a, b) => format!("{} parked", a + b),
    }
}

pub fn pin_icon() -> String {
    let path = std::env::var_os("HERDR_PLUGIN_CONFIG_DIR")
        .map(|dir| PathBuf::from(dir).join("config.toml"));
    path.and_then(|path| fs::read_to_string(path).ok())
        .and_then(|text| text.parse::<toml::Table>().ok())
        .and_then(|table| table.get("pin_icon")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| DEFAULT_PIN_ICON.to_owned())
}

fn shell_command(argv: &[String]) -> String {
    let quote = |arg: &String| {
        if !arg.is_empty()
            && arg
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_./:=@%+,".contains(c))
        {
            arg.clone()
        } else {
            format!("'{}'", arg.replace('\'', "'\\''"))
        }
    };
    argv.iter().map(quote).collect::<Vec<_>>().join(" ")
}

fn items(snapshot: &Snapshot, now: i64) -> Vec<Item> {
    snapshot
        .rows
        .iter()
        .map(|row| {
            let agent = row.agent_index.map(|index| &snapshot.agents[index]);
            let location = match agent {
                Some(agent) => location(agent, &snapshot.workspaces, &snapshot.tabs),
                None => row
                    .thread
                    .workspace_label
                    .clone()
                    .unwrap_or_else(|| "-".into()),
            };
            let kind = agent
                .and_then(|agent| agent.display_agent.clone())
                .or_else(|| row.thread.agent.clone())
                .unwrap_or_else(|| "-".into());
            let cwd = row.thread.cwd.as_deref().unwrap_or_default();
            Item {
                pane_id: agent.map(|agent| agent.pane_id.clone()),
                search: format!(
                    "{kind} {} {} {location} {cwd}",
                    row.label(),
                    row.thread.label
                ),
                location,
                kind,
                status: row.status_text(now),
                focused: agent.is_some_and(|agent| agent.focused),
                row: row.clone(),
            }
        })
        .collect()
}

fn thread_label(agent: &Agent) -> String {
    if let Some(name) = agent
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        return name.to_owned();
    }
    let title = agent
        .terminal_title_stripped
        .as_deref()
        .unwrap_or_default()
        .trim();
    let brands = ["claude code", "claude", "codex", "pi", "π", "opencode"];
    let stripped = brands
        .iter()
        .find_map(|brand| {
            let rest = title.get(brand.len()..)?;
            let separated = rest.is_empty()
                || rest.starts_with(|c: char| c.is_whitespace() || "-—·:".contains(c));
            (separated && title[..brand.len()].eq_ignore_ascii_case(brand)).then_some(rest)
        })
        .map_or(title, |rest| {
            rest.trim_start_matches(|c: char| c.is_whitespace() || "-—·:".contains(c))
        });
    if stripped.is_empty() || stripped == "-" {
        UNTITLED.into()
    } else {
        stripped.into()
    }
}

fn location(agent: &Agent, workspaces: &[Workspace], tabs: &[Tab]) -> String {
    let Some(workspace) = workspaces
        .iter()
        .find(|w| w.workspace_id == agent.workspace_id)
    else {
        return "-".into();
    };
    let tab = tabs.iter().find(|t| t.tab_id == agent.tab_id);
    match tab {
        Some(tab) if workspace.tab_count > 1 || tab.label != tab.number.to_string() => {
            format!("{} · {}", workspace.label, tab.label)
        }
        _ => workspace.label.clone(),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use serde_json::json;

    use super::*;
    use crate::fake_herdr::{self, FakeHerdr};
    use crate::picker::Picker;
    use crate::triage::Park;

    const NOW: i64 = 1_800_000_000;
    const KEY: &str = "session:claude:id:id-w1:p1";

    fn resumable_agent(pane: &str, status: &str, seq: u64) -> Value {
        let mut agent = fake_herdr::agent(pane, status, seq);
        agent["agent_session"] = json!({
            "source": "herdr:claude", "agent": "claude", "kind": "id", "value": format!("id-{pane}")
        });
        agent
    }

    fn two_resumable_agents() -> FakeHerdr {
        FakeHerdr::start(vec![
            resumable_agent("w1:p1", "idle", 3),
            resumable_agent("w1:p2", "idle", 4),
        ])
    }

    fn connect(fake: &FakeHerdr) -> (Session, Rc<RefCell<Vec<i64>>>) {
        let armed = Rc::new(RefCell::new(Vec::new()));
        let record = armed.clone();
        let session = Session::new(
            Herdr::at(&fake.socket).unwrap(),
            store(fake),
            || NOW,
            move |until| {
                record.borrow_mut().push(until);
                Ok(())
            },
        );
        (session, armed)
    }

    fn store(fake: &FakeHerdr) -> Store {
        Store::at(fake.state_dir())
    }

    fn hook_fires_during(fake: &FakeHerdr, method: &str) {
        let store = store(fake);
        let mut fired = false;
        fake.on(method, move |_| {
            if !std::mem::replace(&mut fired, true) {
                store.request_sync().unwrap();
            }
        });
    }

    fn methods(fake: &FakeHerdr) -> Vec<String> {
        let mut requests = fake.requests.lock().unwrap();
        let methods = requests
            .iter()
            .map(|request| request["method"].as_str().unwrap().to_owned())
            .collect();
        requests.clear();
        methods
    }

    fn done(key: &str) -> Outcome {
        Outcome::Done {
            key: key.into(),
            notice: None,
            wake_failed: None,
        }
    }

    #[test]
    fn each_operation_makes_the_herdr_calls_it_always_has() {
        let fake = FakeHerdr::start(vec![
            resumable_agent("w1:p1", "idle", 3),
            resumable_agent("w1:p2", "idle", 4),
            fake_herdr::agent("w1:p3", "working", 5),
        ]);
        let (mut session, _) = connect(&fake);
        let (list, view_set, report) = ("agent.list", "agent.view.set", "pane.report_metadata");
        let read = [list, "workspace.list"];

        session.view().unwrap();
        let first_view = [&read[..], &["tab.list", report, report, report, view_set]].concat();
        assert_eq!(methods(&fake), first_view);

        assert_eq!(session.act(KEY, Action::ToggleSettle).unwrap(), done(KEY));
        let closing = [&read[..], &[report, view_set, list, "pane.close"]].concat();
        assert_eq!(methods(&fake), closing);

        let refused = session.act("terminal:t-w1:p3", Action::ToggleSettle);
        assert!(matches!(refused, Ok(Outcome::Refused(_))));
        assert_eq!(methods(&fake), [&read[..], &[view_set]].concat());

        assert_eq!(session.undo().unwrap(), Outcome::Reopened);
        let reopening = [&read[..], &[view_set, "tab.create", "pane.send_input"]].concat();
        assert_eq!(methods(&fake), reopening);

        session
            .act("session:claude:id:id-w1:p2", Action::TogglePin)
            .unwrap();
        assert_eq!(methods(&fake), [&read[..], &[report, view_set]].concat());

        session
            .rename("terminal:t-w1:p3", Some("x".into()))
            .unwrap();
        assert_eq!(
            methods(&fake),
            [&read[..], &["tab.list", view_set]].concat()
        );

        session.toggle_section(Section::Working).unwrap();
        assert_eq!(
            methods(&fake),
            [&read[..], &["tab.list", view_set]].concat()
        );

        *fake.fail_method.lock().unwrap() = Some("pane.close".into());
        assert!(
            session
                .act("session:claude:id:id-w1:p2", Action::ToggleSettle)
                .is_err()
        );
        assert_eq!(methods(&fake), closing);
    }

    #[test]
    fn the_view_locates_agents_by_tab_and_actions_do_not_read_tabs() {
        let fake = FakeHerdr::start(vec![fake_herdr::agent("w1:p1", "idle", 3)]);
        *fake.tabs.lock().unwrap() = vec![json!({
            "tab_id": "w1:t1", "label": "Release \"候補\"", "number": 1
        })];
        let (mut session, _) = connect(&fake);
        let view = session.view().unwrap();
        assert_eq!(view.items[0].location, "acme-api · Release \"候補\"");
        let mut picker = Picker::new(view.items, None, &view.open_sections);
        for ch in "/候補".chars() {
            picker.key(
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                NOW,
                &Local,
            );
        }
        assert_eq!(picker.matched(), 1, "search finds the tab label");
        fake.take("tab.list");
        session.act("terminal:t-w1:p1", Action::TogglePin).unwrap();
        assert!(fake.take("tab.list").is_empty());
    }

    #[test]
    fn settling_a_resumable_agent_closes_its_pane() {
        let fake = two_resumable_agents();
        let (mut session, _) = connect(&fake);
        assert_eq!(session.act(KEY, Action::ToggleSettle).unwrap(), done(KEY));
        assert_eq!(fake.take("pane.close"), [json!({"pane_id": "w1:p1"})]);
        let shown = store(&fake).transact(|state| state.ordered(&[])).unwrap();
        assert_eq!(
            shown.len(),
            1,
            "the settled thread is listed as closed right away"
        );
        assert_eq!(shown[0].park, Some(Park::Settled));
    }

    #[test]
    fn a_close_herdr_refuses_leaves_the_thread_open_and_undo_does_not_reopen_it() {
        let fake = two_resumable_agents();
        let (mut session, _) = connect(&fake);
        *fake.fail_method.lock().unwrap() = Some("pane.close".into());
        assert!(session.act(KEY, Action::ToggleSettle).is_err());
        *fake.fail_method.lock().unwrap() = None;
        let closed = store(&fake).transact(|state| state.ordered(&[])).unwrap();
        assert!(closed.is_empty(), "the thread is not listed as closed");

        fake.agents
            .lock()
            .unwrap()
            .retain(|a| a["pane_id"] != "w1:p1");
        assert_ne!(
            session.undo().unwrap(),
            Outcome::Reopened,
            "undo reopens only a pane it closed"
        );
        assert!(fake.take("tab.create").is_empty());
    }

    #[test]
    fn unsettling_a_closed_thread_resumes_it_in_a_new_tab_of_its_workspace_once() {
        let fake = two_resumable_agents();
        let (mut session, _) = connect(&fake);
        session.act(KEY, Action::ToggleSettle).unwrap();
        store(&fake)
            .transact(|state| state.records.get_mut(KEY).unwrap().live = false)
            .unwrap();

        assert_eq!(
            session.act(KEY, Action::ToggleSettle).unwrap(),
            Outcome::Reopened
        );
        assert_eq!(
            fake.take("tab.create"),
            [json!({"workspace_id": "w1", "cwd": "/tmp", "focus": true})]
        );
        assert_eq!(
            fake.take("pane.send_input"),
            [json!({"pane_id": "w1:p91", "text": "claude --resume id-w1:p1", "keys": ["Enter"]})]
        );

        session.act(KEY, Action::Reopen).unwrap();
        assert!(
            fake.take("tab.create").is_empty(),
            "a pending reopen is focused, not repeated"
        );
        assert_eq!(fake.take("pane.focus"), [json!({"pane_id": "w1:p91"})]);

        *fake.fail_method.lock().unwrap() = Some("pane.focus".into());
        session.act(KEY, Action::Reopen).unwrap();
        assert_eq!(
            fake.take("tab.create").len(),
            1,
            "a pending tab Herdr can no longer focus is launched again"
        );
    }

    #[test]
    fn undoing_a_settle_that_closed_the_pane_reopens_the_thread() {
        let fake = two_resumable_agents();
        let (mut session, _) = connect(&fake);
        session.act(KEY, Action::ToggleSettle).unwrap();
        assert_eq!(fake.take("pane.close"), [json!({"pane_id": "w1:p1"})]);

        assert_eq!(session.undo().unwrap(), Outcome::Reopened);
        assert_eq!(
            fake.take("pane.send_input"),
            [json!({"pane_id": "w1:p91", "text": "claude --resume id-w1:p1", "keys": ["Enter"]})]
        );
        let parked = store(&fake).transact(|state| state.records[KEY].park);
        assert_eq!(parked.unwrap(), None);
    }

    #[test]
    fn an_action_on_a_thread_herdr_has_since_rekeyed_lands_on_it() {
        let fake = two_resumable_agents();
        let (mut session, _) = connect(&fake);
        let outcome = session.act("terminal:t-w1:p1", Action::ToggleSettle);
        assert_eq!(outcome.unwrap(), done(KEY));
        let parked = store(&fake).transact(|state| state.records[KEY].park);
        assert_eq!(parked.unwrap(), Some(Park::Settled));
    }

    #[test]
    fn a_session_left_over_from_another_agent_is_not_trusted() {
        let mut stale = resumable_agent("w1:p1", "idle", 3);
        stale["agent"] = json!("codex");
        let fake = FakeHerdr::start(vec![stale, resumable_agent("w1:p2", "idle", 4)]);
        let (mut session, _) = connect(&fake);
        session.sync().unwrap();
        let keys = store(&fake)
            .transact(|state| state.records.keys().cloned().collect::<Vec<_>>())
            .unwrap();
        assert!(keys.contains(&"terminal:t-w1:p1".to_owned()), "{keys:?}");
        session
            .act("terminal:t-w1:p1", Action::ToggleSettle)
            .unwrap();
        assert!(
            fake.take("pane.close").is_empty(),
            "a codex agent is not closed as a claude thread"
        );
    }

    #[test]
    fn a_failed_resume_closes_the_tab_it_opened_and_can_be_retried() {
        let fake = two_resumable_agents();
        let (mut session, _) = connect(&fake);
        session.act(KEY, Action::ToggleSettle).unwrap();
        *fake.fail_method.lock().unwrap() = Some("pane.send_input".into());
        assert!(session.act(KEY, Action::Reopen).is_err());
        assert_eq!(
            fake.take("pane.close").last(),
            Some(&json!({"pane_id": "w1:p91"}))
        );

        *fake.fail_method.lock().unwrap() = None;
        fake.take("tab.create");
        session.act(KEY, Action::Reopen).unwrap();
        assert_eq!(
            fake.take("tab.create").len(),
            1,
            "the retry opens a fresh tab"
        );
    }

    fn codex_and_claude() -> FakeHerdr {
        let mut codex = fake_herdr::agent("w1:p1", "idle", 3);
        codex["agent"] = json!("codex");
        codex["agent_session"] = json!({
            "source": "herdr:codex", "agent": "codex", "kind": "id", "value": "0199-abc"
        });
        FakeHerdr::start(vec![codex, resumable_agent("w1:p2", "idle", 4)])
    }

    #[test]
    fn reopening_codex_tells_herdr_its_session_right_away() {
        let fake = codex_and_claude();
        let (mut session, _) = connect(&fake);
        let key = "session:codex:id:0199-abc";
        session.act(key, Action::ToggleSettle).unwrap();
        session.act(key, Action::Reopen).unwrap();
        let reported = fake.take("pane.report_agent_session");
        assert_eq!(reported.len(), 1);
        assert_eq!(reported[0]["pane_id"], "w1:p91");
        assert_eq!(reported[0]["source"], "herdr:codex");
        assert_eq!(reported[0]["agent"], "codex");
        assert_eq!(reported[0]["agent_session_id"], "0199-abc");
        assert_eq!(
            fake.take("pane.send_input")[0]["text"],
            "codex resume 0199-abc"
        );
    }

    #[test]
    fn a_failed_codex_session_report_closes_the_tab_it_opened() {
        let fake = codex_and_claude();
        let (mut session, _) = connect(&fake);
        let key = "session:codex:id:0199-abc";
        session.act(key, Action::ToggleSettle).unwrap();
        *fake.fail_method.lock().unwrap() = Some("pane.report_agent_session".into());
        assert!(session.act(key, Action::Reopen).is_err());
        assert_eq!(
            fake.take("pane.close").last(),
            Some(&json!({"pane_id": "w1:p91"}))
        );
    }

    #[test]
    fn a_sync_requested_before_a_command_takes_the_store_is_covered_by_its_sync() {
        let fake = FakeHerdr::start(vec![resumable_agent("w1:p1", "idle", 3)]);
        let (mut session, _) = connect(&fake);
        store(&fake).request_sync().unwrap();
        session.view().unwrap();
        assert!(!store(&fake).sync_requested(), "the request was served");
        assert_eq!(fake.take("agent.list").len(), 1);
    }

    #[test]
    fn a_sync_request_arriving_during_an_action_is_served_after_it() {
        let fake = two_resumable_agents();
        let (mut session, _) = connect(&fake);
        hook_fires_during(&fake, "pane.close");
        session.act(KEY, Action::ToggleSettle).unwrap();
        assert!(!store(&fake).sync_requested());
        let calls = methods(&fake);
        let closed = calls.iter().position(|m| m == "pane.close").unwrap();
        let synced = calls.iter().rposition(|m| m == "agent.list").unwrap();
        assert!(synced > closed, "{calls:?}");
    }

    #[test]
    fn a_failing_sync_still_serves_waiting_hook_requests_and_publishes_what_it_can() {
        let fake = two_resumable_agents();
        let (mut session, _) = connect(&fake);
        *fake.fail_method.lock().unwrap() = Some("pane.report_metadata".into());
        store(&fake).request_sync().unwrap();
        assert!(session.view().is_err());
        assert!(
            !store(&fake).sync_requested(),
            "the waiting hook's request was served"
        );
        assert!(
            fake.take("pane.report_metadata").len() >= 2,
            "every pane was tried"
        );
        assert!(
            !fake.take("agent.view.set").is_empty(),
            "the view was still set"
        );
    }

    #[test]
    fn a_failed_reopen_still_serves_waiting_hook_requests() {
        let fake = two_resumable_agents();
        let (mut session, _) = connect(&fake);
        session.act(KEY, Action::ToggleSettle).unwrap();
        *fake.fail_method.lock().unwrap() = Some("pane.send_input".into());
        hook_fires_during(&fake, "tab.create");
        assert!(session.act(KEY, Action::Reopen).is_err());
        assert!(
            !store(&fake).sync_requested(),
            "a hook that fired during the reopen is served"
        );
    }

    #[test]
    fn an_agent_that_starts_working_before_its_pane_closes_keeps_it() {
        let fake = two_resumable_agents();
        let (mut session, _) = connect(&fake);
        fake.on("agent.view.set", |agents| {
            agents[0]["agent_status"] = json!("working");
            agents[0]["state_change_seq"] = json!(9);
        });
        assert_eq!(session.act(KEY, Action::ToggleSettle).unwrap(), done(KEY));
        assert!(fake.take("pane.close").is_empty());
    }

    #[test]
    fn a_snooze_arms_its_wake_only_once_triage_accepts_it() {
        let fake = FakeHerdr::start(vec![fake_herdr::agent("w1:p1", "blocked", 3)]);
        let (mut session, armed) = connect(&fake);
        let key = "terminal:t-w1:p1";
        let refused = session.act(key, Action::Snooze(NOW + 3600)).unwrap();
        assert!(matches!(refused, Outcome::Refused(_)), "{refused:?}");
        assert!(armed.borrow().is_empty());

        fake.agents.lock().unwrap()[0]["agent_status"] = json!("idle");
        let snoozed = session.act(key, Action::Snooze(NOW + 3600)).unwrap();
        assert_eq!(snoozed, done(key));
        session.view().unwrap();
        assert_eq!(*armed.borrow(), [NOW + 3600], "armed once, by the snooze");
    }

    #[test]
    fn undoing_a_wake_rearms_the_snooze_it_restores() {
        let fake = FakeHerdr::start(vec![fake_herdr::agent("w1:p1", "idle", 3)]);
        let (mut session, armed) = connect(&fake);
        let key = "terminal:t-w1:p1";
        session.act(key, Action::Snooze(NOW + 3600)).unwrap();
        session.act(key, Action::Wake).unwrap();
        armed.borrow_mut().clear();
        assert_eq!(session.undo().unwrap(), done(key));
        assert_eq!(*armed.borrow(), [NOW + 3600]);
    }

    #[test]
    fn wakes_are_armed_for_what_was_saved_even_when_publishing_fails() {
        let fake = FakeHerdr::start(vec![
            fake_herdr::agent("w1:p1", "idle", 3),
            fake_herdr::agent("w1:p2", "idle", 4),
        ]);
        let (mut session, armed) = connect(&fake);
        session.view().unwrap();
        fake.agents
            .lock()
            .unwrap()
            .retain(|a| a["pane_id"] != "w1:p2");
        *fake.fail_method.lock().unwrap() = Some("agent.view.set".into());
        assert!(
            session
                .act("terminal:t-w1:p1", Action::Snooze(NOW + 3600))
                .is_err()
        );
        let mut armed = armed.borrow().clone();
        armed.sort_unstable();
        assert_eq!(
            armed,
            [NOW + 10, NOW + 3600],
            "the exit check and the snooze"
        );
    }

    #[test]
    fn a_snooze_whose_wake_timer_fails_says_so() {
        let fake = FakeHerdr::start(vec![fake_herdr::agent("w1:p1", "idle", 3)]);
        let mut session = Session::new(
            Herdr::at(&fake.socket).unwrap(),
            store(&fake),
            || NOW,
            |_| Err(io::Error::other("no processes left")),
        );
        let outcome = session.act("terminal:t-w1:p1", Action::Snooze(NOW + 3600));
        assert_eq!(
            outcome.unwrap(),
            Outcome::Done {
                key: "terminal:t-w1:p1".into(),
                notice: None,
                wake_failed: Some("no processes left".into()),
            }
        );
    }

    #[test]
    fn resume_commands_are_shell_quoted() {
        let argv = [
            "pi".to_owned(),
            "--session".to_owned(),
            "/s/it's here.jsonl".to_owned(),
        ];
        assert_eq!(
            shell_command(&argv),
            "pi --session '/s/it'\\''s here.jsonl'"
        );
    }

    #[test]
    fn the_sidebar_header_counts_parked_threads_within_herdrs_limit() {
        let row = |park| Row {
            agent_index: None,
            key: String::new(),
            seq: 0,
            display: triage::Display::Idle,
            park,
            pinned: false,
            changed_at: None,
            title: None,
            thread: Thread::default(),
        };
        assert_eq!(view_label(&[row(None)]), "triage");
        assert_eq!(
            view_label(&[row(Some(Park::Settled)), row(Some(Park::Settled))]),
            "2 settled"
        );
        let many: Vec<Row> = (0..1000)
            .map(|i| {
                row(Some(if i % 2 == 0 {
                    Park::Settled
                } else {
                    Park::Snoozed { until: 0 }
                }))
            })
            .collect();
        assert_eq!(view_label(&many), "1000 parked");
        let both = [
            row(Some(Park::Settled)),
            row(Some(Park::Snoozed { until: 0 })),
        ];
        assert_eq!(view_label(&both), "2 parked");
        assert!(
            view_label(&many).chars().count() <= 16,
            "fits beside Herdr's 'agents' title"
        );
    }

    fn agent(name: Option<&str>, title: &str) -> Agent {
        serde_json::from_value(json!({
            "pane_id": "p", "terminal_id": "t", "workspace_id": "w", "tab_id": "t1",
            "agent_status": "idle", "focused": false,
            "name": name, "terminal_title_stripped": title
        }))
        .unwrap()
    }

    #[test]
    fn thread_labels_prefer_names_then_titles_without_the_agent_brand() {
        assert_eq!(
            thread_label(&agent(Some("api refactor"), "Claude Code - x")),
            "api refactor"
        );
        assert_eq!(
            thread_label(&agent(None, "Claude Code - fix flaky test")),
            "fix flaky test"
        );
        assert_eq!(thread_label(&agent(None, "π - docs pass")), "docs pass");
        assert_eq!(thread_label(&agent(Some("  "), "Codex")), "Untitled thread");
        assert_eq!(thread_label(&agent(None, "pinning bug")), "pinning bug");
    }
}
