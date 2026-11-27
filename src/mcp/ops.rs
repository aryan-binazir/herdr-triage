use std::io;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, TimeZone};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::Env;
use super::config::{Config, HARNESSES, Selection};
use crate::herdr::{self, Herdr, Refused, Repository, Workspace};
use crate::picker::{Item, Target};
use crate::session::{Outcome, Session};
use crate::triage::{Action, Display, Park, Want};

/// Inside the five minutes the README tells Codex to allow (it allows 60 by default).
pub const LAUNCH_BUDGET: Duration = Duration::from_secs(240);
const RESERVED_AFTER_CHECKOUT: Duration = Duration::from_secs(60);
const START_TIMEOUT: Duration = Duration::from_secs(40);
const POLL: Duration = Duration::from_millis(200);
const TURN_START_MS: u64 = 10_000;
const WAIT_DEFAULT_MS: u64 = 30_000;
const WAIT_MAX_MS: u64 = 50_000;
const RULE_STROKES: usize = 8;
const READ_DEFAULT_LINES: u64 = 100;
const READ_MAX_LINES: u64 = 2_000;
const READ_DEFAULT_CHARS: usize = 12_000;
const READ_MAX_CHARS: usize = 60_000;
const LIST_DEFAULT: usize = 50;
const QUICK: Duration = Duration::from_secs(10);
const MARGIN: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq)]
pub struct Failure {
    pub code: String,
    pub message: String,
    pub detail: Map<String, Value>,
}

impl Failure {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_owned(),
            message: message.into(),
            detail: Map::new(),
        }
    }

    fn with(mut self, name: &str, value: impl Into<Value>) -> Self {
        self.detail.insert(name.to_owned(), value.into());
        self
    }
}

impl From<io::Error> for Failure {
    fn from(error: io::Error) -> Self {
        Failure::new("herdr_unavailable", error.to_string())
    }
}

impl From<Refused> for Failure {
    fn from(refused: Refused) -> Self {
        let hint = match refused.code.as_str() {
            "agent_blocked" => {
                " The agent is waiting on an approval or a question: read it and ask the user."
            }
            "agent_prompt_stalled" => {
                " The message was typed but no turn was seen to start: read the thread before \
                 sending again."
            }
            "not_git_worktree" => {
                " Agents launch into a new worktree, so the project has to be a Git repository."
            }
            "worktree_create_failed" => " If the branch is taken, pass another `branch`.",
            _ => "",
        };
        Failure::new(
            &refused.code,
            format!("{}.{hint}", refused.message.trim_end_matches('.')),
        )
    }
}

type Done = Result<Value, Failure>;

pub struct Call<'a> {
    env: &'a Env,
    client: &'a str,
    herdr: Herdr,
    session: Session,
    config: &'a Config,
    caller: Option<Caller>,
}

#[derive(Clone)]
struct Caller {
    launched_by: String,
    workspace: Option<String>,
}

impl<'a> Call<'a> {
    pub fn open(env: &'a Env, client: &'a str, config: &'a Config) -> Result<Self, Failure> {
        Ok(Self {
            env,
            client,
            config,
            herdr: Herdr::at(&env.socket)?,
            session: (env.session)()?,
            caller: None,
        })
    }

    fn request(
        &mut self,
        method: &str,
        params: &Value,
        within: Duration,
    ) -> Result<Result<Value, Refused>, Failure> {
        let asked = self.herdr.request(method, params, within);
        asked.map_err(|error| self.late(error))
    }

    fn late(&self, error: io::Error) -> Failure {
        match self.herdr.deadline {
            Some(deadline) if Instant::now() >= deadline => out_of_time(),
            _ => error.into(),
        }
    }

    fn ask(&mut self, method: &str, params: &Value, within: Duration) -> Done {
        Ok(self.request(method, params, within)??)
    }

    fn caller(&mut self) -> Caller {
        if let Some(caller) = &self.caller {
            return caller.clone();
        }
        let pane = self.env.caller_pane.clone().and_then(|pane| {
            let found = self
                .ask("pane.get", &json!({"pane_id": pane}), QUICK)
                .ok()?;
            Some(found["pane"].clone()).filter(|pane| pane["terminal_id"].is_string())
        });
        let caller = match pane {
            Some(pane) => Caller {
                launched_by: format!(
                    "terminal:{}",
                    pane["terminal_id"].as_str().unwrap_or_default()
                ),
                workspace: pane["workspace_id"].as_str().map(str::to_owned),
            },
            None => Caller {
                launched_by: format!("client:{}", self.client),
                workspace: None,
            },
        };
        self.caller = Some(caller.clone());
        caller
    }

