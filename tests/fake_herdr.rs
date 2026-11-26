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

fn server_dir(herdr: &FakeHerdr) -> std::path::PathBuf {
    std::fs::read_dir(herdr.state_dir())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.is_dir())
        .unwrap()
}

fn park(herdr: &FakeHerdr, key: &str, park: &str, until: Option<i64>) {
    let server_dir = server_dir(herdr);
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
fn the_sidebar_hides_parked_agents_and_counts_only_snoozed_ones() {
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
    assert_eq!(views[0]["label"], "1 snoozed");
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

#[test]
fn unchanged_sync_leaves_the_database_unwritten() {
    let herdr = FakeHerdr::start(vec![agent("w1:p1", "idle", 3)]);
    herdr.run(&["sync"]);
    let server_dir = server_dir(&herdr);
    let observer = rusqlite::Connection::open(server_dir.join("triage.sqlite3")).unwrap();
    observer
        .execute("UPDATE threads SET touched_at = touched_at - 1", [])
        .unwrap();
    let version = || {
        observer
            .query_row("PRAGMA data_version", [], |r| r.get::<_, i64>(0))
            .unwrap()
    };
    let before = version();
    herdr.run(&["sync"]);
    assert_eq!(version(), before);
    herdr.agents.lock().unwrap()[0]["state_change_seq"] = json!(4);
    herdr.run(&["sync"]);
    assert_ne!(version(), before);
}

#[test]
fn wakers_share_each_deadline_and_remove_expired_locks() {
    let herdr = FakeHerdr::start(vec![]);
    herdr.run(&["sync"]);
    herdr.take("agent.list");
    let server = server_dir(&herdr);
    let now = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap();
    let mut wakers: Vec<_> = [now + 4, now + 4, now + 4, now + 4, now + 6]
        .into_iter()
        .map(|until| {
            Command::new(env!("CARGO_BIN_EXE_herdr-triage"))
                .args(["wake-at", &until.to_string()])
                .env("HERDR_SOCKET_PATH", &herdr.socket)
                .env("HERDR_PLUGIN_STATE_DIR", herdr.state_dir())
                .spawn()
                .unwrap()
        })
        .collect();
    let started = std::time::Instant::now();
    let (sleepers, locks) = loop {
        let sleepers = wakers
            .iter_mut()
            .filter_map(|waker| waker.try_wait().unwrap().is_none().then_some(()))
            .count();
        let locks = [now + 4, now + 6]
            .into_iter()
            .filter(|until| server.join(format!("wake-{until}.lock")).exists())
            .count();
        if (sleepers, locks) == (2, 2) || started.elapsed() > std::time::Duration::from_millis(2500)
        {
            break (sleepers, locks);
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    for waker in &mut wakers {
        assert!(waker.wait().unwrap().success());
    }
    assert_eq!(sleepers, 2, "one sleeper per deadline");
    assert_eq!(locks, 2, "both waiting deadlines have locks in their store");
    assert_eq!(herdr.take("agent.list").len(), 2, "one sync per deadline");
    let servers = std::fs::read_dir(herdr.state_dir()).unwrap();
    for server in servers
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_dir())
    {
        for entry in std::fs::read_dir(server).unwrap() {
            assert!(
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("wake-"),
                "expired wake lock remains"
            );
        }
    }
}

#[test]
fn startup_prunes_only_expired_wake_locks() {
    let herdr = FakeHerdr::start(vec![]);
    herdr.run(&["sync"]);
    let server = server_dir(&herdr);
    let past = server.join("wake-0.lock");
    let future = server.join("wake-4102444800.lock");
    let unrelated = server.join("wake-unknown.lock");
    for path in [&past, &future, &unrelated] {
        std::fs::write(path, "").unwrap();
    }
    herdr.run(&["startup"]);
    assert!(!past.exists());
    assert!(future.exists());
    assert!(unrelated.exists());
}

#[test]
fn an_overdue_waker_started_during_sync_requests_another_sync() {
    let herdr = FakeHerdr::start(vec![]);
    herdr.run(&["sync"]);
    herdr.take("agent.list");
    let state_lock = std::fs::OpenOptions::new()
        .write(true)
        .open(server_dir(&herdr).join("state.lock"))
        .unwrap();
    *herdr.list_delay.lock().unwrap() = std::time::Duration::from_secs(2);
    let spawn = || {
        Command::new(env!("CARGO_BIN_EXE_herdr-triage"))
            .args(["wake-at", "0"])
            .env("HERDR_SOCKET_PATH", &herdr.socket)
            .env("HERDR_PLUGIN_STATE_DIR", herdr.state_dir())
            .spawn()
            .unwrap()
    };
    let mut first = spawn();
    let started = std::time::Instant::now();
    let syncing = loop {
        match state_lock.try_lock() {
            Err(std::fs::TryLockError::WouldBlock) => break true,
            Ok(()) => state_lock.unlock().unwrap(),
            Err(error) => panic!("{error}"),
        }
        if started.elapsed() > std::time::Duration::from_secs(3) {
            break false;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    let mut second = spawn();
    assert!(first.wait().unwrap().success());
    assert!(second.wait().unwrap().success());
    assert!(syncing, "first waker never entered sync");
    assert_eq!(herdr.take("agent.list").len(), 2);
}

#[test]
fn hooks_keep_a_link_to_the_binary_where_mcp_clients_start_it() {
    let herdr = FakeHerdr::start(vec![agent("w1:p1", "idle", 3)]);
    let link = herdr.dir.path().join("state/herdr-triage");
    std::fs::create_dir_all(link.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink("/gone/herdr-triage", &link).unwrap();
    herdr.run(&["sync"]);
    let binary = std::fs::canonicalize(env!("CARGO_BIN_EXE_herdr-triage")).unwrap();
    assert_eq!(std::fs::read_link(&link).unwrap(), binary);
    herdr.run(&["startup"]);
    assert_eq!(std::fs::read_link(&link).unwrap(), binary);
    assert!(!link.with_extension("new").exists());
}

#[test]
fn mcp_serves_a_client_over_stdio_and_leaves_a_settings_file_to_edit() {
    use std::io::Write;
    use std::process::Stdio;

    let herdr = FakeHerdr::start(vec![agent("w1:p1", "done", 3)]);
    let config = herdr.dir.path().join("xdg");
    let mut server = Command::new(env!("CARGO_BIN_EXE_herdr-triage"))
        .arg("mcp")
        .env("HERDR_SOCKET_PATH", &herdr.socket)
        .env("HERDR_PLUGIN_STATE_DIR", herdr.dir.path().join("state"))
        .env("XDG_CONFIG_HOME", &config)
        .env("HERDR_PANE_ID", "w1:p1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let requests = [
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
               "params": {"protocolVersion": "2025-06-18", "clientInfo": {"name": "test"}}}),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
               "params": {"name": "herdr_threads", "arguments": {}}}),
        json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
               "params": {"name": "herdr_launch", "arguments": {"title": "Review the diff"}}}),
    ];
    let mut stdin = server.stdin.take().unwrap();
    for request in requests {
        writeln!(stdin, "{request}").unwrap();
    }
    drop(stdin);
    let output = server.wait_with_output().unwrap();
    assert!(output.status.success());
    let replies: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();

    assert_eq!(replies.len(), 3, "the notification gets no reply");
    assert_eq!(replies[0]["result"]["protocolVersion"], "2025-06-18");
    let threads = &replies[1]["result"]["structuredContent"]["threads"];
    assert_eq!(threads[0]["pane_id"], "w1:p1");
    assert_eq!(threads[0]["state"], "done");
    let launched = &replies[2]["result"];
    assert_eq!(launched["isError"], false, "{launched}");
    assert_eq!(launched["structuredContent"]["launched"]["harness"], "pi");
    assert_eq!(
        herdr.take("worktree.create")[0]["workspace_id"],
        "w1",
        "a caller in a Herdr pane launches in its own project"
    );
    let settings = std::fs::read_to_string(config.join("herdr-triage/mcp.yaml")).unwrap();
    assert!(settings.contains("default_harness: pi"));
}

