use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::Op;

const PROTOCOL_VERSION: &str = "2025-06-18";
/// Never visited: T3 hands the authorization code back in its reply instead.
const REDIRECT: &str = "http://127.0.0.1/herdr-triage/callback";
/// The run mode up to which T3 lets herdr-triage change threads.
const ACCESS: &str = "auto";
const MAX_BODY_BYTES: u64 = 1 << 20;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// The `t3` launcher mounts the desktop app before it runs.
const CLI_TIMEOUT: Duration = Duration::from_secs(30);

pub struct Client {
    home: PathBuf,
    token_file: PathBuf,
    agent: ureq::Agent,
    credential: Option<Credential>,
    session: Option<Session>,
    next_id: u64,
    pub(crate) cli_timeout: Duration,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Done {
    Applied,
    /// T3 settles a thread that is working once its turn ends.
    AtTurnEnd,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Credential {
    environment_id: String,
    token: String,
    expires_at: i64,
}

struct Session {
    origin: String,
    id: String,
}

struct Server {
    origin: String,
    environment_id: String,
}

enum Failure {
    Unreachable,
    Unauthorized,
    SessionGone,
    Other(io::Error),
}

impl From<io::Error> for Failure {
    fn from(error: io::Error) -> Self {
        Failure::Other(error)
    }
}

fn other(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}

impl Client {
    pub fn from_env(state_root: &Path) -> Option<Self> {
        super::home().map(|home| Self::new(home, state_root))
    }

    pub fn new(home: PathBuf, state_root: &Path) -> Self {
        let agent = ureq::Agent::config_builder()
            .proxy(None)
            .max_redirects(0)
            .http_status_as_error(false)
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_global(Some(REQUEST_TIMEOUT))
            .build()
            .new_agent();
        Self {
            home,
            token_file: state_root.join("t3-token.json"),
            agent,
            credential: None,
            session: None,
            next_id: 0,
            cli_timeout: CLI_TIMEOUT,
        }
    }

    /// Has T3 make `op` on `thread`. A 401 or a forgotten session means T3 applied nothing, so
    /// those are retried once; a reply that never comes is not, since T3 may have applied it.
    pub fn organize(&mut self, thread: &str, op: Op) -> io::Result<Done> {
        let server = discover(&self.home)?;
        let now = chrono::Utc::now().timestamp();
        let usable = |credential: &Credential| {
            credential.environment_id == server.environment_id && credential.expires_at > now + 60
        };
        if !self.credential.as_ref().is_some_and(usable) {
            self.credential = read_credential(&self.token_file).filter(usable);
        }
        let mut signed_in = false;
        let mut restarted = false;
        loop {
            if self.credential.is_none() {
                self.session = None;
                self.credential = Some(self.sign_in(&server)?);
                signed_in = true;
            }
            match self.send(&server, thread, op) {
                Ok(done) => return Ok(done),
                Err(Failure::Unreachable) => return Err(other("T3 isn't running")),
                Err(Failure::Unauthorized) if !signed_in => self.credential = None,
                Err(Failure::Unauthorized) => {
                    return Err(other("T3 refused herdr-triage's new sign-in"));
                }
                Err(Failure::SessionGone) if !restarted => {
                    restarted = true;
                    self.session = None;
                }
                Err(Failure::SessionGone) => return Err(other("T3 keeps dropping the session")),
                Err(Failure::Other(error)) => return Err(error),
            }
        }
    }

    fn send(&mut self, server: &Server, thread: &str, op: Op) -> Result<Done, Failure> {
        let session = match &self.session {
            Some(session) if session.origin == server.origin => session.id.clone(),
            _ => {
                let id = self.initialize(server)?;
                self.session = Some(Session {
                    origin: server.origin.clone(),
                    id: id.clone(),
                });
                id
            }
        };
        let mut arguments = json!({"threadId": thread, "action": action(op)});
        if let Op::Snooze { until } = op {
            let until = chrono::DateTime::from_timestamp(until, 0)
                .ok_or_else(|| other("snooze time out of range"))?;
            arguments["snoozedUntil"] = json!(until.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string());
        }
        let id = self.request_id();
        let request = json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": "t3_thread_organize", "arguments": arguments}});
        let reply = self
            .mcp(server, Some(&session), &request)
            .map_err(|failure| match failure {
                Failure::Other(error) => Failure::Other(other(format!(
                    "{error}; T3 may have made the change, so check the thread in T3"
                ))),
                failure => failure,
            })?
            .1
            .ok_or_else(|| other("T3 sent no reply"))?;
        let result = rpc_result(&reply)?;
        let text = result["content"][0]["text"].as_str().unwrap_or_default();
        let answer: Value = serde_json::from_str(text).unwrap_or(Value::Null);
        if result["isError"] == json!(true) {
            let message = answer["message"].as_str().unwrap_or(text);
            return Err(other(format!("T3: {message}")).into());
        }
        if !answer.is_object() {
            return Err(other("T3 sent an unreadable reply; check the thread in T3").into());
        }
        Ok(if answer["settlesWhenTurnEnds"] == json!(true) {
            Done::AtTurnEnd
        } else {
            Done::Applied
        })
    }

    fn initialize(&mut self, server: &Server) -> Result<String, Failure> {
        let id = self.request_id();
        let request = json!({"jsonrpc": "2.0", "id": id, "method": "initialize",
            "params": {"protocolVersion": PROTOCOL_VERSION, "capabilities": {},
                "clientInfo": {"name": "herdr-triage", "version": env!("CARGO_PKG_VERSION")}}});
        let (session, reply) = self.mcp(server, None, &request)?;
        rpc_result(&reply.ok_or_else(|| other("T3 sent no reply"))?)?;
        let session = session.ok_or_else(|| other("T3 started no MCP session"))?;
        let notice = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        self.mcp(server, Some(&session), &notice)?;
        Ok(session)
    }

    fn request_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn mcp(
        &self,
        server: &Server,
        session: Option<&str>,
        message: &Value,
    ) -> Result<(Option<String>, Option<Value>), Failure> {
        let token = self
            .credential
            .as_ref()
            .map(|credential| credential.token.as_str())
            .unwrap_or_default();
        let mut request = self
            .agent
            .post(format!("{}/mcp", server.origin))
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream");
        if let Some(session) = session {
            request = request
                .header("mcp-session-id", session)
                .header("mcp-protocol-version", PROTOCOL_VERSION);
        }
        let response = request.send(message.to_string()).map_err(failure)?;
        let (status, headers, body) = read(response)?;
        match status {
            401 => return Err(Failure::Unauthorized),
            404 if session.is_some() => return Err(Failure::SessionGone),
            200..=299 => {}
            _ => return Err(refused(status, &body).into()),
        }
        let named = header(&headers, "mcp-session-id");
        let Some(id) = message.get("id") else {
            return Ok((named, None));
        };
        let streamed = header(&headers, "content-type")
            .is_some_and(|kind| kind.starts_with("text/event-stream"));
        let reply = if streamed {
            reply_in_stream(&body, id)
        } else {
            serde_json::from_str::<Value>(&body)
                .ok()
                .filter(|reply| reply.get("id") == Some(id))
        };
        let reply = reply.ok_or_else(|| other("T3 sent an unreadable reply"))?;
        Ok((named, Some(reply)))
    }

    fn sign_in(&self, server: &Server) -> io::Result<Credential> {
        let metadata = self.get_json(&format!(
            "{}/.well-known/oauth-authorization-server",
            server.origin
        ))?;
        let endpoint = |key: &str| {
            metadata[key]
                .as_str()
                .filter(|url| url.starts_with(&format!("{}/", server.origin)))
                .map(str::to_owned)
                .ok_or_else(|| other(format!("T3 named no {key} of its own")))
        };
        let (registration, token_endpoint) = (
            endpoint("registration_endpoint")?,
            endpoint("token_endpoint")?,
        );
        let issuer = metadata["issuer"].as_str().unwrap_or_default().to_owned();
        let registered = self.post_json(
            &registration,
            &json!({"client_name": "herdr-triage", "redirect_uris": [REDIRECT],
                "token_endpoint_auth_method": "none"}),
        )?;
        let client_id = registered["client_id"]
            .as_str()
            .ok_or_else(|| other("T3 registered no client"))?
            .to_owned();
        let pairing = self.pairing_code()?;
        let verifier = random_token(32)?;
        let state = random_token(16)?;
        let resource = format!("{}/mcp", server.origin);
        let decision = self.post_json(
            &format!("{}/oauth/mcp/decision", server.origin),
            &json!({"authorization": {"response_type": "code", "client_id": client_id,
                    "redirect_uri": REDIRECT,
                    "code_challenge": URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
                    "code_challenge_method": "S256", "state": state, "resource": resource},
                "decision": {"_tag": "pairing-code", "access": ACCESS, "code": pairing}}),
        )?;
        let redirect = decision["redirectTo"]
            .as_str()
            .ok_or_else(|| other("T3 did not approve the sign-in"))?;
        let query = redirect
            .strip_prefix(REDIRECT)
            .and_then(|rest| rest.strip_prefix('?'))
            .ok_or_else(|| other("T3 approved the sign-in for another address"))?;
        let params = parse_query(query);
        if let Some(error) = params.iter().find(|(key, _)| key == "error") {
            return Err(other(format!("T3 refused the sign-in: {}", error.1)));
        }
        let param = |key: &str| {
            params
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.as_str())
        };
        if param("state") != Some(state.as_str()) || param("iss").is_some_and(|iss| iss != issuer) {
            return Err(other("T3's sign-in answer did not match the request"));
        }
        let code = param("code").ok_or_else(|| other("T3 sent no sign-in code"))?;
        let response = self
            .agent
            .post(&token_endpoint)
            .send_form([
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", REDIRECT),
                ("client_id", &client_id),
                ("code_verifier", &verifier),
                ("resource", &resource),
            ])
            .map_err(unsent)?;
        let granted = json_body(response)?;
        let token = granted["access_token"]
            .as_str()
            .filter(|token| !token.is_empty())
            .ok_or_else(|| other("T3 issued no token"))?;
        let lifetime = granted["expires_in"].as_i64().unwrap_or(0);
        let credential = Credential {
            environment_id: server.environment_id.clone(),
            token: token.to_owned(),
            expires_at: chrono::Utc::now().timestamp() + lifetime,
        };
        let _ = write_private(&self.token_file, &serde_json::to_vec(&credential)?);
        Ok(credential)
    }

    /// A one-time code from `t3 auth pairing create`, which can approve at `ACCESS`.
    fn pairing_code(&self) -> io::Result<String> {
        let launcher = self.home.join("bin/t3");
        let program = if launcher.is_file() {
            launcher
        } else {
            PathBuf::from("t3")
        };
        let mut child = Command::new(&program)
            .args([
                "auth",
                "pairing",
                "create",
                "--scope",
                "orchestration:read",
                "--scope",
                "orchestration:operate",
                "--ttl",
                "5m",
                "--label",
                "herdr-triage",
                "--json",
            ])
            .env("T3CODE_HOME", &self.home)
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| other(format!("cannot run {}: {error}", program.display())))?;
        let stdout = child.stdout.take();
        let (sender, output) = mpsc::channel();
        thread::spawn(move || {
            let mut text = String::new();
            if let Some(stdout) = stdout {
                let _ = stdout.take(64 * 1024).read_to_string(&mut text);
            }
            let _ = sender.send(text);
        });
        let deadline = Instant::now() + self.cli_timeout;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                stop_group(&mut child);
                return Err(other("t3 took too long to issue a pairing code"));
            }
            thread::sleep(Duration::from_millis(25));
        };
        let text = output
            .recv_timeout(Duration::from_secs(1))
            .unwrap_or_default();
        let issued: Value = serde_json::from_str(text.trim()).unwrap_or(Value::Null);
        match issued["credential"].as_str() {
            Some(code) if status.success() && !code.is_empty() => Ok(code.to_owned()),
            _ => Err(other("t3 issued no pairing code")),
        }
    }

    fn get_json(&self, url: &str) -> io::Result<Value> {
        let response = self.agent.get(url).call().map_err(unsent)?;
        json_body(response)
    }

    fn post_json(&self, url: &str, body: &Value) -> io::Result<Value> {
        let response = self
            .agent
            .post(url)
            .header("content-type", "application/json")
            .send(body.to_string())
            .map_err(unsent)?;
        json_body(response)
    }
}

