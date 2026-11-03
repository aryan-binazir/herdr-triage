use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};

use chrono::TimeZone;
use serde::{Deserialize, Serialize};

const CLOSED_RETENTION_SECS: i64 = 30 * 86_400;
const EXIT_GRACE_SECS: i64 = 10;
const REOPEN_PENDING_SECS: i64 = 60;
/// Herdr restores agents after a restart by relaunching them, which changes their state without
/// anyone using them; changes this soon after a restart are not new activity.
const RESTORE_SETTLE_SECS: i64 = 20;
const REOPEN_ADOPT_SECS: i64 = 300;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Idle,
    Working,
    Blocked,
    Done,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRef {
    pub source: String,
    pub kind: String,
    pub value: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thread {
    pub label: String,
    pub agent: Option<String>,
    pub cwd: Option<String>,
    pub workspace_id: Option<String>,
    pub workspace_label: Option<String>,
    pub session: Option<SessionRef>,
}

impl Thread {
    pub fn resume_argv(&self) -> Option<Vec<String>> {
        let session = self.session.as_ref()?;
        let agent = self.agent.as_deref()?;
        if session.source != format!("herdr:{agent}") || session.value.is_empty() {
            return None;
        }
        let value = session.value.clone();
        match (agent, session.kind.as_str()) {
            ("claude", "id") => Some(vec!["claude".into(), "--resume".into(), value]),
            ("codex", "id") => Some(vec!["codex".into(), "resume".into(), value]),
            ("pi", "id") => Some(vec!["pi".into(), "--session".into(), value]),
            ("pi", "path") if value.starts_with('/') => {
                Some(vec!["pi".into(), "--session".into(), value])
            }
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Observed {
    pub key: String,
    pub pre_session_key: Option<String>,
    pub pane_id: String,
    pub status: Status,
    pub seq: u64,
    pub focused: bool,
    pub thread: Thread,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Park {
    Settled,
    Snoozed { until: i64 },
}

impl Park {
    pub fn section(self) -> Section {
        match self {
            Park::Snoozed { .. } => Section::Snoozed,
            Park::Settled => Section::Settled,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Section {
    Snoozed,
    Settled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Unread {
    Manual,
    Woke,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub park: Option<Park>,
    #[serde(default)]
    pub unread: Option<Unread>,
    #[serde(default)]
    pub ack_seq: Option<u64>,
    /// Herdr's change counter restarts with the server, so it is only comparable within one generation.
    pub generation: String,
    pub last_seq: u64,
    #[serde(default)]
    pub changed_at: Option<i64>,
    #[serde(default, alias = "last_seen")]
    pub touched_at: i64,
    #[serde(default = "live_by_default")]
    pub live: bool,
    #[serde(default)]
    pub ended: bool,
    #[serde(default)]
    pub missing_since: Option<i64>,
    #[serde(default)]
    pub last_status: Status,
    #[serde(default)]
    pub thread: Thread,
    #[serde(default)]
    pub reopen_pane: Option<String>,
    #[serde(default)]
    pub reopen_at: Option<i64>,
    /// Pane ids are reused across Herdr servers.
    #[serde(default)]
    pub reopen_generation: Option<String>,
    #[serde(default)]
    pub restored_at: Option<i64>,
}

fn live_by_default() -> bool {
    true
}

impl Record {
    fn touch(&mut self, now: i64) {
        self.touched_at = self.touched_at.max(now);
    }

    fn resumable(&self) -> bool {
        self.thread.resume_argv().is_some()
    }

    fn end_session(&mut self) {
        let unseen_done = self.last_status == Status::Done && self.ack_seq != Some(self.last_seq);
        if self.park.is_none() {
            if self.unread.is_some() || unseen_done {
                self.unread.get_or_insert(Unread::Manual);
            } else {
                self.park = Some(Park::Settled);
            }
        }
        self.live = false;
        self.ended = true;
        self.missing_since = None;
    }
}

#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub records: BTreeMap<String, Record>,
    #[serde(default)]
    pub open_sections: BTreeSet<Section>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Display {
    Blocked,
    Working,
    Done,
    Unread(Unread),
    Idle,
    Unknown,
}

impl Display {
    pub fn busy(self) -> Option<&'static str> {
        match self {
            Display::Working => Some("working"),
            Display::Blocked => Some("blocked"),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    ToggleSettle,
    Snooze(i64),
    Wake,
    ToggleUnread,
    TogglePin,
    Reopen,
    Forget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal(pub String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum After {
    Nothing,
    ClosePane,
    Reopen(Box<ReopenPlan>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReopenPlan {
    pub key: String,
    pub argv: Vec<String>,
    pub thread: Thread,
    pub pending_pane: Option<String>,
    pub pending_generation: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Row {
    pub agent_index: Option<usize>,
    pub key: String,
    pub seq: u64,
    pub display: Display,
    pub park: Option<Park>,
    pub pinned: bool,
    pub changed_at: Option<i64>,
    pub thread: Thread,
}

impl Row {
    pub fn live(&self) -> bool {
        self.agent_index.is_some()
    }

    pub fn status_text<Tz: TimeZone>(&self, now: i64, tz: &Tz) -> String
    where
        Tz::Offset: std::fmt::Display,
    {
        let text = self.park_text(now, tz).unwrap_or_else(|| {
            let word = match self.display {
                Display::Blocked => "blocked",
                Display::Working => "working",
                Display::Done => "done",
                Display::Unread(Unread::Manual) => return "unread".into(),
                Display::Unread(Unread::Woke) => return "woke".into(),
                Display::Idle => "idle",
                Display::Unknown => "unknown",
            };
            match self.changed_at {
                Some(at) if self.live() => format!("{word} {}", age(now - at)),
                _ => word.into(),
            }
        });
        match (self.live(), self.park) {
            (false, _) => format!("{text} · closed"),
            (true, Some(_)) => format!("{text} · stays open"),
            (true, None) => text,
        }
    }

    pub fn sidebar_label<Tz: TimeZone>(&self, pin_icon: &str, now: i64, tz: &Tz) -> Option<String>
    where
        Tz::Offset: std::fmt::Display,
    {
        let triage = self.park_text(now, tz).or(match self.display {
            Display::Unread(Unread::Manual) => Some("unread".into()),
            Display::Unread(Unread::Woke) => Some("woke".into()),
            _ => None,
        });
        let pin = (self.pinned && self.park.is_none()).then(|| pin_icon.to_owned());
        let label = [pin, triage]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ");
        (!label.is_empty()).then_some(label)
    }

    pub fn rank(&self) -> u8 {
        match (self.park, self.pinned) {
            (Some(Park::Settled), _) => 3,
            (Some(Park::Snoozed { .. }), _) => 2,
            (None, true) => 0,
            (None, false) => 1,
        }
    }

    fn park_text<Tz: TimeZone>(&self, now: i64, tz: &Tz) -> Option<String>
    where
        Tz::Offset: std::fmt::Display,
    {
        match self.park? {
            Park::Settled => Some("settled".into()),
            Park::Snoozed { until } => Some(format!("snoozed → {}", wake_time(until, now, tz))),
        }
    }
}

fn age(secs: i64) -> String {
    match secs.max(0) {
        s if s < 60 => "<1m".into(),
        s if s < 3_600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3_600),
        s => format!("{}d", s / 86_400),
    }
}

fn local<Tz: TimeZone>(at: i64, tz: &Tz) -> chrono::DateTime<Tz> {
    chrono::DateTime::from_timestamp(at, 0)
        .unwrap_or_default()
        .with_timezone(tz)
}

fn wake_time<Tz: TimeZone>(until: i64, now: i64, tz: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let (until, now) = (local(until, tz), local(now, tz));
    let days = (until.date_naive() - now.date_naive()).num_days();
    match days {
        0 => until.format("%H:%M").to_string(),
        1 => until.format("tmrw %H:%M").to_string(),
        _ => until.format("%a %H:%M").to_string(),
    }
}

pub fn snooze_presets<Tz: TimeZone>(now: i64, tz: &Tz) -> Vec<(String, i64)> {
    let mut presets = vec![
        ("30m".to_owned(), now + 1_800),
        ("1h".to_owned(), now + 3_600),
        ("3h".to_owned(), now + 10_800),
    ];
    let tomorrow = local(now, tz).date_naive().succ_opt();
    if let Some(morning) = tomorrow
        .and_then(|day| day.and_hms_opt(9, 0, 0))
        .and_then(|at| tz.from_local_datetime(&at).earliest())
    {
        presets.push(("tomorrow 09:00".to_owned(), morning.timestamp()));
    }
    presets
}

fn display(agent: &Observed, record: Option<&Record>) -> Display {
    match agent.status {
        Status::Blocked => Display::Blocked,
        Status::Working => Display::Working,
        status => {
            if let Some(unread) = record.and_then(|r| r.unread) {
                return Display::Unread(unread);
            }
            match status {
                Status::Done if record.and_then(|r| r.ack_seq) != Some(agent.seq) => Display::Done,
                Status::Unknown => Display::Unknown,
                _ => Display::Idle,
            }
        }
    }
}

fn closed_display(record: &Record) -> Display {
    record.unread.map_or(Display::Idle, Display::Unread)
}

impl State {
    /// Returns when agents that just went missing must be checked again, since nothing else may
    /// sync once the exit grace period is over.
    pub fn reconcile(&mut self, generation: &str, agents: &[Observed], now: i64) -> Vec<i64> {
        let mut exit_checks = Vec::new();
        for agent in agents {
            self.adopt_previous_key(agent, generation, now);
            let existed = self.records.contains_key(&agent.key);
            let record = self
                .records
                .entry(agent.key.clone())
                .or_insert_with(|| Record {
                    generation: generation.to_owned(),
                    last_seq: agent.seq,
                    ..Record::default()
                });
            // A thread that ended or is being reopened comes back by choice, not by Herdr's restore.
            let returned_by_choice = existed && (record.ended || record.reopen_pane.is_some());
            if existed && record.ended {
                record.park = None;
            }
            let restoring = record
                .restored_at
                .is_some_and(|at| now - at < RESTORE_SETTLE_SECS);
            if record.generation != generation || agent.seq < record.last_seq {
                generation.clone_into(&mut record.generation);
                record.ack_seq = None;
                record.restored_at = (!returned_by_choice).then_some(now);
            } else if agent.seq > record.last_seq && !restoring {
                record.park = None;
                record.unread = None;
                record.changed_at = Some(now);
            }
            record.last_seq = agent.seq;
            record.last_status = agent.status;
            record.thread = agent.thread.clone();
            record.live = true;
            record.ended = false;
            record.missing_since = None;
            record.reopen_pane = None;
            record.reopen_at = None;
            record.reopen_generation = None;
            if matches!(record.park, Some(Park::Snoozed { until }) if until <= now) {
                record.park = None;
                record.unread = Some(Unread::Woke);
            }
            if agent.focused && record.unread == Some(Unread::Woke) {
                record.unread = None;
            }
            record.touch(now);
        }
        let present: BTreeSet<&str> = agents.iter().map(|agent| agent.key.as_str()).collect();
        for (key, record) in &mut self.records {
            if present.contains(key.as_str()) || !record.live {
                continue;
            }
            if record.generation != generation {
                record.live = false;
                continue;
            }
            let since = *record.missing_since.get_or_insert_with(|| {
                if !exit_checks.contains(&(now + EXIT_GRACE_SECS)) {
                    exit_checks.push(now + EXIT_GRACE_SECS);
                }
                now
            });
            if now - since >= EXIT_GRACE_SECS {
                record.end_session();
            }
        }
        for record in self.records.values_mut() {
            if matches!(record.park, Some(Park::Snoozed { until }) if until <= now) {
                record.park = None;
                record.unread = Some(Unread::Woke);
            }
        }
        self.records.retain(|_, record| {
            record.live
                || (record.resumable()
                    && (record.pinned || now - record.touched_at <= CLOSED_RETENTION_SECS))
        });
        exit_checks
    }

    fn adopt_previous_key(&mut self, agent: &Observed, generation: &str, now: i64) {
        if self.records.contains_key(&agent.key) {
            return;
        }
        let reopened_here = || {
            self.records
                .iter()
                .find(|(_, r)| {
                    !r.live
                        && agent.thread.session.is_some()
                        && r.thread.agent == agent.thread.agent
                        && r.reopen_pane.as_deref() == Some(agent.pane_id.as_str())
                        && r.reopen_generation.as_deref() == Some(generation)
                        && r.reopen_at.is_some_and(|at| now - at <= REOPEN_ADOPT_SECS)
                })
                .map(|(key, _)| key.clone())
        };
        if let Some(reopened) = reopened_here() {
            if let Some(provisional) = &agent.pre_session_key {
                self.records.remove(provisional);
            }
            if let Some(record) = self.records.remove(&reopened) {
                self.records.insert(agent.key.clone(), record);
            }
            return;
        }
        let previous = agent
            .pre_session_key
            .clone()
            .filter(|key| self.records.contains_key(key));
        if let Some(record) = previous.and_then(|key| self.records.remove(&key)) {
            self.records.insert(agent.key.clone(), record);
        }
    }

    pub fn toggle_section(&mut self, section: Section) {
        if !self.open_sections.remove(&section) {
            self.open_sections.insert(section);
        }
    }

    pub fn reopening(&mut self, key: &str, pane_id: &str, generation: &str, now: i64) {
        if let Some(record) = self.records.get_mut(key) {
            record.reopen_pane = Some(pane_id.to_owned());
            record.reopen_at = Some(now);
            record.reopen_generation = Some(generation.to_owned());
        }
    }

    pub fn session_closed(&mut self, key: &str) {
        if let Some(record) = self.records.get_mut(key) {
            record.live = false;
            record.ended = true;
            record.missing_since = None;
        }
    }

    pub fn pending_wakes(&self) -> Vec<i64> {
        self.records
            .values()
            .filter_map(|record| match record.park {
                Some(Park::Snoozed { until }) => Some(until),
                _ => None,
            })
            .collect()
    }

    pub fn apply(
        &mut self,
        key: &str,
        live_agent: Option<&Observed>,
        action: Action,
        now: i64,
    ) -> Result<After, Refusal> {
        let gone = || Refusal("that thread is gone".into());
        let shown = match live_agent {
            Some(agent) => display(agent, self.records.get(key)),
            None => closed_display(self.records.get(key).ok_or_else(gone)?),
        };
        if action == Action::Forget {
            if live_agent.is_some() {
                return Err(Refusal("only closed threads can be forgotten".into()));
            }
            self.records.remove(key);
            return Ok(After::Nothing);
        }
        let record = match live_agent {
            Some(agent) => self
                .records
                .entry(key.to_owned())
                .or_insert_with(|| Record {
                    last_seq: agent.seq,
                    live: true,
                    ..Record::default()
                }),
            None => self.records.get_mut(key).ok_or_else(gone)?,
        };
        record.touch(now);
        let seq = live_agent.map_or(record.last_seq, |agent| agent.seq);
        if let Some(busy) = shown.busy()
            && matches!(
                action,
                Action::ToggleSettle | Action::Snooze(_) | Action::ToggleUnread
            )
        {
            let verb = match action {
                Action::ToggleSettle => "settle",
                Action::Snooze(_) => "snooze",
                _ => "mark unread",
            };
            return Err(Refusal(format!("can't {verb} a {busy} agent")));
        }
        let live = live_agent.is_some();
        let unpark = |record: &mut Record| {
            record.park = None;
            if live {
                Ok(After::Nothing)
            } else {
                reopen_plan(key, record, now)
            }
        };
        match action {
            Action::TogglePin => {
                record.pinned = !record.pinned;
                Ok(After::Nothing)
            }
            Action::Wake | Action::Reopen => unpark(record),
            Action::ToggleSettle if record.park == Some(Park::Settled) => unpark(record),
            Action::ToggleSettle => Ok(park(record, seq, Park::Settled, live)),
            Action::Snooze(until) => Ok(park(record, seq, Park::Snoozed { until }, live)),
            Action::ToggleUnread if matches!(shown, Display::Done | Display::Unread(_)) => {
                record.unread = None;
                record.ack_seq = Some(seq);
                Ok(After::Nothing)
            }
            Action::ToggleUnread => {
                record.unread = Some(Unread::Manual);
                record.park = None;
                Ok(After::Nothing)
            }
            Action::Forget => Ok(After::Nothing),
        }
    }

    pub fn ordered(&self, agents: &[Observed]) -> Vec<Row> {
        let live = agents.iter().enumerate().map(|(index, agent)| {
            let record = self.records.get(&agent.key);
            Row {
                agent_index: Some(index),
                key: agent.key.clone(),
                seq: agent.seq,
                display: display(agent, record),
                park: record.and_then(|r| r.park),
                pinned: record.is_some_and(|r| r.pinned),
                changed_at: record.and_then(|r| r.changed_at),
                thread: agent.thread.clone(),
            }
        });
        let closed = self
            .records
            .iter()
            .filter(|(_, r)| !r.live)
            .map(|(key, record)| Row {
                agent_index: None,
                key: key.clone(),
                seq: record.last_seq,
                display: closed_display(record),
                park: record.park,
                pinned: record.pinned,
                changed_at: record.changed_at,
                thread: record.thread.clone(),
            });
        let mut rows: Vec<Row> = live.chain(closed).collect();
        rows.sort_by_key(|row| {
            let recency = if row.live() {
                row.seq
            } else {
                u64::try_from(row.changed_at.unwrap_or(0)).unwrap_or(0)
            };
            (row.rank(), !row.live(), Reverse(recency), Reverse(row.seq))
        });
        rows
    }
}

fn reopen_plan(key: &str, record: &Record, now: i64) -> Result<After, Refusal> {
    let argv = record
        .thread
        .resume_argv()
        .ok_or_else(|| Refusal("this thread has no session to resume".into()))?;
    let pending_pane = record.reopen_pane.clone().filter(|_| {
        record
            .reopen_at
            .is_some_and(|at| now - at < REOPEN_PENDING_SECS)
    });
    Ok(After::Reopen(Box::new(ReopenPlan {
        key: key.to_owned(),
        argv,
        thread: record.thread.clone(),
        pending_pane,
        pending_generation: record.reopen_generation.clone(),
    })))
}

pub fn next_attention<'a>(rows: &'a [Row], current: Option<&str>) -> Option<&'a Row> {
    let open = |row: &&Row| row.park.is_none() && row.live();
    let queue: Vec<&Row> = rows
        .iter()
        .filter(open)
        .filter(|row| row.display == Display::Blocked)
        .chain(
            rows.iter()
                .filter(open)
                .filter(|row| matches!(row.display, Display::Done | Display::Unread(_))),
        )
        .collect();
    let start = current
        .and_then(|key| queue.iter().position(|row| row.key == key))
        .map_or(0, |at| at + 1);
    queue
        .iter()
        .cycle()
        .skip(start)
        .take(queue.len())
        .find(|row| Some(row.key.as_str()) != current)
        .copied()
}

fn park(record: &mut Record, seq: u64, park: Park, live: bool) -> After {
    record.park = Some(park);
    record.unread = None;
    record.ack_seq = Some(seq);
    if live && record.resumable() {
        After::ClosePane
    } else {
        After::Nothing
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GEN: &str = "g1";
    const NOW: i64 = 1_000_000;

    fn act(state: &mut State, agent: &Observed, action: Action) -> Result<After, Refusal> {
        state.apply(&agent.key, Some(agent), action, 0)
    }

    fn resumable(key: &str, status: Status, seq: u64) -> Observed {
        Observed {
            thread: Thread {
                label: format!("thread {key}"),
                agent: Some("claude".into()),
                workspace_id: Some("w1".into()),
                session: Some(SessionRef {
                    source: "herdr:claude".into(),
                    kind: "id".into(),
                    value: format!("id-{key}"),
                }),
                ..Thread::default()
            },
            ..agent(key, status, seq)
        }
    }

    fn agent(key: &str, status: Status, seq: u64) -> Observed {
        Observed {
            pane_id: format!("pane-{key}"),
            thread: Thread::default(),
            key: key.into(),
            pre_session_key: None,
            status,
            seq,
            focused: false,
        }
    }

    fn keys(state: &State, agents: &[Observed]) -> Vec<String> {
        state
            .ordered(agents)
            .iter()
            .map(|row| row.key.clone())
            .collect()
    }

    #[test]
    fn open_rows_sort_by_most_recent_change_regardless_of_status() {
        let agents = [
            agent("old-blocked", Status::Blocked, 3),
            agent("new-working", Status::Working, 9),
            agent("mid-done", Status::Done, 6),
        ];
        let mut state = State::default();
        state.reconcile(GEN, &agents, NOW);
        assert_eq!(
            keys(&state, &agents),
            ["new-working", "mid-done", "old-blocked"]
        );
    }

    #[test]
    fn pinned_rows_lead_and_parked_rows_sink_snoozed_above_settled() {
        let agents = [
            agent("settled", Status::Idle, 9),
            agent("snoozed", Status::Done, 8),
            agent("open", Status::Idle, 7),
            agent("pinned", Status::Idle, 1),
            agent("pinned-settled", Status::Idle, 2),
        ];
        let mut state = State::default();
        state.reconcile(GEN, &agents, NOW);
        act(&mut state, &agents[0], Action::ToggleSettle).unwrap();
        act(&mut state, &agents[1], Action::Snooze(NOW + 60)).unwrap();
        act(&mut state, &agents[3], Action::TogglePin).unwrap();
        act(&mut state, &agents[4], Action::TogglePin).unwrap();
        act(&mut state, &agents[4], Action::ToggleSettle).unwrap();
        assert_eq!(
            keys(&state, &agents),
            ["pinned", "open", "snoozed", "settled", "pinned-settled"]
        );
    }

    #[test]
    fn settle_and_snooze_are_refused_while_working_or_blocked() {
        let agents = [
            agent("w", Status::Working, 1),
            agent("b", Status::Blocked, 2),
        ];
        let mut state = State::default();
        state.reconcile(GEN, &agents, NOW);
        for a in &agents {
            assert!(act(&mut state, a, Action::ToggleSettle).is_err());
            assert!(act(&mut state, a, Action::Snooze(NOW + 60)).is_err());
            assert!(act(&mut state, a, Action::ToggleUnread).is_err());
        }
        assert!(act(&mut state, &agents[0], Action::TogglePin).is_ok());
    }

    fn shown(state: &State, agent: &Observed) -> (Display, Option<Park>) {
        let rows = state.ordered(std::slice::from_ref(agent));
        let row = rows.into_iter().find(|row| row.key == agent.key).unwrap();
        (row.display, row.park)
    }

    #[test]
    fn new_activity_unparks_and_clears_unread() {
        let idle = agent("a", Status::Idle, 4);
        let other = agent("b", Status::Idle, 4);
        let mut state = State::default();
        state.reconcile(GEN, &[idle.clone(), other.clone()], NOW);
        act(&mut state, &idle, Action::ToggleSettle).unwrap();
        act(&mut state, &other, Action::ToggleUnread).unwrap();

        let after = [agent("a", Status::Idle, 6), agent("b", Status::Working, 5)];
        state.reconcile(GEN, &after, NOW + 1);
        assert_eq!(shown(&state, &after[0]), (Display::Idle, None));
        assert_eq!(shown(&state, &after[1]), (Display::Working, None));
        let back = agent("b", Status::Idle, 6);
        state.reconcile(GEN, std::slice::from_ref(&back), NOW + 2);
        assert_eq!(shown(&state, &back), (Display::Idle, None));
    }

    #[test]
    fn snooze_expiry_wakes_as_unread_and_focus_clears_only_woke() {
        let done = agent("a", Status::Done, 4);
        let mut state = State::default();
        state.reconcile(GEN, std::slice::from_ref(&done), NOW);
        act(&mut state, &done, Action::Snooze(NOW + 60)).unwrap();

        state.reconcile(GEN, std::slice::from_ref(&done), NOW + 59);
        assert_eq!(
            shown(&state, &done),
            (Display::Idle, Some(Park::Snoozed { until: NOW + 60 }))
        );

        state.reconcile(GEN, std::slice::from_ref(&done), NOW + 60);
        assert_eq!(shown(&state, &done), (Display::Unread(Unread::Woke), None));

        let focused = Observed {
            focused: true,
            ..done.clone()
        };
        state.reconcile(GEN, std::slice::from_ref(&focused), NOW + 61);
        assert_eq!(shown(&state, &done), (Display::Idle, None));

        act(&mut state, &done, Action::ToggleUnread).unwrap();
        state.reconcile(GEN, std::slice::from_ref(&focused), NOW + 63);
        assert_eq!(
            shown(&state, &done),
            (Display::Unread(Unread::Manual), None)
        );
    }

    #[test]
    fn activity_beats_a_snooze_expiring_at_the_same_time() {
        let idle = agent("a", Status::Idle, 4);
        let mut state = State::default();
        state.reconcile(GEN, std::slice::from_ref(&idle), NOW);
        act(&mut state, &idle, Action::Snooze(NOW + 60)).unwrap();
        let working = agent("a", Status::Working, 5);
        state.reconcile(GEN, std::slice::from_ref(&working), NOW + 120);
        assert_eq!(shown(&state, &working), (Display::Working, None));
    }

    #[test]
    fn a_new_server_generation_keeps_triage_but_forgets_acknowledgements() {
        let done = agent("a", Status::Done, 40);
        let mut state = State::default();
        state.reconcile(GEN, std::slice::from_ref(&done), NOW);
        act(&mut state, &done, Action::ToggleUnread).unwrap();
        assert_eq!(shown(&state, &done).0, Display::Idle);
        let other = agent("b", Status::Idle, 41);
        state.reconcile(GEN, std::slice::from_ref(&other), NOW);
        act(&mut state, &other, Action::ToggleSettle).unwrap();

        let restarted = [agent("a", Status::Done, 2), agent("b", Status::Idle, 2)];
        state.reconcile("g2", &restarted, NOW + 5);
        assert_eq!(shown(&state, &restarted[0]), (Display::Done, None));
        assert_eq!(
            shown(&state, &restarted[1]),
            (Display::Idle, Some(Park::Settled))
        );
        let later = agent("b", Status::Working, 3);
        state.reconcile(
            "g2",
            std::slice::from_ref(&later),
            NOW + 6 + RESTORE_SETTLE_SECS,
        );
        assert_eq!(shown(&state, &later).1, None);
    }

    #[test]
    fn closed_threads_untouched_for_thirty_days_are_pruned_unless_pinned() {
        let (a, b) = (
            resumable("a", Status::Idle, 1),
            resumable("b", Status::Idle, 1),
        );
        let mut state = State::default();
        state.reconcile(GEN, &[a.clone(), b.clone()], NOW);
        act(&mut state, &b, Action::TogglePin).unwrap();
        state.reconcile(GEN, &[], NOW + 1);
        state.reconcile(GEN, &[], NOW + 20);
        state.reconcile(GEN, &[], NOW + 30 * 86_400 - 1);
        assert_eq!(state.records.len(), 2);
        state.reconcile(GEN, &[], NOW + 30 * 86_400 + 21);
        assert_eq!(state.records.keys().collect::<Vec<_>>(), ["b"]);
    }

    #[test]
    fn next_attention_visits_blocked_then_done_or_unread_and_cycles() {
        let agents = [
            agent("done-new", Status::Done, 9),
            agent("blocked-old", Status::Blocked, 2),
            agent("working", Status::Working, 8),
            agent("unread", Status::Idle, 7),
            agent("blocked-new", Status::Blocked, 6),
            agent("snoozed-done", Status::Done, 5),
        ];
        let mut state = State::default();
        state.reconcile(GEN, &agents, NOW);
        act(&mut state, &agents[3], Action::ToggleUnread).unwrap();
        act(&mut state, &agents[5], Action::Snooze(NOW + 60)).unwrap();
        let rows = state.ordered(&agents);
        let next = |current: Option<&str>| next_attention(&rows, current).map(|r| r.key.clone());

        assert_eq!(next(None).as_deref(), Some("blocked-new"));
        assert_eq!(next(Some("working")).as_deref(), Some("blocked-new"));
        assert_eq!(next(Some("blocked-new")).as_deref(), Some("blocked-old"));
        assert_eq!(next(Some("blocked-old")).as_deref(), Some("done-new"));
        assert_eq!(next(Some("done-new")).as_deref(), Some("unread"));
        assert_eq!(next(Some("unread")).as_deref(), Some("blocked-new"));
    }

    #[test]
    fn next_attention_has_nothing_when_only_the_current_agent_needs_you() {
        let agents = [
            agent("blocked", Status::Blocked, 1),
            agent("idle", Status::Idle, 2),
        ];
        let mut state = State::default();
        state.reconcile(GEN, &agents, NOW);
        let rows = state.ordered(&agents);
        assert!(next_attention(&rows, Some("blocked")).is_none());
        assert!(next_attention(&rows, Some("idle")).is_some());
    }

    fn tz() -> chrono::FixedOffset {
        chrono::FixedOffset::east_opt(0).unwrap()
    }

    const ONE_PM: i64 = 1_793_710_800;

    #[test]
    fn picker_status_text_shows_state_and_age() {
        let agents = [
            agent("w", Status::Working, 2),
            agent("d", Status::Done, 2),
            agent("i", Status::Idle, 2),
            agent("fresh", Status::Blocked, 1),
        ];
        let mut state = State::default();
        state.reconcile(
            GEN,
            &[
                agent("w", Status::Idle, 1),
                agent("d", Status::Idle, 1),
                agent("i", Status::Idle, 1),
            ],
            ONE_PM - 7_200,
        );
        state.reconcile(GEN, &agents, ONE_PM - 240);
        let rows = state.ordered(&agents);
        let text = |key: &str| {
            rows.iter()
                .find(|r| r.key == key)
                .unwrap()
                .status_text(ONE_PM, &tz())
        };
        assert_eq!(text("w"), "working 4m");
        assert_eq!(text("d"), "done 4m");
        assert_eq!(text("i"), "idle 4m");
        assert_eq!(text("fresh"), "blocked");
    }

    #[test]
    fn parked_and_unread_labels_for_picker_and_sidebar() {
        let agents = [
            agent("snoozed", Status::Idle, 1),
            agent("settled", Status::Idle, 1),
            agent("pinned-unread", Status::Idle, 1),
            agent("pinned", Status::Working, 1),
            agent("plain", Status::Idle, 1),
            agent("tomorrow", Status::Idle, 1),
        ];
        let mut state = State::default();
        state.reconcile(GEN, &agents, ONE_PM);
        act(&mut state, &agents[0], Action::Snooze(ONE_PM + 5_400)).unwrap();
        act(&mut state, &agents[1], Action::ToggleSettle).unwrap();
        act(&mut state, &agents[2], Action::TogglePin).unwrap();
        act(&mut state, &agents[2], Action::ToggleUnread).unwrap();
        act(&mut state, &agents[3], Action::TogglePin).unwrap();
        act(&mut state, &agents[5], Action::Snooze(ONE_PM + 20 * 3_600)).unwrap();
        let rows = state.ordered(&agents);
        let row = |key: &str| rows.iter().find(|r| r.key == key).unwrap();

        assert_eq!(
            row("snoozed").status_text(ONE_PM, &tz()),
            "snoozed → 14:30 · stays open"
        );
        assert_eq!(
            row("tomorrow").status_text(ONE_PM, &tz()),
            "snoozed → tmrw 09:00 · stays open"
        );
        assert_eq!(
            row("settled").status_text(ONE_PM, &tz()),
            "settled · stays open"
        );
        assert_eq!(row("pinned-unread").status_text(ONE_PM, &tz()), "unread");

        let side = |key: &str| row(key).sidebar_label("P", ONE_PM, &tz());
        assert_eq!(side("snoozed").as_deref(), Some("snoozed → 14:30"));
        assert_eq!(side("settled").as_deref(), Some("settled"));
        assert_eq!(side("pinned-unread").as_deref(), Some("P unread"));
        assert_eq!(side("pinned").as_deref(), Some("P"));
        assert_eq!(side("plain"), None);
    }

    #[test]
    fn snooze_presets_end_at_fixed_offsets_and_tomorrow_morning() {
        let presets = snooze_presets(ONE_PM, &tz());
        let labels: Vec<&str> = presets.iter().map(|(label, _)| label.as_str()).collect();
        assert_eq!(labels, ["30m", "1h", "3h", "tomorrow 09:00"]);
        let untils: Vec<i64> = presets.iter().map(|(_, until)| *until).collect();
        assert_eq!(
            untils,
            [
                ONE_PM + 1_800,
                ONE_PM + 3_600,
                ONE_PM + 10_800,
                ONE_PM + 20 * 3_600
            ]
        );
    }

    #[test]
    fn triage_carries_over_when_herdr_first_reports_the_agents_session() {
        let by_terminal = agent("terminal:t1", Status::Idle, 4);
        let mut state = State::default();
        state.reconcile(GEN, std::slice::from_ref(&by_terminal), ONE_PM);
        act(&mut state, &by_terminal, Action::TogglePin).unwrap();
        act(&mut state, &by_terminal, Action::ToggleSettle).unwrap();

        let by_session = Observed {
            pre_session_key: Some("terminal:t1".into()),
            ..agent("session:claude:id:abc", Status::Idle, 4)
        };
        state.reconcile(GEN, std::slice::from_ref(&by_session), ONE_PM + 1);
        let row = state.ordered(std::slice::from_ref(&by_session)).remove(0);
        assert!(row.pinned);
        assert_eq!(row.park, Some(Park::Settled));
        assert!(!state.records.contains_key("terminal:t1"));
    }

    #[test]
    fn herdr_restoring_a_parked_agent_does_not_unpark_it() {
        // Herdr restores the session by relaunching it: the agent shows working, then idle.
        let kept_open = resumable("a", Status::Idle, 40);
        let mut state = State::default();
        state.reconcile(GEN, std::slice::from_ref(&kept_open), ONE_PM);
        act(&mut state, &kept_open, Action::ToggleSettle).unwrap();
        for (status, seq, at) in [
            (Status::Working, 1, 5),
            (Status::Idle, 2, 8),
            (Status::Idle, 3, 20),
        ] {
            let restoring = resumable("a", status, seq);
            state.reconcile("g2", std::slice::from_ref(&restoring), ONE_PM + at);
            assert_eq!(
                shown(&state, &restoring).1,
                Some(Park::Settled),
                "at +{at}s"
            );
        }
        let used_again = resumable("a", Status::Working, 4);
        state.reconcile("g2", std::slice::from_ref(&used_again), ONE_PM + 60);
        assert_eq!(
            shown(&state, &used_again).1,
            None,
            "real activity after the restore unparks"
        );
    }

    fn closed(state: &State, key: &str) -> Row {
        state
            .ordered(&[])
            .into_iter()
            .find(|row| row.key == key)
            .unwrap()
    }

    #[test]
    fn a_finished_thread_whose_session_ends_settles_as_closed_after_a_grace_period() {
        let idle = resumable("a", Status::Idle, 4);
        let mut state = State::default();
        state.reconcile(GEN, std::slice::from_ref(&idle), NOW);
        state.reconcile(GEN, &[], NOW + 1);
        assert!(
            state.ordered(&[]).is_empty(),
            "a missing agent is not closed yet"
        );
        state.reconcile(GEN, &[], NOW + 12);
        let row = closed(&state, "a");
        assert_eq!((row.agent_index, row.park), (None, Some(Park::Settled)));
        assert_eq!(row.status_text(NOW + 12, &tz()), "settled · closed");
    }

    #[test]
    fn a_thread_with_unseen_results_stays_open_as_unread_when_its_session_ends() {
        let done = resumable("a", Status::Done, 4);
        let mut state = State::default();
        state.reconcile(GEN, std::slice::from_ref(&done), NOW);
        state.reconcile(GEN, &[], NOW + 1);
        state.reconcile(GEN, &[], NOW + 12);
        let row = closed(&state, "a");
        assert_eq!(
            (row.display, row.park),
            (Display::Unread(Unread::Manual), None)
        );
        assert_eq!(row.status_text(NOW + 12, &tz()), "unread · closed");
    }

    #[test]
    fn agents_missing_after_a_server_restart_are_not_treated_as_exits() {
        let idle = resumable("a", Status::Idle, 4);
        let mut state = State::default();
        state.reconcile(GEN, std::slice::from_ref(&idle), NOW);
        state.reconcile("g2", &[], NOW + 1);
        state.reconcile("g2", &[], NOW + 60);
        let row = closed(&state, "a");
        assert_eq!(row.park, None);
        assert_eq!(row.status_text(NOW + 60, &tz()), "idle · closed");
    }

    #[test]
    fn a_brief_detection_gap_changes_nothing() {
        let idle = resumable("a", Status::Idle, 4);
        let mut state = State::default();
        state.reconcile(GEN, std::slice::from_ref(&idle), NOW);
        state.reconcile(GEN, &[], NOW + 5);
        state.reconcile(GEN, std::slice::from_ref(&idle), NOW + 8);
        state.reconcile(GEN, &[], NOW + 15);
        assert!(state.ordered(&[]).is_empty());
    }

    #[test]
    fn agents_that_cannot_be_resumed_are_forgotten_once_gone() {
        let plain = agent("terminal:t1", Status::Idle, 4);
        let mut state = State::default();
        state.reconcile(GEN, std::slice::from_ref(&plain), NOW);
        act(&mut state, &plain, Action::TogglePin).unwrap();
        state.reconcile(GEN, &[], NOW + 1);
        state.reconcile(GEN, &[], NOW + 12);
        assert!(state.records.is_empty());
    }

    #[test]
    fn a_closed_thread_that_comes_back_is_unparked() {
        let idle = resumable("a", Status::Idle, 4);
        let mut state = State::default();
        state.reconcile(GEN, std::slice::from_ref(&idle), NOW);
        state.reconcile(GEN, &[], NOW + 1);
        state.reconcile(GEN, &[], NOW + 12);
        state.reconcile(GEN, std::slice::from_ref(&idle), NOW + 30);
        assert_eq!(shown(&state, &idle), (Display::Idle, None));
    }

    #[test]
    fn settling_or_snoozing_a_resumable_agent_closes_its_pane() {
        let (a, b) = (
            resumable("a", Status::Idle, 1),
            resumable("b", Status::Done, 2),
        );
        let plain = agent("terminal:t1", Status::Idle, 3);
        let mut state = State::default();
        state.reconcile(GEN, &[a.clone(), b.clone(), plain.clone()], NOW);
        assert_eq!(
            act(&mut state, &a, Action::ToggleSettle),
            Ok(After::ClosePane)
        );
        assert_eq!(
            act(&mut state, &b, Action::Snooze(NOW + 60)),
            Ok(After::ClosePane)
        );
        assert_eq!(
            act(&mut state, &plain, Action::ToggleSettle),
            Ok(After::Nothing)
        );
        let row = state
            .ordered(std::slice::from_ref(&plain))
            .into_iter()
            .find(|r| r.key == plain.key)
            .unwrap();
        assert_eq!(row.status_text(NOW, &tz()), "settled · stays open");
    }

    fn closed_settled(state: &mut State, agent: &Observed) {
        state.reconcile(GEN, std::slice::from_ref(agent), NOW);
        act(state, agent, Action::ToggleSettle).unwrap();
        state.reconcile(GEN, &[], NOW + 1);
        state.reconcile(GEN, &[], NOW + 12);
    }

    #[test]
    fn unsettling_waking_or_reopening_a_closed_thread_reopens_its_session() {
        let a = resumable("a", Status::Idle, 1);
        let mut state = State::default();
        closed_settled(&mut state, &a);
        let Ok(After::Reopen(plan)) = state.apply("a", None, Action::ToggleSettle, NOW + 20) else {
            panic!("unsettle should reopen");
        };
        assert_eq!(plan.argv, ["claude", "--resume", "id-a"]);
        assert_eq!(plan.thread.workspace_id.as_deref(), Some("w1"));
        assert_eq!(closed(&state, "a").park, None);

        state
            .apply("a", None, Action::Snooze(NOW + 60), NOW + 21)
            .unwrap();
        assert!(matches!(
            state.apply("a", None, Action::Wake, NOW + 22),
            Ok(After::Reopen(_))
        ));
        assert!(matches!(
            state.apply("a", None, Action::Reopen, NOW + 23),
            Ok(After::Reopen(_))
        ));
    }

    #[test]
    fn marking_a_closed_parked_thread_unread_brings_it_back_without_reopening() {
        let a = resumable("a", Status::Idle, 1);
        let mut state = State::default();
        closed_settled(&mut state, &a);
        assert_eq!(
            state.apply("a", None, Action::ToggleUnread, NOW + 20),
            Ok(After::Nothing)
        );
        let row = closed(&state, "a");
        assert_eq!(
            (row.display, row.park),
            (Display::Unread(Unread::Manual), None)
        );
        assert_eq!(row.status_text(NOW + 20, &tz()), "unread · closed");
    }

    #[test]
    fn only_closed_threads_can_be_forgotten() {
        let (a, b) = (
            resumable("a", Status::Idle, 1),
            resumable("b", Status::Idle, 2),
        );
        let mut state = State::default();
        closed_settled(&mut state, &a);
        state.reconcile(GEN, std::slice::from_ref(&b), NOW + 13);
        assert!(act(&mut state, &b, Action::Forget).is_err());
        assert_eq!(
            state.apply("a", None, Action::Forget, NOW + 14),
            Ok(After::Nothing)
        );
        assert!(!state.records.contains_key("a"));
    }

    #[test]
    fn a_closed_snooze_wakes_into_the_open_list_without_launching() {
        let a = resumable("a", Status::Idle, 1);
        let mut state = State::default();
        state.reconcile(GEN, std::slice::from_ref(&a), NOW);
        act(&mut state, &a, Action::Snooze(NOW + 60)).unwrap();
        state.reconcile(GEN, &[], NOW + 1);
        state.reconcile(GEN, &[], NOW + 12);
        assert_eq!(
            closed(&state, "a").park,
            Some(Park::Snoozed { until: NOW + 60 })
        );
        state.reconcile(GEN, &[], NOW + 61);
        let row = closed(&state, "a");
        assert_eq!(
            (row.display, row.park),
            (Display::Unread(Unread::Woke), None)
        );
        assert_eq!(row.status_text(NOW + 61, &tz()), "woke · closed");
    }

    #[test]
    fn closed_threads_follow_live_ones_by_last_activity() {
        let agents = [
            resumable("old", Status::Idle, 1),
            resumable("new", Status::Idle, 2),
        ];
        let live = resumable("live", Status::Idle, 3);
        let mut state = State::default();
        state.reconcile(GEN, &agents, NOW);
        act(&mut state, &agents[0], Action::ToggleUnread).unwrap();
        state.reconcile(GEN, &[agents[1].clone()], NOW + 100);
        act(&mut state, &agents[1], Action::ToggleUnread).unwrap();
        for at in [NOW + 200, NOW + 220] {
            state.reconcile(GEN, std::slice::from_ref(&live), at);
        }
        let order: Vec<String> = state
            .ordered(std::slice::from_ref(&live))
            .into_iter()
            .map(|r| r.key)
            .collect();
        assert_eq!(order, ["live", "new", "old"]);
    }

    #[test]
    fn a_reopened_session_reporting_a_new_id_keeps_its_thread() {
        let a = resumable("a", Status::Idle, 1);
        let mut state = State::default();
        closed_settled(&mut state, &a);
        state.apply("a", None, Action::Reopen, NOW + 20).unwrap();
        state.reopening("a", "pane-new", GEN, NOW + 20);
        let resumed = Observed {
            pane_id: "pane-new".into(),
            ..resumable("a2", Status::Idle, 9)
        };
        state.reconcile(GEN, std::slice::from_ref(&resumed), NOW + 25);
        assert!(!state.records.contains_key("a"));
        assert!(state.records.contains_key("a2"));
    }

    #[test]
    fn only_official_claude_codex_and_pi_sessions_are_resumable() {
        let thread = |agent: &str, source: &str, kind: &str, value: &str| Thread {
            agent: Some(agent.into()),
            session: Some(SessionRef {
                source: source.into(),
                kind: kind.into(),
                value: value.into(),
            }),
            ..Thread::default()
        };
        assert_eq!(
            thread("claude", "herdr:claude", "id", "x")
                .resume_argv()
                .unwrap(),
            ["claude", "--resume", "x"]
        );
        assert_eq!(
            thread("codex", "herdr:codex", "id", "x")
                .resume_argv()
                .unwrap(),
            ["codex", "resume", "x"]
        );
        assert_eq!(
            thread("pi", "herdr:pi", "path", "/s/x.jsonl")
                .resume_argv()
                .unwrap(),
            ["pi", "--session", "/s/x.jsonl"]
        );
        assert_eq!(
            thread("pi", "herdr:pi", "id", "x").resume_argv().unwrap(),
            ["pi", "--session", "x"]
        );
        assert!(
            thread("pi", "herdr:pi", "path", "relative")
                .resume_argv()
                .is_none()
        );
        assert!(
            thread("claude", "custom", "id", "x")
                .resume_argv()
                .is_none()
        );
        assert!(
            thread("claude", "herdr:claude", "path", "/x")
                .resume_argv()
                .is_none()
        );
        assert!(
            thread("opencode", "herdr:opencode", "id", "x")
                .resume_argv()
                .is_none()
        );
    }

    #[test]
    fn records_imported_from_earlier_versions_keep_their_parking() {
        let legacy: Record = serde_json::from_str(
            r#"{"park":{"kind":"settled"},"generation":"g1","last_seq":4,"last_seen":1}"#,
        )
        .unwrap();
        let mut state = State::default();
        state.records.insert("a".into(), legacy);
        let idle = agent("a", Status::Idle, 4);
        state.reconcile(GEN, std::slice::from_ref(&idle), NOW);
        assert_eq!(shown(&state, &idle).1, Some(Park::Settled));
    }

    #[test]
    fn a_closed_thread_resumed_by_hand_after_a_restart_comes_back() {
        let idle = resumable("a", Status::Idle, 4);
        let mut state = State::default();
        closed_settled(&mut state, &idle);
        let restarted = resumable("a", Status::Idle, 1);
        state.reconcile("g2", std::slice::from_ref(&restarted), NOW + 100);
        assert_eq!(shown(&state, &restarted).1, None);
    }

    #[test]
    fn agents_restored_after_a_restart_keep_their_parking() {
        let kept_open = resumable("a", Status::Idle, 4);
        let mut state = State::default();
        state.reconcile(GEN, std::slice::from_ref(&kept_open), NOW);
        act(&mut state, &kept_open, Action::ToggleSettle).unwrap();
        state.reconcile("g2", &[], NOW + 1);
        let restored = resumable("a", Status::Idle, 1);
        state.reconcile("g2", std::slice::from_ref(&restored), NOW + 5);
        assert_eq!(shown(&state, &restored).1, Some(Park::Settled));
    }

    #[test]
    fn a_reopen_waits_for_the_resumed_session_before_carrying_the_thread_over() {
        let a = resumable("a", Status::Idle, 1);
        let mut state = State::default();
        closed_settled(&mut state, &a);
        state.apply("a", None, Action::Reopen, NOW + 20).unwrap();
        state.reopening("a", "pane-new", GEN, NOW + 20);
        let starting = Observed {
            pane_id: "pane-new".into(),
            ..agent("terminal:t9", Status::Idle, 1)
        };
        state.reconcile(GEN, std::slice::from_ref(&starting), NOW + 22);
        assert!(
            state.records["a"].thread.session.is_some(),
            "the saved session survives"
        );
        let other_kind = Observed {
            pane_id: "pane-new".into(),
            thread: Thread {
                agent: Some("pi".into()),
                ..resumable("p", Status::Idle, 1).thread
            },
            ..resumable("p", Status::Idle, 1)
        };
        state.reconcile(GEN, std::slice::from_ref(&other_kind), NOW + 24);
        assert!(
            state.records.contains_key("a"),
            "a different agent does not take the thread"
        );
    }

    #[test]
    fn a_stale_reopen_marker_does_not_capture_a_later_agent() {
        let a = resumable("a", Status::Idle, 1);
        let mut state = State::default();
        closed_settled(&mut state, &a);
        state.apply("a", None, Action::Reopen, NOW + 20).unwrap();
        state.reopening("a", "pane-new", GEN, NOW + 20);
        let much_later = Observed {
            pane_id: "pane-new".into(),
            ..resumable("a2", Status::Idle, 9)
        };
        state.reconcile(GEN, std::slice::from_ref(&much_later), NOW + 86_400);
        assert!(state.records.contains_key("a") && state.records.contains_key("a2"));
    }

    #[test]
    fn a_snooze_ending_while_you_watch_the_agent_does_not_mark_it_unread() {
        let plain = agent("a", Status::Idle, 4);
        let mut state = State::default();
        state.reconcile(GEN, std::slice::from_ref(&plain), NOW);
        act(&mut state, &plain, Action::Snooze(NOW + 60)).unwrap();
        let watched = Observed {
            focused: true,
            ..plain.clone()
        };
        state.reconcile(GEN, std::slice::from_ref(&watched), NOW + 61);
        assert_eq!(shown(&state, &watched), (Display::Idle, None));
    }

    #[test]
    fn closed_threads_order_by_their_last_activity_not_when_they_were_last_seen() {
        let agents = [
            resumable("early", Status::Idle, 1),
            resumable("late", Status::Idle, 1),
        ];
        let mut state = State::default();
        state.reconcile(GEN, &agents, NOW);
        let early_change = [resumable("early", Status::Idle, 2), agents[1].clone()];
        state.reconcile(GEN, &early_change, NOW + 10);
        let late_change = [early_change[0].clone(), resumable("late", Status::Idle, 2)];
        state.reconcile(GEN, &late_change, NOW + 20);
        for at in [NOW + 30, NOW + 41] {
            state.reconcile(GEN, &[], at);
        }
        let order: Vec<String> = state.ordered(&[]).into_iter().map(|r| r.key).collect();
        assert_eq!(order, ["late", "early"]);
    }

    #[test]
    fn a_resumed_session_seen_before_its_id_still_merges_into_its_thread() {
        let a = resumable("a", Status::Idle, 1);
        let mut state = State::default();
        closed_settled(&mut state, &a);
        act_closed_pin(&mut state, "a");
        state.apply("a", None, Action::Reopen, NOW + 20).unwrap();
        state.reopening("a", "pane-new", GEN, NOW + 20);
        let terminal_first = Observed {
            pane_id: "pane-new".into(),
            ..agent("terminal:t9", Status::Idle, 1)
        };
        state.reconcile(GEN, std::slice::from_ref(&terminal_first), NOW + 21);
        let with_session = Observed {
            pane_id: "pane-new".into(),
            pre_session_key: Some("terminal:t9".into()),
            ..resumable("a2", Status::Idle, 2)
        };
        state.reconcile(GEN, std::slice::from_ref(&with_session), NOW + 22);
        assert_eq!(state.records.keys().collect::<Vec<_>>(), ["a2"]);
        assert!(
            state.records["a2"].pinned,
            "the reopened thread's pin carries over"
        );
    }

    fn act_closed_pin(state: &mut State, key: &str) {
        state.apply(key, None, Action::TogglePin, NOW + 15).unwrap();
    }

    #[test]
    fn a_pending_reopen_does_not_survive_a_server_restart() {
        let a = resumable("a", Status::Idle, 1);
        let mut state = State::default();
        closed_settled(&mut state, &a);
        state.apply("a", None, Action::Reopen, NOW + 20).unwrap();
        state.reopening("a", "pane-new", GEN, NOW + 20);
        let after_restart = Observed {
            pane_id: "pane-new".into(),
            ..resumable("b", Status::Idle, 1)
        };
        state.reconcile("g2", std::slice::from_ref(&after_restart), NOW + 30);
        assert!(state.records.contains_key("a") && state.records.contains_key("b"));
    }

    #[test]
    fn closed_threads_without_recorded_changes_order_by_their_last_change_counter() {
        let agents = [
            resumable("quiet", Status::Idle, 3),
            resumable("busy", Status::Idle, 8),
        ];
        let mut state = State::default();
        state.reconcile(GEN, &agents, NOW);
        for at in [NOW + 1, NOW + 12] {
            state.reconcile(GEN, &[], at);
        }
        let order: Vec<String> = state.ordered(&[]).into_iter().map(|r| r.key).collect();
        assert_eq!(order, ["busy", "quiet"]);
    }

    #[test]
    fn a_quiet_closed_thread_touched_later_does_not_outrank_an_active_one() {
        let agents = [
            resumable("active", Status::Idle, 1),
            resumable("quiet", Status::Idle, 9),
        ];
        let mut state = State::default();
        state.reconcile(GEN, &agents, NOW);
        state.reconcile(
            GEN,
            &[resumable("active", Status::Idle, 2), agents[1].clone()],
            NOW + 10,
        );
        for at in [NOW + 20, NOW + 31] {
            state.reconcile(GEN, &[], at);
        }
        state
            .apply("quiet", None, Action::TogglePin, NOW + 40)
            .unwrap();
        state
            .apply("quiet", None, Action::TogglePin, NOW + 41)
            .unwrap();
        let order: Vec<String> = state.ordered(&[]).into_iter().map(|r| r.key).collect();
        assert_eq!(order, ["active", "quiet"]);
    }

    #[test]
    fn reconcile_reports_when_a_missing_agent_must_be_checked_again() {
        let idle = resumable("a", Status::Idle, 4);
        let mut state = State::default();
        assert!(
            state
                .reconcile(GEN, std::slice::from_ref(&idle), NOW)
                .is_empty()
        );
        assert_eq!(
            state.reconcile(GEN, &[], NOW + 1),
            [NOW + 1 + EXIT_GRACE_SECS]
        );
        assert!(
            state.reconcile(GEN, &[], NOW + 3).is_empty(),
            "armed once per disappearance"
        );
    }

    #[test]
    fn a_thread_reopened_after_a_restart_is_not_treated_as_restoring() {
        let kept = resumable("a", Status::Idle, 40);
        let mut state = State::default();
        closed_settled(&mut state, &kept);
        state.apply("a", None, Action::Reopen, NOW + 100).unwrap();
        let reopened = resumable("a", Status::Idle, 1);
        state.reconcile("g2", std::slice::from_ref(&reopened), NOW + 200);
        act(&mut state, &reopened, Action::ToggleSettle).unwrap();
        let working = resumable("a", Status::Working, 2);
        state.reconcile("g2", std::slice::from_ref(&working), NOW + 205);
        assert_eq!(
            shown(&state, &working).1,
            None,
            "real use of a reopened thread unparks it"
        );
    }

    #[test]
    fn agents_vanishing_together_need_only_one_follow_up() {
        let agents = [
            resumable("a", Status::Idle, 1),
            resumable("b", Status::Idle, 2),
        ];
        let mut state = State::default();
        state.reconcile(GEN, &agents, NOW);
        assert_eq!(
            state.reconcile(GEN, &[], NOW + 1),
            [NOW + 1 + EXIT_GRACE_SECS]
        );
    }
}