struct Desktop {
    herdr: FakeHerdr,
    claude: std::path::PathBuf,
    codex: std::path::PathBuf,
}

fn desktop() -> Desktop {
    let herdr = FakeHerdr::start(vec![]);
    let home = herdr.dir.path().join("home");
    let claude = if cfg!(target_os = "macos") {
        home.join("Library/Application Support/Claude")
    } else {
        home.join(".config/Claude")
    };
    let codex = home.join(".codex");
    std::fs::create_dir_all(&claude).unwrap();
    std::fs::create_dir_all(&codex).unwrap();
    Desktop {
        herdr,
        claude: claude.join("claude_desktop_config.json"),
        codex: codex.join("config.toml"),
    }
}

impl Desktop {
    fn connect(&self) -> Result<String, String> {
        let output = Command::new(env!("CARGO_BIN_EXE_herdr-triage"))
            .args(["mcp", "connect"])
            .env("HOME", self.herdr.dir.path().join("home"))
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("CODEX_HOME")
            .env("HERDR_SOCKET_PATH", &self.herdr.socket)
            .env("HERDR_PLUGIN_STATE_DIR", self.herdr.state_dir())
            .output()
            .unwrap();
        let said = if output.status.success() {
            Ok(String::from_utf8(output.stdout).unwrap().trim().to_owned())
        } else {
            let said = String::from_utf8(output.stderr).unwrap();
            Err(said.trim().trim_start_matches("herdr-triage: ").to_owned())
        };
        let notes = self.herdr.take("notification.show");
        let (title, shown) = match &said {
            Ok(shown) => ("Herdr MCP connected", shown),
            Err(shown) => ("Herdr MCP connect failed", shown),
        };
        assert_eq!(notes.len(), 1, "{shown}");
        assert_eq!(notes[0], json!({"title": title, "body": shown}));
        said
    }

