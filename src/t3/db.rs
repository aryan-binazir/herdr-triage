//! Reads this machine's T3 Code threads straight from T3's own database, read-only: T3 owns
//! that data, and the picker only ever looks. A database T3 has reshaped is an error, never a
//! guess, and costs the picker nothing but its T3 rows.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};

use super::{Snapshot, T3Thread};

/// T3's threads that are neither deleted nor archived, each with its latest run. Subagent and
/// side-chat threads belong to their parent, as in T3's own sidebar.
const THREADS: &str = "
    SELECT t.thread_id, t.project_id, t.title,
           COALESCE(t.provider_instance_id, t.default_provider),
           json_extract(t.payload_json, '$.settledOverride') IS 'settled',
           json_extract(t.payload_json, '$.snoozedUntil'),
           json_extract(t.payload_json, '$.pinnedAt') IS NOT NULL,
           json_extract(t.payload_json, '$.lastVisitedAt'),
           t.updated_at, r.status, r.completed_at
    FROM orchestration_v2_projection_threads t
    LEFT JOIN orchestration_v2_projection_runs r
      ON r.thread_id = t.thread_id
     AND r.ordinal = (SELECT MAX(ordinal) FROM orchestration_v2_projection_runs
                      WHERE thread_id = t.thread_id)
    WHERE t.deleted_at IS NULL AND t.archived_at IS NULL
      AND json_extract(t.payload_json, '$.lineage.relationshipToParent') IS NOT 'subagent'
      AND COALESCE(json_extract(t.payload_json, '$.lineage.sideChat'), 0) = 0";

const PROJECTS: &str = "SELECT project_id, title FROM projection_projects WHERE deleted_at IS NULL";

const FINISHED: [&str; 5] = [
    "completed",
    "interrupted",
    "failed",
    "cancelled",
    "rolled_back",
];

pub struct Reader {
    home: PathBuf,
}

impl Reader {
    /// The T3 install in `$T3CODE_HOME`, else `~/.t3`.
    pub fn from_env() -> Option<Self> {
        let set = |name| std::env::var_os(name).filter(|value| !value.is_empty());
        set("T3CODE_HOME")
            .map(PathBuf::from)
            .or_else(|| set("HOME").map(|home| PathBuf::from(home).join(".t3")))
            .map(Self::new)
    }

    pub fn new(home: PathBuf) -> Self {
        Self { home }
    }

