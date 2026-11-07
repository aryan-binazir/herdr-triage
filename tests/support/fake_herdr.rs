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
            hooks: Arc::new(Mutex::new(Vec::new())),
        };
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
                        agents
                            .lock()
                            .unwrap()
                            .retain(|a| a["pane_id"] != params["pane_id"]);
                        json!({"type": "ok"})
                    }
                    "tab.create" => {
                        opened += 1;
                        let workspace = params["workspace_id"].as_str().unwrap_or("w1");
                        json!({"type": "tab_created",
                               "tab": {"tab_id": format!("{workspace}:t9{opened}"), "workspace_id": workspace},
                               "root_pane": {"pane_id": format!("{workspace}:p9{opened}")}})
                    }
                    "notification.show" => json!({"type": "notification_show", "shown": true}),
                    "agent.view.set" => json!({"type": "agent_view", "active": true}),
                    _ => json!({"type": "ok"}),
                };
                for (method, hook) in hooks.lock().unwrap().iter_mut() {
                    if request["method"] == method.as_str() {
                        hook(&mut agents.lock().unwrap());
                    }
                }
                requests.lock().unwrap().push(request.clone());
                let reply = if failing {
                    json!({"id": request["id"], "error": {"code": "failed", "message": "injected"}})
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