    fn link(&self) -> String {
        let link = self.herdr.state_dir().join("herdr-triage");
        link.to_str().unwrap().to_owned()
    }

    fn claude_server(&self) -> Value {
        let settings = std::fs::read_to_string(&self.claude).unwrap();
        serde_json::from_str::<Value>(&settings).unwrap()["mcpServers"]["herdr"].clone()
    }

    fn codex_server(&self) -> toml::Value {
        let settings = std::fs::read_to_string(&self.codex).unwrap();
        settings.parse::<toml::Table>().unwrap()["mcp_servers"]["herdr"].clone()
    }
}

#[test]
fn connect_sets_both_desktop_apps_to_start_the_server_and_keeps_their_other_settings() {
    use std::os::unix::fs::PermissionsExt;

    let desktop = desktop();
    let claude_before =
        "{\n  \"preferences\": {\n    \"sidebarMode\": \"chat\"\n  },\n  \"alpha\": true\n}";
    std::fs::write(&desktop.claude, claude_before).unwrap();
    std::fs::set_permissions(&desktop.claude, std::fs::Permissions::from_mode(0o640)).unwrap();
    let codex_before = "# mine\nmodel = \"gpt-5.2-codex\"   # keep\n\n[mcp_servers.linear]\nurl = \"https://mcp.linear.app/mcp\"\n\n[projects.\"/src\"]\ntrust_level = \"trusted\"\n";
    std::fs::write(&desktop.codex, codex_before).unwrap();

    assert_eq!(
        desktop.connect().unwrap(),
        "Herdr MCP connected to Claude Desktop, ChatGPT and Codex: restart to load it"
    );

    let link = desktop.link();
    assert_eq!(
        std::fs::canonicalize(&link).unwrap(),
        std::fs::canonicalize(env!("CARGO_BIN_EXE_herdr-triage")).unwrap()
    );
    let home = desktop.herdr.dir.path().join("home");
    let pinned = json!({
        "HERDR_PLUGIN_STATE_DIR": desktop.herdr.state_dir(),
        "XDG_CONFIG_HOME": home.join(".config"),
    });
    assert_eq!(
        desktop.claude_server(),
        json!({"command": link, "args": ["mcp"], "env": pinned})
    );
    let claude_after = std::fs::read_to_string(&desktop.claude).unwrap();
    let unchanged = claude_before.strip_suffix("\n}").unwrap();
    assert!(claude_after.starts_with(unchanged), "{claude_after}");
    let mode = std::fs::metadata(&desktop.claude).unwrap().permissions();
    assert_eq!(mode.mode() & 0o777, 0o640);

    let server = desktop.codex_server();
    assert_eq!(server["command"].as_str(), Some(link.as_str()));
    assert_eq!(server["args"], toml::Value::from(vec!["mcp"]));
    assert_eq!(
        server["env_vars"],
        toml::Value::from(vec!["HERDR_PANE_ID", "HERDR_SOCKET_PATH"])
    );
    assert_eq!(server["tool_timeout_sec"].as_integer(), Some(300));
    assert_eq!(serde_json::to_value(&server["env"]).unwrap(), pinned);
    let codex_after = std::fs::read_to_string(&desktop.codex).unwrap();
    for kept in codex_before.split("\n\n") {
        assert!(codex_after.contains(kept.trim_end()), "{codex_after}");
    }

    let settings = std::fs::read_to_string(home.join(".config/herdr-triage/mcp.yaml")).unwrap();
    assert!(settings.contains("enabled: true"));

    desktop.connect().unwrap();
    assert_eq!(
        std::fs::read_to_string(&desktop.claude).unwrap(),
        claude_after
    );
    assert_eq!(
        std::fs::read_to_string(&desktop.codex).unwrap(),
        codex_after
    );
}

