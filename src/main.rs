mod herdr;
mod picker;
mod store;
mod triage;
mod ui;

#[cfg(test)]
#[path = "../tests/support/fake_herdr.rs"]
mod fake_herdr;

use std::collections::BTreeSet;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use chrono::Local;
use ratatui::crossterm::event::{self, Event, KeyEventKind};
use serde_json::{Map, Value};

use herdr::{Agent, Herdr, Tab, Workspace};
use picker::{Effect, Item, Picker};
use store::Store;
use triage::{
    Action, After, Observed, Park, Refusal, ReopenPlan, Row, Section, SessionRef, State, Thread,
};

const DEFAULT_PIN_ICON: &str = "\u{f435}";
const WALL_CLOCK_RECHECK_SECS: u64 = 30;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("sync") => connect().and_then(|mut herdr| hook_sync(&mut herdr)),
        Some("startup") => startup(),
        Some("next") => next(),
        Some("open") => open(),
        Some("picker") => run_picker(),
        Some("wake-at") => wake_at(args.get(1).and_then(|at| at.parse().ok())),
        _ => Err(io::Error::other(
            "usage: herdr-triage <open|sync|startup|next|wake-at TIME>",
        )),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("herdr-triage: {error}");
            ExitCode::FAILURE
        }
    }
}

fn connect() -> io::Result<Herdr> {
    Herdr::connect()
}

fn store_for(herdr: &Herdr) -> Store {
    Store::for_socket(herdr.socket_path())
}

struct Snapshot {
    agents: Vec<Agent>,
    workspaces: Vec<Workspace>,
    tabs: Vec<Tab>,
    rows: Vec<Row>,
    open_sections: BTreeSet<Section>,
    undoable: bool,
}

fn now() -> i64 {
    Local::now().timestamp()
}

fn sync<R>(
    herdr: &mut Herdr,
    store: &Store,
    fetch_tabs: bool,
    edit: impl FnOnce(&mut State, &[Observed]) -> R,
) -> io::Result<(Snapshot, R)> {
    let synced = store
        .transact(|state| {
            store.take_sync_request();
            sync_locked(herdr, state, fetch_tabs, edit)
        })
        .and_then(|synced| synced);
    let served = serve_sync_requests(herdr, store);
    let synced = synced?;
    served?;
    Ok(synced)
}

/// Hooks that found the store busy left a request; whoever held it makes sure it is served.
fn serve_sync_requests(herdr: &mut Herdr, store: &Store) -> io::Result<()> {
    let mut failure = None;
    while store.sync_requested() {
        let synced = store.try_transact(|state| {
            store.take_sync_request();
            sync_locked(herdr, state, false, |_, _| ())
        })?;
        match synced {
            None => break,
            Some(Err(error)) => drop(failure.get_or_insert(error)),
            Some(Ok(_)) => {}
        }
    }
    failure.map_or(Ok(()), Err)
}

