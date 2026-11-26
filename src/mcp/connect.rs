use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, fchown};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, TomlError, value};

use super::config;
use crate::store;

const NAME: &str = "herdr";
/// Codex hides its environment from an MCP server, bar the variables named here.
const CODEX_ENV_VARS: [&str; 2] = ["HERDR_PANE_ID", "HERDR_SOCKET_PATH"];
/// Codex gives a tool 60 seconds otherwise; a launch into a large repository takes up to four
/// minutes.
const CODEX_TOOL_TIMEOUT_SECS: i64 = 300;

struct Server {
    command: String,
    /// An app starts the server with little of the environment Herdr runs the plugin in (Claude
    /// passes on `HOME` and `PATH`, Codex what it is told to), so where the plugin keeps its
    /// settings and state is pinned: the server reads the same off switch and triage database.
    env: Vec<(&'static str, String)>,
}

struct App {
    name: &'static str,
    settings: PathBuf,
    with_server: fn(&str, &Server) -> Result<String, String>,
}

/// The app that calls itself `ChatGPT` runs Codex, and shares its settings with the Codex command.
fn apps() -> Vec<App> {
    let absolute = |dir: &PathBuf| dir.is_absolute();
    let home = std::env::var_os("HOME").map(PathBuf::from).filter(absolute);
    let claude = if cfg!(target_os = "macos") {
        home.as_ref()
            .map(|home| home.join("Library/Application Support"))
    } else {
        // Herdr can be given a config home the desktop never sees: Claude is then in the usual one.
        let usual = home.as_ref().map(|home| home.join(".config"));
        let has_claude = |dir: &PathBuf| dir.join("Claude").is_dir();
        config::xdg_config_home()
            .into_iter()
            .chain(usual)
            .find(has_claude)
    };
    let codex = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .filter(absolute)
        .or_else(|| home.map(|home| home.join(".codex")));
    let claude = claude.map(|dir| App {
        name: "Claude Desktop",
        settings: dir.join("Claude/claude_desktop_config.json"),
        with_server: claude_settings,
    });
    let codex = codex.map(|dir| App {
        name: "ChatGPT and Codex",
        settings: dir.join("config.toml"),
        with_server: codex_settings,
    });
    claude.into_iter().chain(codex).collect()
}

/// Sets each app whose settings folder is on this machine to start the plugin's server, leaving
/// the rest of its settings as they were, and says what it did: as an `Err` when there was an app
/// it could not set.
pub fn connect() -> Result<String, String> {
    let text = |path: &Path| {
        let unusable = || {
            format!(
                "{} is not a path an app's settings can hold",
                path.display()
            )
        };
        path.to_str().map(str::to_owned).ok_or_else(unusable)
    };
    let state = store::state_root();
    let state = std::path::absolute(&state).unwrap_or(state);
    let binary = state.join("herdr-triage");
    // A link left at an older binary would start a server that has no off switch.
    let running = std::env::current_exe().and_then(fs::canonicalize);
    if running.is_err() || fs::canonicalize(&binary).ok() != running.ok() {
        return Err(format!(
            "The link to the server's binary at {} could not be brought up to date",
            binary.display()
        ));
    }
    let mut server = Server {
        command: text(&binary)?,
        env: vec![("HERDR_PLUGIN_STATE_DIR", text(&state)?)],
    };
    if let Some(config) = config::xdg_config_home() {
        server.env.push(("XDG_CONFIG_HOME", text(&config)?));
    }

    let (mut connected, mut failed) = (Vec::new(), Vec::new());
    for app in apps() {
        if !app.settings.parent().is_some_and(Path::is_dir) {
            continue;
        }
        match write(&app, &server) {
            Ok(()) => connected.push(app.name),
            Err(error) => failed.push(format!(
                "Not connected to {}: {}: {error}",
                app.name,
                app.settings.display()
            )),
        }
    }
    let said = (!connected.is_empty()).then(|| {
        format!(
            "Herdr MCP connected to {}: restart to load it",
            connected.join(", ")
        )
    });
    match (said, failed.is_empty()) {
        (Some(said), true) => Ok(said),
        (None, true) => Err(
            "No Claude Desktop, ChatGPT or Codex found to connect: open a new app once first"
                .into(),
        ),
        (said, false) => Err(said
            .into_iter()
            .chain(failed)
            .collect::<Vec<_>>()
            .join(". ")),
    }
}

fn write(app: &App, server: &Server) -> io::Result<()> {
    // Settings kept in a dotfiles repository are a link, which a rename would replace.
    let file = match fs::canonicalize(&app.settings) {
        Ok(file) => file,
        Err(_) if app.settings.is_symlink() => {
            return Err(io::Error::other("a link to a file that is not there"));
        }
        Err(_) => app.settings.clone(),
    };
    let read = || match fs::read_to_string(&file) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    };
    let before = read()?;
    let after = (app.with_server)(before.as_deref().unwrap_or_default(), server)
        .map_err(io::Error::other)?;
    if before.as_ref() == Some(&after) {
        return Ok(());
    }
    // A file already at this name is not this run's to write over or remove.
    let staged = file.with_extension(format!("herdr-triage-{}", std::process::id()));
    let mut staging = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&staged)?;
    let replaced = staging
        .write_all(after.as_bytes())
        .and_then(|()| match fs::metadata(&file) {
            // These files hold other servers' tokens, and a new file is its writer's: without
            // the old one's owner and group, its mode would open them to someone else.
            Ok(old) => fchown(&staging, Some(old.uid()), Some(old.gid()))
                .and_then(|()| staging.set_permissions(old.permissions())),
            Err(_) => Ok(()),
        })
        .and_then(|()| staging.sync_all())
        .and_then(|()| {
            if read()? == before {
                fs::rename(&staged, &file)
            } else {
                Err(io::Error::other(
                    "the app saved its settings meanwhile: run this again",
                ))
            }
        });
    if replaced.is_err() {
        let _ = fs::remove_file(&staged);
    }
    replaced
}