#[test]
fn connect_passes_over_an_app_that_is_not_installed() {
    let desktop = desktop();
    std::fs::remove_dir(desktop.claude.parent().unwrap()).unwrap();
    assert_eq!(
        desktop.connect().unwrap(),
        "Herdr MCP connected to ChatGPT and Codex: restart to load it"
    );
    assert!(!desktop.claude.parent().unwrap().exists());
    assert_eq!(
        desktop.codex_server()["args"],
        toml::Value::from(vec!["mcp"])
    );

    std::fs::remove_dir_all(desktop.codex.parent().unwrap()).unwrap();
    assert_eq!(
        desktop.connect().unwrap_err(),
        "No Claude Desktop, ChatGPT or Codex found to connect: open a new app once first"
    );
}

#[test]
fn connect_changes_nothing_on_a_machine_where_the_server_is_turned_off() {
    let desktop = desktop();
    let settings = desktop.herdr.dir.path().join("home/.config/herdr-triage");
    std::fs::create_dir_all(&settings).unwrap();
    std::fs::write(settings.join("mcp.yaml"), "enabled: false\n").unwrap();
    let refused = desktop.connect().unwrap_err();
    assert!(refused.contains("turned off on this machine"), "{refused}");
    assert!(refused.contains("mcp.yaml"), "{refused}");
    assert!(!desktop.claude.exists());
    assert!(!desktop.codex.exists());
    assert!(!desktop.herdr.state_dir().join("herdr-triage").exists());
}