    fn threads(&mut self) -> Result<Threads, Failure> {
        let caller = self.caller().launched_by;
        let items = self.session.view()?.items;
        Ok(Threads { items, caller })
    }

    fn lookup(&mut self, id: &str) -> Result<(Threads, String), Failure> {
        let threads = self.threads()?;
        if threads.find(id).is_ok() || !id.starts_with("terminal:") {
            return Ok((threads, id.to_owned()));
        }
        let renamed = self
            .herdr
            .agents()?
            .into_iter()
            .find(|agent| agent.terminal_key() == id);
        Ok((
            threads,
            renamed.map_or_else(|| id.to_owned(), |agent| agent.key()),
        ))
    }

    /// `workspace.list` names the checkout only for some, so the rest are asked for one
    /// repository at a time.
    fn workspaces(&mut self) -> Result<Vec<Workspace>, Failure> {
        let mut listed = self.ask("workspace.list", &json!({}), QUICK)?;
        let mut workspaces: Vec<Workspace> = serde_json::from_value(listed["workspaces"].take())
            .map_err(|error| Failure::new("herdr_unavailable", error.to_string()))?;
        for at in 0..workspaces.len() {
            if workspaces[at].worktree.is_some() {
                continue;
            }
            let of = Repository::Workspace(&workspaces[at].workspace_id);
            let Ok(Ok(listed)) = self.herdr.worktrees(of, QUICK) else {
                continue;
            };
            for (open_in, checkout) in listed.open_by_workspace_id {
                let unplaced = workspaces
                    .iter_mut()
                    .find(|w| w.workspace_id == open_in && w.worktree.is_none());
                if let Some(workspace) = unplaced {
                    workspace.worktree = Some(checkout);
                }
            }
        }
        Ok(workspaces)
    }
}

pub fn config(env: &Env) -> Result<Config, Failure> {
    Config::load(env.config.as_deref())
        .map_err(|error| Failure::new("bad_config", error.to_string()))
}

struct Threads {
    items: Vec<Item>,
    caller: String,
}

impl Threads {
    fn mine(&self, item: &Item) -> bool {
        item.row.launched_by.as_deref() == Some(self.caller.as_str())
    }

    fn find(&self, id: &str) -> Result<&Item, Failure> {
        let pane = Target::Pane(id.to_owned());
        self.items
            .iter()
            .find(|item| item.row.key == id || item.target == pane)
            .ok_or_else(|| {
                Failure::new(
                    "thread_not_found",
                    format!(
                        "no thread `{id}`. A thread's id changes once, when its agent reports its \
                         session: herdr_threads lists the current ids."
                    ),
                )
            })
    }

    fn pane(&self, id: &str) -> Result<(&Item, String), Failure> {
        let item = self.find(id)?;
        match &item.target {
            Target::Pane(pane) => Ok((item, pane.clone())),
            _ => Err(Failure::new(
                "thread_closed",
                "that thread's pane is closed: herdr_organize with action `reopen` brings it back",
            )),
        }
    }

    fn describe(&self, item: &Item) -> Value {
        let row = &item.row;
        let state = match row.display {
            Display::Blocked => "blocked",
            Display::Working => "working",
            Display::Done => "done",
            Display::Unread(_) => "unread",
            Display::Idle => "idle",
            Display::Unknown => "unknown",
        };
        let (parked, until) = match row.park {
            Some(Park::Settled) => (Some("settled"), None),
            Some(Park::Snoozed { until }) => (Some("snoozed"), Some(rfc3339(until))),
            None => (None, None),
        };
        json!({
            "thread": row.key,
            "title": row.label(),
            "agent": item.kind,
            "state": state,
            "parked": parked,
            "snoozed_until": until,
            "pinned": row.pinned,
            "open": row.live(),
            "pane_id": match &item.target { Target::Pane(pane) => Some(pane), _ => None },
            "where": item.location,
            "cwd": row.thread.cwd,
            "mine": self.mine(item),
        })
    }
}

