mod config;
mod ops;

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use serde_json::{Value, json};

use crate::herdr;
use crate::session::Session;
use crate::store;
use ops::{Call, Failure, LaunchArgs};

/// Revisions without JSON-RPC batches, which this server does not take.
const PROTOCOLS: [&str; 2] = [NEWEST_PROTOCOL, "2024-11-05"];
const NEWEST_PROTOCOL: &str = "2025-06-18";

const INSTRUCTIONS: &str = "\
Starts and drives coding agents in the user's Herdr terminal session. Each agent is a thread the \
user sees in their own triage list. herdr_launch starts one in a project, in a new Git worktree; \
then herdr_wait for its turn to end and herdr_read what it did. Reach for \
threads marked `mine` (the ones you launched) unless the user names another. A thread whose state \
is `blocked` is waiting on an approval or a question that is the user's to answer: report it, never \
answer it. Use herdr_organize's `forget` only when the user asks for it.";

pub struct Env {
    pub socket: PathBuf,
    pub config: Option<PathBuf>,
    pub caller_pane: Option<String>,
    pub session: Box<dyn Fn() -> io::Result<Session>>,
}

impl Env {
    pub fn of_process() -> Self {
        let config = config::path();
        if let Some(path) = &config {
            config::write_template(path);
        }
        Self {
            socket: herdr::socket_path(),
            config,
            caller_pane: std::env::var("HERDR_PANE_ID")
                .ok()
                .filter(|pane| !pane.is_empty()),
            session: Box::new(|| Session::connect().map(Session::in_background)),
        }
    }
}

/// Each plugin update puts the binary somewhere new.
pub fn link_for_clients() {
    let Ok(binary) = std::env::current_exe() else {
        return;
    };
    let link = store::state_root().join("herdr-triage");
    if fs::read_link(&link).is_ok_and(|target| target == binary) {
        return;
    }
    let staged = link.with_extension(format!("new-{}", std::process::id()));
    let _ = fs::remove_file(&staged);
    let linked = link
        .parent()
        .map_or(Ok(()), fs::create_dir_all)
        .and_then(|()| std::os::unix::fs::symlink(&binary, &staged))
        .and_then(|()| fs::rename(&staged, &link));
    if linked.is_err() {
        let _ = fs::remove_file(&staged);
    }
}

struct Tool {
    name: &'static str,
    title: &'static str,
    description: &'static str,
    properties: Value,
    required: &'static [&'static str],
    read_only: bool,
    destructive: bool,
}

fn thread_id() -> Value {
    json!({"type": "string", "description": "A thread id from herdr_threads or herdr_launch, or its pane id."})
}

fn tools() -> Vec<Tool> {
    let mut tools = starting_tools();
    tools.extend(thread_tools());
    tools
}

fn starting_tools() -> Vec<Tool> {
    vec![
        Tool {
            name: "herdr_projects",
            title: "List Herdr projects",
            description: "The projects open in Herdr: one per repository, with its workspaces and what a launch there starts by default.",
            properties: json!({}),
            required: &[],
            read_only: true,
            destructive: false,
        },
        Tool {
            name: "herdr_capabilities",
            title: "Show launch settings",
            description: "The harnesses herdr_launch can start (pi, claude, codex), each one's default model and effort, per-project overrides, and the file the user sets them in.",
            properties: json!({}),
            required: &[],
            read_only: true,
            destructive: false,
        },
        Tool {
            name: "herdr_launch",
            title: "Launch an agent",
            description: "Start a new agent thread in a new Git worktree of a project open in Herdr (its own workspace), and give it its first message. Leave out harness, model and effort to use the user's defaults. Returns its `thread` id. Next: herdr_wait, then herdr_read.",
            properties: json!({
                "title": {"type": "string", "description": "A short name for the work. Names the thread, and the branch unless `branch` is given."},
                "project": {"type": "string", "description": "A project name from herdr_projects, a workspace id or label, or a path inside the repository. Left out: the project you are running in, when you run inside Herdr."},
                "message": {"type": "string", "description": "The agent's first prompt. Left out, it starts idle."},
                "harness": {"type": "string", "enum": config::HARNESSES},
                "model": {"type": "string", "description": "A model name the harness accepts."},
                "effort": {"type": "string", "description": "The harness's reasoning effort, such as low, medium or high."},
                "branch": {"type": "string", "description": "The new worktree's branch. An existing one is checked out as it is, ignoring `base`."},
                "base": {"type": "string", "description": "The ref the new branch starts from, such as origin/main."},
                "request_id": {"type": "string", "description": "Any unique string. Repeat it when retrying a launch whose answer you did not get, so it is not started twice."},
            }),
            required: &["title"],
            read_only: false,
            destructive: false,
        },
    ]
}