/// Stops the launcher and everything it started: TERM first, so its own cleanup runs.
fn stop_group(child: &mut std::process::Child) {
    let group = format!("-{}", child.id());
    let signal = |name: &str| {
        let _ = Command::new("kill")
            .args([name, &group])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    };
    signal("-TERM");
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline && matches!(child.try_wait(), Ok(None)) {
        thread::sleep(Duration::from_millis(20));
    }
    signal("-KILL");
    let _ = child.wait();
}

fn action(op: Op) -> &'static str {
    match op {
        Op::Settle => "settle",
        Op::Unsettle => "unsettle",
        Op::Snooze { .. } => "snooze",
        Op::Unsnooze => "unsnooze",
        Op::Pin => "pin",
        Op::Unpin => "unpin",
        Op::MarkUnread => "mark_unread",
    }
}

fn discover(home: &Path) -> io::Result<Server> {
    let userdata = home.join("userdata");
    let runtime = fs::read_to_string(userdata.join("server-runtime.json"))
        .map_err(|_| other("T3 isn't running"))?;
    let runtime: Value = serde_json::from_str(&runtime).map_err(|_| other("T3 isn't running"))?;
    let origin = runtime["origin"]
        .as_str()
        .ok_or_else(|| other("T3 isn't running"))?;
    if !is_loopback(origin) {
        return Err(other("T3 isn't running on this machine"));
    }
    let environment_id = fs::read_to_string(userdata.join("environment-id"))?
        .trim()
        .to_owned();
    if environment_id.is_empty() {
        return Err(other("T3 has no environment id"));
    }
    Ok(Server {
        origin: origin.trim_end_matches('/').to_owned(),
        environment_id,
    })
}

