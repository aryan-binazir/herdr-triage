#![allow(clippy::unwrap_used)]

mod support;

use std::process::{Command, Output};

use serde_json::{Value, json};
use support::fake_herdr::{FakeHerdr, agent};

trait RunBinary {
    fn run(&self, args: &[&str]) -> Output;
}

impl RunBinary for FakeHerdr {
    fn run(&self, args: &[&str]) -> Output {
        let output = Command::new(env!("CARGO_BIN_EXE_herdr-triage"))
            .args(args)
            .env("HERDR_SOCKET_PATH", &self.socket)
            .env("HERDR_PLUGIN_ID", "ar.triage")
            .env("HERDR_PLUGIN_STATE_DIR", self.dir.path().join("state"))
            .env("HERDR_PLUGIN_CONFIG_DIR", self.dir.path().join("config"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }
}

fn park(herdr: &FakeHerdr, key: &str, park: &str, until: Option<i64>) {
    let server_dir = std::fs::read_dir(herdr.state_dir())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let db = rusqlite::Connection::open(server_dir.join("triage.sqlite3")).unwrap();
    let changed = db
        .execute(
            "UPDATE threads SET park = ?1, snooze_until = ?2 WHERE key = ?3",
            rusqlite::params![park, until, key],
        )
        .unwrap();
    assert_eq!(changed, 1, "no thread {key}");
}

#[test]
fn sync_publishes_rank_tokens_and_installs_the_agents_view() {
    let herdr = FakeHerdr::start(vec![agent("w1:p1", "idle", 3), agent("w1:p2", "done", 4)]);
    herdr.run(&["sync"]);

    let mut reports = herdr.take("pane.report_metadata");
    reports.sort_by_key(|r| r["pane_id"].as_str().unwrap().to_owned());
    assert_eq!(reports.len(), 2);
    assert_eq!(reports[0]["pane_id"], "w1:p1");
    assert_eq!(reports[0]["source"], "plugin:ar.triage");
    assert_eq!(
        reports[0]["tokens"],
        json!({"triage_rank": "1", "triage": null})
    );

    let views = herdr.take("agent.view.set");
    assert_eq!(views.len(), 1);
    assert_eq!(views[0]["source"], "plugin:ar.triage");
    assert_eq!(views[0]["label"], "triage");
    assert_eq!(
        views[0]["filter"],
        json!({"op": "not", "filter": {"op": "in", "field": {"token": "triage_rank"}, "values": ["2", "3"]}})
    );
    assert_eq!(
        views[0]["sort"],
        json!([
            {"field": {"token": "triage_rank"}, "order": "asc"},
            {"field": "state_change_seq", "order": "desc"}
        ])
    );
}

#[test]
fn the_sidebar_hides_parked_agents_and_counts_them_in_its_header() {
    let herdr = FakeHerdr::start(vec![
        agent("w1:p1", "idle", 3),
        agent("w1:p2", "idle", 4),
        agent("w1:p3", "idle", 5),
        agent("w1:p4", "idle", 6),
    ]);
    herdr.run(&["sync"]);
    park(&herdr, "terminal:t-w1:p1", "snoozed", Some(4_102_444_800));
    park(&herdr, "terminal:t-w1:p2", "settled", None);
    park(&herdr, "terminal:t-w1:p3", "settled", None);
    herdr.take("agent.view.set");

    herdr.run(&["sync"]);
    let views = herdr.take("agent.view.set");
    assert_eq!(views[0]["label"], "3 parked");
}

#[test]
fn sync_reports_only_tokens_herdr_is_missing_or_holds_wrong() {
    let herdr = FakeHerdr::start(vec![agent("w1:p1", "idle", 3), agent("w1:p2", "idle", 4)]);
    herdr.run(&["sync"]);
    assert_eq!(herdr.take("pane.report_metadata").len(), 2);

    herdr
        .agents
        .lock()
        .unwrap()
        .push(agent("w1:p3", "working", 5));
    herdr.run(&["sync"]);
    let reports = herdr.take("pane.report_metadata");
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0]["pane_id"], "w1:p3");

    for agent in herdr.agents.lock().unwrap().iter_mut() {
        agent["tokens"] = json!({});
    }
    herdr.run(&["startup"]);
    assert_eq!(herdr.take("pane.report_metadata").len(), 3);
}