fn rfc3339(at: i64) -> String {
    Local
        .timestamp_opt(at, 0)
        .single()
        .map_or_else(|| at.to_string(), |time| time.to_rfc3339())
}

fn decode<T: for<'de> Deserialize<'de>>(arguments: Value) -> Result<T, Failure> {
    let arguments = if arguments.is_null() {
        json!({})
    } else {
        arguments
    };
    serde_json::from_value(arguments)
        .map_err(|error| Failure::new("invalid_arguments", error.to_string()))
}

struct Project<'a> {
    key: &'a str,
    name: &'a str,
    root: Option<&'a str>,
    workspaces: Vec<&'a Workspace>,
}

impl<'a> Project<'a> {
    /// Herdr refuses a linked worktree's workspace as a new worktree's source.
    fn worktree_source(&self) -> Option<&'a Workspace> {
        self.workspaces
            .iter()
            .find(|w| w.worktree.as_ref().is_none_or(|c| !c.is_linked_worktree))
            .copied()
    }

    fn names(&self) -> Vec<&'a str> {
        let labels = self.workspaces.iter().map(|w| w.label.as_str());
        std::iter::once(self.name).chain(labels).collect()
    }

    fn holds(&self, workspace_id: Option<&str>) -> bool {
        self.workspaces
            .iter()
            .any(|w| Some(w.workspace_id.as_str()) == workspace_id)
    }
}

fn projects(workspaces: &[Workspace]) -> Vec<Project<'_>> {
    let mut projects: Vec<Project> = Vec::new();
    for workspace in workspaces {
        let (key, name, root) = match &workspace.worktree {
            Some(c) => (
                c.repo_key.as_str(),
                c.repo_name.as_str(),
                Some(c.repo_root.as_str()),
            ),
            None => (
                workspace.workspace_id.as_str(),
                workspace.label.as_str(),
                None,
            ),
        };
        match projects.iter_mut().find(|project| project.key == key) {
            Some(project) => project.workspaces.push(workspace),
            None => projects.push(Project {
                key,
                name,
                root,
                workspaces: vec![workspace],
            }),
        }
    }
    projects
}

fn find_project<'a>(
    call: &mut Call,
    workspaces: &'a [Workspace],
    query: &str,
) -> Result<Project<'a>, Failure> {
    let mut all = projects(workspaces);
    let known: Vec<String> = all.iter().map(|project| project.name.to_owned()).collect();
    let by_id = all
        .iter()
        .position(|p| p.workspaces.iter().any(|w| w.workspace_id == query));
    if let Some(at) = by_id {
        return Ok(all.swap_remove(at));
    }
    if Path::new(query).is_absolute() {
        let found = call.herdr.worktrees(Repository::Containing(query), QUICK);
        let key = found.map_err(|error| call.late(error))??.repo_key;
        return match all
            .iter()
            .position(|project| Some(project.key) == key.as_deref())
        {
            Some(at) => Ok(all.swap_remove(at)),
            None => Err(Failure::new(
                "project_not_open",
                format!(
                    "`{query}` is in a repository with no workspace open in Herdr: open one first"
                ),
            )
            .with("projects", known)),
        };
    }
    let named: Vec<usize> = (0..all.len())
        .filter(|&at| {
            let project = &all[at];
            project.name.eq_ignore_ascii_case(query)
                || project
                    .workspaces
                    .iter()
                    .any(|w| w.label.eq_ignore_ascii_case(query))
        })
        .collect();
    match named[..] {
        [at] => Ok(all.swap_remove(at)),
        [] => Err(Failure::new(
            "project_not_found",
            format!("no open project `{query}`: pass a name from herdr_projects, a workspace id, or a path"),
        )
        .with("projects", known)),
        _ => {
            let ids: Vec<&str> = named
                .iter()
                .flat_map(|&at| all[at].workspaces.iter().map(|w| w.workspace_id.as_str()))
                .collect();
            Err(Failure::new(
                "project_ambiguous",
                format!("more than one open project answers to `{query}`: pass a workspace id or a path"),
            )
            .with("workspace_ids", ids))
        }
    }
}

fn selection_json(selection: &Selection) -> Value {
    json!({"harness": selection.harness, "model": selection.model, "effort": selection.effort})
}

