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
    /// Creates a file when a method is called, standing in for a hook that fires meanwhile.
    pub touch_on: Arc<Mutex<Option<(String, PathBuf)>>>,
}

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
            touch_on: Arc::new(Mutex::new(None)),
        };
        let (agents, workspaces, requests, fail, list_delay, touch_on) = (
            fake.agents.clone(),
            fake.workspaces.clone(),
            fake.requests.clone(),
            fake.fail_method.clone(),
            fake.list_delay.clone(),
            fake.touch_on.clone(),
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
                let result = match request["method"].as_str().unwrap() {
                    "agent.list" => {
                        std::thread::sleep(*list_delay.lock().unwrap());
                        json!({"type": "agent_list", "agents": *agents.lock().unwrap()})
                    }
                    "workspace.list" => {
                        json!({"type": "workspace_list", "workspaces": *workspaces.lock().unwrap()})
                    }
                    "tab.list" => json!({"type": "tab_list", "tabs": []}),
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
                if let Some((method, path)) = &*touch_on.lock().unwrap()
                    && request["method"] == method.as_str()
                {
                    std::fs::write(path, b"").unwrap();
                }
                requests.lock().unwrap().push(request.clone());
                let reply = if fail.lock().unwrap().as_deref() == request["method"].as_str() {
                    json!({"id": request["id"], "error": {"code": "failed", "message": "injected"}})
                } else {
                    json!({"id": request["id"], "result": result})
                };
                writeln!(writer, "{reply}").unwrap();
            }
        });
        fake
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
