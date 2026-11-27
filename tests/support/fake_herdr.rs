#![allow(clippy::unwrap_used)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{Value, json};

pub struct FakeHerdr {
    pub dir: tempfile::TempDir,
    pub socket: PathBuf,
    pub agents: Arc<Mutex<Vec<Value>>>,
    pub workspaces: Arc<Mutex<Vec<Value>>>,
    pub requests: Arc<Mutex<Vec<Value>>>,
    pub fail_method: Arc<Mutex<Option<String>>>,
    pub list_delay: Arc<Mutex<std::time::Duration>>,
    pub tabs: Arc<Mutex<Vec<Value>>>,
    pub notification_shown: Arc<Mutex<bool>>,
    pub fail_code: Arc<Mutex<String>>,
    pub worktree_lists: Arc<Mutex<std::collections::BTreeMap<String, Value>>>,
    pub screen: Arc<Mutex<String>>,
    hooks: Arc<Mutex<Vec<(String, Hook)>>>,
}

type Hook = Box<dyn FnMut(&mut Vec<Value>) + Send>;

impl FakeHerdr {
    pub fn start(agents: Vec<Value>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("herdr.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let fake = Self {
            dir,
            socket,
            agents: Arc::new(Mutex::new(agents)),
            workspaces: Arc::new(Mutex::new(vec![
                json!({"workspace_id": "w1", "label": "acme-api", "number": 1, "tab_count": 1, "pane_count": 2}),
            ])),
            requests: Arc::new(Mutex::new(Vec::new())),
            fail_method: Arc::new(Mutex::new(None)),
            list_delay: Arc::new(Mutex::new(std::time::Duration::ZERO)),
            tabs: Arc::new(Mutex::new(Vec::new())),
            notification_shown: Arc::new(Mutex::new(true)),
            fail_code: Arc::new(Mutex::new("failed".into())),
            screen: Arc::new(Mutex::new(String::new())),
            worktree_lists: Arc::default(),
            hooks: Arc::new(Mutex::new(Vec::new())),
        };
        let shown = fake.notification_shown.clone();
        let (fail_code, screen) = (fake.fail_code.clone(), fake.screen.clone());
        let worktree_lists = fake.worktree_lists.clone();
        let (agents, workspaces, tabs, requests, fail, list_delay, hooks) = (
            fake.agents.clone(),
            fake.workspaces.clone(),
            fake.tabs.clone(),
            fake.requests.clone(),
            fake.fail_method.clone(),
            fake.list_delay.clone(),
            fake.hooks.clone(),
        );
        thread::spawn(move || {
            let mut opened = 0;
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let mut writer = stream.try_clone().unwrap();
                let mut line = String::new();
                if BufReader::new(stream).read_line(&mut line).unwrap_or(0) == 0 {
                    continue;
                }
                let request: Value = serde_json::from_str(&line).unwrap();
                let params = &request["params"];
                // A refused request leaves Herdr as it was.
                let failing = fail.lock().unwrap().as_deref() == request["method"].as_str();
                let method = if failing {
                    ""
                } else {
                    request["method"].as_str().unwrap()
                };
                let result = match method {
                    "agent.list" => {
                        std::thread::sleep(*list_delay.lock().unwrap());
                        json!({"type": "agent_list", "agents": *agents.lock().unwrap()})
                    }
                    "workspace.list" => {
                        json!({"type": "workspace_list", "workspaces": *workspaces.lock().unwrap()})
                    }
                    "tab.list" => json!({"type": "tab_list", "tabs": *tabs.lock().unwrap()}),
                    "pane.report_metadata" => {
                        store_tokens(&mut agents.lock().unwrap(), params);
                        json!({"type": "ok"})
                    }
                    "pane.close" => {
                        close_pane(&mut agents.lock().unwrap(), &workspaces, params);
                        json!({"type": "ok"})
                    }
                    "notification.show" => {
                        let shown = *shown.lock().unwrap();
                        json!({"type": "notification_show", "shown": shown, "reason": (!shown).then_some("rate_limited")})
                    }
                    "agent.view.set" => json!({"type": "agent_view", "active": true}),
                    method => agent_api(
                        method,
                        params,
                        &mut agents.lock().unwrap(),
                        &screen,
                        &worktree_lists.lock().unwrap(),
                        &workspaces.lock().unwrap(),
                        &mut opened,
                    ),
                };
                for (method, hook) in hooks.lock().unwrap().iter_mut() {
                    if request["method"] == method.as_str() {
                        hook(&mut agents.lock().unwrap());
                    }
                }
                requests.lock().unwrap().push(request.clone());
                let reply = if failing {
                    json!({"id": request["id"], "error": {"code": *fail_code.lock().unwrap(), "message": "injected"}})
                } else {
                    json!({"id": request["id"], "result": result})
                };
                // Herdr serializes the reply to a string and writes it whole (src/api/server.rs);
                // formatting straight into the socket would send it in thousands of fragments.
                writer.write_all(reply.to_string().as_bytes()).unwrap();
                writer.write_all(b"\n").unwrap();
            }
        });
        fake
    }

    pub fn sized(count: usize, workspaces: usize) -> Self {
        assert!(workspaces > 0);
        let agents = (0..count).map(|i| {
            let workspace = format!("w{}", i % workspaces + 1);
            let pane = format!("{workspace}:p{i}");
            let mut row = agent(&pane, ["done", "blocked", "working", "idle"][i % 4], 1);
            row["workspace_id"] = json!(workspace);
            row["tab_id"] = json!(format!("{workspace}:t{i}"));
            row["agent_session"] = json!({"source":"herdr:claude", "agent":"claude", "kind":"id", "value":format!("bench-{i}")});
            row
        }).collect();
        let fake = Self::start(agents);
        *fake.workspaces.lock().unwrap() = (0..workspaces).map(|i| json!({
            "workspace_id":format!("w{}", i+1), "label":format!("bench-{}", i+1),
            "number":i+1, "tab_count":count.div_ceil(workspaces), "pane_count":count.div_ceil(workspaces)
        })).collect();
        fake
    }

    /// Runs `hook` on Herdr's agents each time `method` is called, after answering it: something
    /// that happens in Herdr, or a hook that fires, while a command is midway.
    #[allow(
        dead_code,
        reason = "only the unit tests stage changes midway through a command"
    )]
    pub fn on(&self, method: &str, hook: impl FnMut(&mut Vec<Value>) + Send + 'static) {
        self.hooks
            .lock()
            .unwrap()
            .push((method.to_owned(), Box::new(hook)));
    }

    /// Refuses every call to `method` until `stop_failing`, answering with `fail_code`.
    #[allow(dead_code, reason = "the benches never fail a call")]
    pub fn fail(&self, method: &str) {
        *self.fail_method.lock().unwrap() = Some(method.to_owned());
    }

    #[allow(dead_code, reason = "only the unit tests stop failing midway")]
    pub fn stop_failing(&self) {
        *self.fail_method.lock().unwrap() = None;
    }

    pub fn counts(&self) -> std::collections::BTreeMap<String, usize> {
        let mut counts = std::collections::BTreeMap::new();
        for request in self.requests.lock().unwrap().iter() {
            *counts
                .entry(request["method"].as_str().unwrap().to_owned())
                .or_default() += 1;
        }
        counts
    }

    pub fn state_dir(&self) -> PathBuf {
        self.dir.path().join("state")
    }

    pub fn take(&self, method: &str) -> Vec<Value> {
        let mut requests = self.requests.lock().unwrap();
        let (taken, kept) = requests.drain(..).partition(|r| r["method"] == method);
        *requests = kept;
        taken.into_iter().map(|r| r["params"].clone()).collect()
    }
}