fn sync_locked<R>(
    herdr: &mut Herdr,
    state: &mut State,
    fetch_tabs: bool,
    edit: impl FnOnce(&mut State, &[Observed]) -> R,
) -> io::Result<(Snapshot, R)> {
    let HerdrView {
        generation,
        agents,
        workspaces,
        tabs,
    } = read_herdr(herdr, fetch_tabs)?;
    let observed: Vec<Observed> = agents.iter().map(|a| observe(a, &workspaces)).collect();
    let now = now();
    for at in state.reconcile(&generation, &observed, now) {
        // A failed waker only delays closing the thread until the next event.
        let _ = spawn_waker(at);
    }
    let edited = edit(state, &observed);
    let rows = state.ordered(&observed);
    publish(herdr, &agents, &rows, now)?;
    let open_sections = state.open_sections.clone();
    Ok((
        Snapshot {
            agents,
            workspaces,
            tabs,
            rows,
            open_sections,
            undoable: state.undo.is_some(),
        },
        edited,
    ))
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

fn observe(agent: &Agent, workspaces: &[Workspace]) -> Observed {
    let session = agent.session();
    Observed {
        key: agent.key(),
        pre_session_key: session.map(|_| agent.terminal_key()),
        pane_id: agent.pane_id.clone(),
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
            workspace_label: workspaces
                .iter()
                .find(|w| w.workspace_id == agent.workspace_id)
                .map(|w| w.label.clone()),
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
        let rank = row.rank().to_string();
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
    let view = herdr.set_view(&view_label(rows));
    failure.map_or(view, Err)
}

fn view_label(rows: &[Row]) -> String {
    let count =
        |want: fn(Park) -> bool| rows.iter().filter(|row| row.park.is_some_and(want)).count();
    let snoozed = count(|park| matches!(park, Park::Snoozed { .. }));
    let settled = count(|park| park == Park::Settled);
    // Herdr right-aligns the label after its own "agents" title, so it has to stay short.
    match (snoozed, settled) {
        (0, 0) => "triage".into(),
        (n, 0) => format!("{n} snoozed"),
        (0, n) => format!("{n} settled"),
        (a, b) => format!("{} parked", a + b),
    }
}

fn pin_icon() -> String {
    let path = std::env::var_os("HERDR_PLUGIN_CONFIG_DIR")
        .map(|dir| std::path::PathBuf::from(dir).join("config.toml"));
    path.and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| text.parse::<toml::Table>().ok())
        .and_then(|table| table.get("pin_icon")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| DEFAULT_PIN_ICON.to_owned())
}

fn startup() -> io::Result<()> {
    let mut herdr = connect()?;
    let store = store_for(&herdr);
    store.prune_wake_locks(now());
    let deadlines: BTreeSet<_> = store
        .transact(|state| state.pending_wakes())?
        .into_iter()
        .collect();
    for until in deadlines {
        spawn_waker(until)?;
    }
    hook_sync(&mut herdr)
}

fn next() -> io::Result<()> {
    let mut herdr = connect()?;
    let store = store_for(&herdr);
    let (snapshot, ()) = sync(&mut herdr, &store, false, |_, _| ())?;
    let invoked_from = invocation_context()["focused_pane_id"]
        .as_str()
        .map(str::to_owned);
    let current = snapshot
        .agents
        .iter()
        .find(|agent| match &invoked_from {
            Some(pane) => &agent.pane_id == pane,
            None => agent.focused,
        })
        .map(Agent::key);
    let next = triage::next_attention(&snapshot.rows, current.as_deref());
    match next.and_then(|row| row.agent_index) {
        Some(index) => herdr.focus(&snapshot.agents[index].pane_id),
        None => herdr.notify("Nothing needs you"),
    }
}

fn open() -> io::Result<()> {
    connect()?.open_picker(invocation_context()["focused_pane_id"].as_str())
}

fn invocation_context() -> Value {
    std::env::var("HERDR_PLUGIN_CONTEXT_JSON")
        .ok()
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default()
}

/// Herdr runs at most 32 plugin commands at once and rejects the rest, so a hook that finds a
/// sync already running leaves it a request to sync again and exits instead of waiting.
fn hook_sync(herdr: &mut Herdr) -> io::Result<()> {
    let store = store_for(herdr);
    store.request_sync()?;
    serve_sync_requests(herdr, &store)
}

/// Herdr plugins get no timers or daemons.
fn spawn_waker(until: i64) -> io::Result<()> {
    Command::new(std::env::current_exe()?)
        .args(["wake-at", &until.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map(drop)
}

fn wake_at(until: Option<i64>) -> io::Result<()> {
    let until = until.ok_or_else(|| io::Error::other("wake-at needs a unix time"))?;
    let path = Store::for_socket(&herdr::socket_path()).wake_lock_path(until)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)?;
    match lock.try_lock() {
        Ok(()) => (),
        Err(std::fs::TryLockError::WouldBlock) => return Ok(()),
        Err(std::fs::TryLockError::Error(error)) => return Err(error),
    }
    while now() <= until {
        let left = u64::try_from(until - now() + 1).unwrap_or(1);
        std::thread::sleep(Duration::from_secs(left.min(WALL_CLOCK_RECHECK_SECS)));
    }
    let _ = std::fs::remove_file(path);
    let result = hook_sync(&mut connect()?);
    drop(lock);
    result
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Performed {
    key: String,
    refusal: Option<String>,
    notice: Option<String>,
    reopened: bool,
}

fn perform(herdr: &mut Herdr, store: &Store, key: &str, action: Action) -> io::Result<Performed> {
    carry_out(herdr, store, |state, observed| {
        let key = current_key(observed, key);
        let agent = observed.iter().find(|agent| agent.key == key);
        let after = state.apply(key, agent, action, now())?;
        Ok((key.to_owned(), after))
    })
}

/// Herdr rekeys a thread once it learns its session, possibly after the picker last looked.
fn current_key<'a>(observed: &'a [Observed], key: &'a str) -> &'a str {
    observed
        .iter()
        .find(|agent| agent.pre_session_key.as_deref() == Some(key))
        .map_or(key, |agent| agent.key.as_str())
}

fn carry_out(
    herdr: &mut Herdr,
    store: &Store,
    edit: impl FnOnce(&mut State, &[Observed]) -> Result<(String, After), Refusal>,
) -> io::Result<Performed> {
    let (snapshot, after) = sync(herdr, store, false, edit)?;
    let mut performed = Performed::default();
    let outcome = match after {
        Err(refusal) => {
            performed.refusal = Some(refusal.0);
            Ok(())
        }
        Ok((key, after)) => {
            performed.key.clone_from(&key);
            match after {
                After::Nothing => Ok(()),
                After::ClosePane => close_session(herdr, store, &snapshot, &key)
                    .map(|notice| performed.notice = notice),
                After::Reopen(plan) => {
                    reopen(herdr, store, &snapshot, &plan).map(|()| performed.reopened = true)
                }
            }
        }
    };
    let served = serve_sync_requests(herdr, store);
    outcome?;
    served?;
    Ok(performed)
}

fn close_session(
    herdr: &mut Herdr,
    store: &Store,
    snapshot: &Snapshot,
    key: &str,
) -> io::Result<Option<String>> {
    let Some(agent) = snapshot.agents.iter().find(|agent| agent.key() == key) else {
        return Ok(None);
    };
    let would_close_workspace = snapshot
        .workspaces
        .iter()
        .find(|w| w.workspace_id == agent.workspace_id)
        .is_none_or(|w| w.pane_count <= 1);
    if would_close_workspace {
        return Ok(Some(
            "kept open: it is the last pane in its workspace".into(),
        ));
    }
    // Ask Herdr about the pane itself, under the lock: sibling panes on one session share a record,
    // and the agent may have started working since the action without any hook syncing yet.
    let still_closable = store.transact(|state| -> io::Result<bool> {
        let Some(record) = state.records.get(key).filter(|record| {
            record.live
                && record.missing_since.is_none()
                && record.park.is_some()
                && record.generation == herdr.server_generation()
        }) else {
            return Ok(false);
        };
        let restoring = record.restoring(now());
        let quiet_since_action = herdr.agents()?.iter().any(|current| {
            current.terminal_id == agent.terminal_id
                && !matches!(
                    current.agent_status,
                    triage::Status::Working | triage::Status::Blocked
                )
                && (current.state_change_seq == agent.state_change_seq || restoring)
        });
        if quiet_since_action {
            state.closing(key, true);
        }
        Ok(quiet_since_action)
    })??;
    if !still_closable {
        return Ok(None);
    }
    if let Err(error) = herdr.close_pane(&agent.pane_id) {
        store.transact(|state| state.closing(key, false))?;
        return Err(error);
    }
    store.transact(|state| state.session_closed(key))?;
    Ok(None)
}

fn reopen(
    herdr: &mut Herdr,
    store: &Store,
    snapshot: &Snapshot,
    plan: &ReopenPlan,
) -> io::Result<()> {
    if let Some(pane) = &plan.pending_pane
        && plan.pending_generation.as_deref() == Some(herdr.server_generation().as_str())
        && herdr.focus_pane(pane).is_ok()
    {
        return Ok(());
    }
    let workspace = plan
        .thread
        .workspace_id
        .as_deref()
        .filter(|id| snapshot.workspaces.iter().any(|w| w.workspace_id == *id));
    let cwd = plan
        .thread
        .cwd
        .as_deref()
        .filter(|dir| Path::new(dir).is_dir());
    store.transact(|state| {
        let pane = herdr.create_tab(workspace, cwd)?;
        // Codex reports a resumed session only after its next turn; Herdr can know it now.
        let launched = match (plan.thread.agent.as_deref(), &plan.thread.session) {
            (Some("codex"), Some(session)) => herdr.report_codex_session(&pane, &session.value),
            _ => Ok(()),
        }
        .and_then(|()| herdr.run_command(&pane, &shell_command(&plan.argv)));
        if let Err(error) = launched {
            let _ = herdr.close_pane(&pane);
            return Err(error);
        }
        state.reopening(&plan.key, &pane, &herdr.server_generation(), now());
        Ok(())
    })?
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

fn run_picker() -> io::Result<()> {
    let mut herdr = connect()?;
    let store = store_for(&herdr);
    let focus = std::env::var("HERDR_TRIAGE_FOCUS").ok();
    let pin_icon = pin_icon();
    let (snapshot, ()) = sync(&mut herdr, &store, true, |_, _| ())?;
    let mut picker = Picker::new(items(&snapshot), focus.as_deref(), &snapshot.open_sections);
    picker.set_undoable(snapshot.undoable);

    let mut terminal = ratatui::init();
    let started = Instant::now();
    let mut refreshed = Instant::now();
    let outcome = loop {
        let tick = usize::try_from(started.elapsed().as_millis() / 125).unwrap_or(0);
        if let Err(error) = terminal.draw(|frame| ui::draw(frame, &mut picker, tick, &pin_icon)) {
            break Err(error);
        }
        match event::poll(Duration::from_millis(100)) {
            Ok(true) => {}
            Ok(false) => {
                if refreshed.elapsed() >= Duration::from_secs(1) {
                    refreshed = Instant::now();
                    refresh(&mut herdr, &store, &mut picker, |_, _| ());
                }
                continue;
            }
            Err(error) => break Err(error),
        }
        let key = match event::read() {
            Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => key,
            Ok(_) => continue,
            Err(error) => break Err(error),
        };
        match picker.key(key, now(), &Local) {
            Effect::None => {}
            Effect::Close => break Ok(None),
            Effect::Focus(pane) => break Ok(Some(pane)),
            Effect::Rename { key, title } => {
                refresh(&mut herdr, &store, &mut picker, |state, observed| {
                    state.rename(current_key(observed, &key), title, now());
                });
            }
            Effect::ToggleSection(section) => {
                refresh(&mut herdr, &store, &mut picker, |state, _| {
                    state.toggle_section(section);
                });
            }
            Effect::Apply { key, action } => {
                let waker_error = match action {
                    Action::Snooze(until) => spawn_waker(until).err(),
                    _ => None,
                };
                let from = picker.selected_row().unwrap_or(0);
                let performed = perform(&mut herdr, &store, &key, action);
                let done = match &performed {
                    Ok(p) if p.refusal.is_none() => Some(p.key.clone()),
                    _ => None,
                };
                if show_unless_reopened(&mut herdr, &store, &mut picker, performed) {
                    break Ok(None);
                }
                if let Some(key) = done
                    && matches!(action, Action::ToggleSettle | Action::Snooze(_))
                {
                    picker.move_on_from(&key, from);
                }
                refreshed = Instant::now();
                if let Some(error) = waker_error {
                    picker.notify(format!("snooze saved, but its wake timer failed: {error}"));
                }
            }
            Effect::Undo => {
                let mut wake = None;
                let performed = carry_out(&mut herdr, &store, |state, _| {
                    let undone = state.undo(now())?;
                    if let Some(Park::Snoozed { until }) = state.records[&undone.0].park {
                        wake = Some(until);
                    }
                    Ok(undone)
                });
                let done = matches!(&performed, Ok(p) if p.refusal.is_none());
                if show_unless_reopened(&mut herdr, &store, &mut picker, performed) {
                    break Ok(None);
                }
                refreshed = Instant::now();
                if done {
                    picker.inform("undone".into());
                }
                if let Some(error) = wake.and_then(|until| spawn_waker(until).err()) {
                    picker.notify(format!(
                        "snooze restored, but its wake timer failed: {error}"
                    ));
                }
            }
        }
    };
    ratatui::restore();
    match outcome? {
        Some(pane) => herdr.focus(&pane),
        None => Ok(()),
    }
}

fn show_unless_reopened(
    herdr: &mut Herdr,
    store: &Store,
    picker: &mut Picker,
    performed: io::Result<Performed>,
) -> bool {
    match performed {
        Ok(Performed { reopened: true, .. }) => return true,
        Ok(Performed {
            refusal, notice, ..
        }) => {
            refresh(herdr, store, picker, |_, _| ());
            if let Some(notice) = notice {
                picker.inform(notice);
            }
            if let Some(refusal) = refusal {
                picker.notify(refusal);
            }
        }
        Err(error) => picker.notify(format!("herdr: {error}")),
    }
    false
}

fn refresh(
    herdr: &mut Herdr,
    store: &Store,
    picker: &mut Picker,
    edit: impl FnOnce(&mut State, &[Observed]),
) {
    match sync(herdr, store, true, edit) {
        Ok((snapshot, ())) => {
            picker.refresh(items(&snapshot), &snapshot.open_sections);
            picker.set_undoable(snapshot.undoable);
        }
        Err(error) => picker.notify(format!("herdr: {error}")),
    }
}

fn items(snapshot: &Snapshot) -> Vec<Item> {
    let now = now();
    snapshot
        .rows
        .iter()
        .map(|row| {
            let agent = row.agent_index.map(|index| &snapshot.agents[index]);
            let label = match &row.title {
                Some(title) => title.clone(),
                None if row.thread.label.is_empty() => "Untitled thread".into(),
                None => row.thread.label.clone(),
            };
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
                key: row.key.clone(),
                search: format!("{kind} {label} {} {location} {cwd}", row.thread.label),
                label,
                location,
                kind,
                display: row.display,
                park: row.park,
                pinned: row.pinned,
                status: row.status_text(now),
                focused: agent.is_some_and(|agent| agent.focused),
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
        "Untitled thread".into()
    } else {
        stripped.into()
    }
}

fn location(agent: &Agent, workspaces: &[herdr::Workspace], tabs: &[herdr::Tab]) -> String {
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
    use super::*;
    use fake_herdr::FakeHerdr;
    use serde_json::json;

    fn resumable_agent(pane: &str, status: &str, seq: u64) -> Value {
        let mut agent = fake_herdr::agent(pane, status, seq);
        agent["agent_session"] = json!({
            "source": "herdr:claude", "agent": "claude", "kind": "id", "value": format!("id-{pane}")
        });
        agent
    }

    fn connected(fake: &FakeHerdr) -> (Herdr, Store) {
        (
            Herdr::at(&fake.socket).unwrap(),
            Store::at(fake.state_dir()),
        )
    }

    #[test]
    fn picker_refresh_fetches_tabs_and_items_search_custom_labels() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let fake = FakeHerdr::start(vec![fake_herdr::agent("w1:p1", "idle", 3)]);
        let (mut herdr, store) = connected(&fake);
        let (mut snapshot, ()) = sync(&mut herdr, &store, false, |_, _| ()).unwrap();
        snapshot.tabs = vec![Tab {
            tab_id: "w1:t1".into(),
            label: "Release \"候補\"".into(),
            number: 1,
        }];
        let shown = items(&snapshot);
        assert_eq!(shown[0].location, "acme-api · Release \"候補\"");
        let mut picker = Picker::new(shown, None, &snapshot.open_sections);
        for ch in "/候補".chars() {
            picker.key(
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                now(),
                &Local,
            );
        }
        assert_eq!(picker.matched(), 1);
        refresh(&mut herdr, &store, &mut picker, |_, _| ());
        assert_eq!(fake.take("tab.list").len(), 1);
        perform(&mut herdr, &store, "terminal:t-w1:p1", Action::TogglePin).unwrap();
        assert!(fake.take("tab.list").is_empty());
    }

    #[test]
    fn settling_a_resumable_agent_closes_its_pane() {
        let fake = FakeHerdr::start(vec![
            resumable_agent("w1:p1", "idle", 3),
            resumable_agent("w1:p2", "idle", 4),
        ]);
        let (mut herdr, store) = connected(&fake);
        let performed = perform(
            &mut herdr,
            &store,
            "session:claude:id:id-w1:p1",
            Action::ToggleSettle,
        )
        .unwrap();
        assert_eq!(
            performed,
            Performed {
                key: "session:claude:id:id-w1:p1".into(),
                ..Performed::default()
            }
        );
        assert_eq!(fake.take("pane.close"), [json!({"pane_id": "w1:p1"})]);
        let shown = store.transact(|state| state.ordered(&[])).unwrap();
        assert_eq!(
            shown.len(),
            1,
            "the settled thread is listed as closed right away"
        );
        assert_eq!(shown[0].park, Some(Park::Settled));
    }

    #[test]
    fn snoozing_an_idle_resumable_agent_closes_its_pane_once() {
        let fake = FakeHerdr::start(vec![resumable_agent("w1:p1", "idle", 3)]);
        let (mut herdr, store) = connected(&fake);
        let until = now() + 3600;
        perform(
            &mut herdr,
            &store,
            "session:claude:id:id-w1:p1",
            Action::Snooze(until),
        )
        .unwrap();
        assert_eq!(fake.take("pane.close"), [json!({"pane_id": "w1:p1"})]);
    }

    #[test]
    fn hook_activity_between_settle_and_close_keeps_the_working_pane_open() {
        let fake = FakeHerdr::start(vec![resumable_agent("w1:p1", "idle", 3)]);
        let (mut herdr, store) = connected(&fake);
        let key = "session:claude:id:id-w1:p1";
        let (snapshot, after) = sync(&mut herdr, &store, false, |state, observed| {
            state.apply(key, observed.first(), Action::ToggleSettle, now())
        })
        .unwrap();
        assert_eq!(after.unwrap(), After::ClosePane);
        fake.agents.lock().unwrap()[0]["agent_status"] = json!("working");
        fake.agents.lock().unwrap()[0]["state_change_seq"] = json!(4);
        sync(&mut herdr, &store, false, |_, _| ()).unwrap();
        assert_eq!(store.transact(|s| s.records[key].park).unwrap(), None);
        assert_eq!(
            close_session(&mut herdr, &store, &snapshot, key).unwrap(),
            None
        );
        assert!(fake.take("pane.close").is_empty());
    }

    #[test]
    fn hook_activity_between_snooze_and_close_keeps_the_working_pane_open() {
        let fake = FakeHerdr::start(vec![resumable_agent("w1:p1", "idle", 3)]);
        let (mut herdr, store) = connected(&fake);
        let key = "session:claude:id:id-w1:p1";
        let until = now() + 3600;
        let (snapshot, after) = sync(&mut herdr, &store, false, |state, observed| {
            state.apply(key, observed.first(), Action::Snooze(until), now())
        })
        .unwrap();
        assert_eq!(after.unwrap(), After::ClosePane);
        fake.agents.lock().unwrap()[0]["agent_status"] = json!("working");
        fake.agents.lock().unwrap()[0]["state_change_seq"] = json!(4);
        sync(&mut herdr, &store, false, |_, _| ()).unwrap();
        assert_eq!(
            store.transact(|s| s.records[key].park).unwrap(),
            Some(Park::Snoozed { until })
        );
        assert_eq!(
            close_session(&mut herdr, &store, &snapshot, key).unwrap(),
            None
        );
        assert!(fake.take("pane.close").is_empty());
    }

    #[test]
    fn herdr_restore_churn_between_settle_and_close_still_closes_the_idle_pane() {
        let fake = FakeHerdr::start(vec![resumable_agent("w1:p1", "idle", 3)]);
        let (mut herdr, store) = connected(&fake);
        let key = "session:claude:id:id-w1:p1";
        sync(&mut herdr, &store, false, |_, _| ()).unwrap();
        store
            .transact(|s| s.records.get_mut(key).unwrap().generation = "previous-server".into())
            .unwrap();
        let (snapshot, after) = sync(&mut herdr, &store, false, |state, observed| {
            state.apply(key, observed.first(), Action::ToggleSettle, now())
        })
        .unwrap();
        assert_eq!(after.unwrap(), After::ClosePane);
        fake.agents.lock().unwrap()[0]["state_change_seq"] = json!(4);
        sync(&mut herdr, &store, false, |_, _| ()).unwrap();
        assert_eq!(
            store.transact(|s| s.records[key].park).unwrap(),
            Some(Park::Settled)
        );
        close_session(&mut herdr, &store, &snapshot, key).unwrap();
        assert_eq!(fake.take("pane.close"), [json!({"pane_id": "w1:p1"})]);
    }

    #[test]
    fn a_second_pane_on_the_same_session_does_not_block_the_close() {
        let a = resumable_agent("w1:p1", "idle", 3);
        let mut b = resumable_agent("w1:p2", "idle", 4);
        b["agent_session"] = a["agent_session"].clone();
        let fake = FakeHerdr::start(vec![a, b]);
        let (mut herdr, store) = connected(&fake);
        perform(
            &mut herdr,
            &store,
            "session:claude:id:id-w1:p1",
            Action::ToggleSettle,
        )
        .unwrap();
        assert_eq!(fake.take("pane.close"), [json!({"pane_id": "w1:p1"})]);
    }

    /// Panes A and B share one session. A is parked, then starts `status` before the close, and a
    /// hook syncs B after A, so the shared record reads idle.
    fn close_after_a_sibling_hides_the_target_turning(action: Action, status: &str, restart: bool) {
        let a = resumable_agent("w1:p1", "idle", 3);
        let mut b = resumable_agent("w1:p2", "idle", 4);
        b["agent_session"] = a["agent_session"].clone();
        let fake = FakeHerdr::start(vec![a, b]);
        let (mut herdr, store) = connected(&fake);
        let key = "session:claude:id:id-w1:p1";
        if restart {
            sync(&mut herdr, &store, false, |_, _| ()).unwrap();
        }
        let (snapshot, after) = sync(&mut herdr, &store, false, |state, observed| {
            state.apply(key, observed.first(), action, now())
        })
        .unwrap();
        assert_eq!(after.unwrap(), After::ClosePane);
        fake.agents.lock().unwrap()[0]["agent_status"] = json!(status);
        fake.agents.lock().unwrap()[0]["state_change_seq"] = json!(5);
        sync(&mut herdr, &store, false, |_, _| ()).unwrap();
        close_session(&mut herdr, &store, &snapshot, key).unwrap();
        assert!(fake.take("pane.close").is_empty(), "closed a {status} pane");
    }

    #[test]
    fn a_snoozed_pane_that_starts_working_stays_open_whatever_its_sibling_shows() {
        close_after_a_sibling_hides_the_target_turning(
            Action::Snooze(now() + 3600),
            "working",
            false,
        );
    }

    #[test]
    fn a_settled_pane_that_starts_working_during_restore_stays_open_whatever_its_sibling_shows() {
        close_after_a_sibling_hides_the_target_turning(Action::ToggleSettle, "working", true);
    }

    #[test]
    fn a_settled_pane_that_blocks_during_restore_stays_open_whatever_its_sibling_shows() {
        close_after_a_sibling_hides_the_target_turning(Action::ToggleSettle, "blocked", true);
    }

    #[test]
    fn an_idle_pane_closes_even_while_a_sibling_on_its_session_works() {
        let a = resumable_agent("w1:p1", "idle", 3);
        let mut b = resumable_agent("w1:p2", "working", 4);
        b["agent_session"] = a["agent_session"].clone();
        let fake = FakeHerdr::start(vec![a, b]);
        let (mut herdr, store) = connected(&fake);
        let performed = perform(
            &mut herdr,
            &store,
            "session:claude:id:id-w1:p1",
            Action::ToggleSettle,
        )
        .unwrap();
        assert_eq!(performed.refusal, None);
        assert_eq!(fake.take("pane.close"), [json!({"pane_id": "w1:p1"})]);
    }

    #[test]
    fn a_pane_that_starts_working_before_any_hook_syncs_stays_open() {
        let fake = FakeHerdr::start(vec![resumable_agent("w1:p1", "idle", 3)]);
        let (mut herdr, store) = connected(&fake);
        let key = "session:claude:id:id-w1:p1";
        let (snapshot, after) = sync(&mut herdr, &store, false, |state, observed| {
            state.apply(key, observed.first(), Action::ToggleSettle, now())
        })
        .unwrap();
        assert_eq!(after.unwrap(), After::ClosePane);
        fake.agents.lock().unwrap()[0]["agent_status"] = json!("working");
        fake.agents.lock().unwrap()[0]["state_change_seq"] = json!(4);
        close_session(&mut herdr, &store, &snapshot, key).unwrap();
        assert!(fake.take("pane.close").is_empty());
    }

    #[test]
    fn the_last_pane_of_a_workspace_stays_open_when_settled() {
        let fake = FakeHerdr::start(vec![resumable_agent("w1:p1", "idle", 3)]);
        fake.workspaces.lock().unwrap()[0]["pane_count"] = json!(1);
        let (mut herdr, store) = connected(&fake);
        let performed = perform(
            &mut herdr,
            &store,
            "session:claude:id:id-w1:p1",
            Action::ToggleSettle,
        )
        .unwrap();
        assert!(performed.notice.unwrap().contains("last pane"));
        assert!(fake.take("pane.close").is_empty());
    }

    #[test]
    fn unsettling_a_closed_thread_resumes_it_in_a_new_tab_of_its_workspace_once() {
        let fake = FakeHerdr::start(vec![
            resumable_agent("w1:p1", "idle", 3),
            resumable_agent("w1:p2", "idle", 4),
        ]);
        let (mut herdr, store) = connected(&fake);
        let key = "session:claude:id:id-w1:p1";
        perform(&mut herdr, &store, key, Action::ToggleSettle).unwrap();
        store
            .transact(|state| {
                let record = state.records.get_mut(key).unwrap();
                record.live = false;
            })
            .unwrap();

        let performed = perform(&mut herdr, &store, key, Action::ToggleSettle).unwrap();
        assert!(performed.reopened);
        assert_eq!(
            fake.take("tab.create"),
            [json!({"workspace_id": "w1", "cwd": "/tmp", "focus": true})]
        );
        assert_eq!(
            fake.take("pane.send_input"),
            [json!({"pane_id": "w1:p91", "text": "claude --resume id-w1:p1", "keys": ["Enter"]})]
        );

        perform(&mut herdr, &store, key, Action::Reopen).unwrap();
        assert!(
            fake.take("tab.create").is_empty(),
            "a pending reopen is focused, not repeated"
        );
        assert_eq!(fake.take("pane.focus"), [json!({"pane_id": "w1:p91"})]);
    }

    #[test]
    fn undoing_a_settle_that_closed_the_pane_reopens_the_thread() {
        let fake = FakeHerdr::start(vec![
            resumable_agent("w1:p1", "idle", 3),
            resumable_agent("w1:p2", "idle", 4),
        ]);
        let (mut herdr, store) = connected(&fake);
        let key = "session:claude:id:id-w1:p1";
        perform(&mut herdr, &store, key, Action::ToggleSettle).unwrap();
        assert_eq!(fake.take("pane.close"), [json!({"pane_id": "w1:p1"})]);
        fake.agents.lock().unwrap().remove(0);

        let performed = carry_out(&mut herdr, &store, |state, _| state.undo(now())).unwrap();
        assert!(performed.reopened);
        assert_eq!(
            fake.take("pane.send_input"),
            [json!({"pane_id": "w1:p91", "text": "claude --resume id-w1:p1", "keys": ["Enter"]})]
        );
        let parked = store.transact(|state| state.records[key].park).unwrap();
        assert_eq!(parked, None);
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
        serde_json::from_value(serde_json::json!({
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

    #[test]
    fn an_action_on_a_thread_herdr_has_since_rekeyed_lands_on_it() {
        let fake = FakeHerdr::start(vec![
            resumable_agent("w1:p1", "idle", 3),
            resumable_agent("w1:p2", "idle", 4),
        ]);
        let (mut herdr, store) = connected(&fake);
        let performed =
            perform(&mut herdr, &store, "terminal:t-w1:p1", Action::ToggleSettle).unwrap();
        assert_eq!(performed.refusal, None);
        assert_eq!(performed.key, "session:claude:id:id-w1:p1");
        let parked = store
            .transact(|state| state.records["session:claude:id:id-w1:p1"].park)
            .unwrap();
        assert_eq!(parked, Some(Park::Settled));
    }

    #[test]
    fn a_session_left_over_from_another_agent_is_not_trusted() {
        let mut stale = resumable_agent("w1:p1", "idle", 3);
        stale["agent"] = json!("codex");
        let fake = FakeHerdr::start(vec![stale, resumable_agent("w1:p2", "idle", 4)]);
        let (mut herdr, store) = connected(&fake);
        let keys = store
            .transact(|state| state.records.keys().cloned().collect::<Vec<_>>())
            .and_then(|_| {
                sync(&mut herdr, &store, false, |state, _| {
                    state.records.keys().cloned().collect::<Vec<_>>()
                })
            })
            .unwrap()
            .1;
        assert!(keys.contains(&"terminal:t-w1:p1".to_owned()), "{keys:?}");
        perform(&mut herdr, &store, "terminal:t-w1:p1", Action::ToggleSettle).unwrap();
        assert!(
            fake.take("pane.close").is_empty(),
            "a codex agent is not closed as a claude thread"
        );
    }

    #[test]
    fn a_failed_resume_closes_the_tab_it_opened_and_can_be_retried() {
        let fake = FakeHerdr::start(vec![
            resumable_agent("w1:p1", "idle", 3),
            resumable_agent("w1:p2", "idle", 4),
        ]);
        let (mut herdr, store) = connected(&fake);
        let key = "session:claude:id:id-w1:p1";
        perform(&mut herdr, &store, key, Action::ToggleSettle).unwrap();
        *fake.fail_method.lock().unwrap() = Some("pane.send_input".into());
        assert!(perform(&mut herdr, &store, key, Action::Reopen).is_err());
        assert_eq!(
            fake.take("pane.close").last(),
            Some(&json!({"pane_id": "w1:p91"}))
        );

        *fake.fail_method.lock().unwrap() = None;
        fake.take("tab.create");
        perform(&mut herdr, &store, key, Action::Reopen).unwrap();
        assert_eq!(
            fake.take("tab.create").len(),
            1,
            "the retry opens a fresh tab"
        );
    }

    #[test]
    fn a_pending_reopen_from_another_server_is_not_focused() {
        let fake = FakeHerdr::start(vec![
            resumable_agent("w1:p1", "idle", 3),
            resumable_agent("w1:p2", "idle", 4),
        ]);
        let (mut herdr, store) = connected(&fake);
        let key = "session:claude:id:id-w1:p1";
        perform(&mut herdr, &store, key, Action::ToggleSettle).unwrap();
        perform(&mut herdr, &store, key, Action::Reopen).unwrap();
        store
            .transact(|state| {
                let record = state.records.get_mut(key).unwrap();
                record.live = false;
                record.reopen_generation = Some("an earlier server".into());
            })
            .unwrap();
        fake.take("tab.create");
        perform(&mut herdr, &store, key, Action::Reopen).unwrap();
        assert!(fake.take("pane.focus").is_empty());
        assert_eq!(fake.take("tab.create").len(), 1);
    }

    #[test]
    fn reopening_codex_tells_herdr_its_session_right_away() {
        let mut codex = fake_herdr::agent("w1:p1", "idle", 3);
        codex["agent"] = json!("codex");
        codex["agent_session"] = json!({
            "source": "herdr:codex", "agent": "codex", "kind": "id", "value": "0199-abc"
        });
        let fake = FakeHerdr::start(vec![codex, resumable_agent("w1:p2", "idle", 4)]);
        let (mut herdr, store) = connected(&fake);
        let key = "session:codex:id:0199-abc";
        perform(&mut herdr, &store, key, Action::ToggleSettle).unwrap();
        perform(&mut herdr, &store, key, Action::Reopen).unwrap();
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
    fn a_sync_requested_while_another_command_held_the_store_still_happens() {
        let fake = FakeHerdr::start(vec![resumable_agent("w1:p1", "idle", 3)]);
        let (mut herdr, store) = connected(&fake);
        sync(&mut herdr, &store, false, |_, _| ()).unwrap();
        fake.take("agent.list");
        store.request_sync().unwrap();
        sync(&mut herdr, &store, false, |_, _| ()).unwrap();
        assert!(!store.sync_requested(), "the request was served");
        assert_eq!(
            fake.take("agent.list").len(),
            1,
            "a request made before the sync is covered by it"
        );
    }

    #[test]
    fn a_failed_codex_session_report_closes_the_tab_it_opened() {
        let mut codex = fake_herdr::agent("w1:p1", "idle", 3);
        codex["agent"] = json!("codex");
        codex["agent_session"] = json!({
            "source": "herdr:codex", "agent": "codex", "kind": "id", "value": "0199-abc"
        });
        let fake = FakeHerdr::start(vec![codex, resumable_agent("w1:p2", "idle", 4)]);
        let (mut herdr, store) = connected(&fake);
        let key = "session:codex:id:0199-abc";
        perform(&mut herdr, &store, key, Action::ToggleSettle).unwrap();
        *fake.fail_method.lock().unwrap() = Some("pane.report_agent_session".into());
        assert!(perform(&mut herdr, &store, key, Action::Reopen).is_err());
        assert_eq!(
            fake.take("pane.close").last(),
            Some(&json!({"pane_id": "w1:p91"}))
        );
    }

    #[test]
    fn a_failing_sync_still_serves_waiting_hook_requests_and_publishes_what_it_can() {
        let fake = FakeHerdr::start(vec![
            resumable_agent("w1:p1", "idle", 3),
            resumable_agent("w1:p2", "idle", 4),
        ]);
        let (mut herdr, store) = connected(&fake);
        *fake.fail_method.lock().unwrap() = Some("pane.report_metadata".into());
        store.request_sync().unwrap();
        assert!(sync(&mut herdr, &store, false, |_, _| ()).is_err());
        assert!(
            !store.sync_requested(),
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
        let fake = FakeHerdr::start(vec![
            resumable_agent("w1:p1", "idle", 3),
            resumable_agent("w1:p2", "idle", 4),
        ]);
        let (mut herdr, store) = connected(&fake);
        let key = "session:claude:id:id-w1:p1";
        perform(&mut herdr, &store, key, Action::ToggleSettle).unwrap();
        *fake.fail_method.lock().unwrap() = Some("pane.send_input".into());
        let request = std::fs::read_dir(fake.state_dir())
            .map(|_| fake.state_dir().join("sync.requested"))
            .unwrap();
        *fake.touch_on.lock().unwrap() = Some(("tab.create".into(), request));
        assert!(perform(&mut herdr, &store, key, Action::Reopen).is_err());
        assert!(
            !store.sync_requested(),
            "a hook that fired during the reopen is served"
        );
    }
}