#[test]
fn next_advances_from_the_pane_it_was_invoked_from() {
    let mut focused = agent("w1:p3", "blocked", 2);
    focused["focused"] = json!(true);
    let herdr = FakeHerdr::start(vec![focused, agent("w1:p1", "blocked", 9)]);
    let output = Command::new(env!("CARGO_BIN_EXE_herdr-triage"))
        .arg("next")
        .env("HERDR_SOCKET_PATH", &herdr.socket)
        .env("HERDR_PLUGIN_ID", "ar.triage")
        .env("HERDR_PLUGIN_STATE_DIR", herdr.dir.path().join("state"))
        .env(
            "HERDR_PLUGIN_CONTEXT_JSON",
            r#"{"focused_pane_id":"w1:p1"}"#,
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(herdr.take("agent.focus"), [json!({"target": "w1:p3"})]);
}

#[test]
fn next_focuses_blocked_agents_before_finished_ones() {
    let herdr = FakeHerdr::start(vec![
        agent("w1:p1", "done", 9),
        agent("w1:p2", "working", 8),
        agent("w1:p3", "blocked", 2),
    ]);
    herdr.run(&["next"]);
    assert_eq!(herdr.take("agent.focus"), [json!({"target": "w1:p3"})]);
}

#[test]
fn a_vanished_agent_gets_a_follow_up_sync_after_the_exit_grace_period() {
    let herdr = FakeHerdr::start(vec![agent("w1:p1", "idle", 3)]);
    herdr.run(&["sync"]);
    herdr.agents.lock().unwrap().clear();
    herdr.run(&["sync"]);
    herdr.take("agent.list");
    // Nothing else talks to this fixture, so a later sync can only come from the waker.
    std::thread::sleep(std::time::Duration::from_secs(12));
    assert_eq!(herdr.take("agent.list").len(), 1);
}

#[test]
fn a_burst_of_hooks_coalesces_into_a_few_syncs() {
    let herdr = FakeHerdr::start(vec![agent("w1:p1", "idle", 3), agent("w1:p2", "idle", 4)]);
    herdr.run(&["sync"]);
    herdr.take("agent.list");
    *herdr.list_delay.lock().unwrap() = std::time::Duration::from_millis(100);
    let hooks: Vec<_> = (0..30)
        .map(|_| {
            Command::new(env!("CARGO_BIN_EXE_herdr-triage"))
                .arg("sync")
                .env("HERDR_SOCKET_PATH", &herdr.socket)
                .env("HERDR_PLUGIN_ID", "ar.triage")
                .env("HERDR_PLUGIN_STATE_DIR", herdr.state_dir())
                .spawn()
                .unwrap()
        })
        .collect();
    for mut hook in hooks {
        assert!(hook.wait().unwrap().success());
    }
    let syncs = herdr.take("agent.list").len();
    assert!((1..=6).contains(&syncs), "{syncs} syncs ran for 30 hooks");
}

#[test]
fn startup_rearms_snooze_timers_even_when_publishing_fails() {
    let herdr = FakeHerdr::start(vec![agent("w1:p1", "idle", 3)]);
    herdr.run(&["sync"]);
    let state_dir = herdr.state_dir();
    let since_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    let unique_to_this_run = since_epoch.subsec_nanos() % 100_000;
    let wake =
        i64::try_from(since_epoch.as_secs()).unwrap() + 3_600 + i64::from(unique_to_this_run);
    park(&herdr, "terminal:t-w1:p1", "snoozed", Some(wake));

    *herdr.fail_method.lock().unwrap() = Some("agent.view.set".into());
    let output = Command::new(env!("CARGO_BIN_EXE_herdr-triage"))
        .arg("startup")
        .env("HERDR_SOCKET_PATH", &herdr.socket)
        .env("HERDR_PLUGIN_STATE_DIR", &state_dir)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let sleeper = format!("{} wake-at {wake}", env!("CARGO_BIN_EXE_herdr-triage"));
    let sleepers = Command::new("pgrep")
        .args(["-x", "-f", &sleeper])
        .output()
        .unwrap();
    let pids = String::from_utf8_lossy(&sleepers.stdout).trim().to_owned();
    for pid in pids.lines() {
        let _ = Command::new("kill").arg(pid).status();
    }
    assert_ne!(pids, "", "no wake timer for the pending snooze");
}

#[test]
fn next_says_so_when_nothing_needs_you() {
    let herdr = FakeHerdr::start(vec![
        agent("w1:p1", "idle", 1),
        agent("w1:p2", "working", 2),
    ]);
    herdr.run(&["next"]);
    assert_eq!(herdr.take("agent.focus"), Vec::<Value>::new());
    let notes = herdr.take("notification.show");
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0]["title"], "Nothing needs you");
}

#[test]
fn open_launches_the_picker_popup_aimed_at_the_invoking_pane() {
    let herdr = FakeHerdr::start(vec![]);
    let output = Command::new(env!("CARGO_BIN_EXE_herdr-triage"))
        .arg("open")
        .env("HERDR_SOCKET_PATH", &herdr.socket)
        .env("HERDR_PLUGIN_ID", "ar.triage")
        .env(
            "HERDR_PLUGIN_CONTEXT_JSON",
            r#"{"focused_pane_id":"w1:p7"}"#,
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let opened = herdr.take("plugin.pane.open");
    assert_eq!(opened.len(), 1);
    assert_eq!(opened[0]["plugin_id"], "ar.triage");
    assert_eq!(opened[0]["entrypoint"], "picker");
    assert_eq!(opened[0]["focus"], true);
    assert_eq!(opened[0]["env"]["HERDR_TRIAGE_FOCUS"], "w1:p7");
}

#[test]
fn sync_and_next_do_not_depend_on_tabs() {
    let herdr = FakeHerdr::start(vec![agent("w1:p1", "blocked", 3)]);
    *herdr.fail_method.lock().unwrap() = Some("tab.list".into());
    herdr.run(&["sync"]);
    let requests = std::mem::take(&mut *herdr.requests.lock().unwrap());
    let methods: Vec<_> = requests
        .iter()
        .map(|r| r["method"].as_str().unwrap())
        .collect();
    assert_eq!(
        methods,
        [
            "agent.list",
            "workspace.list",
            "pane.report_metadata",
            "agent.view.set"
        ]
    );
    assert_eq!(
        requests[2]["params"]["tokens"],
        json!({"triage_rank": "1", "triage": null})
    );
    let view = requests[3]["params"].clone();
    assert_eq!(view["label"], "triage");
    herdr.run(&["next"]);
    let requests = std::mem::take(&mut *herdr.requests.lock().unwrap());
    let methods: Vec<_> = requests
        .iter()
        .map(|r| r["method"].as_str().unwrap())
        .collect();
    assert_eq!(
        methods,
        [
            "agent.list",
            "workspace.list",
            "agent.view.set",
            "agent.focus"
        ]
    );
    assert_eq!(requests[2]["params"], view);
    assert_eq!(requests[3]["params"], json!({"target": "w1:p1"}));
}