fn thread_tools() -> Vec<Tool> {
    let thread = thread_id();
    vec![
        Tool {
            name: "herdr_send",
            title: "Message an agent",
            description: "Send a message to an open thread's agent, as if typed at its prompt. Any thread can be reached; prefer ones marked `mine`. Refused with `agent_working` mid-turn (herdr_wait first), and with `agent_blocked` while the agent waits on an approval or a question. Next: herdr_wait.",
            properties: json!({"thread": thread, "message": {"type": "string"}}),
            required: &["thread", "message"],
            read_only: false,
            destructive: false,
        },
        Tool {
            name: "herdr_read",
            title: "Read an agent's screen",
            description: "The most recent text on an open thread's terminal, newest last, with the thread's state. It is the screen, not a transcript: ask for more `lines` to see further back.",
            properties: json!({
                "thread": thread,
                "lines": {"type": "integer", "description": "Rows to read. Default 100, at most 2000."},
                "max_chars": {"type": "integer", "description": "Keep this much of the end. Default 12000, at most 60000."},
            }),
            required: &["thread"],
            read_only: true,
            destructive: false,
        },
        Tool {
            name: "herdr_wait",
            title: "Wait for an agent",
            description: "Wait until an open thread's agent stops working: idle, done, or blocked on the user. Returns at once if it already has. On `timed_out`, call again or end your turn.",
            properties: json!({
                "thread": thread,
                "timeout_ms": {"type": "integer", "description": "Default 30000, at most 50000."},
            }),
            required: &["thread"],
            read_only: true,
            destructive: false,
        },
        Tool {
            name: "herdr_threads",
            title: "List agent threads",
            description: "Agent threads in Herdr with their triage state, the ones you launched (`mine`) first. Settled threads are left out unless asked for.",
            properties: json!({
                "project": {"type": "string", "description": "Only this project's threads."},
                "mine": {"type": "boolean", "description": "Only threads you launched."},
                "include_settled": {"type": "boolean"},
                "limit": {"type": "integer", "description": "Default 50."},
            }),
            required: &[],
            read_only: true,
            destructive: false,
        },
        Tool {
            name: "herdr_organize",
            title: "Triage a thread",
            description: "Change a thread's place in the user's triage list. settle / unsettle: done with it, or not after all. snooze / wake: hide it until `until` or for `minutes`. reopen: bring a closed thread's agent back with its conversation. pin / unpin. mark_unread / mark_read. rename: set `title` (empty restores the automatic one). undo: take back the last change, by anyone; needs no `thread`. forget: drop a closed thread for good, only when the user asks. An action already in effect changes nothing. Settling closes the agent's pane, and snoozing closes it when the agent can be reopened later and is neither working nor blocked; the last pane of a workspace stays open. Settling a working or blocked agent is refused.",
            properties: json!({
                "action": {"type": "string", "enum": ["settle", "unsettle", "snooze", "wake", "reopen", "pin", "unpin", "mark_unread", "mark_read", "rename", "undo", "forget"]},
                "thread": thread,
                "until": {"type": "string", "description": "For snooze: an RFC 3339 time with its offset."},
                "minutes": {"type": "integer", "description": "For snooze: how long from now."},
                "title": {"type": "string", "description": "For rename."},
            }),
            required: &["action"],
            read_only: false,
            destructive: true,
        },
    ]
}

fn listing(tool: &Tool) -> Value {
    json!({
        "name": tool.name,
        "title": tool.title,
        "description": tool.description,
        "inputSchema": {
            "type": "object",
            "properties": tool.properties,
            "required": tool.required,
            "additionalProperties": false,
        },
        "annotations": {
            "title": tool.title,
            "readOnlyHint": tool.read_only,
            "destructiveHint": tool.destructive,
            "openWorldHint": false,
        },
    })
}

struct Server {
    env: Env,
    client: String,
    instance: String,
    launches_by_request_id: BTreeMap<String, Result<Value, Failure>>,
}

impl Server {
    fn call(&mut self, name: &str, arguments: Value) -> Result<Value, Failure> {
        let who = format!("{}:{}", self.client, self.instance);
        if name != "herdr_launch" {
            let mut call = Call::open(&self.env, &who)?;
            return match name {
                "herdr_projects" => ops::projects_tool(&mut call, arguments),
                "herdr_capabilities" => ops::capabilities_tool(&mut call, arguments),
                "herdr_send" => ops::send_tool(&mut call, arguments),
                "herdr_read" => ops::read_tool(&mut call, arguments),
                "herdr_wait" => ops::wait_tool(&mut call, arguments),
                "herdr_threads" => ops::threads_tool(&mut call, arguments),
                _ => ops::organize_tool(&mut call, arguments),
            };
        }
        let args = LaunchArgs::decode(arguments)?;
        if let Some(first) = args
            .request_id
            .as_ref()
            .and_then(|id| self.launches_by_request_id.get(id))
        {
            return first.clone();
        }
        let result = Call::open(&self.env, &who)
            .and_then(|mut call| ops::launch_tool(&mut call, &args, ops::LAUNCH_BUDGET));
        let worktree_asked_for = result
            .as_ref()
            .err()
            .is_none_or(|failure| failure.detail.contains_key("branch"));
        if let Some(id) = args.request_id
            && worktree_asked_for
        {
            self.launches_by_request_id.insert(id, result.clone());
        }
        result
    }

    fn answer(&mut self, request: &Value) -> Option<Value> {
        let invalid = |id: &Value, message: &str| {
            let error = json!({"code": -32600, "message": message});
            Some(json!({"jsonrpc": "2.0", "id": id, "error": error}))
        };
        if request.is_array() {
            return invalid(&Value::Null, "batches are not supported");
        }
        let method = request["method"].as_str();
        let id = match (request.get("id"), method) {
            (Some(id), _) if id.is_string() || id.is_i64() || id.is_u64() => id.clone(),
            (None, Some(_)) if request["jsonrpc"] == "2.0" => return None,
            _ => return invalid(&Value::Null, "a request needs a string or integer `id`"),
        };
        let Some(method) = method.filter(|_| request["jsonrpc"] == "2.0") else {
            return invalid(&id, "a request needs `jsonrpc: \"2.0\"` and a `method`");
        };
        let params = &request["params"];
        if !(params.is_object() || request.get("params").is_none()) {
            return invalid(&id, "`params` must be an object");
        }
        let result = match method {
            "initialize" => {
                if let Some(name) = params["clientInfo"]["name"].as_str() {
                    name.clone_into(&mut self.client);
                }
                let asked = params["protocolVersion"].as_str().unwrap_or_default();
                let protocol = PROTOCOLS.iter().find(|known| **known == asked);
                Ok(json!({
                    "protocolVersion": protocol.unwrap_or(&NEWEST_PROTOCOL),
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "herdr-triage", "version": env!("CARGO_PKG_VERSION")},
                    "instructions": INSTRUCTIONS,
                }))
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": tools().iter().map(listing).collect::<Vec<_>>()})),
            "tools/call" => {
                let name = params["name"].as_str().unwrap_or_default();
                let arguments = params["arguments"].clone();
                if !tools().iter().any(|tool| tool.name == name) {
                    Err(json!({"code": -32602, "message": format!("unknown tool `{name}`")}))
                } else if !(arguments.is_object() || arguments.is_null()) {
                    Err(json!({"code": -32602, "message": "`arguments` must be an object"}))
                } else {
                    Ok(tool_result(self.call(name, arguments)))
                }
            }
            method => Err(json!({"code": -32601, "message": format!("no method `{method}`")})),
        };
        Some(match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(error) => json!({"jsonrpc": "2.0", "id": id, "error": error}),
        })
    }
}