#[test]
fn connect_leaves_settings_it_cannot_read_alone_and_still_connects_the_other_app() {
    let desktop = desktop();
    std::fs::write(&desktop.claude, "{ not json").unwrap();
    let said = desktop.connect().unwrap_err();
    let (connected, failed) = said.split_once(". ").unwrap();
    assert_eq!(
        connected,
        "Herdr MCP connected to ChatGPT and Codex: restart to load it"
    );
    assert!(
        failed.starts_with("Not connected to Claude Desktop: "),
        "{failed}"
    );
    assert!(failed.contains("claude_desktop_config.json"), "{failed}");
    assert_eq!(
        std::fs::read_to_string(&desktop.claude).unwrap(),
        "{ not json"
    );
    assert_eq!(
        desktop.codex_server()["args"],
        toml::Value::from(vec!["mcp"])
    );

    let secret = "token = \"hunter2\" oops\n";
    std::fs::write(&desktop.codex, secret).unwrap();
    let said = desktop.connect().unwrap_err();
    assert!(said.contains("config.toml"), "{said}");
    assert!(!said.contains("hunter2"), "{said}");
    assert_eq!(std::fs::read_to_string(&desktop.codex).unwrap(), secret);
}

#[test]
fn connect_repoints_an_entry_already_there_through_a_link_to_the_settings() {
    let desktop = desktop();
    let dotfiles = desktop.herdr.dir.path().join("dotfiles.toml");
    std::fs::write(
        &dotfiles,
        "[mcp_servers.herdr]\ncommand = \"/gone/herdr-triage\"\nargs = [\"mcp\"]\nenv_vars = [\"HERDR_PANE_ID\"]\n",
    )
    .unwrap();
    std::os::unix::fs::symlink(&dotfiles, &desktop.codex).unwrap();
    desktop.connect().unwrap();
    assert_eq!(std::fs::read_link(&desktop.codex).unwrap(), dotfiles);
    let server = desktop.codex_server();
    assert_eq!(server["command"].as_str(), Some(desktop.link().as_str()));
    assert_eq!(server["env_vars"], toml::Value::from(vec!["HERDR_PANE_ID"]));
    assert_eq!(server["tool_timeout_sec"].as_integer(), Some(300));
}

#[test]
fn connect_adds_the_server_to_settings_that_keep_their_servers_inline() {
    let desktop = desktop();
    for inline in [
        "mcp_servers = {}\n",
        "mcp_servers = { linear = { url = \"https://mcp.linear.app/mcp\" } }\n",
        "[mcp_servers]\nherdr = { command = \"/gone/herdr-triage\" }\n",
    ] {
        std::fs::write(&desktop.codex, inline).unwrap();
        desktop.connect().unwrap();
        let server = desktop.codex_server();
        assert_eq!(
            server["command"].as_str(),
            Some(desktop.link().as_str()),
            "{inline}"
        );
        assert!(server["env"]["XDG_CONFIG_HOME"].is_str(), "{inline}");
    }
}

#[test]
fn connect_refuses_settings_it_would_break_or_replace() {
    let desktop = desktop();
    let other_server = "[mcp_servers.herdr]\nurl = \"https://herdr.example/mcp\"\n";
    std::fs::write(&desktop.codex, other_server).unwrap();
    let dotfiles = desktop.herdr.dir.path().join("not-checked-out.json");
    std::os::unix::fs::symlink(&dotfiles, &desktop.claude).unwrap();

    let said = desktop.connect().unwrap_err();
    assert!(
        said.contains("another server named `herdr` is already there"),
        "{said}"
    );
    assert!(
        said.contains("a link to a file that is not there"),
        "{said}"
    );
    assert_eq!(
        std::fs::read_to_string(&desktop.codex).unwrap(),
        other_server
    );
    assert_eq!(std::fs::read_link(&desktop.claude).unwrap(), dotfiles);
    assert!(!dotfiles.exists());

    let other_program =
        "{\"mcpServers\":{\"herdr\":{\"command\":\"node\",\"args\":[\"herdr.js\"]}}}";
    std::fs::write(&dotfiles, other_program).unwrap();
    let other_program_inline = "mcp_servers = { herdr = { command = \"/opt/different-mcp\" } }\n";
    std::fs::write(&desktop.codex, other_program_inline).unwrap();
    let said = desktop.connect().unwrap_err();
    assert_eq!(said.matches("another server named `herdr`").count(), 2);
    assert_eq!(std::fs::read_to_string(&dotfiles).unwrap(), other_program);
    assert_eq!(
        std::fs::read_to_string(&desktop.codex).unwrap(),
        other_program_inline
    );
}

