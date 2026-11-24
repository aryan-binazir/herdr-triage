//! What each tool does: resolve the project or thread the caller named, drive Herdr, and keep
//! triage's record of it. Every refusal names its reason in `Failure::code`.

use std::io;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, TimeZone};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::Env;
use super::config::{Config, HARNESSES, Selection};
use crate::herdr::{self, Herdr, Refused, Workspace};
use crate::picker::{Item, Target};
use crate::session::{Outcome, Session};
use crate::triage::{Action, Display, Park};

const START_TIMEOUT_MS: u64 = 60_000;
/// A new pane's shell is refused as busy until it reaches its first prompt.
const SHELL_TIMEOUT: Duration = Duration::from_secs(15);
const POLL: Duration = Duration::from_millis(200);
/// How long a submitted prompt has to start a turn before the caller is told it may not have.
const TURN_START_MS: u64 = 10_000;
const WAIT_DEFAULT_MS: u64 = 30_000;
const WAIT_MAX_MS: u64 = 300_000;
const RULE: usize = 8;
const READ_DEFAULT_LINES: u64 = 100;
const READ_MAX_LINES: u64 = 2_000;
const READ_DEFAULT_CHARS: usize = 12_000;
const READ_MAX_CHARS: usize = 60_000;
const LIST_DEFAULT: usize = 50;
/// Herdr answers these at once; the margin covers a busy server.
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
                " `checkout: existing` starts the agent in a tab there instead of a worktree."
            }
            "worktree_create_failed" => " If the branch is taken, pass another `branch`.",
            _ => "",
        };
        Failure::new(&refused.code, format!("{}{hint}", refused.message))
    }
}

type Done = Result<Value, Failure>;

/// One tool call's view of the caller, Herdr and triage.
pub struct Call<'a> {
    env: &'a Env,
    client: &'a str,
    herdr: Herdr,
    session: Session,
}

impl<'a> Call<'a> {
    pub fn open(env: &'a Env, client: &'a str) -> Result<Self, Failure> {
        Ok(Self {
            env,
            client,
            herdr: Herdr::at(&env.socket)?,
            session: (env.session)()?,
        })
    }

    fn ask(&mut self, method: &str, params: &Value, within: Duration) -> Done {
        Ok(self.herdr.request(method, params, within)??)
    }

    fn threads(&mut self) -> Result<Threads, Failure> {
        let items = self.session.view()?.items;
        let own = self.env.caller_pane.as_ref().and_then(|pane| {
            let here = Target::Pane(pane.clone());
            items.iter().find(|item| item.target == here)
        });
        let caller = match (own, &self.env.caller_pane) {
            (Some(item), _) => item.row.key.clone(),
            (None, Some(pane)) => format!("pane:{pane}"),
            (None, None) => format!("client:{}", self.client),
        };
        Ok(Threads { items, caller })
    }

    /// Herdr's workspaces, each with the checkout it is in. `workspace.list` names the checkout
    /// only for some, so the rest are asked for one repository at a time.
    fn workspaces(&mut self) -> Result<Vec<Workspace>, Failure> {
        let mut workspaces = self.herdr.workspaces()?;
        for at in 0..workspaces.len() {
            if workspaces[at].worktree.is_some() {
                continue;
            }
            let params = json!({"workspace_id": workspaces[at].workspace_id});
            // A workspace outside any repository is refused here, and stays a project of its own.
            let Ok(Ok(listed)) = self.herdr.request("worktree.list", &params, QUICK) else {
                continue;
            };
            let source = &listed["source"];
            for worktree in listed["worktrees"].as_array().into_iter().flatten() {
                let open_in = worktree["open_workspace_id"].as_str();
                let Some(workspace) = workspaces
                    .iter_mut()
                    .find(|w| Some(w.workspace_id.as_str()) == open_in && w.worktree.is_none())
                else {
                    continue;
                };
                let text = |value: &Value| value.as_str().unwrap_or_default().to_owned();
                workspace.worktree = Some(herdr::Checkout {
                    repo_key: text(&source["repo_key"]),
                    repo_name: text(&source["repo_name"]),
                    repo_root: text(&source["repo_root"]),
                    checkout_path: text(&worktree["path"]),
                    is_linked_worktree: worktree["is_linked_worktree"] == true,
                });
            }
        }
        Ok(workspaces)
    }

    fn config(&self) -> Result<Config, Failure> {
        Config::load(&self.env.config)
            .map_err(|error| Failure::new("bad_config", error.to_string()))
    }
}

