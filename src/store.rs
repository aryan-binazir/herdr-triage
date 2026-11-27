use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::triage::{Park, Record, Section, SessionRef, State, Thread, Unread};

const SCHEMA_VERSION: i32 = 8;

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
    title TEXT,
    launched_by TEXT,
    settle_requested TEXT,
    close_requested TEXT
);
CREATE TABLE IF NOT EXISTS settings (name TEXT PRIMARY KEY, value TEXT NOT NULL);
";

pub fn state_root() -> PathBuf {
    std::env::var_os("HERDR_PLUGIN_STATE_DIR").map_or_else(
        || {
            // An empty or relative XDG_STATE_HOME counts as unset, as the XDG spec says.
            let state = std::env::var_os("XDG_STATE_HOME")
                .map(PathBuf::from)
                .filter(|dir| dir.is_absolute())
                .unwrap_or_else(|| {
                    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state")
                });
            state.join("herdr/plugins/ar.triage")
        },
        PathBuf::from,
    )
}

pub struct Store {
    dir: PathBuf,
}

impl Store {
    /// Herdr shares one plugin state directory across sessions.
    pub fn for_socket(socket: &Path) -> Self {
        Self::at(state_root().join(format!(
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

    pub fn close_lock(&self) -> io::Result<fs::File> {
        fs::create_dir_all(&self.dir)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join("close.lock"))?;
        lock.lock()?;
        Ok(lock)
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
        let before = state.clone();
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

const LATER_COLUMNS: &[(&str, &str)] = &[
    ("ended", "INTEGER NOT NULL DEFAULT 0"),
    ("reopen_generation", "TEXT"),
    ("restored_at", "INTEGER"),
    ("title", "TEXT"),
    ("launched_by", "TEXT"),
    ("settle_requested", "TEXT"),
    ("close_requested", "TEXT"),
];

fn add_missing_columns(connection: &Connection) -> rusqlite::Result<()> {
    let mut columns = connection.prepare("SELECT name FROM pragma_table_info('threads')")?;
    let existing: BTreeSet<String> = columns
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    for &(name, definition) in LATER_COLUMNS {
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
         changed_at, touched_at, live, ended, missing_since, label, agent, cwd, \
         workspace_id, workspace_label, session_source, session_kind, session_value, \
         reopen_pane, reopen_at, reopen_generation, restored_at, title, launched_by, \
         settle_requested, close_requested FROM threads",
    )?;
    let rows = threads.query_map([], |row| {
        let park = match (row.get::<_, Option<String>>(2)?.as_deref(), row.get(3)?) {
            (Some("settled"), _) => Some(Park::Settled),
            (Some("snoozed"), Some(until)) => Some(Park::Snoozed { until }),
            _ => None,
        };
        let session = match (row.get(18)?, row.get(19)?, row.get(20)?) {
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
                Some("left") => Some(Unread::Left),
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
            thread: Thread {
                label: row.get(13)?,
                agent: row.get(14)?,
                cwd: row.get(15)?,
                workspace_id: row.get(16)?,
                workspace_label: row.get(17)?,
                session,
            },
            reopen_pane: row.get(21)?,
            reopen_at: row.get(22)?,
            reopen_generation: row.get(23)?,
            restored_at: row.get(24)?,
            title: row.get(25)?,
            launched_by: row.get(26)?,
            settle_requested: row.get(27)?,
            close_requested: row.get(28)?,
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
    state.last_focused = tx
        .query_row(
            "SELECT value FROM settings WHERE name = 'last_focused'",
            [],
            |row| row.get(0),
        )
        .optional()?;
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
    // last_status is unread by this release; v0.2.6 still requires it and accepts any text.
    let mut insert = tx.prepare(
        "INSERT OR REPLACE INTO threads (key, pinned, park, snooze_until, unread, ack_seq, generation, \
         last_seq, changed_at, touched_at, live, ended, missing_since, last_status, label, agent, \
         cwd, workspace_id, workspace_label, session_source, session_kind, session_value, \
         reopen_pane, reopen_at, reopen_generation, restored_at, title, launched_by, settle_requested, \
         close_requested) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, 'unknown', ?14, ?15, ?16, ?17, \
         ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29)",
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
            Unread::Left => "left",
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
            r.launched_by,
            r.settle_requested,
            r.close_requested,
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
    if before.last_focused != state.last_focused {
        save_last_focused(tx, state.last_focused.as_deref())?;
    }
    Ok(())
}

fn save_last_focused(tx: &Transaction, key: Option<&str>) -> rusqlite::Result<()> {
    match key {
        Some(key) => tx.execute(
            "INSERT OR REPLACE INTO settings VALUES ('last_focused', ?1)",
            [key],
        )?,
        None => tx.execute("DELETE FROM settings WHERE name = 'last_focused'", [])?,
    };
    Ok(())
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use rusqlite::types::Value;

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
            launched_by: Some("client:claude-code".into()),
            settle_requested: Some("term-7".into()),
            close_requested: Some("term-8".into()),
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
        assert_eq!(version(), SCHEMA_VERSION);
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
    fn delta_writes_keep_the_store_equal_to_the_edited_state() {
        let dir = tempfile::tempdir().unwrap();
        let json = r#"{"records":{"legacy":{"pinned":true,"generation":"g","last_seq":3,"last_seen":500}}}"#;
        fs::write(dir.path().join("state.json"), json).unwrap();
        let connection = Connection::open(dir.path().join("triage.sqlite3")).unwrap();
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
        let store = Store::at(dir.path().to_owned());
        let mut expected: State = serde_json::from_str(json).unwrap();
        assert_eq!(store.transact(|s| s.clone()).unwrap(), expected);
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
                        s.last_focused = (i % 4 == 3).then_some(key);
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
            edit(&mut expected);
            store.transact(edit).unwrap();
            assert_eq!(store.transact(|s| s.clone()).unwrap(), expected, "step {i}");
        }
    }

    fn raw_row(connection: &Connection) -> BTreeMap<String, Value> {
        let mut select = connection.prepare("SELECT * FROM threads").unwrap();
        let names: Vec<String> = select.column_names().map(Into::into).collect();
        select
            .query_row([], |row| {
                names
                    .iter()
                    .map(|name| Ok((name.clone(), row.get(name.as_str())?)))
                    .collect()
            })
            .unwrap()
    }

    #[test]
    fn rows_written_by_v0_2_6_load_and_are_rewritten_in_the_same_shape() {
        let dir = tempfile::tempdir().unwrap();
        let old = Connection::open(dir.path().join("triage.sqlite3")).unwrap();
        let mut schema = SCHEMA.to_owned();
        for (name, definition) in LATER_COLUMNS {
            schema = schema
                .replace(&format!("    {name} {definition},\n"), "")
                .replace(&format!(",\n    {name} {definition}\n"), "\n");
        }
        old.execute_batch(&schema).unwrap();
        for (name, definition) in LATER_COLUMNS.iter().rev() {
            old.execute_batch(&format!(
                "ALTER TABLE threads ADD COLUMN {name} {definition}"
            ))
            .unwrap();
        }
        old.pragma_update(None, "user_version", 8).unwrap();
        old.execute_batch(
            "INSERT INTO threads (key, pinned, park, snooze_until, unread, ack_seq, generation, \
             last_seq, changed_at, touched_at, live, ended, missing_since, last_status, label, agent, \
             cwd, workspace_id, workspace_label, session_source, session_kind, session_value, \
             reopen_pane, reopen_at, reopen_generation, restored_at, title, launched_by, \
             settle_requested, close_requested) \
             VALUES ('k', 1, 'snoozed', 1800, 'woke', 7, 'g1', 9, 1000, 1100, 0, 1, 1050, 'done', \
             'fix flaky test', 'codex', '/work/api', 'w2', 'acme-api', 'herdr:codex', 'id', \
             '0199-abc', 'w2:p4', 1200, 'g0', 1300, 'checkout flow', 'client:claude-code', \
             'term-7', 'term-8')",
        )
        .unwrap();
        let written = raw_row(&old);
        let store = Store::at(dir.path().to_owned());
        assert_eq!(
            store.transact(|s| s.records["k"].clone()).unwrap(),
            record()
        );
        store
            .transact(|s| s.records.get_mut("k").unwrap().title = None)
            .unwrap();
        store
            .transact(|s| s.records.insert("k".into(), record()))
            .unwrap();
        let mut rewritten = raw_row(&old);
        assert_eq!(
            rewritten.insert("last_status".into(), Value::Text("done".into())),
            Some(Value::Text("unknown".into()))
        );
        assert_eq!(rewritten, written);
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
                state.last_focused = Some("terminal:t1".into());
                state.undo.clone()
            })
            .unwrap();
        let state = store.transact(|state| state.clone()).unwrap();
        assert_eq!(state.last_focused.as_deref(), Some("terminal:t1"));
        assert!(state.undo.is_some());
        assert_eq!(state.undo, saved_undo);
        assert_eq!(state.records["session:codex:id:0199-abc"], record());
        assert_eq!(state.records.len(), 2);
        assert_eq!(
            state.open_sections,
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
