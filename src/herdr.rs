#![allow(
    clippy::struct_field_names,
    reason = "wire types mirror Herdr's JSON field names"
)]

use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::triage::Status;

const TIMEOUT: Duration = Duration::from_secs(3);
const WORKTREE_TIMEOUT: Duration = Duration::from_secs(60);

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
    #[serde(default)]
    pub worktree: Option<Checkout>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Checkout {
    pub repo_key: String,
    pub repo_name: String,
    pub repo_root: String,
    pub checkout_path: String,
    #[serde(default)]
    pub is_linked_worktree: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    pub code: String,
    pub message: String,
}

impl Workspace {
    pub fn project_label(&self) -> String {
        match &self.worktree {
            Some(Checkout {
                repo_name,
                is_linked_worktree: true,
                ..
            }) if !self.label.starts_with(repo_name.as_str()) => {
                format!("{repo_name}/{}", self.label)
            }
            _ => self.label.clone(),
        }
    }
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
    pub deadline: Option<Instant>,
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
            deadline: None,
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
        self.call_within(method, params, TIMEOUT)
    }

    fn call_within(
        &mut self,
        method: &str,
        params: &Value,
        timeout: Duration,
    ) -> io::Result<Value> {
        self.request(method, params, timeout)?
            .map_err(|refused| refusal(method, &refused))
    }

    pub fn request(
        &mut self,
        method: &str,
        params: &Value,
        timeout: Duration,
    ) -> io::Result<Result<Value, Refused>> {
        let timeout = match self.deadline {
            None => timeout,
            Some(deadline) => {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "out of time"));
                }
                timeout.min(left)
            }
        };
        self.next_id += 1;
        let id = format!("triage-{}", self.next_id);
        let request = json!({"id": id, "method": method, "params": params});
        // Herdr answers one request per connection, then closes it.
        let mut stream = UnixStream::connect(&self.path)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
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
                let text = |field: &str, missing: &str| {
                    error[field].as_str().unwrap_or(missing).to_owned()
                };
                return Ok(Err(Refused {
                    code: text("code", "failed"),
                    message: text("message", "request failed"),
                }));
            }
            return Ok(Ok(reply["result"].take()));
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

    pub fn notify(&mut self, title: &str) -> io::Result<bool> {
        let result = self.call("notification.show", &json!({"title": title}))?;
        Ok(result["shown"].as_bool().unwrap_or(true))
    }

    pub fn notify_at_length(&mut self, title: &str, body: &str) -> io::Result<()> {
        let params = json!({"title": title, "body": body});
        self.call("notification.show", &params).map(drop)
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

    pub fn has_pane(&mut self, pane_id: &str) -> io::Result<bool> {
        let found = self.request("pane.get", &json!({"pane_id": pane_id}), TIMEOUT)?;
        Ok(found.is_ok())
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
        focus: bool,
    ) -> io::Result<String> {
        let params = json!({"workspace_id": workspace_id, "cwd": cwd, "focus": focus});
        let result = self.call("tab.create", &params)?;
        root_pane(&result).ok_or_else(|| io::Error::other("tab.create returned no pane"))
    }

    pub fn worktrees(
        &mut self,
        of: Repository,
        within: Duration,
    ) -> io::Result<Result<Worktrees, Refused>> {
        let params = match of {
            Repository::Active => json!({}),
            Repository::Workspace(workspace_id) => json!({"workspace_id": workspace_id}),
            Repository::Containing(cwd) => json!({"cwd": cwd}),
        };
        let listed = self.request("worktree.list", &params, within)?;
        Ok(listed.map(|listed| Worktrees::read(&listed)))
    }

    pub fn repository_root(&mut self, workspace_id: Option<&str>) -> io::Result<String> {
        let of = workspace_id.map_or(Repository::Active, Repository::Workspace);
        self.worktrees(of, TIMEOUT)?
            .map_err(|refused| refusal("worktree.list", &refused))?
            .repo_root
            .ok_or_else(|| io::Error::other("worktree.list returned no repository"))
    }

    pub fn create_worktree(&mut self, root: &str, branch: &str) -> io::Result<String> {
        let params = json!({"cwd": root, "branch": branch, "focus": true});
        let result = self.call_within("worktree.create", &params, WORKTREE_TIMEOUT)?;
        root_pane(&result).ok_or_else(|| io::Error::other("worktree.create returned no pane"))
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

fn refusal(method: &str, refused: &Refused) -> io::Error {
    io::Error::other(format!("{method}: {}", refused.message))
}

/// The pane that `tab.create` or `worktree.create` opened.
pub fn root_pane(created: &Value) -> Option<String> {
    created["root_pane"]["pane_id"].as_str().map(str::to_owned)
}

/// Which repository `worktree.list` lists.
#[derive(Clone, Copy)]
pub enum Repository<'a> {
    /// The active workspace's.
    Active,
    Workspace(&'a str),
    Containing(&'a str),
}

/// A `worktree.list` reply. Herdr leaves fields out, so each is read on its own and one bad row
/// spares the rest.
pub struct Worktrees {
    pub repo_key: Option<String>,
    pub repo_root: Option<String>,
    /// Each checkout open in a workspace, with that workspace's id.
    pub open: Vec<(String, Checkout)>,
}

impl Worktrees {
    fn read(listed: &Value) -> Self {
        let source = &listed["source"];
        let text = |value: &Value| value.as_str().unwrap_or_default().to_owned();
        let open = listed["worktrees"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|worktree| {
                let workspace_id = worktree["open_workspace_id"].as_str()?.to_owned();
                let checkout = Checkout {
                    repo_key: text(&source["repo_key"]),
                    repo_name: text(&source["repo_name"]),
                    repo_root: text(&source["repo_root"]),
                    checkout_path: text(&worktree["path"]),
                    is_linked_worktree: worktree["is_linked_worktree"] == true,
                };
                Some((workspace_id, checkout))
            })
            .collect();
        Self {
            repo_key: source["repo_key"].as_str().map(str::to_owned),
            repo_root: source["repo_root"].as_str().map(str::to_owned),
            open,
        }
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