fn agent_api(
    method: &str,
    params: &Value,
    agents: &mut Vec<Value>,
    screen: &Mutex<String>,
    worktree_lists: &std::collections::BTreeMap<String, Value>,
    workspaces: &[Value],
    opened: &mut i32,
) -> Value {
    match method {
        "worktree.list" => {
            let found = if let Some(cwd) = params["cwd"].as_str() {
                worktree_lists.values().find(|listing| {
                    let source = listing["source"]["repo_root"].as_str().unwrap();
                    let worktrees = listing["worktrees"].as_array().unwrap();
                    std::iter::once(source)
                        .chain(worktrees.iter().map(|w| w["path"].as_str().unwrap()))
                        .any(|dir| std::path::Path::new(cwd).starts_with(dir))
                })
            } else {
                worktree_lists.get(params["workspace_id"].as_str().unwrap_or_default())
            };
            found
                .cloned()
                .unwrap_or_else(|| worktree_source(workspaces, params))
        }
        "worktree.create" => {
            *opened += 1;
            let workspace = format!("w9{opened}");
            let branch = params["branch"].as_str().unwrap_or("detached");
            json!({"type": "worktree_created",
                   "workspace": {"workspace_id": workspace},
                   "tab": {"tab_id": format!("{workspace}:t1"), "workspace_id": workspace},
                   "root_pane": {"pane_id": format!("{workspace}:p1")},
                   "worktree": {"path": format!("/worktrees/{branch}"), "branch": branch}})
        }
        "tab.create" => {
            *opened += 1;
            let workspace = params["workspace_id"].as_str().unwrap_or("w1");
            json!({"type": "tab_created",
                   "tab": {"tab_id": format!("{workspace}:t9{opened}"), "workspace_id": workspace},
                   "root_pane": {"pane_id": format!("{workspace}:p9{opened}")}})
        }
        "agent.start" => {
            let pane = params["pane_id"].as_str().unwrap();
            let mut started = agent(pane, "idle", 1);
            started["workspace_id"] = json!(pane.split(':').next().unwrap());
            started["agent"] = params["kind"].clone();
            started["name"] = params["name"].clone();
            agents.push(started.clone());
            json!({"type": "agent_started", "agent": started, "argv": params["args"]})
        }
        "agent.prompt" => {
            if let Some(agent) = agents.iter_mut().find(|a| a["pane_id"] == params["target"]) {
                agent["agent_status"] = json!("working");
                agent["state_change_seq"] = json!(agent["state_change_seq"].as_u64().unwrap() + 1);
            }
            json!({"type": "agent_prompted"})
        }
        "agent.get" => {
            let mut found = agents
                .iter()
                .find(|a| a["pane_id"] == params["target"])
                .cloned();
            if let Some(agent) = &mut found {
                agent["interactive_ready"] = json!(agent["agent_status"] != "blocked");
            }
            json!({"type": "agent_info", "agent": found})
        }
        "pane.get" => {
            let pane = params["pane_id"].as_str().unwrap();
            let workspace = agents.iter().find(|a| a["pane_id"] == pane).map_or_else(
                || json!(pane.split(':').next()),
                |a| a["workspace_id"].clone(),
            );
            json!({"type": "pane_info", "pane": {
                "pane_id": pane, "workspace_id": workspace, "terminal_id": format!("t-{pane}"),
            }})
        }
        "agent.wait" => json!({"type": "wait_matched"}),
        "agent.read" | "pane.read" => json!({"type": "pane_read", "read": {
            "pane_id": params["target"], "text": *screen.lock().unwrap(), "truncated": false,
        }}),
        _ => json!({"type": "ok"}),
    }
}

