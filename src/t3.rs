mod client;
mod db;

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use crate::picker::{Item, Target};
use crate::triage::{Display, Park, Row, Thread};

pub use client::{Client, Done};

fn home() -> Option<PathBuf> {
    let set = |name| std::env::var_os(name).filter(|value| !value.is_empty());
    set("T3CODE_HOME")
        .map(PathBuf::from)
        .or_else(|| set("HOME").map(|home| PathBuf::from(home).join(".t3")))
}
pub use db::Reader;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Settle,
    Unsettle,
    Snooze { until: i64 },
    Unsnooze,
    Pin,
    Unpin,
    MarkUnread,
}

const THREAD_POLL_INTERVAL: Duration = Duration::from_secs(2);

const OPENER_EXIT_TIMEOUT: Duration = Duration::from_secs(2);

const SETTLED_RETENTION_SECS: i64 = 30 * 86_400;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub environment_id: String,
    pub projects: BTreeMap<String, String>,
    pub threads: Vec<T3Thread>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct T3Thread {
    pub id: String,
    pub project_id: String,
    pub title: String,
    pub status: String,
    pub provider: String,
    pub settled: bool,
    pub snoozed_until: Option<i64>,
    pub pinned: bool,
    pub unseen: bool,
    pub updated_at: i64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Update {
    Threads(Option<Snapshot>),
    Done {
        thread: String,
        op: Op,
        result: Result<Done, String>,
    },
}

pub struct Worker {
    requests: Option<Sender<(String, Op)>>,
    pub updates: Receiver<Update>,
}

impl Worker {
    pub fn start(state_root: &Path) -> Self {
        let mut client = Client::from_env(state_root);
        Self::spawn(
            Reader::from_env(),
            move |thread: &str, op: Op| match client.as_mut() {
                Some(client) => client.organize(thread, op),
                None => Err(io::Error::other("T3 isn't installed here")),
            },
            THREAD_POLL_INTERVAL,
        )
    }

    pub(crate) fn spawn(
        reader: Option<Reader>,
        mut act: impl FnMut(&str, Op) -> io::Result<Done> + Send + 'static,
        every: Duration,
    ) -> Self {
        let (updates_sender, updates) = mpsc::channel();
        let Some(reader) = reader else {
            return Self {
                requests: None,
                updates,
            };
        };
        let (requests, commands) = mpsc::channel::<(String, Op)>();
        thread::spawn(move || {
            let mut last = None;
            let read = |last: &mut Option<Option<Snapshot>>| {
                let Ok(snapshot) = reader.read() else {
                    return true;
                };
                if last.as_ref() == Some(&snapshot) {
                    return true;
                }
                *last = Some(snapshot.clone());
                updates_sender.send(Update::Threads(snapshot)).is_ok()
            };
            let mut next_poll = Instant::now();
            loop {
                let wait = next_poll.saturating_duration_since(Instant::now());
                match commands.recv_timeout(wait) {
                    Ok((thread, op)) => {
                        let result = act(&thread, op).map_err(|error| error.to_string());
                        let done = Update::Done { thread, op, result };
                        if !read(&mut last) || updates_sender.send(done).is_err() {
                            return;
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        if !read(&mut last) {
                            return;
                        }
                        next_poll = Instant::now() + every;
                    }
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
        });
        Self {
            requests: Some(requests),
            updates,
        }
    }

    pub fn request(&self, thread: &str, op: Op) -> bool {
        self.requests
            .as_ref()
            .is_some_and(|requests| requests.send((thread.to_owned(), op)).is_ok())
    }
}

pub fn row_key(thread: &str) -> String {
    format!("t3:{thread}")
}

pub fn items(snapshot: &Snapshot, now: i64) -> Vec<Item> {
    snapshot
        .threads
        .iter()
        .filter(|thread| !thread.settled || now - thread.updated_at <= SETTLED_RETENTION_SECS)
        .map(|thread| item(snapshot, thread, now))
        .collect()
}

fn item(snapshot: &Snapshot, thread: &T3Thread, now: i64) -> Item {
    let location = snapshot
        .projects
        .get(&thread.project_id)
        .cloned()
        .unwrap_or_else(|| "-".into());
    let kind = format!("t3 {}", provider_name(&thread.provider));
    let snoozed = thread.snoozed_until.filter(|&until| until > now);
    let park = match (snoozed, thread.settled) {
        (Some(until), _) => Some(Park::Snoozed { until }),
        (None, true) => Some(Park::Settled),
        (None, false) => None,
    };
    let row = Row {
        agent_index: None,
        key: row_key(&thread.id),
        seq: 0,
        display: display(thread),
        park,
        pinned: thread.pinned && !thread.settled,
        changed_at: Some(thread.updated_at),
        thread: Thread {
            label: thread.title.clone(),
            agent: Some(kind.clone()),
            workspace_label: Some(location.clone()),
            ..Thread::default()
        },
        title: None,
    };
    Item {
        target: Target::T3 {
            thread: thread.id.clone(),
            link: link(&snapshot.environment_id, &thread.id),
        },
        search: format!("{kind} {} {location}", thread.title),
        status: row.state_text(now, true),
        location,
        kind,
        focused: false,
        row,
    }
}

fn display(thread: &T3Thread) -> Display {
    match thread.status.as_str() {
        "preparing" | "queued" | "starting" | "running" => Display::Working,
        "waiting" => Display::Blocked,
        "completed" | "interrupted" | "failed" | "cancelled" | "rolled_back" if thread.unseen => {
            Display::Done
        }
        "completed" | "interrupted" | "failed" | "cancelled" | "rolled_back" | "idle" => {
            Display::Idle
        }
        _ => Display::Unknown,
    }
}

fn provider_name(provider: &str) -> &str {
    match provider {
        "claudeAgent" => "claude",
        other => other,
    }
}

pub fn open(link: &str) -> io::Result<()> {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    open_with(opener, link)
}

fn open_with(opener: &str, link: &str) -> io::Result<()> {
    let mut child = Command::new(opener)
        .arg(link)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + OPENER_EXIT_TIMEOUT;
    while Instant::now() < deadline {
        match child.try_wait()? {
            Some(status) if status.success() => return Ok(()),
            Some(status) => return Err(io::Error::other(format!("{opener} {status}"))),
            None => thread::sleep(Duration::from_millis(20)),
        }
    }
    Ok(())
}

fn link(environment_id: &str, thread_id: &str) -> String {
    format!(
        "t3code://threads/{}/{}",
        encode(environment_id),
        encode(thread_id)
    )
}

/// T3's own link segment encoding: `encodeURIComponent` that also escapes `.!~*'()`.
fn encode(segment: &str) -> String {
    segment
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_' {
                char::from(byte).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::triage::Section;

    const NOW: i64 = 1_793_710_800;
    const ENV: &str = "68a0648f-f112-4d91-a9e9-9ab9b19eeeb1";

    fn thread(id: &str, status: &str, provider: &str, updated_ago: i64) -> T3Thread {
        T3Thread {
            id: id.into(),
            project_id: "proj-1".into(),
            title: format!("title {id}"),
            status: status.into(),
            provider: provider.into(),
            settled: false,
            snoozed_until: None,
            pinned: false,
            unseen: true,
            updated_at: NOW - updated_ago,
        }
    }

    fn snapshot(threads: Vec<T3Thread>) -> Snapshot {
        Snapshot {
            environment_id: ENV.into(),
            projects: BTreeMap::from([("proj-1".into(), "bball-lab".into())]),
            threads,
        }
    }

    fn shown(items: &[Item]) -> Vec<(String, String, String, String)> {
        items
            .iter()
            .map(|item| {
                (
                    item.row.label().to_owned(),
                    item.location.clone(),
                    item.kind.clone(),
                    item.status.clone(),
                )
            })
            .collect()
    }

    #[test]
    fn t3_threads_show_as_rows_in_the_words_herdr_rows_use() {
        let mut snoozed = thread("c", "completed", "codex", 600);
        snoozed.snoozed_until = Some(NOW + 45 * 60);
        let mut settled = thread("d", "completed", "codex", 86_400);
        settled.settled = true;
        let mut orphan = thread("f", "mystery", "grok", 30);
        orphan.project_id = "gone".into();
        let items = items(
            &snapshot(vec![
                thread("a", "running", "codex", 180),
                thread("b", "waiting", "claudeAgent", 3_600),
                snoozed,
                settled,
                thread("e", "idle", "claudeAgent", 2 * 86_400),
                orphan,
                thread("g", "completed", "codex", 7_200),
            ]),
            NOW,
        );
        assert_eq!(
            shown(&items),
            [
                ("title a", "bball-lab", "t3 codex", "working 3m"),
                ("title b", "bball-lab", "t3 claude", "blocked 1h"),
                ("title c", "bball-lab", "t3 codex", "snoozed · 45m"),
                ("title d", "bball-lab", "t3 codex", "settled"),
                ("title e", "bball-lab", "t3 claude", "idle 2d"),
                ("title f", "-", "t3 grok", "unknown <1m"),
                ("title g", "bball-lab", "t3 codex", "done 2h"),
            ]
            .map(|(a, b, c, d)| (a.into(), b.into(), c.into(), d.into()))
        );
        let displays: Vec<Display> = items.iter().map(|item| item.row.display).collect();
        assert_eq!(
            displays,
            [
                Display::Working,
                Display::Blocked,
                Display::Done,
                Display::Done,
                Display::Idle,
                Display::Unknown,
                Display::Done,
            ]
        );
        assert_eq!(
            items[2].row.park,
            Some(Park::Snoozed {
                until: NOW + 45 * 60
            })
        );
        assert_eq!(items[3].row.park, Some(Park::Settled));
        assert!(items.iter().all(|item| !item.row.pinned));
        assert!(items[0].search.contains("bball-lab") && items[0].search.contains("t3 codex"));
    }

    #[test]
    fn a_t3_row_opens_its_thread_in_the_t3_app() {
        let id = "thread:delegated-task:command%3Amcp%3A1341bff3-d5ac-419d-bfd0-1331a5ec4918%3Adelegate-task%3A3c8b7723-c838-45b5-87c6-8cfc630d83ed";
        let items = items(&snapshot(vec![thread(id, "completed", "codex", 60)]), NOW);
        let expected = "t3code://threads/68a0648f-f112-4d91-a9e9-9ab9b19eeeb1/thread%3Adelegated-task%3Acommand%253Amcp%253A1341bff3-d5ac-419d-bfd0-1331a5ec4918%253Adelegate-task%253A3c8b7723-c838-45b5-87c6-8cfc630d83ed";
        assert_eq!(
            items[0].target,
            Target::T3 {
                thread: id.into(),
                link: expected.into()
            }
        );
        assert_eq!(items[0].pane_id(), None);
        assert_eq!(items[0].row.key, format!("t3:{id}"));
    }

    #[test]
    fn working_t3_threads_wait_in_the_working_section_and_parked_ones_in_theirs() {
        let mut snoozed = thread("c", "running", "codex", 60);
        snoozed.snoozed_until = Some(NOW + 60);
        let items = items(
            &snapshot(vec![
                thread("a", "running", "codex", 60),
                thread("b", "completed", "codex", 60),
                snoozed,
            ]),
            NOW,
        );
        let sections: Vec<Option<Section>> = items.iter().map(Item::section).collect();
        assert_eq!(
            sections,
            [Some(Section::Working), None, Some(Section::Snoozed)]
        );
    }

    #[test]
    fn settled_t3_threads_untouched_for_30_days_are_left_out() {
        let mut recent = thread("recent", "completed", "codex", 29 * 86_400);
        recent.settled = true;
        let mut old = thread("old", "completed", "codex", 31 * 86_400);
        old.settled = true;
        let open_but_old = thread("open", "completed", "codex", 90 * 86_400);
        let items = items(&snapshot(vec![recent, old, open_but_old]), NOW);
        let labels: Vec<&str> = items.iter().map(|item| item.row.label()).collect();
        assert_eq!(labels, ["title recent", "title open"]);
    }

    #[test]
    fn a_finished_t3_thread_you_have_opened_since_is_idle_not_done() {
        let seen = T3Thread {
            unseen: false,
            ..thread("seen", "completed", "codex", 7_200)
        };
        let items = items(&snapshot(vec![seen]), NOW);
        assert_eq!(items[0].row.display, Display::Idle);
        assert_eq!(items[0].status, "idle 2h");
    }

    #[test]
    fn a_pinned_t3_thread_stays_pinned_and_out_of_the_working_section() {
        let pinned = |id, status, settled| T3Thread {
            pinned: true,
            settled,
            ..thread(id, status, "codex", 60)
        };
        let items = items(
            &snapshot(vec![
                pinned("busy", "running", false),
                pinned("parked", "completed", true),
            ]),
            NOW,
        );
        assert!(items[0].row.pinned);
        assert_eq!(items[0].section(), None);
        assert!(!items[1].row.pinned, "settling unpins, as in T3");
    }

    #[test]
    fn a_snooze_whose_time_has_come_wakes_even_in_a_snapshot_read_before() {
        let woke = T3Thread {
            snoozed_until: Some(NOW - 1),
            ..thread("woke", "completed", "codex", 600)
        };
        let items = items(&snapshot(vec![woke]), NOW);
        assert_eq!(items[0].row.park, None);
        assert_eq!(items[0].section(), None);
    }

    #[test]
    fn a_launcher_that_fails_is_reported_and_one_still_running_has_taken_the_link() {
        assert!(open_with("false", "t3code://threads/e/t").is_err());
        assert!(open_with("true", "t3code://threads/e/t").is_ok());
        assert!(open_with("herdr-triage-no-such-opener", "t3code://threads/e/t").is_err());
        let started = std::time::Instant::now();
        assert!(open_with("sleep", "30").is_ok());
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
    }

    #[test]
    fn a_settled_thread_snoozed_since_shows_as_snoozed_as_in_t3() {
        let both = T3Thread {
            settled: true,
            snoozed_until: Some(NOW + 600),
            ..thread("both", "completed", "codex", 60)
        };
        let items = items(&snapshot(vec![both]), NOW);
        assert_eq!(items[0].row.park, Some(Park::Snoozed { until: NOW + 600 }));
    }
}