struct Threads {
    items: Vec<Item>,
    /// Who the caller is in `Record::launched_by`: its own thread when it runs in Herdr.
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

    /// The pane of an open thread; a closed one has to be reopened first.
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

/// Workspaces of one repository, or one workspace Herdr found no repository in.
struct Project<'a> {
    name: &'a str,
    root: Option<&'a str>,
    workspaces: Vec<&'a Workspace>,
}

impl<'a> Project<'a> {
    /// The repository's own checkout when it is open, since worktrees branch off it.
    fn primary(&self) -> &'a Workspace {
        self.workspaces
            .iter()
            .find(|w| w.worktree.as_ref().is_none_or(|c| !c.is_linked_worktree))
            .unwrap_or(&self.workspaces[0])
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

    /// How deep inside this project `path` is, for picking the nearest of nested checkouts.
    fn depth_of(&self, path: &str) -> Option<usize> {
        let checkouts = self
            .workspaces
            .iter()
            .filter_map(|w| w.worktree.as_ref())
            .flat_map(|c| [c.checkout_path.as_str(), c.repo_root.as_str()]);
        checkouts
            .filter(|dir| Path::new(path).starts_with(dir))
            .map(str::len)
            .max()
    }
}

fn projects(workspaces: &[Workspace]) -> Vec<Project<'_>> {
    let mut projects: Vec<(&str, Project)> = Vec::new();
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
        match projects.iter_mut().find(|(known, _)| *known == key) {
            Some((_, project)) => project.workspaces.push(workspace),
            None => projects.push((
                key,
                Project {
                    name,
                    root,
                    workspaces: vec![workspace],
                },
            )),
        }
    }
    projects.into_iter().map(|(_, project)| project).collect()
}

enum Place<'a> {
    Open(Project<'a>),
    /// A directory no workspace is open in; Herdr opens one for its worktree.
    Unopened(&'a str),
}

fn place<'a>(workspaces: &'a [Workspace], query: &'a str) -> Result<Place<'a>, Failure> {
    let mut all = projects(workspaces);
    let named: Vec<usize> = (0..all.len())
        .filter(|&at| all[at].name.eq_ignore_ascii_case(query))
        .collect();
    if named.len() > 1 {
        let roots: Vec<&str> = named.iter().filter_map(|&at| all[at].root).collect();
        return Err(Failure::new(
            "project_ambiguous",
            format!("more than one open project is called `{query}`: pass its path instead"),
        )
        .with("paths", roots));
    }
    let by_workspace = || {
        all.iter().position(|project| {
            project
                .workspaces
                .iter()
                .any(|w| w.workspace_id == query || w.label.eq_ignore_ascii_case(query))
        })
    };
    let by_path = || {
        let depths = all.iter().enumerate();
        depths
            .filter_map(|(at, project)| Some((project.depth_of(query)?, at)))
            .max()
            .map(|(_, at)| at)
    };
    if let Some(at) = named
        .first()
        .copied()
        .or_else(by_workspace)
        .or_else(by_path)
    {
        return Ok(Place::Open(all.swap_remove(at)));
    }
    if Path::new(query).is_absolute() && Path::new(query).is_dir() {
        return Ok(Place::Unopened(query));
    }
    let known: Vec<&str> = all.iter().map(|project| project.name).collect();
    Err(Failure::new(
        "project_not_found",
        format!("no open project `{query}`: pass a name from `projects`, or a repository's path"),
    )
    .with("projects", known))
}

fn selection_json(selection: &Selection) -> Value {
    json!({"harness": selection.harness, "model": selection.model, "effort": selection.effort})
}

pub fn projects_tool(call: &mut Call, arguments: Value) -> Done {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Args {}
    let Args {} = decode(arguments)?;
    let config = call.config()?;
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
    let config = call.config()?;
    let harnesses: Map<String, Value> = HARNESSES
        .iter()
        .map(|name| {
            let harness = config.harness(name);
            let settings = json!({
                "model": harness.model, "effort": harness.effort, "args": harness.args,
            });
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
        "max_open": config.limits.max_open,
        "config_file": call.env.config,
        "note": "model and effort are passed to the harness as given; null means its own default",
    }))
}

#[derive(Deserialize, Default, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Checkout {
    #[default]
    Worktree,
    Existing,
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
    #[serde(default)]
    checkout: Checkout,
    branch: Option<String>,
    base: Option<String>,
    pub request_id: Option<String>,
}

impl LaunchArgs {
    pub fn decode(arguments: Value) -> Result<Self, Failure> {
        let args: Self = decode(arguments)?;
        if args.title.trim().is_empty() {
            return Err(Failure::new("invalid_arguments", "title is empty"));
        }
        Ok(args)
    }
}