fn store_tokens(agents: &mut [Value], params: &Value) {
    let Some(agent) = agents
        .iter_mut()
        .find(|a| a["pane_id"] == params["pane_id"])
    else {
        return;
    };
    if agent.get("tokens").is_none() {
        agent["tokens"] = json!({});
    }
    for (name, value) in params["tokens"].as_object().unwrap() {
        let tokens = agent["tokens"].as_object_mut().unwrap();
        if value.is_null() {
            tokens.remove(name);
        } else {
            tokens.insert(name.clone(), value.clone());
        }
    }
}

fn worktree_source(workspaces: &[Value], params: &Value) -> Value {
    let active = workspaces.first();
    let workspace = workspaces
        .iter()
        .find(|w| w["workspace_id"] == params["workspace_id"])
        .or(active);
    let root = workspace.map_or_else(String::new, |w| {
        w["worktree"]["repo_root"].as_str().map_or_else(
            || format!("/work/{}", w["label"].as_str().unwrap_or("")),
            str::to_owned,
        )
    });
    json!({"type": "worktree_list", "source": {"repo_root": root}, "worktrees": []})
}

fn close_pane(agents: &mut Vec<Value>, workspaces: &Mutex<Vec<Value>>, params: &Value) {
    let closed = agents.iter().find(|a| a["pane_id"] == params["pane_id"]);
    if let Some(workspace) = closed.map(|a| a["workspace_id"].clone()) {
        for w in workspaces.lock().unwrap().iter_mut() {
            if w["workspace_id"] == workspace
                && let Some(count) = w["pane_count"].as_u64()
            {
                w["pane_count"] = json!(count.saturating_sub(1));
            }
        }
    }
    agents.retain(|a| a["pane_id"] != params["pane_id"]);
}

pub fn agent(pane: &str, status: &str, seq: u64) -> Value {
    json!({
        "pane_id": pane, "terminal_id": format!("t-{pane}"), "workspace_id": "w1", "tab_id": "w1:t1",
        "agent": "claude", "agent_status": status, "state_change_seq": seq,
        "focused": false, "revision": 1, "cwd": "/tmp"
    })
}

#[cfg(test)]
mod bench_contracts {

    #[test]
    fn sized_fixture_spreads_resumable_agents_across_workspaces() {
        let fake = super::FakeHerdr::sized(12, 3);
        let agents = fake.agents.lock().unwrap();
        assert_eq!(agents.len(), 12);
        assert_eq!(fake.workspaces.lock().unwrap().len(), 3);
        assert_eq!(
            agents.iter().filter(|a| a["workspace_id"] == "w2").count(),
            4
        );
        assert_eq!(agents[0]["agent_session"]["source"], "herdr:claude");
    }

    #[test]
    fn request_counts_report_each_method_exactly() {
        let fake = super::FakeHerdr::start(vec![]);
        *fake.requests.lock().unwrap() = vec![
            serde_json::json!({"method":"agent.list"}),
            serde_json::json!({"method":"agent.list"}),
            serde_json::json!({"method":"tab.list"}),
        ];
        assert_eq!(
            fake.counts(),
            std::collections::BTreeMap::from([
                ("agent.list".to_owned(), 2),
                ("tab.list".to_owned(), 1)
            ])
        );
    }
}