/// An entry under the server's name that this action did not write, or one from before a plugin
/// update moved the binary.
fn is_another_server(command: Option<&str>, has_url: bool) -> bool {
    let ours = |command: &str| {
        let program = Path::new(command).file_name();
        program.is_some_and(|name| name == "herdr-triage")
    };
    has_url || command.is_some_and(|command| !ours(command))
}

fn taken() -> String {
    format!("another server named `{NAME}` is already there")
}

fn claude_settings(text: &str, server: &Server) -> Result<String, String> {
    let mut settings: Value = if text.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(text).map_err(|error| error.to_string())?
    };
    let misshapen = || format!("`mcpServers.{NAME}` has no object to go in");
    let entry = settings
        .as_object_mut()
        .ok_or_else(misshapen)?
        .entry("mcpServers")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(misshapen)?
        .entry(NAME)
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(misshapen)?;
    let command = entry
        .get("command")
        .map(|command| command.as_str().unwrap_or_default());
    if is_another_server(command, entry.contains_key("url")) {
        return Err(taken());
    }
    entry.insert("command".into(), server.command.as_str().into());
    entry.insert("args".into(), json!(["mcp"]));
    let env = entry
        .entry("env")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(misshapen)?;
    for (name, value) in &server.env {
        env.insert((*name).into(), value.as_str().into());
    }
    let mut after = serde_json::to_string_pretty(&settings).map_err(|error| error.to_string())?;
    if text.ends_with('\n') {
        after.push('\n');
    }
    Ok(after)
}

fn codex_settings(text: &str, server: &Server) -> Result<String, String> {
    // The error in full quotes the line it is about, which can hold another server's token.
    let unquoted = |error: TomlError| error.message().replace('\n', ", ");
    let mut settings: DocumentMut = text.parse().map_err(unquoted)?;
    let misshapen = || format!("`mcp_servers.{NAME}` has no table to go in");
    let mut servers = Table::new();
    servers.set_implicit(true);
    let servers = settings
        .entry("mcp_servers")
        .or_insert(Item::Table(servers));
    // An inline table holds only values: it drops a table put in it without a word.
    let new_entry = if servers.is_inline_table() {
        value(InlineTable::new())
    } else {
        toml_edit::table()
    };
    let entry = servers
        .as_table_like_mut()
        .ok_or_else(misshapen)?
        .entry(NAME)
        .or_insert(new_entry)
        .as_table_like_mut()
        .ok_or_else(misshapen)?;
    let command = entry
        .get("command")
        .map(|command| command.as_str().unwrap_or_default());
    if is_another_server(command, entry.contains_key("url")) {
        return Err(taken());
    }
    entry.insert("command", value(server.command.as_str()));
    entry.insert("args", value(Array::from_iter(["mcp"])));
    if !entry.contains_key("env_vars") {
        entry.insert("env_vars", value(Array::from_iter(CODEX_ENV_VARS)));
    }
    if !entry.contains_key("tool_timeout_sec") {
        entry.insert("tool_timeout_sec", value(CODEX_TOOL_TIMEOUT_SECS));
    }
    let env = entry
        .entry("env")
        .or_insert(value(InlineTable::new()))
        .as_table_like_mut()
        .ok_or_else(misshapen)?;
    for (name, pinned) in &server.env {
        env.insert(name, value(pinned.as_str()));
    }
    Ok(settings.to_string())
}
