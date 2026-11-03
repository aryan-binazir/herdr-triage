use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::triage::{Park, Record, Section, SessionRef, State, Status, Thread, Unread};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS threads (
    key TEXT PRIMARY KEY,
    pinned INTEGER NOT NULL,
    park TEXT,
    snooze_until INTEGER,
    unread TEXT,
    ack_seq INTEGER,
    generation TEXT NOT NULL,
    last_seq INTEGER NOT NULL,
    changed_at INTEGER,
    touched_at INTEGER NOT NULL,
    live INTEGER NOT NULL,
    ended INTEGER NOT NULL DEFAULT 0,
    missing_since INTEGER,
    last_status TEXT NOT NULL,
    label TEXT NOT NULL,
    agent TEXT,
    cwd TEXT,
    workspace_id TEXT,
    workspace_label TEXT,
    session_source TEXT,
    session_kind TEXT,
    session_value TEXT,
    reopen_pane TEXT,
    reopen_at INTEGER,
    reopen_generation TEXT,
    restored_at INTEGER,
    title TEXT
);
CREATE TABLE IF NOT EXISTS settings (name TEXT PRIMARY KEY, value TEXT NOT NULL);
";

pub struct Store {
    dir: PathBuf,
}

impl Store {
    /// Herdr shares one plugin state directory across sessions.
    pub fn for_socket(socket: &Path) -> Self {
        let root = std::env::var_os("HERDR_PLUGIN_STATE_DIR").map_or_else(
            || {
                let state = std::env::var_os("XDG_STATE_HOME").map_or_else(
                    || {
                        PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
                            .join(".local/state")
                    },
                    PathBuf::from,
                );
                state.join("herdr/plugins/ar.triage")
            },
            PathBuf::from,
        );
        Self::at(root.join(format!(
            "{:016x}",
            fnv1a(socket.as_os_str().as_encoded_bytes())
        )))
    }

    pub fn at(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn transact<T>(&self, change: impl FnOnce(&mut State) -> T) -> io::Result<T> {
        let lock = self.lock_file()?;
        lock.lock()?;
        self.change_locked(change)
    }

    /// Like [`Store::transact`], but returns `None` at once if another process holds the store.
    pub fn try_transact<T>(&self, change: impl FnOnce(&mut State) -> T) -> io::Result<Option<T>> {
        let lock = self.lock_file()?;
        match lock.try_lock() {
            Ok(()) => self.change_locked(change).map(Some),
            Err(fs::TryLockError::WouldBlock) => Ok(None),
            Err(fs::TryLockError::Error(error)) => Err(error),
        }
    }

    /// Asks whichever process holds the store to sync once more before it lets go.
    pub fn request_sync(&self) -> io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        fs::File::create(self.sync_request()).map(drop)
    }

    pub fn take_sync_request(&self) -> bool {
        fs::remove_file(self.sync_request()).is_ok()
    }

    pub fn sync_requested(&self) -> bool {
        self.sync_request().exists()
    }

    fn sync_request(&self) -> PathBuf {
        self.dir.join("sync.requested")
    }

    fn lock_file(&self) -> io::Result<fs::File> {
        fs::create_dir_all(&self.dir)?;
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join("state.lock"))
    }

    fn change_locked<T>(&self, change: impl FnOnce(&mut State) -> T) -> io::Result<T> {
        let mut connection = Connection::open(self.dir.join("triage.sqlite3")).map_err(broken)?;
        connection
            .busy_timeout(std::time::Duration::from_secs(10))
            .map_err(broken)?;
        connection.execute_batch(SCHEMA).map_err(broken)?;
        add_missing_columns(&connection).map_err(broken)?;
        self.import_legacy_json(&mut connection)?;
        let tx = connection.transaction().map_err(broken)?;
        let mut state = load(&tx).map_err(broken)?;
        let result = change(&mut state);
        save(&tx, &state).map_err(broken)?;
        tx.commit().map_err(broken)?;
        Ok(result)
    }

    fn import_legacy_json(&self, connection: &mut Connection) -> io::Result<()> {
        let json = self.dir.join("state.json");
        let bytes = match fs::read(&json) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        let tx = connection.transaction().map_err(broken)?;
        let empty: bool = tx
            .query_row("SELECT NOT EXISTS (SELECT 1 FROM threads)", [], |row| {
                row.get(0)
            })
            .map_err(broken)?;
        if empty {
            let state: State = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
            save(&tx, &state).map_err(broken)?;
        }
        tx.commit().map_err(broken)?;
        fs::rename(&json, json.with_extension("json.imported"))
    }
}