fn is_loopback(origin: &str) -> bool {
    let Some(authority) = origin.strip_prefix("http://") else {
        return false;
    };
    let authority = authority.trim_end_matches('/');
    let host = match authority.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => host,
        _ => authority,
    };
    matches!(host, "127.0.0.1" | "localhost" | "[::1]")
}

fn unsent(error: ureq::Error) -> io::Error {
    match failure(error) {
        Failure::Unreachable => other("T3 isn't running"),
        Failure::Other(error) => error,
        Failure::Unauthorized | Failure::SessionGone => other("T3 refused the request"),
    }
}

fn failure(error: ureq::Error) -> Failure {
    match error {
        ureq::Error::ConnectionFailed => Failure::Unreachable,
        ureq::Error::Io(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
            Failure::Unreachable
        }
        error => Failure::Other(other(format!("T3: {error}"))),
    }
}

fn read(
    mut response: ureq::http::Response<ureq::Body>,
) -> io::Result<(u16, ureq::http::HeaderMap, String)> {
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let body = response
        .body_mut()
        .with_config()
        .limit(MAX_BODY_BYTES)
        .read_to_string()
        .map_err(|error| other(format!("T3: {error}")))?;
    Ok((status, headers, body))
}

fn header(headers: &ureq::http::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

fn json_body(response: ureq::http::Response<ureq::Body>) -> io::Result<Value> {
    let (status, _, body) = read(response)?;
    if !(200..=299).contains(&status) {
        return Err(refused(status, &body));
    }
    serde_json::from_str(&body).map_err(|_| other("T3 sent an unreadable reply"))
}

fn refused(status: u16, body: &str) -> io::Error {
    let reply: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let message = reply["message"]
        .as_str()
        .or_else(|| reply["error_description"].as_str())
        .or_else(|| reply["error"].as_str())
        .map_or_else(
            || format!("T3 answered {status}"),
            |why| format!("T3: {why}"),
        );
    other(message)
}

fn rpc_result(reply: &Value) -> Result<&Value, Failure> {
    if let Some(error) = reply.get("error") {
        let message = error["message"].as_str().unwrap_or("unknown error");
        return Err(other(format!("T3: {message}")).into());
    }
    Ok(&reply["result"])
}

fn reply_in_stream(body: &str, id: &Value) -> Option<Value> {
    let mut data = String::new();
    for line in body.lines().chain([""]) {
        if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
        } else if line.is_empty() && !data.is_empty() {
            if let Ok(message) = serde_json::from_str::<Value>(&data)
                && message.get("id") == Some(id)
            {
                return Some(message);
            }
            data.clear();
        }
    }
    None
}

fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (percent_decode(key), percent_decode(value)))
        .collect()
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let hex = bytes
            .get(at + 1..at + 3)
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match (bytes[at], hex) {
            (b'%', Some(byte)) => {
                out.push(byte);
                at += 3;
            }
            (b'+', _) => {
                out.push(b' ');
                at += 1;
            }
            (byte, _) => {
                out.push(byte);
                at += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn random_token(bytes: usize) -> io::Result<String> {
    let mut buffer = vec![0; bytes];
    getrandom::fill(&mut buffer).map_err(|error| other(format!("no randomness: {error}")))?;
    Ok(URL_SAFE_NO_PAD.encode(buffer))
}

fn read_credential(path: &Path) -> Option<Credential> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

fn write_private(path: &Path, contents: &[u8]) -> io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let staging = dir.join(format!(".t3-token.{}.tmp", std::process::id()));
    let _ = fs::remove_file(&staging);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&staging)?;
    let written = file
        .write_all(contents)
        .and_then(|()| file.sync_all())
        .and_then(|()| fs::rename(&staging, path));
    if written.is_err() {
        let _ = fs::remove_file(&staging);
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;
    use std::fs;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Mutex};
    use std::thread;

    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};

    const ENV: &str = "env-1";
    const NOV_15_2026_0900_UTC: i64 = 1_794_733_200;

    #[derive(Default)]
    struct Server {
        pairing_codes: Vec<String>,
        pkce_challenges_by_code: BTreeMap<String, String>,
        valid_tokens: Vec<String>,
        sessions: Vec<String>,
        organize_reply: Option<(bool, String)>,
        stream_replies: bool,
        rpc_error: Option<String>,
        calls: Vec<(String, String, Value)>,
    }

    struct FakeT3 {
        dir: tempfile::TempDir,
        server: Arc<Mutex<Server>>,
        port: u16,
    }

    impl FakeT3 {
        fn start() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let server = Arc::new(Mutex::new(Server::default()));
            let shared = Arc::clone(&server);
            thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { break };
                    let shared = Arc::clone(&shared);
                    thread::spawn(move || serve(stream, &shared, port));
                }
            });
            let fake = Self { dir, server, port };
            let userdata = fake.home().join("userdata");
            fs::create_dir_all(&userdata).unwrap();
            fs::write(userdata.join("environment-id"), format!("{ENV}\n")).unwrap();
            fake.write_runtime(&format!("http://127.0.0.1:{port}"));
            let bin = fake.home().join("bin");
            fs::create_dir_all(&bin).unwrap();
            let cli = bin.join("t3");
            fs::write(
                &cli,
                "#!/bin/sh\necho \"$*\" >> \"$T3CODE_HOME/cli.log\"\n\
                 n=$(wc -l < \"$T3CODE_HOME/cli.log\" | tr -d ' ')\n\
                 echo \"{\\\"id\\\":\\\"p$n\\\",\\\"credential\\\":\\\"CODE$n\\\",\\\"label\\\":\\\"herdr-triage\\\"}\"\n",
            )
            .unwrap();
            fs::set_permissions(&cli, fs::Permissions::from_mode(0o755)).unwrap();
            fake
        }

        fn home(&self) -> PathBuf {
            self.dir.path().join("t3home")
        }

        fn state(&self) -> PathBuf {
            self.dir.path().join("state")
        }

        fn write_runtime(&self, origin: &str) {
            fs::write(
                self.home().join("userdata/server-runtime.json"),
                json!({"version": 1, "origin": origin, "port": self.port}).to_string(),
            )
            .unwrap();
        }

        fn client(&self) -> Client {
            Client::new(self.home(), &self.state())
        }

        fn cli_calls(&self) -> Vec<String> {
            fs::read_to_string(self.home().join("cli.log"))
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect()
        }

        fn calls(&self) -> Vec<(String, String, Value)> {
            std::mem::take(&mut self.server.lock().unwrap().calls)
        }

        fn names(&self) -> Vec<String> {
            self.calls().into_iter().map(|call| call.1).collect()
        }
    }

    fn serve(mut stream: TcpStream, shared: &Mutex<Server>, port: u16) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let mut parts = line.split_whitespace();
        let (verb, path) = (
            parts.next().unwrap_or_default().to_owned(),
            parts.next().unwrap_or_default().to_owned(),
        );
        let mut headers = BTreeMap::new();
        loop {
            let mut header = String::new();
            reader.read_line(&mut header).unwrap();
            let header = header.trim_end();
            if header.is_empty() {
                break;
            }
            if let Some((name, value)) = header.split_once(':') {
                headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
            }
        }
        let length: usize = headers
            .get("content-length")
            .map_or(0, |len| len.parse().unwrap());
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        let body = String::from_utf8(body).unwrap();
        let (status, extra, reply) = respond(
            &mut shared.lock().unwrap(),
            port,
            &verb,
            &path,
            &headers,
            &body,
        );
        let response = format!(
            "HTTP/1.1 {status}\r\n{}content-length: {}\r\nconnection: close\r\n{extra}\r\n{reply}",
            if extra.contains("content-type") {
                ""
            } else {
                "content-type: application/json\r\n"
            },
            reply.len()
        );
        stream.write_all(response.as_bytes()).unwrap();
    }

    fn form(body: &str) -> BTreeMap<String, String> {
        body.split('&')
            .filter_map(|pair| pair.split_once('='))
            .map(|(k, v)| (k.to_owned(), decode(v)))
            .collect()
    }

    fn decode(value: &str) -> String {
        let bytes = value.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'%' => {
                    out.push(u8::from_str_radix(&value[i + 1..i + 3], 16).unwrap());
                    i += 3;
                }
                b'+' => {
                    out.push(b' ');
                    i += 1;
                }
                b => {
                    out.push(b);
                    i += 1;
                }
            }
        }
        String::from_utf8(out).unwrap()
    }

    fn base64url(bytes: &[u8]) -> String {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let n = chunk
                .iter()
                .enumerate()
                .fold(0u32, |n, (i, &b)| n | u32::from(b) << (16 - 8 * i));
            for i in 0..=chunk.len() {
                out.push(char::from(ALPHABET[(n >> (18 - 6 * i) & 63) as usize]));
            }
        }
        out
    }

    fn respond(
        server: &mut Server,
        port: u16,
        verb: &str,
        path: &str,
        headers: &BTreeMap<String, String>,
        body: &str,
    ) -> (&'static str, String, String) {
        let origin = format!("http://127.0.0.1:{port}");
        let redirect = "http://127.0.0.1/herdr-triage/callback";
        match (verb, path) {
            ("GET", "/.well-known/oauth-authorization-server") => {
                server
                    .calls
                    .push((path.into(), "metadata".into(), Value::Null));
                let body = json!({"issuer": origin,
                    "authorization_endpoint": format!("{origin}/oauth/mcp/authorize"),
                    "token_endpoint": format!("{origin}/oauth/mcp/token"),
                    "registration_endpoint": format!("{origin}/oauth/mcp/register")});
                ("200 OK", String::new(), body.to_string())
            }
            ("POST", "/oauth/mcp/register") => {
                let request: Value = serde_json::from_str(body).unwrap();
                server
                    .calls
                    .push((path.into(), "register".into(), request.clone()));
                let body = json!({"client_id": "client-1", "client_name": request["client_name"],
                    "redirect_uris": request["redirect_uris"], "grant_types": ["authorization_code"],
                    "response_types": ["code"], "token_endpoint_auth_method": "none"});
                ("201 Created", String::new(), body.to_string())
            }
            ("POST", "/oauth/mcp/decision") => {
                let request: Value = serde_json::from_str(body).unwrap();
                server
                    .calls
                    .push((path.into(), "decision".into(), request.clone()));
                let code = request["decision"]["code"].as_str().unwrap_or_default();
                let authorization = &request["authorization"];
                let fine = request["decision"]["_tag"] == "pairing-code"
                    && server.pairing_codes.iter().any(|known| known == code)
                    && authorization["client_id"] == "client-1"
                    && authorization["redirect_uri"] == redirect
                    && authorization["code_challenge_method"] == "S256"
                    && authorization["resource"] == format!("{origin}/mcp");
                if !fine {
                    let body = json!({"_tag": "AuthMcpApprovalError", "message": "bad approval"});
                    return ("400 Bad Request", String::new(), body.to_string());
                }
                server.pairing_codes.retain(|known| known != code);
                let auth_code = format!("auth-{}", server.pkce_challenges_by_code.len() + 1);
                server.pkce_challenges_by_code.insert(
                    auth_code.clone(),
                    authorization["code_challenge"].as_str().unwrap().to_owned(),
                );
                let state = authorization["state"].as_str().unwrap_or_default();
                let iss = origin.replace(':', "%3A").replace('/', "%2F");
                let body = json!({"redirectTo": format!("{redirect}?code={auth_code}&state={state}&iss={iss}")});
                ("200 OK", String::new(), body.to_string())
            }
            ("POST", "/oauth/mcp/token") => {
                let request = form(body);
                server.calls.push((
                    path.into(),
                    "token".into(),
                    serde_json::to_value(&request).unwrap(),
                ));
                let challenge = request
                    .get("code")
                    .and_then(|code| server.pkce_challenges_by_code.remove(code));
                let verified = challenge.is_some_and(|challenge| {
                    base64url(&Sha256::digest(request["code_verifier"].as_bytes())) == challenge
                });
                if !verified
                    || request.get("grant_type").map(String::as_str) != Some("authorization_code")
                    || request.get("client_id").map(String::as_str) != Some("client-1")
                    || request.get("redirect_uri").map(String::as_str) != Some(redirect)
                {
                    return (
                        "400 Bad Request",
                        String::new(),
                        json!({"error": "invalid_grant"}).to_string(),
                    );
                }
                let token = format!("token-{}", server.valid_tokens.len() + 1);
                server.valid_tokens.push(token.clone());
                let body = json!({"access_token": token, "token_type": "Bearer",
                    "expires_in": 2_592_000, "scope": "orchestration:read orchestration:operate"});
                ("200 OK", String::new(), body.to_string())
            }
            ("POST", "/mcp") => respond_mcp(server, path, headers, body),
            _ => ("404 Not Found", String::new(), "{}".into()),
        }
    }

    fn respond_mcp(
        server: &mut Server,
        path: &str,
        headers: &BTreeMap<String, String>,
        body: &str,
    ) -> (&'static str, String, String) {
        let request: Value = serde_json::from_str(body).unwrap();
        let method = request["method"].as_str().unwrap_or_default().to_owned();
        let name = if method == "tools/call" {
            request["params"]["name"]
                .as_str()
                .unwrap_or_default()
                .to_owned()
        } else {
            method.clone()
        };
        server.calls.push((
            path.into(),
            name.clone(),
            request["params"]["arguments"].clone(),
        ));
        let token = headers
            .get("authorization")
            .and_then(|auth| auth.strip_prefix("Bearer "));
        if !token.is_some_and(|token| server.valid_tokens.iter().any(|t| t == token)) {
            return (
                "401 Unauthorized",
                String::new(),
                json!({"error": "invalid_token"}).to_string(),
            );
        }
        if method == "initialize" {
            let id = format!("session-{}", server.sessions.len() + 1);
            server.sessions.push(id.clone());
            let body = json!({"jsonrpc": "2.0", "id": request["id"], "result": {
                "protocolVersion": "2025-06-18", "capabilities": {},
                "serverInfo": {"name": "T3 Code", "version": "0.1.7"}}});
            return (
                "200 OK",
                format!("mcp-session-id: {id}\r\n"),
                body.to_string(),
            );
        }
        let session = headers.get("mcp-session-id");
        if !session.is_some_and(|session| server.sessions.contains(session)) {
            return ("404 Not Found", String::new(), "{}".into());
        }
        if method == "notifications/initialized" {
            return ("202 Accepted", String::new(), String::new());
        }
        if let Some(message) = &server.rpc_error {
            let body = json!({"jsonrpc": "2.0", "id": request["id"],
                "error": {"code": -32602, "message": message}});
            return ("200 OK", String::new(), body.to_string());
        }
        let (is_error, text) = server
            .organize_reply
            .clone()
            .unwrap_or((false, json!({"sequence": 7}).to_string()));
        let body = json!({"jsonrpc": "2.0", "id": request["id"], "result": {
            "content": [{"type": "text", "text": text}], "isError": is_error}});
        if server.stream_replies {
            let other = json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": {}});
            let stream =
                format!("event: message\ndata: {other}\n\nevent: message\ndata: {body}\n\n");
            return (
                "200 OK",
                "content-type: text/event-stream\r\n".into(),
                stream,
            );
        }
        ("200 OK", String::new(), body.to_string())
    }

    fn accept_pairing(fake: &FakeT3, codes: &[&str]) {
        fake.server
            .lock()
            .unwrap()
            .pairing_codes
            .extend(codes.iter().map(|code| (*code).to_owned()));
    }

    #[test]
    fn the_first_change_signs_in_with_a_one_time_code_then_sends_t3_the_command() {
        let fake = FakeT3::start();
        accept_pairing(&fake, &["CODE1"]);
        let done = fake
            .client()
            .organize(
                "t1",
                Op::Snooze {
                    until: NOV_15_2026_0900_UTC,
                },
            )
            .unwrap();
        assert_eq!(done, Done::Applied);

        let calls = fake.calls();
        let names: Vec<&str> = calls.iter().map(|call| call.1.as_str()).collect();
        assert_eq!(
            names,
            [
                "metadata",
                "register",
                "decision",
                "token",
                "initialize",
                "notifications/initialized",
                "t3_thread_organize",
            ]
        );
        assert_eq!(calls[1].2["client_name"], "herdr-triage");
        assert_eq!(calls[2].2["decision"]["access"], "auto");
        assert_eq!(
            calls[6].2,
            json!({"threadId": "t1", "action": "snooze", "snoozedUntil": "2026-11-15T09:00:00.000Z"})
        );
        assert_eq!(
            fake.cli_calls(),
            [
                "auth pairing create --scope orchestration:read --scope orchestration:operate --ttl 5m --label herdr-triage --json"
            ]
        );
    }

    #[test]
    fn each_op_sends_t3_its_action() {
        let fake = FakeT3::start();
        accept_pairing(&fake, &["CODE1"]);
        let mut client = fake.client();
        let ops = [
            (Op::Settle, "settle"),
            (Op::Unsettle, "unsettle"),
            (Op::Unsnooze, "unsnooze"),
            (Op::Pin, "pin"),
            (Op::Unpin, "unpin"),
            (Op::MarkUnread, "mark_unread"),
        ];
        for (op, _) in ops {
            client.organize("t1", op).unwrap();
        }
        let calls = fake.calls();
        let sent: Vec<&Value> = calls
            .iter()
            .filter(|call| call.1 == "t3_thread_organize")
            .map(|call| &call.2)
            .collect();
        let expected: Vec<Value> = ops
            .iter()
            .map(|(_, action)| json!({"threadId": "t1", "action": action}))
            .collect();
        assert_eq!(sent, expected.iter().collect::<Vec<_>>());
        let initializes = calls.iter().filter(|call| call.1 == "initialize").count();
        assert_eq!(initializes, 1, "one MCP session for the client");
    }

    #[test]
    fn the_token_is_private_and_a_later_picker_reuses_it_without_signing_in() {
        let fake = FakeT3::start();
        accept_pairing(&fake, &["CODE1"]);
        fake.client().organize("t1", Op::Pin).unwrap();
        let mode = fs::metadata(fake.state().join("t3-token.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        fake.calls();

        fake.client().organize("t1", Op::Unpin).unwrap();
        assert_eq!(
            fake.names(),
            [
                "initialize",
                "notifications/initialized",
                "t3_thread_organize"
            ]
        );
        assert_eq!(fake.cli_calls().len(), 1);
    }

    #[test]
    fn an_unpersisted_token_serves_this_client_but_the_next_client_signs_in_again() {
        let fake = FakeT3::start();
        accept_pairing(&fake, &["CODE1", "CODE2"]);
        fs::create_dir_all(fake.state().join("t3-token.json")).unwrap();
        let mut client = fake.client();
        client.organize("t1", Op::Pin).unwrap();
        client.organize("t1", Op::Unpin).unwrap();
        assert_eq!(fake.cli_calls().len(), 1);

        fake.client().organize("t1", Op::Pin).unwrap();
        assert_eq!(fake.cli_calls().len(), 2);
        let left: Vec<_> = fs::read_dir(fake.state())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(left, ["t3-token.json"], "no copy of the token left behind");
    }

    #[test]
    fn a_token_t3_no_longer_accepts_is_replaced_once() {
        let fake = FakeT3::start();
        accept_pairing(&fake, &["CODE1", "CODE2", "CODE3"]);
        fake.client().organize("t1", Op::Pin).unwrap();
        fake.server.lock().unwrap().valid_tokens.clear();
        fake.client().organize("t1", Op::Unpin).unwrap();
        assert_eq!(fake.cli_calls().len(), 2);

        let mut refusing = fake.client();
        fake.server.lock().unwrap().valid_tokens.clear();
        fake.server.lock().unwrap().pairing_codes.clear();
        let error = refusing.organize("t1", Op::Pin).unwrap_err();
        assert!(error.to_string().contains("T3"), "{error}");
        assert_eq!(fake.cli_calls().len(), 3, "no retry loop");
    }

    #[test]
    fn a_token_for_another_t3_or_one_past_its_expiry_is_not_sent() {
        for stale in [
            json!({"environmentId": "elsewhere", "token": "token-9", "expiresAt": NOV_15_2026_0900_UTC * 2}),
            json!({"environmentId": ENV, "token": "token-9", "expiresAt": 1}),
        ] {
            let fake = FakeT3::start();
            accept_pairing(&fake, &["CODE1"]);
            fake.server
                .lock()
                .unwrap()
                .valid_tokens
                .push("token-9".into());
            fs::create_dir_all(fake.state()).unwrap();
            fs::write(fake.state().join("t3-token.json"), stale.to_string()).unwrap();
            fake.client().organize("t1", Op::Pin).unwrap();
            assert_eq!(fake.cli_calls().len(), 1, "{stale}");
        }
    }

    #[test]
    fn t3_refusing_a_change_is_reported_in_its_own_words() {
        let fake = FakeT3::start();
        accept_pairing(&fake, &["CODE1"]);
        fake.server.lock().unwrap().organize_reply = Some((
            true,
            json!({"_tag": "OrchestratorMcpFailure", "code": "orchestration_error",
                "message": "Thread t1 has no completed run to mark unread."})
            .to_string(),
        ));
        let error = fake.client().organize("t1", Op::MarkUnread).unwrap_err();
        assert_eq!(
            error.to_string(),
            "T3: Thread t1 has no completed run to mark unread."
        );
    }

    #[test]
    fn settling_a_working_thread_settles_it_when_its_turn_ends() {
        let fake = FakeT3::start();
        accept_pairing(&fake, &["CODE1"]);
        fake.server.lock().unwrap().organize_reply =
            Some((false, json!({"settlesWhenTurnEnds": true}).to_string()));
        assert_eq!(
            fake.client().organize("t1", Op::Settle).unwrap(),
            Done::AtTurnEnd
        );
    }

    #[test]
    fn without_t3_running_here_nothing_is_sent_and_it_says_so() {
        let fake = FakeT3::start();
        fs::remove_file(fake.home().join("userdata/server-runtime.json")).unwrap();
        let error = fake.client().organize("t1", Op::Pin).unwrap_err();
        assert_eq!(error.to_string(), "T3 isn't running");

        let closed = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = closed.local_addr().unwrap().port();
        drop(closed);
        fake.write_runtime(&format!("http://127.0.0.1:{port}"));
        let error = fake.client().organize("t1", Op::Pin).unwrap_err();
        assert_eq!(error.to_string(), "T3 isn't running");

        fake.write_runtime(&format!("http://192.0.2.1:{}", fake.port));
        let error = fake.client().organize("t1", Op::Pin).unwrap_err();
        assert_eq!(error.to_string(), "T3 isn't running on this machine");
        assert!(fake.cli_calls().is_empty());
    }

    #[test]
    fn an_mcp_session_t3_forgot_is_started_again_without_signing_in() {
        let fake = FakeT3::start();
        accept_pairing(&fake, &["CODE1"]);
        let mut client = fake.client();
        client.organize("t1", Op::Pin).unwrap();
        fake.server.lock().unwrap().sessions.clear();
        client.organize("t1", Op::Unpin).unwrap();
        assert_eq!(fake.cli_calls().len(), 1);
    }

    #[test]
    fn a_streamed_reply_is_read_and_a_json_rpc_error_is_reported() {
        let fake = FakeT3::start();
        accept_pairing(&fake, &["CODE1"]);
        let mut client = fake.client();
        fake.server.lock().unwrap().stream_replies = true;
        fake.server.lock().unwrap().organize_reply =
            Some((false, json!({"settlesWhenTurnEnds": true}).to_string()));
        assert_eq!(client.organize("t1", Op::Settle).unwrap(), Done::AtTurnEnd);

        fake.server.lock().unwrap().rpc_error = Some("Invalid params".into());
        let error = client.organize("t1", Op::Settle).unwrap_err();
        assert_eq!(error.to_string(), "T3: Invalid params");
    }

    #[test]
    fn a_t3_command_that_hangs_is_stopped() {
        let fake = FakeT3::start();
        fs::write(fake.home().join("bin/t3"), "#!/bin/sh\nsleep 30\n").unwrap();
        let mut client = fake.client();
        client.cli_timeout = std::time::Duration::from_millis(200);
        let started = std::time::Instant::now();
        let error = client.organize("t1", Op::Pin).unwrap_err();
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert!(error.to_string().contains("t3 took too long"), "{error}");
    }

    #[test]
    fn a_reply_that_says_nothing_is_not_taken_for_success() {
        let fake = FakeT3::start();
        accept_pairing(&fake, &["CODE1"]);
        fake.server.lock().unwrap().organize_reply = Some((false, "not json".into()));
        let error = fake.client().organize("t1", Op::Pin).unwrap_err();
        assert!(
            error.to_string().contains("check the thread in T3"),
            "{error}"
        );
    }

    #[test]
    fn a_t3_command_that_hangs_is_stopped_with_everything_it_started() {
        let fake = FakeT3::start();
        fs::write(
            fake.home().join("bin/t3"),
            "#!/bin/sh\nsleep 30 &\necho $! > \"$T3CODE_HOME/child.pid\"\nwait\n",
        )
        .unwrap();
        let mut client = fake.client();
        client.cli_timeout = std::time::Duration::from_millis(300);
        assert!(client.organize("t1", Op::Pin).is_err());
        let child = fs::read_to_string(fake.home().join("child.pid")).unwrap();
        thread::sleep(std::time::Duration::from_millis(300));
        let alive = std::process::Command::new("kill")
            .args(["-0", child.trim()])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(!alive, "the launcher's child {} still runs", child.trim());
    }
}