/// Where a launch put its agent's shell.
struct Opened {
    pane: String,
    detail: Map<String, Value>,
}

pub fn launch_tool(call: &mut Call, args: &LaunchArgs) -> Done {
    let config = call.config()?;
    let threads = call.threads()?;
    // Settling one frees its place even when its pane stays open, as a worktree's only pane does.
    let launched =
        |item: &&Item| item.row.live() && item.row.launched_by.is_some() && item.row.park.is_none();
    if threads.items.iter().filter(launched).count() >= config.limits.max_open {
        return Err(Failure::new(
            "too_many_open",
            format!(
                "{} launched threads are open and not settled (max_open in the config file): settle some first",
                config.limits.max_open
            ),
        ));
    }
    let workspaces = call.workspaces()?;
    let own_workspace = || {
        let own = threads
            .items
            .iter()
            .find(|item| item.row.key == threads.caller)?;
        own.row.thread.workspace_id.as_deref()
    };
    let query = args
        .project
        .as_deref()
        .or_else(own_workspace)
        .ok_or_else(|| {
            Failure::new(
                "project_required",
                "pass `project`: herdr_projects lists the open ones",
            )
        })?;
    let place = place(&workspaces, query)?;
    let names = match &place {
        Place::Open(project) => project.names(),
        Place::Unopened(_) => Vec::new(),
    };
    let selection = config.select(
        &names,
        args.harness.as_deref(),
        args.model.as_deref(),
        args.effort.as_deref(),
    );
    let command = selection
        .argv()
        .map_err(|error| Failure::new("invalid_arguments", error))?;
    let opened = open_shell(call, &config, &place, args)?;
    let with_place = |mut failure: Failure| {
        failure.detail.extend(opened.detail.clone());
        failure
            .message
            .push_str(" What was opened for it is still there.");
        failure
    };
    let title = args.title.trim();
    let start = json!({
        "name": format!("{}-{}", slug(title, 32), suffix()?),
        "kind": selection.harness,
        "pane_id": opened.pane,
        "args": command,
        "timeout_ms": START_TIMEOUT_MS,
    });
    let ready = match start_agent(call, &start) {
        Ok(ready) => ready,
        // No agent means no thread to read, so the pane's last words go with the refusal.
        Err(failure) => {
            let screen = screen_of(call, &opened.pane);
            return Err(with_place(failure).with("screen", screen));
        }
    };
    let thread = call
        .session
        .stamp_launch(&opened.pane, &threads.caller, title)?;
    let message = args
        .message
        .as_deref()
        .filter(|text| !text.trim().is_empty());
    let sent = match message {
        Some(text) if ready => Some(prompt(call, &opened.pane, text).map_err(with_place)?),
        _ => None,
    };
    let next = match (ready, sent) {
        (false, _) => {
            "The agent stopped at a dialog before taking input, so nothing was sent: \
                       herdr_read it and tell the user."
        }
        (true, Some(())) => "herdr_wait for the turn to end, then herdr_read.",
        (true, None) => "It is idle: herdr_send gives it work.",
    };
    let mut result = opened.detail;
    result.insert(
        "thread".into(),
        thread.as_deref().unwrap_or(&opened.pane).into(),
    );
    result.insert("pane_id".into(), opened.pane.into());
    result.insert("launched".into(), selection_json(&selection));
    result.insert("ready".into(), ready.into());
    result.insert("sent".into(), sent.is_some().into());
    result.insert("next".into(), next.into());
    Ok(Value::Object(result))
}

