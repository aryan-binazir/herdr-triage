use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::triage::{Park, Record, Section, SessionRef, State, Status, Thread, Unread};

const SCHEMA_VERSION: i32 = 5;

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

    pub fn wake_lock_path(&self, until: i64) -> io::Result<PathBuf> {
        fs::create_dir_all(&self.dir)?;
        Ok(self.dir.join(format!("wake-{until}.lock")))
    }

    pub fn prune_wake_locks(&self, now: i64) {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let expired = name
                .to_str()
                .and_then(|name| name.strip_prefix("wake-"))
                .and_then(|name| name.strip_suffix(".lock"))
                .and_then(|deadline| deadline.parse::<i64>().ok())
                .is_some_and(|deadline| deadline < now);
            if expired {
                let _ = fs::remove_file(entry.path());
            }
        }
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
        let version: i32 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(broken)?;
        if version < SCHEMA_VERSION {
            connection.execute_batch(SCHEMA).map_err(broken)?;
            add_missing_columns(&connection).map_err(broken)?;
            connection
                .pragma_update(None, "user_version", SCHEMA_VERSION)
                .map_err(broken)?;
        }
        self.import_legacy_json(&mut connection)?;
        let tx = connection.transaction().map_err(broken)?;
        let mut state = load(&tx).map_err(broken)?;
        let State {
            records,
            open_sections,
            undo,
        } = &state;
        let before = State {
            records: records.clone(),
            open_sections: open_sections.clone(),
            undo: undo.clone(),
        };
        let result = change(&mut state);
        save(&tx, &state, &before).map_err(broken)?;
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
            let before = load(&tx).map_err(broken)?;
            save(&tx, &state, &before).map_err(broken)?;
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
    let mut threads = tx.prepare(
        "SELECT key, pinned, park, snooze_until, unread, ack_seq, generation, last_seq, \
         changed_at, touched_at, live, ended, missing_since, last_status, label, agent, cwd, \
         workspace_id, workspace_label, session_source, session_kind, session_value, \
         reopen_pane, reopen_at, reopen_generation, restored_at, title FROM threads",
    )?;
    let rows = threads.query_map([], |row| {
        let park = match (row.get::<_, Option<String>>(2)?.as_deref(), row.get(3)?) {
            (Some("settled"), _) => Some(Park::Settled),
            (Some("snoozed"), Some(until)) => Some(Park::Snoozed { until }),
            _ => None,
        };
        let session = match (row.get(19)?, row.get(20)?, row.get(21)?) {
            (Some(source), Some(kind), Some(value)) => Some(SessionRef {
                source,
                kind,
                value,
            }),
            _ => None,
        };
        let record = Record {
            pinned: row.get(1)?,
            park,
            unread: match row.get::<_, Option<String>>(4)?.as_deref() {
                Some("manual") => Some(Unread::Manual),
                Some("woke") => Some(Unread::Woke),
                _ => None,
            },
            ack_seq: row
                .get::<_, Option<i64>>(5)?
                .and_then(|seq| u64::try_from(seq).ok()),
            generation: row.get(6)?,
            last_seq: u64::try_from(row.get::<_, i64>(7)?).unwrap_or(0),
            changed_at: row.get(8)?,
            touched_at: row.get(9)?,
            live: row.get(10)?,
            ended: row.get(11)?,
            missing_since: row.get(12)?,
            last_status: status_from(&row.get::<_, String>(13)?),
            thread: Thread {
                label: row.get(14)?,
                agent: row.get(15)?,
                cwd: row.get(16)?,
                workspace_id: row.get(17)?,
                workspace_label: row.get(18)?,
                session,
            },
            reopen_pane: row.get(22)?,
            reopen_at: row.get(23)?,
            reopen_generation: row.get(24)?,
            restored_at: row.get(25)?,
            title: row.get(26)?,
        };
        Ok((row.get::<_, String>(0)?, record))
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
            "working" => Some(Section::Working),
            "snoozed" => Some(Section::Snoozed),
            "settled" => Some(Section::Settled),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    let undo: Option<String> = tx
        .query_row(
            "SELECT value FROM settings WHERE name = 'undo'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    state.undo = undo.and_then(|json| serde_json::from_str(&json).ok());
    Ok(state)
}

fn save(tx: &Transaction, state: &State, before: &State) -> rusqlite::Result<()> {
    if state == before {
        return Ok(());
    }
    {
        let mut delete = tx.prepare("DELETE FROM threads WHERE key = ?1")?;
        for key in before.records.keys() {
            if !state.records.contains_key(key) {
                delete.execute([key])?;
            }
        }
    }
    let mut insert = tx.prepare(
        "INSERT OR REPLACE INTO threads (key, pinned, park, snooze_until, unread, ack_seq, generation, \
         last_seq, changed_at, touched_at, live, ended, missing_since, last_status, label, agent, \
         cwd, workspace_id, workspace_label, session_source, session_kind, session_value, \
         reopen_pane, reopen_at, reopen_generation, restored_at, title) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, \
         ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27)",
    )?;
    for (key, r) in &state.records {
        if before.records.get(key) == Some(r) {
            continue;
        }
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
    if before.open_sections != state.open_sections {
        let sections: Vec<&str> = state
            .open_sections
            .iter()
            .map(|section| match section {
                Section::Working => "working",
                Section::Snoozed => "snoozed",
                Section::Settled => "settled",
            })
            .collect();
        tx.execute(
            "INSERT OR REPLACE INTO settings VALUES ('open_sections', ?1)",
            [sections.join(",")],
        )?;
    }
    if before.undo != state.undo {
        match &state.undo {
            Some(undo) => tx.execute(
                "INSERT OR REPLACE INTO settings VALUES ('undo', ?1)",
                [serde_json::to_string(undo)
                    .map_err(|error| rusqlite::Error::ToSqlConversionFailure(error.into()))?],
            )?,
            None => tx.execute("DELETE FROM settings WHERE name = 'undo'", [])?,
        };
    }
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

    fn save_full(tx: &Transaction, state: &State) -> rusqlite::Result<()> {
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
                Section::Working => "working",
                Section::Snoozed => "snoozed",
                Section::Settled => "settled",
            })
            .collect();
        tx.execute(
            "INSERT OR REPLACE INTO settings VALUES ('open_sections', ?1)",
            [sections.join(",")],
        )?;
        match &state.undo {
            Some(undo) => tx.execute(
                "INSERT OR REPLACE INTO settings VALUES ('undo', ?1)",
                [serde_json::to_string(undo)
                    .map_err(|error| rusqlite::Error::ToSqlConversionFailure(error.into()))?],
            )?,
            None => tx.execute("DELETE FROM settings WHERE name = 'undo'", [])?,
        };
        Ok(())
    }

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
            reopen_generation: Some("g0".into()),
            restored_at: Some(1_300),
            title: Some("checkout flow".into()),
        }
    }

    #[test]
    fn migration_version_is_set_and_newer_versions_are_preserved() {
        assert_eq!(
            usize::try_from(SCHEMA_VERSION).unwrap(),
            LATER_COLUMNS.len() + 1
        );
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().to_owned());
        store.transact(|_| ()).unwrap();
        let observer = Connection::open(dir.path().join("triage.sqlite3")).unwrap();
        let version = || {
            observer
                .pragma_query_value(None, "user_version", |r| r.get::<_, i32>(0))
                .unwrap()
        };
        assert_eq!(version(), 5);
        observer.pragma_update(None, "user_version", 99).unwrap();
        store.transact(|_| ()).unwrap();
        assert_eq!(version(), 99);
        observer.execute_batch(SCHEMA).unwrap();
        add_missing_columns(&observer).unwrap();
        assert_eq!(version(), 99);
    }

    #[test]
    fn a_single_edit_writes_only_that_thread() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().to_owned());
        store
            .transact(|s| {
                s.records.insert("a".into(), record());
                s.records.insert("b".into(), record());
            })
            .unwrap();
        let observer = Connection::open(dir.path().join("triage.sqlite3")).unwrap();
        observer.execute_batch("CREATE TABLE writes (key TEXT);
            CREATE TRIGGER record_insert AFTER INSERT ON threads BEGIN INSERT INTO writes VALUES (new.key); END;
            CREATE TRIGGER record_update AFTER UPDATE ON threads BEGIN INSERT INTO writes VALUES (new.key); END;
            CREATE TRIGGER record_delete AFTER DELETE ON threads BEGIN INSERT INTO writes VALUES (old.key); END;
            CREATE TRIGGER setting_insert AFTER INSERT ON settings BEGIN INSERT INTO writes VALUES (new.name); END;
            CREATE TRIGGER setting_update AFTER UPDATE ON settings BEGIN INSERT INTO writes VALUES (new.name); END;
            CREATE TRIGGER setting_delete AFTER DELETE ON settings BEGIN INSERT INTO writes VALUES (old.name); END;").unwrap();
        store
            .transact(|s| s.records.get_mut("a").unwrap().title = Some("new".into()))
            .unwrap();
        let writes = observer
            .prepare("SELECT key FROM writes")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(writes, ["a"]);
        observer.execute("DELETE FROM writes", []).unwrap();
        store.transact(|_| ()).unwrap();
        assert_eq!(
            observer
                .query_row("SELECT COUNT(*) FROM writes", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn delta_matches_full_writer_for_edits_and_legacy_import() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let json = r#"{"records":{"legacy":{"pinned":true,"generation":"g","last_seq":3,"last_seen":500}}}"#;
        fs::write(a.path().join("state.json"), json).unwrap();
        fs::write(b.path().join("state.json"), json).unwrap();
        let delta = Store::at(b.path().to_owned());
        for dir in [a.path(), b.path()] {
            let connection = Connection::open(dir.join("triage.sqlite3")).unwrap();
            connection.execute_batch(SCHEMA).unwrap();
            connection
                .execute(
                    "INSERT INTO settings VALUES ('open_sections', 'working')",
                    [],
                )
                .unwrap();
            let mut stale = State::default();
            stale.records.insert("stale".into(), record());
            stale.rename("stale", Some("stale title".into()), 1);
            connection
                .execute(
                    "INSERT INTO settings VALUES ('undo', ?1)",
                    [serde_json::to_string(&stale.undo.unwrap()).unwrap()],
                )
                .unwrap();
        }
        let mut connection = Connection::open(a.path().join("triage.sqlite3")).unwrap();
        let tx = connection.transaction().unwrap();
        let imported: State = serde_json::from_str(json).unwrap();
        save_full(&tx, &imported).unwrap();
        tx.commit().unwrap();
        fs::rename(
            a.path().join("state.json"),
            a.path().join("state.json.imported"),
        )
        .unwrap();
        delta.transact(|_| ()).unwrap();
        for i in 0..60 {
            let edit = |s: &mut State| {
                let key = format!("k{}", i % 7);
                match i % 6 {
                    0 => {
                        s.records.insert(key, record());
                    }
                    1 => {
                        for r in s.records.values_mut() {
                            r.title = Some(format!("title{i}"));
                            r.last_seq += 1;
                        }
                    }
                    2 => {
                        s.records.remove(&key);
                    }
                    3 => {
                        s.toggle_section(Section::Settled);
                    }
                    4 => {
                        if let Some(k) = s.records.keys().next().cloned() {
                            s.rename(&k, Some(format!("rename{i}")), i);
                        }
                    }
                    _ => {
                        s.undo = None;
                        if let Some(k) = s.records.keys().next().cloned() {
                            let r = s.records.remove(&k).unwrap();
                            s.records.insert(format!("rekey{i}"), r);
                        }
                    }
                }
            };
            let mut connection = Connection::open(a.path().join("triage.sqlite3")).unwrap();
            let tx = connection.transaction().unwrap();
            let mut expected = load(&tx).unwrap();
            edit(&mut expected);
            save_full(&tx, &expected).unwrap();
            tx.commit().unwrap();
            delta.transact(edit).unwrap();
            let read = |dir: &Path| {
                let mut connection = Connection::open(dir.join("triage.sqlite3")).unwrap();
                load(&connection.transaction().unwrap()).unwrap()
            };
            assert_eq!(read(a.path()), read(b.path()), "step {i}");
        }
    }

    #[test]
    fn columns_added_in_a_different_order_load_correctly() {
        let dir = tempfile::tempdir().unwrap();
        let old = Connection::open(dir.path().join("triage.sqlite3")).unwrap();
        let mut schema = SCHEMA.to_owned();
        for (name, definition) in LATER_COLUMNS {
            schema = schema
                .replace(&format!("    {name} {definition},\n"), "")
                .replace(&format!(",\n    {name} {definition}\n"), "\n");
        }
        old.execute_batch(&schema).unwrap();
        for (name, definition) in LATER_COLUMNS.into_iter().rev() {
            old.execute_batch(&format!(
                "ALTER TABLE threads ADD COLUMN {name} {definition}"
            ))
            .unwrap();
        }
        let tx = old.unchecked_transaction().unwrap();
        let mut expected = State::default();
        expected.records.insert("k".into(), record());
        save_full(&tx, &expected).unwrap();
        tx.commit().unwrap();
        let store = Store::at(dir.path().to_owned());
        assert_eq!(
            store.transact(|s| s.records["k"].clone()).unwrap(),
            record()
        );
    }

    #[test]
    fn unchanged_transactions_leave_the_database_unwritten() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().to_owned());
        store
            .transact(|s| {
                s.records.insert("k".into(), record());
            })
            .unwrap();
        let observer = Connection::open(dir.path().join("triage.sqlite3")).unwrap();
        let version = || {
            observer
                .query_row("PRAGMA data_version", [], |r| r.get::<_, i64>(0))
                .unwrap()
        };
        let before = version();
        store.transact(|_| ()).unwrap();
        assert_eq!(version(), before);
        assert_eq!(store.try_transact(|_| ()).unwrap(), Some(()));
        assert_eq!(version(), before);
    }

    #[test]
    fn threads_open_sections_and_undo_survive_a_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().to_owned());
        let saved_undo = store
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
                state.open_sections.insert(Section::Working);
                state.rename("terminal:t1", Some("release notes".into()), 9);
                state.undo.clone()
            })
            .unwrap();
        let (records, sections, undo) = store
            .transact(|state| {
                (
                    state.records.clone(),
                    state.open_sections.clone(),
                    state.undo.clone(),
                )
            })
            .unwrap();
        assert!(undo.is_some());
        assert_eq!(undo, saved_undo);
        assert_eq!(records["session:codex:id:0199-abc"], record());
        assert_eq!(records.len(), 2);
        assert_eq!(
            sections,
            BTreeSet::from([Section::Working, Section::Settled])
        );
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
