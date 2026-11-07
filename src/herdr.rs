#![allow(
    clippy::struct_field_names,
    reason = "wire types mirror Herdr's JSON field names"
)]

use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::triage::Status;

const TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Deserialize)]
pub struct Agent {
    pub pane_id: String,
    pub terminal_id: String,
    pub workspace_id: String,
    pub tab_id: String,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub display_agent: Option<String>,
    pub agent_status: Status,
    #[serde(default)]
    pub state_change_seq: u64,
    pub focused: bool,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub terminal_title_stripped: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub agent_session: Option<AgentSession>,
    #[serde(default)]
    pub tokens: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentSession {
    #[serde(default)]
    pub source: String,
    pub agent: String,
    pub kind: String,
    pub value: String,
}

impl Agent {
    /// Herdr can keep a stale session reported for a different agent.
    pub fn session(&self) -> Option<&AgentSession> {
        self.agent_session
            .as_ref()
            .filter(|session| self.agent.as_deref() == Some(session.agent.as_str()))
    }

    pub fn key(&self) -> String {
        match self.session() {
            Some(session) => format!(
                "session:{}:{}:{}",
                session.agent, session.kind, session.value
            ),
            None => self.terminal_key(),
        }
    }

    pub fn terminal_key(&self) -> String {
        format!("terminal:{}", self.terminal_id)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Workspace {
    pub workspace_id: String,
    pub label: String,
    #[serde(default)]
    pub tab_count: usize,
    #[serde(default)]
    pub pane_count: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Tab {
    pub tab_id: String,
    pub label: String,
    #[serde(default)]
    pub number: usize,
}

pub struct Herdr {
    path: PathBuf,
    next_id: u64,
}

impl Herdr {
    pub fn connect() -> io::Result<Self> {
        Self::at(&socket_path())
    }

    pub fn at(path: &Path) -> io::Result<Self> {
        UnixStream::connect(path)?;
        Ok(Self {
            path: path.to_owned(),
            next_id: 0,
        })
    }

    pub fn server_generation(&self) -> String {
        std::fs::metadata(&self.path)
            .map(|meta| format!("{}-{}.{}", meta.ino(), meta.ctime(), meta.ctime_nsec()))
            .unwrap_or_default()
    }

    pub fn socket_path(&self) -> &std::path::Path {
        &self.path
    }

    fn call(&mut self, method: &str, params: &Value) -> io::Result<Value> {
        self.next_id += 1;
        let id = format!("triage-{}", self.next_id);
        let request = json!({"id": id, "method": method, "params": params});
        // Herdr answers one request per connection, then closes it.
        let mut stream = UnixStream::connect(&self.path)?;
        stream.set_read_timeout(Some(TIMEOUT))?;
        stream.set_write_timeout(Some(TIMEOUT))?;
        let mut bytes = serde_json::to_vec(&request).map_err(io::Error::other)?;
        bytes.push(b'\n');
        stream.write_all(&bytes)?;
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "herdr closed the socket",
                ));
            }
            let mut reply: Value = serde_json::from_str(&line).map_err(io::Error::other)?;
            if reply["id"] != id {
                continue;
            }
            if let Some(error) = reply.get("error") {
                let message = error["message"].as_str().unwrap_or("request failed");
                return Err(io::Error::other(format!("{method}: {message}")));
            }
            return Ok(reply["result"].take());
        }
    }

    fn list<T: for<'de> Deserialize<'de>>(
        &mut self,
        method: &str,
        field: &str,
    ) -> io::Result<Vec<T>> {
        let mut result = self.call(method, &json!({}))?;
        serde_json::from_value(result[field].take()).map_err(io::Error::other)
    }

    pub fn agents(&mut self) -> io::Result<Vec<Agent>> {
        self.list("agent.list", "agents")
    }

    pub fn workspaces(&mut self) -> io::Result<Vec<Workspace>> {
        self.list("workspace.list", "workspaces")
    }

    pub fn tabs(&mut self) -> io::Result<Vec<Tab>> {
        self.list("tab.list", "tabs")
    }

    pub fn focus(&mut self, pane_id: &str) -> io::Result<()> {
        self.call("agent.focus", &json!({"target": pane_id}))
            .map(drop)
    }

    pub fn notify(&mut self, title: &str) -> io::Result<()> {
        self.call("notification.show", &json!({"title": title}))
            .map(drop)
    }

    pub fn report_tokens(&mut self, pane_id: &str, tokens: &Map<String, Value>) -> io::Result<()> {
        let params = json!({"pane_id": pane_id, "source": source(), "tokens": tokens});
        self.call("pane.report_metadata", &params).map(drop)
    }

    pub fn set_view(&mut self, label: &str, hidden_ranks: &[&str]) -> io::Result<()> {
        let params = json!({
            "source": source(),
            "label": label,
            "filter": {"op": "not", "filter": {
                "op": "in", "field": {"token": "triage_rank"}, "values": hidden_ranks,
            }},
            "sort": [
                {"field": {"token": "triage_rank"}, "order": "asc"},
                {"field": "state_change_seq", "order": "desc"},
            ],
        });
        self.call("agent.view.set", &params).map(drop)
    }

    pub fn focus_pane(&mut self, pane_id: &str) -> io::Result<()> {
        self.call("pane.focus", &json!({"pane_id": pane_id}))
            .map(drop)
    }

    pub fn close_pane(&mut self, pane_id: &str) -> io::Result<()> {
        self.call("pane.close", &json!({"pane_id": pane_id}))
            .map(drop)
    }

    pub fn report_session(
        &mut self,
        pane_id: &str,
        agent: &str,
        session_id: &str,
    ) -> io::Result<()> {
        let params = json!({
            "pane_id": pane_id,
            "source": format!("herdr:{agent}"),
            "agent": agent,
            "agent_session_id": session_id,
            "session_start_source": "resume",
        });
        self.call("pane.report_agent_session", &params).map(drop)
    }

    pub fn create_tab(
        &mut self,
        workspace_id: Option<&str>,
        cwd: Option<&str>,
    ) -> io::Result<String> {
        let params = json!({"workspace_id": workspace_id, "cwd": cwd, "focus": true});
        let result = self.call("tab.create", &params)?;
        result["root_pane"]["pane_id"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| io::Error::other("tab.create returned no pane"))
    }

    pub fn run_command(&mut self, pane_id: &str, command: &str) -> io::Result<()> {
        let params = json!({"pane_id": pane_id, "text": command, "keys": ["Enter"]});
        self.call("pane.send_input", &params).map(drop)
    }

    pub fn open_picker(&mut self, focus_pane: Option<&str>) -> io::Result<()> {
        let mut env = Map::new();
        if let Some(pane) = focus_pane {
            env.insert("HERDR_TRIAGE_FOCUS".into(), pane.into());
        }
        let params =
            json!({"plugin_id": plugin_id(), "entrypoint": "picker", "focus": true, "env": env});
        self.call("plugin.pane.open", &params).map(drop)
    }
}

pub fn plugin_id() -> String {
    std::env::var("HERDR_PLUGIN_ID").unwrap_or_else(|_| "ar.triage".into())
}

fn source() -> String {
    format!("plugin:{}", plugin_id())
}

pub fn socket_path() -> PathBuf {
    if let Some(path) = std::env::var_os("HERDR_SOCKET_PATH") {
        return path.into();
    }
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_default();
    config.join("herdr").join("herdr.sock")
}