#[test]
fn connect_keeps_the_owner_group_and_mode_of_the_settings_it_rewrites() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let desktop = desktop();
    std::fs::write(&desktop.codex, "model = \"gpt-5.2-codex\"\n").unwrap();
    let groups = Command::new("id").arg("-G").output().unwrap();
    let groups = String::from_utf8(groups.stdout).unwrap();
    let mine = std::fs::metadata(&desktop.codex).unwrap().gid();
    let another = groups
        .split_whitespace()
        .map(|group| group.parse::<u32>().unwrap())
        .find(|group| *group != mine);
    std::os::unix::fs::chown(&desktop.codex, None, another).unwrap();
    std::fs::set_permissions(&desktop.codex, std::fs::Permissions::from_mode(0o640)).unwrap();

    desktop.connect().unwrap();
    let rewritten = std::fs::metadata(&desktop.codex).unwrap();
    assert_eq!(rewritten.gid(), another.unwrap_or(mine));
    assert_eq!(rewritten.mode() & 0o777, 0o640);
    assert!(desktop.codex_server()["command"].is_str());
}

#[test]
fn a_server_an_app_starts_drives_the_plugins_herdr_and_obeys_its_off_switch() {
    use std::io::Write;
    use std::process::Stdio;

    let desktop = desktop();
    let home = desktop.herdr.dir.path().join("home");
    let elsewhere = desktop.herdr.dir.path().join("xdg");
    let connect = Command::new(env!("CARGO_BIN_EXE_herdr-triage"))
        .args(["mcp", "connect"])
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &elsewhere)
        .env("CODEX_HOME", desktop.codex.parent().unwrap())
        .env("HERDR_SOCKET_PATH", &desktop.herdr.socket)
        .env("HERDR_PLUGIN_STATE_DIR", desktop.herdr.state_dir())
        .output()
        .unwrap();
    assert!(connect.status.success());
    let claude = desktop.claude_server();
    assert_eq!(
        claude["env"]["XDG_CONFIG_HOME"],
        json!(elsewhere),
        "found in the usual folder, pinned to the plugin's"
    );
    std::fs::create_dir_all(elsewhere.join("herdr")).unwrap();
    std::os::unix::fs::symlink(&desktop.herdr.socket, elsewhere.join("herdr/herdr.sock")).unwrap();

    let ask = |request: Value| {
        let pinned = claude["env"].as_object().unwrap();
        let mut server = Command::new(claude["command"].as_str().unwrap())
            .arg("mcp")
            .env_clear()
            .env("HOME", &home)
            .envs(
                pinned
                    .iter()
                    .map(|(name, value)| (name, value.as_str().unwrap())),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        writeln!(server.stdin.take().unwrap(), "{request}").unwrap();
        let output = server.wait_with_output().unwrap();
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    let threads = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                         "params": {"name": "herdr_threads", "arguments": {}}});
    desktop.herdr.take("agent.list");
    let listed = ask(threads.clone());
    assert_eq!(listed["result"]["isError"], false, "{listed}");
    assert!(!desktop.herdr.take("agent.list").is_empty());

    std::fs::write(elsewhere.join("herdr-triage/mcp.yaml"), "enabled: false\n").unwrap();
    let tools = ask(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}));
    assert_eq!(tools["result"]["tools"], json!([]));
    let refused = ask(threads);
    let refused = &refused["result"]["structuredContent"]["error"];
    assert_eq!(refused["code"], "disabled");
    assert!(desktop.herdr.take("agent.list").is_empty());
}