    /// What T3 has now, or `None` when T3 is not installed here.
    pub fn read(&self, now: i64) -> io::Result<Option<Snapshot>> {
        let userdata = self.home.join("userdata");
        let path = userdata.join("statev2.sqlite");
        if !path.is_file() {
            return Ok(None);
        }
        let environment_id = fs::read_to_string(userdata.join("environment-id"))?
            .trim()
            .to_owned();
        let db = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(io::Error::other)?;
        db.busy_timeout(Duration::from_millis(250))
            .map_err(io::Error::other)?;
        let projects = db
            .prepare(PROJECTS)
            .and_then(|mut query| {
                query
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect::<rusqlite::Result<BTreeMap<String, String>>>()
            })
            .map_err(io::Error::other)?;
        let rows = db
            .prepare(THREADS)
            .and_then(|mut query| {
                query
                    .query_map([], |row| {
                        Ok(Listed {
                            id: row.get(0)?,
                            project_id: row.get(1)?,
                            title: row.get(2)?,
                            provider: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                            settled: row.get(4)?,
                            snoozed_until: row.get(5)?,
                            pinned: row.get(6)?,
                            last_visited_at: row.get(7)?,
                            updated_at: row.get(8)?,
                            status: row.get(9)?,
                            completed_at: row.get(10)?,
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<Listed>>>()
            })
            .map_err(io::Error::other)?;
        let threads = rows
            .into_iter()
            .map(|listed| listed.thread(now))
            .collect::<io::Result<_>>()?;
        Ok(Some(Snapshot {
            environment_id,
            projects,
            threads,
        }))
    }
}

struct Listed {
    id: String,
    project_id: String,
    title: String,
    provider: String,
    settled: bool,
    snoozed_until: Option<String>,
    pinned: bool,
    last_visited_at: Option<String>,
    updated_at: String,
    status: Option<String>,
    completed_at: Option<String>,
}

impl Listed {
    fn thread(self, now: i64) -> io::Result<T3Thread> {
        let updated_at = timestamp(&self.updated_at)?;
        let status = self.status.unwrap_or_else(|| "idle".into());
        let unseen = if FINISHED.contains(&status.as_str()) {
            let finished = self
                .completed_at
                .as_deref()
                .map_or(Ok(updated_at), timestamp)?;
            let visited = self.last_visited_at.as_deref().map(timestamp).transpose()?;
            visited.is_none_or(|visited| visited < finished)
        } else {
            false
        };
        let snoozed_until = self
            .snoozed_until
            .as_deref()
            .map(timestamp)
            .transpose()?
            .filter(|&until| until > now);
        Ok(T3Thread {
            id: self.id,
            project_id: self.project_id,
            title: self.title,
            status,
            provider: self.provider,
            settled: self.settled,
            snoozed_until,
            pinned: self.pinned,
            unseen,
            updated_at,
        })
    }
}

fn timestamp(at: &str) -> io::Result<i64> {
    chrono::DateTime::parse_from_rfc3339(at)
        .map(|at| at.timestamp())
        .map_err(|error| io::Error::other(format!("T3 time {at}: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;
    use std::fs;
    use std::time::Duration;

    use rusqlite::{Connection, params};
    use serde_json::{Value, json};

    use crate::t3::T3Thread;

    const ENV: &str = "env-1";
    /// 2026-11-15T00:00:00Z
    const NOW: i64 = 1_794_700_800;

    /// The tables and columns of T3 Code 0.1.7 that the reader uses, as T3 creates them.
    const SCHEMA: &str = "
        CREATE TABLE orchestration_v2_projection_threads (
          thread_id TEXT PRIMARY KEY, project_id TEXT NOT NULL, title TEXT NOT NULL,
          default_provider TEXT NOT NULL, runtime_mode TEXT NOT NULL,
          interaction_mode TEXT NOT NULL, active_provider_thread_id TEXT,
          created_at TEXT NOT NULL, updated_at TEXT NOT NULL, archived_at TEXT,
          deleted_at TEXT, payload_json TEXT NOT NULL, provider_instance_id TEXT);
        CREATE TABLE orchestration_v2_projection_runs (
          run_id TEXT PRIMARY KEY, thread_id TEXT NOT NULL, ordinal INTEGER NOT NULL,
          provider TEXT NOT NULL, provider_thread_id TEXT, status TEXT NOT NULL,
          requested_at TEXT NOT NULL, completed_at TEXT, payload_json TEXT NOT NULL,
          provider_instance_id TEXT);
        CREATE TABLE projection_projects (
          project_id TEXT PRIMARY KEY, title TEXT NOT NULL, workspace_root TEXT NOT NULL,
          scripts_json TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
          deleted_at TEXT);
    ";

    struct FakeT3 {
        dir: tempfile::TempDir,
        db: Connection,
    }

    impl FakeT3 {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let userdata = dir.path().join("userdata");
            fs::create_dir_all(&userdata).unwrap();
            fs::write(userdata.join("environment-id"), format!("{ENV}\n")).unwrap();
            let db = Connection::open(userdata.join("statev2.sqlite")).unwrap();
            db.execute_batch(SCHEMA).unwrap();
            db.execute(
                "INSERT INTO projection_projects VALUES ('proj-1', 'bball-lab', '/r', '[]', '', '', NULL),
                    ('proj-gone', 'gone', '/g', '[]', '', '', '2026-11-01T00:00:00.000Z')",
                [],
            )
            .unwrap();
            Self { dir, db }
        }

        fn reader(&self) -> Reader {
            Reader::new(self.dir.path().to_path_buf())
        }

        fn thread(&self, id: &str, updated_at: &str, payload: &Value) {
            let mut full = json!({"id": id, "lineage": {"relationshipToParent": null}});
            for (key, value) in payload.as_object().unwrap() {
                full[key] = value.clone();
            }
            self.db
                .execute(
                    "INSERT INTO orchestration_v2_projection_threads VALUES
                     (?1, 'proj-1', ?2, 'codex', 'auto', 'default', NULL, '', ?3, NULL, NULL, ?4, 'codex')",
                    params![id, format!("title {id}"), updated_at, full.to_string()],
                )
                .unwrap();
        }

        fn run(&self, thread: &str, ordinal: i64, status: &str, completed_at: Option<&str>) {
            self.db
                .execute(
                    "INSERT INTO orchestration_v2_projection_runs VALUES
                     (?1, ?2, ?3, 'codex', NULL, ?4, '', ?5, '{}', 'codex')",
                    params![
                        format!("{thread}-{ordinal}"),
                        thread,
                        ordinal,
                        status,
                        completed_at
                    ],
                )
                .unwrap();
        }
    }

    fn at(rfc3339: &str) -> i64 {
        chrono::DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .timestamp()
    }

    fn expected(id: &str, status: &str, updated_at: &str) -> T3Thread {
        T3Thread {
            id: id.into(),
            project_id: "proj-1".into(),
            title: format!("title {id}"),
            status: status.into(),
            provider: "codex".into(),
            settled: false,
            snoozed_until: None,
            pinned: false,
            unseen: false,
            updated_at: at(updated_at),
        }
    }

    #[test]
    fn reads_each_threads_latest_run_and_its_triage() {
        let t3 = FakeT3::new();
        let updated = "2026-11-14T20:00:00.000Z";
        t3.thread("working", updated, &json!({}));
        t3.run("working", 1, "completed", Some("2026-11-14T19:00:00.000Z"));
        t3.run("working", 2, "running", None);
        t3.thread(
            "unseen",
            updated,
            &json!({"lastVisitedAt": "2026-11-14T19:00:00.000Z"}),
        );
        t3.run("unseen", 1, "completed", Some("2026-11-14T19:30:00.000Z"));
        t3.thread(
            "seen",
            updated,
            &json!({"lastVisitedAt": "2026-11-14T19:45:00.000Z", "pinnedAt": updated}),
        );
        t3.run("seen", 1, "failed", Some("2026-11-14T19:30:00.000Z"));
        t3.thread("fresh", updated, &json!({}));
        t3.thread(
            "settled",
            updated,
            &json!({"settledOverride": "settled", "settledAt": updated}),
        );
        t3.thread(
            "snoozed",
            updated,
            &json!({"snoozedUntil": "2026-11-15T09:00:00.000Z"}),
        );
        t3.thread(
            "woke",
            updated,
            &json!({"snoozedUntil": "2026-11-14T23:00:00.000Z"}),
        );

        let snapshot = t3.reader().read(NOW).unwrap().unwrap();
        assert_eq!(snapshot.environment_id, ENV);
        assert_eq!(
            snapshot.projects,
            BTreeMap::from([("proj-1".to_owned(), "bball-lab".to_owned())])
        );
        let mut threads = snapshot.threads;
        threads.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(
            threads,
            [
                expected("fresh", "idle", updated),
                T3Thread {
                    pinned: true,
                    ..expected("seen", "failed", updated)
                },
                T3Thread {
                    settled: true,
                    ..expected("settled", "idle", updated)
                },
                T3Thread {
                    snoozed_until: Some(at("2026-11-15T09:00:00.000Z")),
                    ..expected("snoozed", "idle", updated)
                },
                T3Thread {
                    unseen: true,
                    ..expected("unseen", "completed", updated)
                },
                expected("woke", "idle", updated),
                expected("working", "running", updated),
            ]
        );
    }

    #[test]
    fn leaves_out_subagent_side_chat_archived_and_deleted_threads() {
        let t3 = FakeT3::new();
        let updated = "2026-11-14T20:00:00.000Z";
        t3.thread(
            "kept",
            updated,
            &json!({"lineage": {"relationshipToParent": "fork"}}),
        );
        t3.thread(
            "subagent",
            updated,
            &json!({"lineage": {"relationshipToParent": "subagent"}}),
        );
        t3.thread("side", updated, &json!({"lineage": {"sideChat": true}}));
        t3.thread("archived", updated, &json!({}));
        t3.thread("deleted", updated, &json!({}));
        t3.db
            .execute_batch(
                "UPDATE orchestration_v2_projection_threads SET archived_at = 'x' WHERE thread_id = 'archived';
                 UPDATE orchestration_v2_projection_threads SET deleted_at = 'x' WHERE thread_id = 'deleted';",
            )
            .unwrap();
        let ids: Vec<String> = t3
            .reader()
            .read(NOW)
            .unwrap()
            .unwrap()
            .threads
            .into_iter()
            .map(|thread| thread.id)
            .collect();
        assert_eq!(ids, ["kept"]);
    }

    #[test]
    fn without_t3_installed_there_is_nothing_to_read() {
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(
            Reader::new(empty.path().to_path_buf()).read(NOW).unwrap(),
            None
        );
        assert!(!empty.path().join("userdata/statev2.sqlite").exists());
    }

    #[test]
    fn a_database_t3_has_reshaped_is_an_error_not_a_guess() {
        let t3 = FakeT3::new();
        t3.db
            .execute_batch("DROP TABLE orchestration_v2_projection_runs;")
            .unwrap();
        assert!(t3.reader().read(NOW).is_err());
    }

    #[test]
    fn reading_leaves_t3s_database_untouched() {
        let t3 = FakeT3::new();
        t3.thread("a", "2026-11-14T20:00:00.000Z", &json!({}));
        let path = t3.dir.path().join("userdata/statev2.sqlite");
        let before = fs::read(&path).unwrap();
        t3.reader().read(NOW).unwrap().unwrap();
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn the_watcher_sends_changes_only_and_never_a_failed_read() {
        let t3 = FakeT3::new();
        t3.thread("a", "2026-11-14T20:00:00.000Z", &json!({}));
        let updates = crate::t3::watching(Some(t3.reader()), Duration::from_millis(5));
        let first = updates
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert_eq!(first.threads.len(), 1);
        assert!(
            updates.recv_timeout(Duration::from_millis(100)).is_err(),
            "nothing changed"
        );

        t3.db
            .execute_batch("ALTER TABLE orchestration_v2_projection_threads RENAME TO renamed;")
            .unwrap();
        assert!(
            updates.recv_timeout(Duration::from_millis(100)).is_err(),
            "a failed read keeps the last list"
        );

        t3.db
            .execute_batch("ALTER TABLE renamed RENAME TO orchestration_v2_projection_threads;")
            .unwrap();
        t3.thread("b", "2026-11-14T20:00:00.000Z", &json!({}));
        let second = updates
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert_eq!(second.threads.len(), 2);
    }

    #[test]
    fn without_t3_the_watcher_says_so_once_and_with_no_home_it_stays_silent() {
        let empty = tempfile::tempdir().unwrap();
        let updates = crate::t3::watching(
            Some(Reader::new(empty.path().to_path_buf())),
            Duration::from_millis(5),
        );
        assert_eq!(updates.recv_timeout(Duration::from_secs(5)).unwrap(), None);
        assert!(updates.recv_timeout(Duration::from_millis(100)).is_err());

        let updates = crate::t3::watching(None, Duration::from_millis(5));
        assert!(matches!(
            updates.recv_timeout(Duration::from_millis(100)),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
        ));
    }
}