#[derive(Debug)]
struct DatabaseError(rusqlite::Error);

impl std::fmt::Display for DatabaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "triage database: {}", self.0)
    }
}

impl std::error::Error for DatabaseError {}

const LATER_COLUMNS: [(&str, &str); 4] = [
    ("ended", "INTEGER NOT NULL DEFAULT 0"),
    ("reopen_generation", "TEXT"),
    ("restored_at", "INTEGER"),
    ("title", "TEXT"),
];

fn add_missing_columns(connection: &Connection) -> rusqlite::Result<()> {
    let mut columns = connection.prepare("SELECT name FROM pragma_table_info('threads')")?;
    let existing: BTreeSet<String> = columns
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    for (name, definition) in LATER_COLUMNS {
        if !existing.contains(name) {
            connection.execute_batch(&format!(
                "ALTER TABLE threads ADD COLUMN {name} {definition}"
            ))?;
            if name == "ended" {
                connection.execute_batch("UPDATE threads SET ended = 1 WHERE live = 0")?;
            }
        }
    }
    Ok(())
}

fn broken(error: rusqlite::Error) -> io::Error {
    io::Error::other(DatabaseError(error))
}

fn load(tx: &Transaction) -> rusqlite::Result<State> {
    let mut state = State::default();
    let mut threads = tx.prepare("SELECT * FROM threads")?;
    let rows = threads.query_map([], |row| {
        let park = match (
            row.get::<_, Option<String>>("park")?.as_deref(),
            row.get("snooze_until")?,
        ) {
            (Some("settled"), _) => Some(Park::Settled),
            (Some("snoozed"), Some(until)) => Some(Park::Snoozed { until }),
            _ => None,
        };
        let session = match (
            row.get("session_source")?,
            row.get("session_kind")?,
            row.get("session_value")?,
        ) {
            (Some(source), Some(kind), Some(value)) => Some(SessionRef {
                source,
                kind,
                value,
            }),
            _ => None,
        };
        let record = Record {
            pinned: row.get("pinned")?,
            park,
            unread: match row.get::<_, Option<String>>("unread")?.as_deref() {
                Some("manual") => Some(Unread::Manual),
                Some("woke") => Some(Unread::Woke),
                _ => None,
            },
            ack_seq: row
                .get::<_, Option<i64>>("ack_seq")?
                .and_then(|seq| u64::try_from(seq).ok()),
            generation: row.get("generation")?,
            last_seq: u64::try_from(row.get::<_, i64>("last_seq")?).unwrap_or(0),
            changed_at: row.get("changed_at")?,
            touched_at: row.get("touched_at")?,
            live: row.get("live")?,
            ended: row.get("ended")?,
            missing_since: row.get("missing_since")?,
            last_status: status_from(&row.get::<_, String>("last_status")?),
            thread: Thread {
                label: row.get("label")?,
                agent: row.get("agent")?,
                cwd: row.get("cwd")?,
                workspace_id: row.get("workspace_id")?,
                workspace_label: row.get("workspace_label")?,
                session,
            },
            reopen_pane: row.get("reopen_pane")?,
            reopen_at: row.get("reopen_at")?,
            reopen_generation: row.get("reopen_generation")?,
            restored_at: row.get("restored_at")?,
            title: row.get("title")?,
        };
        Ok((row.get::<_, String>("key")?, record))
    })?;
    for row in rows {
        let (key, record) = row?;
        state.records.insert(key, record);
    }
    let sections: Option<String> = tx
        .query_row(
            "SELECT value FROM settings WHERE name = 'open_sections'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    state.open_sections = sections
        .unwrap_or_default()
        .split(',')
        .filter_map(|name| match name {
            "snoozed" => Some(Section::Snoozed),
            "settled" => Some(Section::Settled),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    Ok(state)
}

fn save(tx: &Transaction, state: &State) -> rusqlite::Result<()> {
    tx.execute("DELETE FROM threads", [])?;
    let mut insert = tx.prepare(
        "INSERT INTO threads (key, pinned, park, snooze_until, unread, ack_seq, generation, \
         last_seq, changed_at, touched_at, live, ended, missing_since, last_status, label, agent, \
         cwd, workspace_id, workspace_label, session_source, session_kind, session_value, \
         reopen_pane, reopen_at, reopen_generation, restored_at, title) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, \
         ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27)",
    )?;
    for (key, r) in &state.records {
        let (park, until) = match r.park {
            Some(Park::Settled) => (Some("settled"), None),
            Some(Park::Snoozed { until }) => (Some("snoozed"), Some(until)),
            None => (None, None),
        };
        let unread = r.unread.map(|unread| match unread {
            Unread::Manual => "manual",
            Unread::Woke => "woke",
        });
        let session = r.thread.session.as_ref();
        insert.execute(params![
            key,
            r.pinned,
            park,
            until,
            unread,
            r.ack_seq.and_then(|seq| i64::try_from(seq).ok()),
            r.generation,
            i64::try_from(r.last_seq).unwrap_or(i64::MAX),
            r.changed_at,
            r.touched_at,
            r.live,
            r.ended,
            r.missing_since,
            status_name(r.last_status),
            r.thread.label,
            r.thread.agent,
            r.thread.cwd,
            r.thread.workspace_id,
            r.thread.workspace_label,
            session.map(|s| &s.source),
            session.map(|s| &s.kind),
            session.map(|s| &s.value),
            r.reopen_pane,
            r.reopen_at,
            r.reopen_generation,
            r.restored_at,
            r.title,
        ])?;
    }
    let sections: Vec<&str> = state
        .open_sections
        .iter()
        .map(|section| match section {
            Section::Snoozed => "snoozed",
            Section::Settled => "settled",
        })
        .collect();
    tx.execute(
        "INSERT OR REPLACE INTO settings VALUES ('open_sections', ?1)",
        [sections.join(",")],
    )?;
    Ok(())
}

fn status_name(status: Status) -> &'static str {
    match status {
        Status::Idle => "idle",
        Status::Working => "working",
        Status::Blocked => "blocked",
        Status::Done => "done",
        Status::Unknown => "unknown",
    }
}

fn status_from(name: &str) -> Status {
    match name {
        "idle" => Status::Idle,
        "working" => Status::Working,
        "blocked" => Status::Blocked,
        "done" => Status::Done,
        _ => Status::Unknown,
    }
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> Record {
        Record {
            pinned: true,
            park: Some(Park::Snoozed { until: 1_800 }),
            unread: Some(Unread::Woke),
            ack_seq: Some(7),
            generation: "g1".into(),
            last_seq: 9,
            changed_at: Some(1_000),
            touched_at: 1_100,
            live: false,
            ended: true,
            missing_since: Some(1_050),
            last_status: Status::Done,
            thread: Thread {
                label: "fix flaky test".into(),
                agent: Some("codex".into()),
                cwd: Some("/work/api".into()),
                workspace_id: Some("w2".into()),
                workspace_label: Some("acme-api".into()),
                session: Some(SessionRef {
                    source: "herdr:codex".into(),
                    kind: "id".into(),
                    value: "0199-abc".into(),
                }),
            },
            reopen_pane: Some("w2:p4".into()),
            reopen_at: Some(1_200),
            reopen_generation: Some("g1".into()),
            restored_at: Some(1_300),
            title: Some("checkout flow".into()),
        }
    }

    #[test]
    fn threads_and_open_sections_survive_a_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().to_owned());
        store
            .transact(|state| {
                state
                    .records
                    .insert("session:codex:id:0199-abc".into(), record());
                state.records.insert(
                    "terminal:t1".into(),
                    Record {
                        generation: "g1".into(),
                        ..Record::default()
                    },
                );
                state.open_sections.insert(Section::Settled);
            })
            .unwrap();
        let (records, sections) = store
            .transact(|state| (state.records.clone(), state.open_sections.clone()))
            .unwrap();
        assert_eq!(records["session:codex:id:0199-abc"], record());
        assert_eq!(records.len(), 2);
        assert_eq!(sections, BTreeSet::from([Section::Settled]));
    }

    #[test]
    fn triage_saved_as_json_by_earlier_versions_is_imported_once() {
        let dir = tempfile::tempdir().unwrap();
        let json = r#"{"records":{"terminal:t9":{"pinned":true,"park":{"kind":"settled"},
            "generation":"g0","last_seq":3,"last_seen":500}}}"#;
        fs::write(dir.path().join("state.json"), json).unwrap();
        let store = Store::at(dir.path().to_owned());
        let imported = store
            .transact(|state| state.records.get("terminal:t9").cloned())
            .unwrap()
            .unwrap();
        assert!(imported.pinned);
        assert!(imported.live, "earlier versions only stored live agents");
        assert_eq!(
            (imported.park, imported.touched_at),
            (Some(Park::Settled), 500)
        );
        assert!(!dir.path().join("state.json").exists());
        assert!(dir.path().join("state.json.imported").exists());
    }

    #[test]
    fn a_corrupt_database_is_reported_and_left_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("triage.sqlite3");
        fs::write(
            &path,
            b"this is not a database, but it is long enough to look like one....",
        )
        .unwrap();
        let error = Store::at(dir.path().to_owned())
            .transact(|_| ())
            .unwrap_err();
        assert!(error.to_string().contains("triage database"), "{error}");
        assert_eq!(fs::read(&path).unwrap()[..7], *b"this is");
    }

    #[test]
    fn databases_from_before_a_column_existed_are_upgraded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("triage.sqlite3");
        let old = Connection::open(&path).unwrap();
        old.execute_batch(
            &SCHEMA
                .replace("    ended INTEGER NOT NULL DEFAULT 0,\n", "")
                .replace("    reopen_generation TEXT,\n", ""),
        )
        .unwrap();
        drop(old);
        let store = Store::at(dir.path().to_owned());
        store
            .transact(|state| state.records.insert("k".into(), record()))
            .unwrap();
        assert_eq!(
            store.transact(|state| state.records["k"].clone()).unwrap(),
            record()
        );
    }

    #[test]
    fn closed_threads_from_before_the_ended_column_stay_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("triage.sqlite3");
        let old = Connection::open(&path).unwrap();
        old.execute_batch(&SCHEMA.replace("    ended INTEGER NOT NULL DEFAULT 0,\n", ""))
            .unwrap();
        old.execute(
            "INSERT INTO threads (key, pinned, generation, last_seq, touched_at, live, last_status, label) \
             VALUES ('closed', 0, 'g1', 1, 1, 0, 'idle', ''), ('open', 0, 'g1', 1, 1, 1, 'idle', '')",
            [],
        )
        .unwrap();
        drop(old);
        let ended = Store::at(dir.path().to_owned())
            .transact(|state| (state.records["closed"].ended, state.records["open"].ended))
            .unwrap();
        assert_eq!(ended, (true, false));
    }
}