pub fn projects_tool(call: &mut Call, arguments: Value) -> Done {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Args {}
    let Args {} = decode(arguments)?;
    let config = call.config;
    let workspaces = call.workspaces()?;
    let threads = call.threads()?;
    let listed: Vec<Value> = projects(&workspaces)
        .iter()
        .map(|project| {
            let open = threads.items.iter().filter(|item| {
                item.row.live() && project.holds(item.row.thread.workspace_id.as_deref())
            });
            let workspaces: Vec<Value> = project
                .workspaces
                .iter()
                .map(|w| {
                    json!({
                        "workspace_id": w.workspace_id,
                        "label": w.label,
                        "path": w.worktree.as_ref().map(|c| &c.checkout_path),
                        "linked_worktree": w.worktree.as_ref().is_some_and(|c| c.is_linked_worktree),
                    })
                })
                .collect();
            json!({
                "project": project.name,
                "repo_root": project.root,
                "workspaces": workspaces,
                "open_threads": open.count(),
                "launches": selection_json(&config.select(&project.names(), None, None, None)),
            })
        })
        .collect();
    Ok(json!({"projects": listed}))
}

pub fn capabilities_tool(call: &mut Call, arguments: Value) -> Done {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Args {}
    let Args {} = decode(arguments)?;
    let config = call.config;
    let harnesses: Map<String, Value> = HARNESSES
        .iter()
        .map(|name| {
            let harness = config.harness(name);
            let settings = json!({"model": harness.model, "effort": harness.effort});
            ((*name).to_owned(), settings)
        })
        .collect();
    let overrides: Map<String, Value> = config
        .projects
        .iter()
        .map(|(name, project)| {
            let settings = json!({
                "harness": project.harness, "model": project.model, "effort": project.effort,
            });
            (name.clone(), settings)
        })
        .collect();
    Ok(json!({
        "default_harness": config.default_harness,
        "harnesses": harnesses,
        "projects": overrides,
        "worktree": {"base": config.worktree.base, "branch_prefix": config.worktree.branch_prefix},
        "config_file": call.env.config,
        "note": "model and effort are passed to the harness as given; null means its own default",
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchArgs {
    title: String,
    project: Option<String>,
    message: Option<String>,
    harness: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    branch: Option<String>,
    base: Option<String>,
    pub request_id: Option<String>,
}

impl LaunchArgs {
    pub fn decode(arguments: Value) -> Result<Self, Failure> {
        let mut args: Self = decode(arguments)?;
        args.title = one_line(&args.title);
        if args.title.is_empty() {
            return Err(Failure::new("invalid_arguments", "title is empty"));
        }
        if let Some(message) = &args.message {
            typeable(message)?;
        }
        Ok(args)
    }
}

struct Opened {
    pane: String,
    detail: Map<String, Value>,
}

pub fn launch_tool(call: &mut Call, args: &LaunchArgs, budget: Duration) -> Done {
    let deadline = Instant::now() + budget;
    call.herdr.deadline = Some(deadline);
    let config = call.config;
    let caller = call.caller();
    let workspaces = call.workspaces()?;
    let query = args
        .project
        .as_deref()
        .or(caller.workspace.as_deref())
        .ok_or_else(|| {
            Failure::new(
                "project_required",
                "pass `project`: herdr_projects lists the open ones",
            )
        })?;
    let project = find_project(call, &workspaces, query)?;
    in_time(deadline)?;
    let selection = config.select(
        &project.names(),
        args.harness.as_deref(),
        args.model.as_deref(),
        args.effort.as_deref(),
    );
    let command = selection
        .argv()
        .map_err(|error| Failure::new("invalid_arguments", error))?;
    let suffix = suffix()?;
    let branch = args.branch.clone().unwrap_or_else(|| {
        let slug = slug(&args.title, 40);
        format!("{}{slug}-{suffix}", config.worktree.branch_prefix)
    });
    let checkout = deadline.saturating_duration_since(Instant::now() + RESERVED_AFTER_CHECKOUT);
    if checkout.is_zero() {
        return Err(out_of_time());
    }
    let opened = open_worktree(
        call,
        config,
        &project,
        &branch,
        args.base.as_deref(),
        checkout,
    )
    .map_err(|failure| failure.with("branch", branch.as_str()))?;
    let with_place = |mut failure: Failure| {
        failure.detail.extend(opened.detail.clone());
        failure
            .message
            .push_str(" What was opened for it is still there.");
        failure
    };
    let start = json!({
        "name": agent_name(&args.title, &suffix),
        "kind": selection.harness,
        "pane_id": opened.pane,
        "args": command,
        "timeout_ms": START_TIMEOUT.as_millis(),
    });
    in_time(deadline).map_err(with_place)?;
    let ready = match start_agent(call, &start, deadline) {
        Ok(ready) => ready,
        Err(failure) => {
            let screen = screen_of(call, &opened.pane);
            return Err(with_place(failure).with("screen", screen));
        }
    };
    let thread = call
        .session
        .stamp_launch(&opened.pane, &caller.launched_by, &args.title, deadline)
        .map_err(|error| match error.kind() {
            io::ErrorKind::TimedOut => with_place(out_of_time()),
            _ => with_place(error.into()),
        })?;
    let message = args
        .message
        .as_deref()
        .filter(|text| !text.trim().is_empty());
    let sent = match message {
        Some(text) if ready => {
            in_time(deadline).map_err(with_place)?;
            Some(prompt(call, &opened.pane, text, deadline).map_err(with_place)?)
        }
        _ => None,
    };
    let mut result = opened.detail;
    result.insert(
        "thread".into(),
        thread.as_deref().unwrap_or(&opened.pane).into(),
    );
    result.insert("launched".into(), selection_json(&selection));
    result.insert("ready".into(), ready.into());
    result.insert("sent".into(), sent.is_some().into());
    result.insert("next".into(), after_launch(ready, sent.is_some()).into());
    Ok(Value::Object(result))
}

fn after_launch(ready: bool, sent: bool) -> &'static str {
    match (ready, sent) {
        (false, _) => {
            "The agent stopped at a dialog before taking input, so nothing was sent: \
             herdr_read it and tell the user."
        }
        (true, true) => SENT,
        (true, false) => "It is idle: herdr_send gives it work.",
    }
}

fn in_time(deadline: Instant) -> Result<(), Failure> {
    if Instant::now() < deadline {
        return Ok(());
    }
    Err(out_of_time())
}

fn out_of_time() -> Failure {
    Failure::new(
        "launch_timeout",
        "the launch ran out of time before its next step",
    )
}

const SENT: &str = "herdr_wait for the turn to end, then herdr_read. Herdr cannot tie a turn to a \
                    message, so check the reply answers yours: a turn someone else started in the \
                    same instant would end first.";

/// Herdr's `agent.start` answers before the agent is up, and refuses a pane whose shell has not
/// reached its first prompt yet.
fn start_agent(call: &mut Call, start: &Value, deadline: Instant) -> Result<bool, Failure> {
    let pane = json!({"target": start["pane_id"]});
    let by = (Instant::now() + START_TIMEOUT).min(deadline);
    loop {
        match call.request("agent.start", start, QUICK)? {
            Ok(_) => break,
            Err(refused) if refused.code == "agent_pane_busy" && Instant::now() < by => {
                thread::sleep(POLL);
            }
            Err(refused) => return Err(refused.into()),
        }
    }
    loop {
        let mut found = match call.request("agent.get", &pane, QUICK)? {
            Ok(found) => found,
            Err(refused) if refused.code == "agent_not_found" => {
                return Err(Failure::new(
                    "agent_start_failed",
                    "the agent's command did not come up: `screen` shows what it printed.",
                ));
            }
            Err(refused) => return Err(refused.into()),
        };
        let agent = found["agent"].take();
        if agent["interactive_ready"] == true {
            return Ok(true);
        }
        if agent["agent_status"] == "blocked" {
            return Ok(false);
        }
        if Instant::now() >= by {
            return Err(Failure::new(
                "agent_start_timeout",
                "the agent did not come up in time: `screen` shows where it is.",
            ));
        }
        thread::sleep(POLL);
    }
}

fn open_worktree(
    call: &mut Call,
    config: &Config,
    project: &Project,
    branch: &str,
    base: Option<&str>,
    checkout: Duration,
) -> Result<Opened, Failure> {
    let mut worktree = json!({
        "branch": branch,
        "base": base.or(config.worktree.base.as_deref()),
        "label": project.name,
        "focus": false,
    });
    match project.worktree_source() {
        Some(source) => worktree["workspace_id"] = json!(source.workspace_id),
        None => worktree["cwd"] = json!(project.root),
    }
    let made = call.ask("worktree.create", &worktree, checkout)?;
    let pane = herdr::root_pane(&made)
        .ok_or_else(|| Failure::new("herdr_unavailable", "Herdr opened no pane"))?;
    Ok(Opened {
        detail: detail(&[
            ("workspace_id", made["workspace"]["workspace_id"].clone()),
            ("path", made["worktree"]["path"].clone()),
            ("branch", made["worktree"]["branch"].clone()),
            ("pane_id", json!(pane)),
        ]),
        pane,
    })
}

fn screen_of(call: &mut Call, pane: &str) -> String {
    let params = json!({
        "pane_id": pane, "source": "recent", "lines": 30, "format": "text", "strip_ansi": true,
    });
    let read = call.ask("pane.read", &params, QUICK).unwrap_or_default();
    let text = compact_screen(read["read"]["text"].as_str().unwrap_or_default());
    let cut = text.chars().count().saturating_sub(800);
    text.chars().skip(cut).collect()
}

fn detail(fields: &[(&str, Value)]) -> Map<String, Value> {
    fields
        .iter()
        .map(|(name, value)| ((*name).to_owned(), value.clone()))
        .collect()
}

/// Herdr's `agent.prompt` wait counts a turn already running, so a message to an agent mid-turn
/// is refused before it goes.
fn prompt(call: &mut Call, pane: &str, text: &str, deadline: Instant) -> Result<(), Failure> {
    let status = call.ask("agent.get", &json!({"target": pane}), QUICK)?;
    if status["agent"]["agent_status"] == "working" {
        return Err(Failure::new(
            "agent_working",
            "it is in the middle of a turn: herdr_wait for it to end, then send.",
        ));
    }
    let params = json!({
        "target": pane,
        "text": text,
        "wait": {"until": ["working", "blocked", "done"], "timeout_ms": TURN_START_MS},
    });
    let within = (Duration::from_millis(TURN_START_MS) + MARGIN)
        .min(deadline.saturating_duration_since(Instant::now()));
    call.ask("agent.prompt", &params, within).map(drop)
}

fn slug(text: &str, max: usize) -> String {
    let mut slug = String::new();
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
        if slug.len() >= max {
            break;
        }
    }
    let slug = slug.trim_end_matches('-');
    if slug.is_empty() {
        "thread".into()
    } else {
        slug.into()
    }
}

/// Herdr's agent names start with a lowercase letter and hold at most 32 of `a-z 0-9 - _`.
fn agent_name(title: &str, suffix: &str) -> String {
    let slug = slug(title, 21);
    let lead = if slug.starts_with(|c: char| c.is_ascii_lowercase()) {
        ""
    } else {
        "t-"
    };
    format!("{lead}{slug}-{suffix}")
}

fn one_line(text: &str) -> String {
    text.split(char::is_control)
        .flat_map(str::split_whitespace)
        .collect::<Vec<_>>()
        .join(" ")
}

fn typeable(text: &str) -> Result<(), Failure> {
    if text
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return Err(Failure::new(
            "invalid_arguments",
            "a message can hold line breaks and tabs, but no other control characters",
        ));
    }
    Ok(())
}

pub fn suffix() -> Result<String, Failure> {
    let mut bytes = [0u8; 4];
    getrandom::fill(&mut bytes)
        .map_err(|error| Failure::new("herdr_unavailable", error.to_string()))?;
    Ok(format!("{:08x}", u32::from_be_bytes(bytes)))
}

pub fn send_tool(call: &mut Call, arguments: Value) -> Done {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Args {
        thread: String,
        message: String,
    }
    let args: Args = decode(arguments)?;
    if args.message.trim().is_empty() {
        return Err(Failure::new("invalid_arguments", "message is empty"));
    }
    typeable(&args.message)?;
    let (threads, id) = call.lookup(&args.thread)?;
    let (item, pane) = threads.pane(&id)?;
    let mine = threads.mine(item);
    let deadline = Instant::now() + Duration::from_millis(TURN_START_MS) + MARGIN;
    prompt(call, &pane, &args.message, deadline)?;
    Ok(json!({
        "thread": item.row.key,
        "sent": true,
        "mine": mine,
        "next": SENT,
    }))
}

pub fn read_tool(call: &mut Call, arguments: Value) -> Done {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Args {
        thread: String,
        lines: Option<u64>,
        max_chars: Option<usize>,
    }
    let args: Args = decode(arguments)?;
    let (threads, id) = call.lookup(&args.thread)?;
    let (item, pane) = threads.pane(&id)?;
    let mut params = json!({
        "target": pane,
        "source": "recent",
        "lines": args.lines.unwrap_or(READ_DEFAULT_LINES).clamp(1, READ_MAX_LINES),
        "format": "text",
        "strip_ansi": true,
    });
    // Herdr scrolls back past the screen of an agent showing a full-screen view only while it
    // is idle, so a blocked one is read as it shows now.
    let read = if let Ok(read) = call.request("agent.read", &params, QUICK)? {
        read
    } else {
        params["source"] = json!("visible");
        call.ask("agent.read", &params, QUICK)?
    };
    let text = compact_screen(read["read"]["text"].as_str().unwrap_or_default());
    let max = args
        .max_chars
        .unwrap_or(READ_DEFAULT_CHARS)
        .clamp(1, READ_MAX_CHARS);
    let cut = text.chars().count().saturating_sub(max);
    let kept: String = text.chars().skip(cut).collect();
    let mut result = threads.describe(item);
    result["text"] = kept.into();
    result["truncated"] = (cut > 0 || read["read"]["truncated"] == true).into();
    Ok(result)
}

fn compact_screen(screen: &str) -> String {
    let mut text = String::new();
    for row in screen.lines().map(str::trim_end) {
        if row.is_empty() && (text.is_empty() || text.ends_with("\n\n")) {
            continue;
        }
        let mut drawn = row.chars();
        let rule = drawn.next().filter(|first| {
            !first.is_alphanumeric()
                && row.chars().count() > RULE_STROKES
                && drawn.all(|c| c == *first)
        });
        match rule {
            Some(stroke) => text.extend(std::iter::repeat_n(stroke, RULE_STROKES)),
            None => text.push_str(row),
        }
        text.push('\n');
    }
    text.trim_end().to_owned()
}

pub fn wait_tool(call: &mut Call, arguments: Value) -> Done {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Args {
        thread: String,
        timeout_ms: Option<u64>,
    }
    let args: Args = decode(arguments)?;
    let (threads, id) = call.lookup(&args.thread)?;
    let (_, pane) = threads.pane(&id)?;
    let timeout = args
        .timeout_ms
        .unwrap_or(WAIT_DEFAULT_MS)
        .clamp(1, WAIT_MAX_MS);
    let params = json!({
        "target": pane, "until": ["idle", "done", "blocked"], "timeout_ms": timeout,
    });
    let within = Duration::from_millis(timeout) + MARGIN;
    let timed_out = match call.request("agent.wait", &params, within)? {
        Ok(_) => false,
        Err(refused) if refused.code == "timeout" => true,
        Err(refused) => return Err(refused.into()),
    };
    let threads = call.threads()?;
    let (item, _) = threads.pane(&pane)?;
    let mut result = threads.describe(item);
    result["timed_out"] = timed_out.into();
    result["next"] = match (timed_out, item.row.display) {
        (true, _) => "Still working, or stopped at a prompt Herdr does not recognize: herdr_read shows which. \
                      Then call herdr_wait again, or end your turn and check later.",
        (false, Display::Blocked) => {
            "It is waiting on an approval or a question: herdr_read it and ask the user. Do not \
             answer it yourself."
        }
        (false, _) => "herdr_read shows what it did.",
    }
    .into();
    Ok(result)
}

pub fn threads_tool(call: &mut Call, arguments: Value) -> Done {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Args {
        project: Option<String>,
        #[serde(default)]
        mine: bool,
        #[serde(default)]
        include_settled: bool,
        limit: Option<usize>,
    }
    let args: Args = decode(arguments)?;
    let threads = call.threads()?;
    let workspaces = match args.project {
        Some(_) => call.workspaces()?,
        None => Vec::new(),
    };
    let project = match args.project.as_deref() {
        Some(query) => Some(find_project(call, &workspaces, query)?),
        None => None,
    };
    let wanted = |item: &&Item| {
        (!args.mine || threads.mine(item))
            && (args.include_settled || item.row.park != Some(Park::Settled))
            && project
                .as_ref()
                .is_none_or(|p| p.holds(item.row.thread.workspace_id.as_deref()))
    };
    let mut listed: Vec<&Item> = threads.items.iter().filter(wanted).collect();
    listed.sort_by_key(|item| !threads.mine(item));
    let limit = args.limit.unwrap_or(LIST_DEFAULT).max(1);
    Ok(json!({
        "threads": listed.iter().take(limit).map(|item| threads.describe(item)).collect::<Vec<_>>(),
        "more": listed.len().saturating_sub(limit),
    }))
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Verb {
    Settle,
    Unsettle,
    Snooze,
    Wake,
    Reopen,
    Pin,
    Unpin,
    MarkUnread,
    MarkRead,
    Rename,
    Forget,
    Undo,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OrganizeArgs {
    action: Verb,
    thread: Option<String>,
    until: Option<String>,
    minutes: Option<i64>,
    title: Option<String>,
}

impl OrganizeArgs {
    fn snooze_until(&self, now: i64) -> Result<i64, Failure> {
        let invalid = |message: &str| Failure::new("invalid_arguments", message);
        let until = match (&self.until, self.minutes) {
            (Some(at), None) => DateTime::parse_from_rfc3339(at)
                .map_err(|_| {
                    invalid("`until` must be an RFC 3339 time, like 2027-01-31T09:00:00-05:00")
                })?
                .timestamp(),
            (None, Some(minutes)) => now.saturating_add(minutes.saturating_mul(60)),
            _ => return Err(invalid("snooze needs `until` or `minutes`, not both")),
        };
        if until <= now {
            return Err(invalid("a snooze has to end in the future"));
        }
        Ok(until)
    }
}

pub fn organize_tool(call: &mut Call, arguments: Value) -> Done {
    let args: OrganizeArgs = decode(arguments)?;
    let outcome = if args.action == Verb::Undo {
        call.session.undo()?
    } else {
        let id = args.thread.as_deref().ok_or_else(|| {
            Failure::new(
                "invalid_arguments",
                "`thread` is required for every action but undo",
            )
        })?;
        let (threads, id) = call.lookup(id)?;
        let key = threads.find(&id)?.row.key.clone();
        let action = match args.action {
            Verb::Rename => None,
            Verb::Settle => Some(Action::Ensure(Want::Settled)),
            Verb::Unsettle => Some(Action::Ensure(Want::Unsettled)),
            Verb::Snooze => Some(Action::SnoozeKeepingBusyOpen(
                args.snooze_until(call.session.now())?,
            )),
            Verb::Wake => Some(Action::Ensure(Want::Awake)),
            Verb::Reopen => Some(Action::Ensure(Want::Open)),
            Verb::Pin => Some(Action::Ensure(Want::Pinned)),
            Verb::Unpin => Some(Action::Ensure(Want::Unpinned)),
            Verb::MarkUnread => Some(Action::Ensure(Want::Unread)),
            Verb::MarkRead => Some(Action::Ensure(Want::Read)),
            Verb::Forget => Some(Action::Forget),
            Verb::Undo => return Err(Failure::new("invalid_arguments", "undo takes no thread")),
        };
        if let Some(action) = action {
            call.session.act(&key, action)?
        } else {
            let title = args.title.as_deref().map(one_line);
            call.session.rename(&key, title)?;
            Outcome::Done {
                key,
                notice: None,
                wake_failed: None,
            }
        }
    };
    match outcome {
        Outcome::Refused(reason) => Err(Failure::new("refused", reason)),
        Outcome::Reopened => Ok(json!({
            "done": true,
            "note": "reopening in a new tab: herdr_threads shows it once its agent is back",
        })),
        Outcome::Done {
            key,
            notice,
            wake_failed,
        } => {
            let threads = call.threads()?;
            let thread = threads.find(&key).ok().map(|item| threads.describe(item));
            let wake = wake_failed.map(|error| format!("its wake timer failed to start: {error}"));
            let notes: Vec<String> = notice.into_iter().chain(wake).collect();
            Ok(json!({"done": true, "thread": thread, "notes": notes}))
        }
    }
}