/// A tool that could not do what was asked still answers, with the reason as its content: the
/// calling model reads it and can act on it, which it cannot with a protocol error.
fn tool_result(done: Result<Value, Failure>) -> Value {
    let (content, failed) = match done {
        Ok(value) => (value, false),
        Err(failure) => {
            let mut error = failure.detail;
            error.insert("code".into(), failure.code.into());
            error.insert("message".into(), failure.message.into());
            (json!({"error": error}), true)
        }
    };
    json!({
        "content": [{"type": "text", "text": content.to_string()}],
        "structuredContent": content,
        "isError": failed,
    })
}

pub fn serve(env: Env, input: impl BufRead, mut output: impl Write) -> io::Result<()> {
    let mut server = Server {
        env,
        client: "unknown".into(),
        instance: ops::suffix().map_err(|failure| io::Error::other(failure.message))?,
        launches_by_request_id: BTreeMap::new(),
    };
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(request) => server.answer(&request),
            Err(error) => Some(json!({
                "jsonrpc": "2.0", "id": null,
                "error": {"code": -32700, "message": error.to_string()},
            })),
        };
        if let Some(reply) = reply {
            writeln!(output, "{reply}")?;
            output.flush()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use chrono::DateTime;
    use serde_json::{Value, json};

    use super::*;
    use crate::fake_herdr::{self, FakeHerdr};
    use crate::herdr::Herdr;
    use crate::store::Store;

    const NOW: i64 = 1_800_000_000;

    struct Rig {
        fake: FakeHerdr,
        server: Server,
        next_id: u64,
    }

    fn rig(agents: Vec<Value>, caller_pane: Option<&str>) -> Rig {
        let fake = FakeHerdr::start(agents);
        *fake.workspaces.lock().unwrap() = vec![
            json!({"workspace_id": "w1", "label": "api", "tab_count": 1, "pane_count": 2,
                   "worktree": {"repo_key": "/src/acme-api/.git", "repo_name": "acme-api",
                                "repo_root": "/src/acme-api", "checkout_path": "/src/acme-api",
                                "is_linked_worktree": false}}),
            json!({"workspace_id": "w2", "label": "api-fix", "tab_count": 1, "pane_count": 1,
                   "worktree": {"repo_key": "/src/acme-api/.git", "repo_name": "acme-api",
                                "repo_root": "/src/acme-api", "checkout_path": "/wt/api-fix",
                                "is_linked_worktree": true}}),
            json!({"workspace_id": "w3", "label": "notes", "tab_count": 1, "pane_count": 1}),
        ];
        let (socket, state) = (fake.socket.clone(), fake.state_dir());
        let env = Env {
            socket: fake.socket.clone(),
            config: Some(fake.dir.path().join("mcp.yaml")),
            caller_pane: caller_pane.map(str::to_owned),
            session: Box::new(move || {
                let session = Session::new(
                    Herdr::at(&socket)?,
                    Store::at(state.clone()),
                    || NOW,
                    |_| Ok(()),
                );
                Ok(session.in_background())
            }),
        };
        let server = Server {
            env,
            client: "test-client".into(),
            instance: "i1".into(),
            launches_by_request_id: BTreeMap::new(),
        };
        Rig {
            fake,
            server,
            next_id: 0,
        }
    }

    impl Rig {
        fn request(&mut self, method: &str, params: Value) -> Value {
            self.next_id += 1;
            let mut request = json!({"jsonrpc": "2.0", "id": self.next_id, "method": method});
            request["params"] = params;
            let reply = self.server.answer(&request).unwrap();
            assert_eq!(reply["id"], self.next_id);
            reply
        }

        fn tool(&mut self, name: &str, arguments: Value) -> Result<Value, Value> {
            let mut params = json!({"name": name});
            params["arguments"] = arguments;
            let reply = self.request("tools/call", params);
            let result = &reply["result"];
            let content = result["structuredContent"].clone();
            assert_eq!(result["content"][0]["text"], content.to_string());
            if result["isError"] == true {
                Err(content)
            } else {
                Ok(content)
            }
        }

        fn ok(&mut self, name: &str, arguments: Value) -> Value {
            self.tool(name, arguments)
                .unwrap_or_else(|content| panic!("{content}"))
        }

        fn failure(&mut self, name: &str, arguments: Value) -> Value {
            match self.tool(name, arguments) {
                Ok(content) => panic!("{content}"),
                Err(content) => content["error"].clone(),
            }
        }

        fn refuse(&self, method: &str, code: &str) {
            *self.fake.fail_method.lock().unwrap() = Some(method.to_owned());
            *self.fake.fail_code.lock().unwrap() = code.to_owned();
        }

        fn config(&self, yaml: &str) {
            fs::write(self.server.env.config.as_ref().unwrap(), yaml).unwrap();
        }

        fn worktrees(&self) -> Vec<Value> {
            let mut asked = self.fake.take("worktree.create");
            for worktree in &mut asked {
                let branch = worktree["branch"].as_str().unwrap();
                worktree["branch"] =
                    json!(branch.rsplit_once('-').map_or(branch, |(stem, _)| stem));
            }
            asked
        }

        fn threads(&mut self) -> Vec<Value> {
            let listed = self.ok("herdr_threads", json!({"include_settled": true}));
            listed["threads"].as_array().unwrap().clone()
        }
    }

    fn resumable(pane: &str, status: &str, seq: u64) -> Value {
        let mut agent = fake_herdr::agent(pane, status, seq);
        agent["agent_session"] = json!({
            "source": "herdr:claude", "agent": "claude", "kind": "id", "value": format!("id-{pane}")
        });
        agent
    }

    fn key(pane: &str) -> String {
        format!("session:claude:id:id-{pane}")
    }

    #[test]
    fn a_client_is_answered_in_the_protocol_it_asked_for_or_the_newest() {
        let mut rig = rig(vec![], None);
        let hello =
            |version: &str| json!({"protocolVersion": version, "clientInfo": {"name": "c"}});
        let old = rig.request("initialize", hello("2024-11-05"));
        assert_eq!(old["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(old["result"]["serverInfo"]["name"], "herdr-triage");
        let unknown = rig.request("initialize", hello("1999-01-01"));
        assert_eq!(unknown["result"]["protocolVersion"], "2025-06-18");
    }

    #[test]
    fn every_tool_is_listed_with_a_closed_schema() {
        let mut rig = rig(vec![], None);
        let listed = rig.request("tools/list", json!({}));
        let tools = listed["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "herdr_projects",
                "herdr_capabilities",
                "herdr_launch",
                "herdr_send",
                "herdr_read",
                "herdr_wait",
                "herdr_threads",
                "herdr_organize"
            ]
        );
        for tool in tools {
            let schema = &tool["inputSchema"];
            assert_eq!(schema["additionalProperties"], false, "{tool}");
            for required in schema["required"].as_array().unwrap() {
                let name = required.as_str().unwrap();
                assert!(schema["properties"].get(name).is_some(), "{tool}");
            }
        }
    }

    #[test]
    fn the_stream_answers_requests_and_stays_quiet_for_notifications() {
        let rig = rig(vec![], None);
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            "\n",
            "\n",
            "not json\n",
            r#"{"jsonrpc":"2.0","id":"a","method":"ping"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":3,"method":"resources/list"}"#,
            "\n",
        );
        let mut output = Vec::new();
        serve(rig.server.env, input.as_bytes(), &mut output).unwrap();
        let replies: Vec<Value> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(replies.len(), 4);
        assert_eq!(replies[0]["id"], 1);
        assert_eq!(replies[1]["error"]["code"], -32700);
        assert_eq!(
            replies[2],
            json!({"jsonrpc": "2.0", "id": "a", "result": {}})
        );
        assert_eq!(replies[3]["error"]["code"], -32601);
    }

    #[test]
    fn malformed_requests_get_protocol_errors() {
        let mut rig = rig(vec![], None);
        let batch = rig
            .server
            .answer(&json!([{"jsonrpc": "2.0", "id": 1, "method": "ping"}]));
        assert_eq!(batch.unwrap()["error"]["code"], -32600);
        for malformed in [
            json!({}),
            json!({"jsonrpc": "1.0", "id": 1, "method": "ping"}),
            json!({"jsonrpc": "2.0", "id": true, "method": "ping"}),
            json!({"jsonrpc": "2.0", "id": null, "method": "ping"}),
            json!({"jsonrpc": "2.0", "id": 1.5, "method": "ping"}),
            json!({"jsonrpc": "2.0", "id": 1, "method": "ping", "params": [1]}),
            json!({"jsonrpc": "2.0", "id": 1, "method": "ping", "params": null}),
        ] {
            let reply = rig.server.answer(&malformed);
            assert_eq!(reply.unwrap()["error"]["code"], -32600, "{malformed}");
        }
        let unknown = rig.request("tools/call", json!({"name": "herdr_nope", "arguments": {}}));
        assert_eq!(unknown["error"]["code"], -32602);
        let positional = rig.request(
            "tools/call",
            json!({"name": "herdr_send", "arguments": ["w1:p1", "hi"]}),
        );
        assert_eq!(positional["error"]["code"], -32602);
        assert!(rig.fake.take("agent.prompt").is_empty());
    }

    #[test]
    fn a_launch_opens_a_worktree_off_the_projects_own_checkout_and_prompts_the_default_agent() {
        let mut rig = rig(vec![], None);
        let launched = rig.ok(
            "herdr_launch",
            json!({"project": "ACME-api", "title": "Fix the login bug!", "message": "fix it"}),
        );

        assert_eq!(
            rig.worktrees(),
            [
                json!({"workspace_id": "w1", "branch": "fix-the-login-bug", "base": null,
                    "label": "acme-api", "focus": false})
            ]
        );
        let start = &rig.fake.take("agent.start")[0];
        assert_eq!(start["kind"], "pi");
        assert_eq!(start["pane_id"], "w91:p1");
        assert_eq!(
            start["args"],
            json!(["--model", "claude-opus-5-5", "--thinking", "medium"])
        );
        assert!(
            start["name"]
                .as_str()
                .unwrap()
                .starts_with("fix-the-login-bug-")
        );
        let prompt = &rig.fake.take("agent.prompt")[0];
        assert_eq!(
            (&prompt["target"], &prompt["text"]),
            (&json!("w91:p1"), &json!("fix it"))
        );

        assert_eq!(launched["thread"], "terminal:t-w91:p1");
        assert_eq!(launched["pane_id"], "w91:p1");
        let branch = launched["branch"].as_str().unwrap();
        assert!(branch.starts_with("fix-the-login-bug-"), "{branch}");
        assert_eq!(launched["path"], format!("/worktrees/{branch}"));
        assert_eq!(launched["launched"]["harness"], "pi");
        assert_eq!(
            (&launched["ready"], &launched["sent"]),
            (&json!(true), &json!(true))
        );

        let threads = rig.threads();
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0]["title"], "Fix the login bug!");
        assert_eq!(threads[0]["mine"], true);
        assert_eq!(threads[0]["state"], "working");
    }

    #[test]
    fn a_launch_takes_its_harness_model_effort_branch_and_base_from_the_call() {
        let mut rig = rig(vec![], None);
        rig.config("worktree: {base: origin/main, branch_prefix: agent/}\n");
        rig.ok(
            "herdr_launch",
            json!({"project": "w2", "title": "t", "harness": "codex",
                   "model": "gpt-5.2", "effort": "high", "branch": "amb/login"}),
        );
        let worktree = &rig.worktrees()[0];
        assert_eq!(
            (&worktree["branch"], &worktree["base"]),
            (&json!("amb/login"), &json!("origin/main"))
        );
        let start = &rig.fake.take("agent.start")[0];
        assert_eq!(start["kind"], "codex");
        assert_eq!(
            start["args"],
            json!([
                "--model",
                "gpt-5.2",
                "--config",
                "model_reasoning_effort=high"
            ])
        );
        assert!(
            rig.fake.take("agent.prompt").is_empty(),
            "no message, so it starts idle"
        );

        rig.ok(
            "herdr_launch",
            json!({"project": "notes", "title": "Tidy up"}),
        );
        assert_eq!(rig.worktrees()[0]["branch"], "agent/tidy-up");
    }

    #[test]
    fn agent_names_keep_to_herdrs_rule_whatever_the_title() {
        let mut rig = rig(vec![], None);
        for title in [
            "42 flaky tests",
            "A very long title that goes on and on and on",
            "!!!",
        ] {
            rig.ok(
                "herdr_launch",
                json!({"project": "acme-api", "title": title}),
            );
        }
        for start in rig.fake.take("agent.start") {
            let name = start["name"].as_str().unwrap();
            assert!(name.len() <= 32, "{name}");
            assert!(name.starts_with(|c: char| c.is_ascii_lowercase()), "{name}");
            let allowed =
                |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || "-_".contains(c);
            assert!(name.chars().all(allowed), "{name}");
        }
    }

    #[test]
    fn a_title_is_kept_on_one_line_and_a_message_without_control_keys() {
        let mut rig = rig(vec![], None);
        rig.ok(
            "herdr_launch",
            json!({"project": "acme-api", "title": "Fix\n  the\u{1b}[31m bug"}),
        );
        assert_eq!(rig.threads()[0]["title"], "Fix the [31m bug");
        let escape = rig.failure(
            "herdr_launch",
            json!({"project": "acme-api", "title": "x", "message": "go\u{1b}[201~\r"}),
        );
        assert_eq!(escape["code"], "invalid_arguments");
    }

    #[test]
    fn a_project_is_found_by_its_workspace_id_first_and_never_by_a_guess() {
        let mut rig = rig(vec![], None);
        rig.fake
            .workspaces
            .lock()
            .unwrap()
            .push(json!({"workspace_id": "w4", "label": "notes", "tab_count": 1, "pane_count": 1}));
        let ambiguous = rig.failure("herdr_launch", json!({"project": "notes", "title": "t"}));
        assert_eq!(ambiguous["code"], "project_ambiguous");
        assert_eq!(ambiguous["workspace_ids"], json!(["w3", "w4"]));
        rig.ok("herdr_launch", json!({"project": "w4", "title": "t"}));
        assert_eq!(rig.worktrees()[0]["workspace_id"], "w4");
        let elsewhere = rig.failure(
            "herdr_launch",
            json!({"project": "/src/other-repo", "title": "t"}),
        );
        assert_eq!(elsewhere["code"], "project_not_open");
        assert!(rig.worktrees().is_empty());
    }

    #[test]
    fn with_only_a_linked_worktree_open_a_launch_branches_off_the_repository_root() {
        let mut rig = rig(vec![], None);
        rig.fake.workspaces.lock().unwrap().remove(0);
        rig.ok("herdr_launch", json!({"project": "acme-api", "title": "t"}));
        assert_eq!(
            rig.worktrees(),
            [json!({"cwd": "/src/acme-api", "branch": "t", "base": null,
                    "label": "acme-api", "focus": false})]
        );
    }

    #[test]
    fn a_retried_launch_with_the_same_request_id_starts_nothing_new() {
        let mut rig = rig(vec![], None);
        let arguments = json!({"project": "acme-api", "title": "Once", "request_id": "r1"});
        let first = rig.ok("herdr_launch", arguments.clone());
        let again = rig.ok("herdr_launch", arguments);
        assert_eq!(first, again);
        assert_eq!(rig.worktrees().len(), 1);
    }

    #[test]
    fn an_agent_that_stops_at_its_own_dialog_is_launched_but_not_prompted() {
        let mut rig = rig(vec![], None);
        rig.fake.on("agent.start", |agents| {
            agents.last_mut().unwrap()["agent_status"] = json!("blocked");
        });
        let launched = rig.ok(
            "herdr_launch",
            json!({"project": "acme-api", "title": "Blocked", "message": "go"}),
        );
        assert_eq!(
            (&launched["ready"], &launched["sent"]),
            (&json!(false), &json!(false))
        );
        assert!(launched["next"].as_str().unwrap().contains("tell the user"));
        assert!(rig.fake.take("agent.prompt").is_empty());
    }

    #[test]
    fn a_launch_waits_for_the_new_panes_shell_before_starting_the_agent() {
        let mut rig = rig(vec![], None);
        rig.refuse("agent.start", "agent_pane_busy");
        let busy = rig.fake.fail_method.clone();
        rig.fake
            .on("agent.start", move |_| *busy.lock().unwrap() = None);
        let launched = rig.ok(
            "herdr_launch",
            json!({"project": "acme-api", "title": "Slow shell"}),
        );
        assert_eq!(launched["ready"], true);
        assert_eq!(rig.fake.take("agent.start").len(), 2);
    }

    #[test]
    fn a_launch_that_cannot_start_its_agent_says_what_it_left_open_and_a_retry_opens_no_more() {
        let mut rig = rig(vec![], None);
        rig.refuse("agent.start", "agent_start_failed");
        let arguments = json!({"project": "acme-api", "title": "Broken", "request_id": "r1"});
        let error = rig.failure("herdr_launch", arguments.clone());
        assert_eq!(error["code"], "agent_start_failed");
        assert_eq!(error["workspace_id"], "w91");
        assert_eq!(error["pane_id"], "w91:p1");
        assert!(
            error["path"]
                .as_str()
                .unwrap()
                .starts_with("/worktrees/broken-")
        );
        assert_eq!(rig.failure("herdr_launch", arguments), error);
        assert_eq!(rig.worktrees().len(), 1);
    }

    #[test]
    fn an_agent_whose_command_exits_is_reported_with_what_it_printed() {
        let mut rig = rig(vec![], None);
        *rig.fake.screen.lock().unwrap() = "$ codex --bogus\nerror: unexpected argument\n".into();
        rig.refuse("agent.get", "agent_not_found");
        let error = rig.failure(
            "herdr_launch",
            json!({"project": "acme-api", "title": "Typo", "message": "go"}),
        );
        assert_eq!(error["code"], "agent_start_failed");
        assert_eq!(
            error["screen"],
            "$ codex --bogus\nerror: unexpected argument"
        );
        assert_eq!(rig.fake.take("pane.read")[0]["pane_id"], "w91:p1");
        assert!(rig.fake.take("agent.prompt").is_empty());
    }

    #[test]
    fn a_launch_needs_a_project_it_can_find() {
        let mut rig = rig(vec![resumable("w3:p1", "working", 2)], None);
        let none = rig.failure("herdr_launch", json!({"title": "Nowhere"}));
        assert_eq!(none["code"], "project_required");
        let unknown = rig.failure(
            "herdr_launch",
            json!({"project": "nope", "title": "Nowhere"}),
        );
        assert_eq!(unknown["code"], "project_not_found");
        assert_eq!(unknown["projects"], json!(["acme-api", "notes"]));
        assert!(rig.worktrees().is_empty());
    }

    #[test]
    fn a_caller_inside_herdr_launches_in_its_own_project_by_default() {
        let mut rig = rig(vec![resumable("w3:p1", "working", 2)], Some("w3:p1"));
        rig.fake.agents.lock().unwrap()[0]["workspace_id"] = json!("w3");
        rig.ok("herdr_launch", json!({"title": "Here"}));
        assert_eq!(rig.worktrees()[0]["workspace_id"], "w3");
        let threads = rig.threads();
        let launched = threads
            .iter()
            .find(|thread| thread["title"] == "Here")
            .unwrap();
        assert_eq!(launched["mine"], true);
    }

    #[test]
    fn a_caller_herdr_does_not_list_as_an_agent_still_launches_in_its_own_project() {
        let mut rig = rig(vec![], Some("w3:p7"));
        rig.ok("herdr_launch", json!({"title": "From a script"}));
        assert_eq!(rig.worktrees()[0]["workspace_id"], "w3");
        assert_eq!(rig.threads()[0]["mine"], true);
    }

    #[test]
    fn a_config_file_that_does_not_parse_stops_a_launch() {
        let mut rig = rig(vec![], None);
        rig.config("default_harness: [\n");
        let error = rig.failure(
            "herdr_launch",
            json!({"project": "acme-api", "title": "One"}),
        );
        assert_eq!(error["code"], "bad_config");
        assert!(rig.worktrees().is_empty());
    }

    #[test]
    fn threads_another_caller_launched_are_reachable_but_listed_after_ones_own() {
        let mut rig = rig(vec![resumable("w1:p1", "idle", 3)], None);
        rig.ok(
            "herdr_launch",
            json!({"project": "acme-api", "title": "Mine"}),
        );
        rig.server.client = "someone-else".into();
        rig.ok(
            "herdr_launch",
            json!({"project": "acme-api", "title": "Theirs"}),
        );

        let threads = rig.threads();
        let titles: Vec<(&str, bool)> = threads
            .iter()
            .map(|t| (t["title"].as_str().unwrap(), t["mine"].as_bool().unwrap()))
            .collect();
        assert_eq!(titles[0], ("Theirs", true));
        assert!(titles[1..].iter().all(|(_, mine)| !mine), "{titles:?}");

        let only_mine = rig.ok("herdr_threads", json!({"mine": true}));
        assert_eq!(only_mine["threads"].as_array().unwrap().len(), 1);
        let sent = rig.ok(
            "herdr_send",
            json!({"thread": key("w1:p1"), "message": "hello"}),
        );
        assert_eq!(
            (&sent["sent"], &sent["mine"]),
            (&json!(true), &json!(false))
        );
        assert_eq!(
            rig.fake.take("agent.prompt").last().unwrap()["target"],
            "w1:p1"
        );
    }

    #[test]
    fn a_launch_whose_worktree_request_went_unanswered_is_not_retried_into_a_second() {
        let mut rig = rig(vec![], None);
        rig.refuse("worktree.create", "worktree_open_failed");
        let arguments = json!({"project": "acme-api", "title": "Big repo", "request_id": "r1"});
        let error = rig.failure("herdr_launch", arguments.clone());
        assert!(error["branch"].as_str().unwrap().starts_with("big-repo-"));
        assert_eq!(rig.failure("herdr_launch", arguments), error);
        assert_eq!(rig.fake.take("worktree.create").len(), 1);
    }

    #[test]
    fn a_launch_past_its_budget_stops_before_its_next_step() {
        let rig = rig(vec![], None);
        rig.fake.on("workspace.list", |_| {
            std::thread::sleep(std::time::Duration::from_millis(50));
        });
        let args = LaunchArgs::decode(json!({"project": "acme-api", "title": "Slow"})).unwrap();
        let mut call = Call::open(&rig.server.env, "test-client:i1").unwrap();
        let budget = std::time::Duration::from_millis(20);
        let error = ops::launch_tool(&mut call, &args, budget).unwrap_err();
        assert_eq!(error.code, "launch_timeout");
        assert!(rig.fake.take("worktree.create").is_empty());
        assert!(rig.fake.take("agent.start").is_empty());
    }

    #[test]
    fn too_little_time_for_a_checkout_fails_before_asking_and_is_not_kept_for_retries() {
        let rig = rig(vec![], None);
        let args = LaunchArgs::decode(json!({"project": "acme-api", "title": "t"})).unwrap();
        let mut call = Call::open(&rig.server.env, "test-client:i1").unwrap();
        let budget = std::time::Duration::from_secs(5);
        let error = ops::launch_tool(&mut call, &args, budget).unwrap_err();
        assert_eq!(error.code, "launch_timeout");
        assert!(
            !error.detail.contains_key("branch"),
            "nothing was asked for"
        );
        assert!(rig.fake.take("worktree.create").is_empty());
    }

    #[test]
    fn a_turn_that_starts_after_the_snapshot_still_stops_a_message() {
        let mut rig = rig(vec![resumable("w1:p1", "idle", 3)], None);
        let end_of_sync = "agent.view.set";
        rig.fake.on(end_of_sync, |agents| {
            agents[0]["agent_status"] = json!("working");
        });
        let error = rig.failure("herdr_send", json!({"thread": "w1:p1", "message": "hi"}));
        assert_eq!(error["code"], "agent_working");
        assert!(rig.fake.take("agent.prompt").is_empty());
    }

    #[test]
    fn a_message_to_an_agent_mid_turn_is_refused_rather_than_queued() {
        let mut rig = rig(vec![resumable("w1:p1", "working", 3)], None);
        let error = rig.failure("herdr_send", json!({"thread": "w1:p1", "message": "also"}));
        assert_eq!(error["code"], "agent_working");
        assert!(rig.fake.take("agent.prompt").is_empty());
    }

    #[test]
    fn the_id_a_launch_handed_out_still_finds_the_thread_after_it_reports_its_session() {
        let mut rig = rig(vec![], None);
        let launched = rig.ok("herdr_launch", json!({"project": "acme-api", "title": "t"}));
        assert_eq!(launched["thread"], "terminal:t-w91:p1");
        rig.fake.agents.lock().unwrap()[0]["agent_session"] = json!({
            "source": "herdr:pi", "agent": "pi", "kind": "id", "value": "s1"
        });
        let read = rig.ok("herdr_read", json!({"thread": "terminal:t-w91:p1"}));
        assert_eq!(read["thread"], "session:pi:id:s1");
        assert_eq!(read["title"], "t");
        assert_eq!(read["mine"], true);
    }

    #[test]
    fn a_message_to_a_blocked_agent_is_refused_with_herdrs_reason() {
        let mut rig = rig(vec![resumable("w1:p1", "blocked", 3)], None);
        rig.refuse("agent.prompt", "agent_blocked");
        let error = rig.failure("herdr_send", json!({"thread": "w1:p1", "message": "yes"}));
        assert_eq!(error["code"], "agent_blocked");
        assert!(error["message"].as_str().unwrap().contains("ask the user"));
    }

    #[test]
    fn reading_keeps_the_end_of_the_screen() {
        let mut rig = rig(vec![resumable("w1:p1", "done", 3)], None);
        *rig.fake.screen.lock().unwrap() = format!(
            "\n\nfirst line   \n{}\n\n\n\nlast line\n\n",
            "─".repeat(120)
        );
        let whole = rig.ok("herdr_read", json!({"thread": key("w1:p1")}));
        assert_eq!(
            whole["text"], "first line\n────────\n\nlast line",
            "padding is dropped"
        );
        assert_eq!(whole["truncated"], false);
        rig.fake.take("agent.read");
        let read = rig.ok(
            "herdr_read",
            json!({"thread": key("w1:p1"), "max_chars": 9}),
        );
        assert_eq!(read["text"], "last line");
        assert_eq!(read["truncated"], true);
        assert_eq!(read["state"], "done");
        let request = &rig.fake.take("agent.read")[0];
        assert_eq!(
            (&request["target"], &request["lines"]),
            (&json!("w1:p1"), &json!(100))
        );
    }

    #[test]
    fn a_screen_herdr_will_not_scroll_back_is_read_as_it_shows_now() {
        let mut rig = rig(vec![resumable("w1:p1", "blocked", 3)], None);
        rig.refuse("agent.read", "agent_not_ready");
        let refusing = rig.fake.fail_method.clone();
        rig.fake
            .on("agent.read", move |_| *refusing.lock().unwrap() = None);
        *rig.fake.screen.lock().unwrap() = "Allow this command?".into();
        let read = rig.ok("herdr_read", json!({"thread": "w1:p1"}));
        assert_eq!(read["text"], "Allow this command?");
        let sources: Vec<Value> = rig
            .fake
            .take("agent.read")
            .iter()
            .map(|read| read["source"].clone())
            .collect();
        assert_eq!(sources, [json!("recent"), json!("visible")]);
    }

    #[test]
    fn a_wait_that_runs_out_is_an_answer_not_a_failure() {
        let mut rig = rig(vec![resumable("w1:p1", "working", 3)], None);
        rig.refuse("agent.wait", "timeout");
        let waited = rig.ok("herdr_wait", json!({"thread": "w1:p1", "timeout_ms": 5}));
        assert_eq!(
            (&waited["timed_out"], &waited["state"]),
            (&json!(true), &json!("working"))
        );

        *rig.fake.fail_method.lock().unwrap() = None;
        rig.fake.agents.lock().unwrap()[0]["agent_status"] = json!("blocked");
        let waited = rig.ok("herdr_wait", json!({"thread": "w1:p1"}));
        assert_eq!(waited["timed_out"], false);
        assert!(waited["next"].as_str().unwrap().contains("ask the user"));
    }

    #[test]
    fn a_working_agent_snoozed_by_an_agent_keeps_its_pane_after_its_turn() {
        let mut elsewhere = resumable("w1:p2", "idle", 4);
        elsewhere["focused"] = json!(true);
        let mut rig = rig(vec![resumable("w1:p1", "working", 3), elsewhere], None);
        let snooze = json!({"action": "snooze", "thread": key("w1:p1"), "minutes": 30});
        assert_eq!(rig.ok("herdr_organize", snooze)["thread"]["open"], true);
        {
            let mut agents = rig.fake.agents.lock().unwrap();
            agents[0]["agent_status"] = json!("done");
            agents[0]["state_change_seq"] = json!(5);
        }
        rig.ok("herdr_threads", json!({}));
        rig.ok("herdr_threads", json!({}));
        assert!(rig.fake.take("pane.close").is_empty());
    }

    #[test]
    fn settling_closes_the_pane_and_unsettling_reopens_it_without_taking_focus() {
        let mut rig = rig(
            vec![resumable("w1:p1", "idle", 3), resumable("w1:p2", "idle", 4)],
            None,
        );
        let thread = key("w1:p1");
        let settled = rig.ok(
            "herdr_organize",
            json!({"action": "settle", "thread": thread}),
        );
        assert_eq!(settled["thread"]["parked"], "settled");
        assert_eq!(settled["thread"]["open"], false);
        assert_eq!(rig.fake.take("pane.close"), [json!({"pane_id": "w1:p1"})]);

        let again = rig.ok(
            "herdr_organize",
            json!({"action": "settle", "thread": thread}),
        );
        assert_eq!(
            again["thread"]["parked"], "settled",
            "settling twice is not a toggle"
        );
        let closed = rig.failure("herdr_send", json!({"thread": thread, "message": "hi"}));
        assert_eq!(closed["code"], "thread_closed");

        rig.ok(
            "herdr_organize",
            json!({"action": "unsettle", "thread": thread}),
        );
        assert_eq!(rig.fake.take("tab.create")[0]["focus"], false);
        assert_eq!(
            rig.fake.take("pane.send_input")[0]["text"],
            "claude --resume id-w1:p1"
        );
    }

    #[test]
    fn a_working_agent_cannot_be_settled() {
        let mut rig = rig(
            vec![
                resumable("w1:p1", "working", 3),
                resumable("w1:p2", "idle", 4),
            ],
            None,
        );
        let error = rig.failure(
            "herdr_organize",
            json!({"action": "settle", "thread": "w1:p1"}),
        );
        assert_eq!(error["code"], "refused");
        assert_eq!(error["message"], "can't settle a working agent");
        assert!(rig.fake.take("pane.close").is_empty());
    }

    #[test]
    fn each_triage_action_reaches_the_state_it_names() {
        let mut rig = rig(
            vec![
                resumable("w1:p1", "working", 3),
                resumable("w1:p2", "idle", 4),
            ],
            None,
        );
        let thread = key("w1:p1");
        let mut organize = |action: &str, extra: Value| {
            let mut arguments = json!({"action": action, "thread": thread});
            arguments
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            rig.ok("herdr_organize", arguments)["thread"].clone()
        };
        assert_eq!(organize("pin", json!({}))["pinned"], true);
        assert_eq!(organize("pin", json!({}))["pinned"], true);
        assert_eq!(organize("unpin", json!({}))["pinned"], false);
        assert_eq!(
            organize("rename", json!({"title": "Login"}))["title"],
            "Login"
        );
        let snoozed = organize("snooze", json!({"minutes": 30}));
        assert_eq!(snoozed["parked"], "snoozed");
        assert_eq!(snoozed["open"], true, "a working agent's pane stays open");
        let until = DateTime::parse_from_rfc3339(snoozed["snoozed_until"].as_str().unwrap());
        assert_eq!(until.unwrap().timestamp(), NOW + 1800);
        assert_eq!(organize("wake", json!({}))["parked"], Value::Null);

        let undone = rig.ok("herdr_organize", json!({"action": "undo"}));
        assert_eq!(undone["thread"]["parked"], "snoozed");
    }

    #[test]
    fn a_snooze_needs_one_time_in_the_future() {
        let mut rig = rig(vec![resumable("w1:p1", "idle", 3)], None);
        for extra in [
            json!({}),
            json!({"minutes": 0}),
            json!({"until": "tomorrow"}),
            json!({"minutes": 5, "until": "2030-01-01T00:00:00Z"}),
        ] {
            let mut arguments = json!({"action": "snooze", "thread": "w1:p1"});
            arguments
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            let error = rig.failure("herdr_organize", arguments);
            assert_eq!(error["code"], "invalid_arguments", "{extra}");
        }
        let snoozed = rig.ok(
            "herdr_organize",
            json!({"action": "snooze", "thread": "w1:p1", "until": "2030-06-01T09:00:00-04:00"}),
        );
        assert_eq!(snoozed["thread"]["parked"], "snoozed");
    }

    #[test]
    fn only_a_closed_thread_can_be_forgotten() {
        let mut rig = rig(
            vec![resumable("w1:p1", "idle", 3), resumable("w1:p2", "idle", 4)],
            None,
        );
        let thread = key("w1:p1");
        let live = rig.failure(
            "herdr_organize",
            json!({"action": "forget", "thread": thread}),
        );
        assert_eq!(live["message"], "only closed threads can be forgotten");
        rig.ok(
            "herdr_organize",
            json!({"action": "settle", "thread": thread}),
        );
        rig.ok(
            "herdr_organize",
            json!({"action": "forget", "thread": thread}),
        );
        assert!(
            rig.threads()
                .iter()
                .all(|listed| listed["thread"] != thread)
        );
    }

    #[test]
    fn an_argument_the_tool_does_not_take_is_refused_by_name() {
        let mut rig = rig(vec![], None);
        let error = rig.failure("herdr_threads", json!({"projects": "acme-api"}));
        assert_eq!(error["code"], "invalid_arguments");
        assert!(error["message"].as_str().unwrap().contains("projects"));
    }

    #[test]
    fn a_workspace_herdr_lists_without_its_repository_is_still_found_by_repository() {
        let mut rig = rig(vec![], None);
        *rig.fake.workspaces.lock().unwrap() = vec![
            json!({"workspace_id": "w1", "label": "main", "tab_count": 1, "pane_count": 1}),
            json!({"workspace_id": "w2", "label": "fix", "tab_count": 1, "pane_count": 1}),
            json!({"workspace_id": "w3", "label": "scratch", "tab_count": 1, "pane_count": 1}),
        ];
        let listing = json!({
            "type": "worktree_list",
            "source": {"repo_key": "/src/acme-api/.git", "repo_name": "acme-api",
                       "repo_root": "/src/acme-api", "source_checkout_path": "/src/acme-api"},
            "worktrees": [
                {"path": "/wt/fix", "is_linked_worktree": true, "open_workspace_id": "w2"},
                {"path": "/src/acme-api", "is_linked_worktree": false, "open_workspace_id": "w1"},
                {"path": "/wt/unopened", "is_linked_worktree": true, "open_workspace_id": null},
            ],
        });
        rig.fake
            .worktree_lists
            .lock()
            .unwrap()
            .insert("w1".into(), listing);

        let projects = rig.ok("herdr_projects", json!({}))["projects"].clone();
        let names: Vec<&str> = projects
            .as_array()
            .unwrap()
            .iter()
            .map(|project| project["project"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["acme-api", "scratch"]);
        assert_eq!(projects[0]["workspaces"][1]["path"], "/wt/fix");
        assert_eq!(
            rig.fake.take("worktree.list").len(),
            2,
            "one per repository, one for scratch"
        );

        rig.ok(
            "herdr_launch",
            json!({"project": "/wt/fix/src", "title": "From a worktree"}),
        );
        let lists = rig.fake.take("worktree.list");
        assert!(
            lists.iter().any(|list| list["cwd"] == "/wt/fix/src"),
            "Herdr resolved the path"
        );
        assert_eq!(
            rig.worktrees()[0]["workspace_id"],
            "w1",
            "the new worktree branches off the repository's own checkout"
        );
    }

    #[test]
    fn projects_group_a_repositorys_workspaces_and_say_what_a_launch_starts() {
        let mut rig = rig(vec![resumable("w1:p1", "idle", 3)], None);
        rig.config("projects: {notes: {harness: claude, model: opus}}\n");
        let projects = rig.ok("herdr_projects", json!({}))["projects"].clone();
        assert_eq!(projects.as_array().unwrap().len(), 2);
        assert_eq!(projects[0]["project"], "acme-api");
        assert_eq!(projects[0]["workspaces"].as_array().unwrap().len(), 2);
        assert_eq!(projects[0]["open_threads"], 1);
        assert_eq!(projects[0]["launches"]["harness"], "pi");
        assert_eq!(
            projects[1]["launches"],
            json!({"harness": "claude", "model": "opus", "effort": null})
        );
        let capabilities = rig.ok("herdr_capabilities", json!({}));
        assert_eq!(capabilities["default_harness"], "pi");
        assert_eq!(
            capabilities["harnesses"]["codex"],
            json!({"model": null, "effort": null})
        );
    }
}