/// Starts the agent and stays until it takes input. `false` when it stopped first at a dialog of
/// its own, such as trusting the directory. Herdr's `agent.start` answers before the agent is up,
/// and refuses a pane whose shell has not reached its first prompt yet.
fn start_agent(call: &mut Call, start: &Value) -> Result<bool, Failure> {
    let pane = json!({"target": start["pane_id"]});
    let shell_by = Instant::now() + SHELL_TIMEOUT;
    loop {
        match call.herdr.request("agent.start", start, QUICK)? {
            Ok(_) => break,
            Err(refused) if refused.code == "agent_pane_busy" && Instant::now() < shell_by => {
                thread::sleep(POLL);
            }
            Err(refused) => return Err(refused.into()),
        }
    }
    let ready_by = Instant::now() + Duration::from_millis(START_TIMEOUT_MS);
    loop {
        let mut found = match call.herdr.request("agent.get", &pane, QUICK)? {
            Ok(found) => found,
            // Herdr gives up on a launch whose command exits or never becomes the agent.
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
        if Instant::now() >= ready_by {
            return Err(Failure::new(
                "agent_start_timeout",
                "the agent did not come up in time: `screen` shows where it is.",
            ));
        }
        thread::sleep(POLL);
    }
}

fn open_shell(
    call: &mut Call,
    config: &Config,
    place: &Place,
    args: &LaunchArgs,
) -> Result<Opened, Failure> {
    let title = args.title.trim();
    let workspace = match place {
        Place::Open(project) => Some(project.primary()),
        Place::Unopened(_) => None,
    };
    if args.checkout == Checkout::Existing {
        let Some(workspace) = workspace else {
            return Err(Failure::new(
                "project_not_open",
                "`checkout: existing` needs a project that is open in Herdr",
            ));
        };
        let cwd = workspace.worktree.as_ref().map(|c| &c.checkout_path);
        let tab = json!({
            "workspace_id": workspace.workspace_id, "cwd": cwd, "label": title, "focus": false,
        });
        let made = call.ask("tab.create", &tab, QUICK)?;
        return Ok(Opened {
            pane: pane_of(&made)?,
            detail: detail(&[
                ("workspace_id", json!(workspace.workspace_id)),
                ("path", json!(cwd)),
            ]),
        });
    }
    let branch = args
        .branch
        .clone()
        .unwrap_or_else(|| format!("{}{}", config.worktree.branch_prefix, slug(title, 48)));
    // The workspace is named for its project, so the thread's row says where it is; the title
    // is on the thread itself. Herdr names an unlabelled one after its branch.
    let label = match place {
        Place::Open(project) => Some(project.name),
        Place::Unopened(_) => None,
    };
    let worktree = json!({
        "workspace_id": workspace.map(|w| &w.workspace_id),
        "cwd": match place { Place::Unopened(path) => Some(path), Place::Open(_) => None },
        "branch": branch,
        "base": args.base.as_ref().or(config.worktree.base.as_ref()),
        "label": label,
        "focus": false,
    });
    // Herdr runs `git worktree add`, which checks out the whole tree.
    let made = call.ask("worktree.create", &worktree, Duration::from_secs(120))?;
    Ok(Opened {
        pane: pane_of(&made)?,
        detail: detail(&[
            ("workspace_id", made["workspace"]["workspace_id"].clone()),
            ("path", made["worktree"]["path"].clone()),
            ("branch", made["worktree"]["branch"].clone()),
        ]),
    })
}

/// The end of what a pane shows, for a launch that left no agent to read.
fn screen_of(call: &mut Call, pane: &str) -> String {
    let params = json!({
        "pane_id": pane, "source": "recent", "lines": 30, "format": "text", "strip_ansi": true,
    });
    let read = call.ask("pane.read", &params, QUICK).unwrap_or_default();
    let text = without_blank_runs(read["read"]["text"].as_str().unwrap_or_default());
    let cut = text.chars().count().saturating_sub(800);
    text.chars().skip(cut).collect()
}

fn detail(fields: &[(&str, Value)]) -> Map<String, Value> {
    fields
        .iter()
        .map(|(name, value)| ((*name).to_owned(), value.clone()))
        .collect()
}

fn pane_of(made: &Value) -> Result<String, Failure> {
    made["root_pane"]["pane_id"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| Failure::new("herdr_unavailable", "Herdr opened no pane"))
}

/// Submits `text` and stays until the turn is seen to start, so a wait that follows cannot
/// mistake the idle moment before it for the turn's end.
fn prompt(call: &mut Call, pane: &str, text: &str) -> Result<(), Failure> {
    let params = json!({
        "target": pane,
        "text": text,
        "wait": {"until": ["working", "blocked", "done"], "timeout_ms": TURN_START_MS},
    });
    let within = Duration::from_millis(TURN_START_MS) + MARGIN;
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

/// Herdr wants each agent's name to be unique; two threads may share a title.
fn suffix() -> Result<String, Failure> {
    let mut bytes = [0u8; 2];
    getrandom::fill(&mut bytes)
        .map_err(|error| Failure::new("herdr_unavailable", error.to_string()))?;
    Ok(format!("{:02x}{:02x}", bytes[0], bytes[1]))
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
    let threads = call.threads()?;
    let (item, pane) = threads.pane(&args.thread)?;
    let mine = threads.mine(item);
    prompt(call, &pane, &args.message)?;
    Ok(json!({
        "thread": item.row.key,
        "sent": true,
        "mine": mine,
        "next": "herdr_wait for the turn to end, then herdr_read.",
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
    let threads = call.threads()?;
    let (item, pane) = threads.pane(&args.thread)?;
    let params = json!({
        "target": pane,
        "source": "recent",
        "lines": args.lines.unwrap_or(READ_DEFAULT_LINES).clamp(1, READ_MAX_LINES),
        "format": "text",
        "strip_ansi": true,
    });
    let read = call.ask("agent.read", &params, QUICK)?;
    let text = without_blank_runs(read["read"]["text"].as_str().unwrap_or_default());
    let max = args
        .max_chars
        .unwrap_or(READ_DEFAULT_CHARS)
        .clamp(1, READ_MAX_CHARS);
    // The end of the screen is the news; what is cut is the oldest of it.
    let cut = text.chars().count().saturating_sub(max);
    let kept: String = text.chars().skip(cut).collect();
    let mut result = threads.describe(item);
    result["text"] = kept.into();
    result["truncated"] = (cut > 0 || read["read"]["truncated"] == true).into();
    Ok(result)
}

/// A terminal's screen is mostly padding: rows lose their trailing spaces, blank rows in a row
/// become one, and a rule drawn across the screen is cut to a few strokes.
fn without_blank_runs(screen: &str) -> String {
    let mut text = String::new();
    for row in screen.lines().map(str::trim_end) {
        if row.is_empty() && (text.is_empty() || text.ends_with("\n\n")) {
            continue;
        }
        let mut drawn = row.chars();
        let rule = drawn.next().filter(|first| {
            !first.is_alphanumeric() && row.chars().count() > RULE && drawn.all(|c| c == *first)
        });
        match rule {
            Some(stroke) => text.extend(std::iter::repeat_n(stroke, RULE)),
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
    let (_, pane) = call.threads()?.pane(&args.thread)?;
    let timeout = args
        .timeout_ms
        .unwrap_or(WAIT_DEFAULT_MS)
        .clamp(1, WAIT_MAX_MS);
    let params = json!({
        "target": pane, "until": ["idle", "done", "blocked"], "timeout_ms": timeout,
    });
    let within = Duration::from_millis(timeout) + MARGIN;
    let timed_out = match call.herdr.request("agent.wait", &params, within)? {
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
    let workspaces = call.workspaces()?;
    let project = match args.project.as_deref() {
        Some(query) => match place(&workspaces, query)? {
            Place::Open(project) => Some(project),
            Place::Unopened(_) => {
                return Err(Failure::new(
                    "project_not_found",
                    format!("no open project `{query}`"),
                ));
            }
        },
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
    // Triage's order within each half: the caller's own threads are the ones it should reach for.
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
        let threads = call.threads()?;
        let item = threads.find(id)?;
        let key = item.row.key.clone();
        if args.action == Verb::Rename {
            call.session.rename(&key, args.title.clone())?;
            Outcome::Done {
                key,
                notice: None,
                wake_failed: None,
            }
        } else {
            match action(&args, item, call.session.now())? {
                Some(action) => call.session.act(&key, action)?,
                None => Outcome::Done {
                    key,
                    notice: Some("already so: nothing changed".into()),
                    wake_failed: None,
                },
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
            Ok(json!({"done": true, "thread": thread, "note": notice.or(wake)}))
        }
    }
}

/// The picker's keys toggle; a caller names the state it wants, so an action already in effect
/// is `None` rather than its opposite.
fn action(args: &OrganizeArgs, item: &Item, now: i64) -> Result<Option<Action>, Failure> {
    let row = &item.row;
    let settled = row.park == Some(Park::Settled);
    let snoozed = matches!(row.park, Some(Park::Snoozed { .. }));
    let when = |wanted: bool, action: Action| Ok(wanted.then_some(action));
    match args.action {
        Verb::Settle => when(!settled, Action::ToggleSettle),
        Verb::Unsettle => when(settled, Action::ToggleSettle),
        Verb::Snooze => Ok(Some(Action::Snooze(args.snooze_until(now)?))),
        Verb::Wake => when(snoozed, Action::Wake),
        Verb::Reopen => when(!row.live(), Action::Reopen),
        Verb::Pin => when(!row.pinned, Action::TogglePin),
        Verb::Unpin => when(row.pinned, Action::TogglePin),
        Verb::MarkUnread => when(!row.display.unseen(), Action::ToggleUnread),
        Verb::MarkRead => when(row.display.unseen(), Action::ToggleUnread),
        Verb::Forget => Ok(Some(Action::Forget)),
        Verb::Rename | Verb::Undo => Ok(None),
    }
}
