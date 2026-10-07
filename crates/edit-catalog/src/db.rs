//! Catalog open, numbered migrations, crash recovery.
//!
//! [`Catalog::open`] applies pending migrations and nothing else. Crash
//! recovery is an explicit second step: call [`Catalog::recover_running`]
//! once at startup to return interrupted jobs to `queued` (see
//! [`crate::jobs`]). Keeping recovery out of the constructor means a second
//! `open` mid-run can never re-queue jobs another handle is running.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Connection;

use crate::error::{Error, Result};

struct Migration {
    version: i32,
    sql: &'static str,
}

const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    sql: include_str!("migrations/001_initial_schema.sql"),
}];

/// Seconds since the Unix epoch, for `*_at` columns.
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The SQLite catalog. One shared connection behind a mutex; worker threads
/// hold their own connections, opened through [`Catalog::open_worker_conn`].
pub struct Catalog {
    path: PathBuf,
    conn: Mutex<Connection>,
}

impl Catalog {
    /// Open (creating) the catalog at `path` and apply pending migrations.
    /// Recovery is deliberately separate: call [`Catalog::recover_running`]
    /// once at startup.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = connect(path)?;
        apply_migrations(&conn)?;
        Ok(Self {
            path: path.to_path_buf(),
            conn: Mutex::new(conn),
        })
    }

    /// Crash recovery: jobs still marked `running` belonged to a dead
    /// process — no worker can be alive to finish them, so they return to
    /// `queued` with their attempt counts intact. Call once at startup, never
    /// while workers are running. Returns the number of recovered jobs.
    pub fn recover_running(&self) -> Result<usize> {
        self.with_conn(recover_running_jobs)
    }

    /// Re-apply pending migrations. Idempotent and cheap once migrated;
    /// `spawn_workers` calls this on the main handle so worker connections
    /// never race on the schema.
    pub(crate) fn ensure_migrated(&self) -> Result<()> {
        self.with_conn(apply_migrations)
    }

    /// Open a worker's private connection over the same file: pragmas, but
    /// no migrations and no recovery. Only `spawn_workers` uses this, after
    /// [`Catalog::ensure_migrated`] ran on the main handle.
    pub(crate) fn open_worker_conn(&self) -> Result<WorkerHandle> {
        Ok(WorkerHandle {
            conn: connect(&self.path)?,
        })
    }

    /// Filesystem path of the catalog database.
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self.conn.lock().map_err(|_| Error::LockPoisoned)?;
        f(&conn)
    }
}

/// A worker's private connection. Claim statements are single atomic
/// `UPDATE ... RETURNING` writes, so N handles race safely.
pub struct WorkerHandle {
    conn: Connection,
}

impl WorkerHandle {
    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }
}

/// Open a connection with the pragmas every handle needs.
fn connect(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| Error::InvalidValue(format!("cannot create catalog dir: {e}")))?;
    }
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    // NORMAL with WAL is process-crash safe (a dead worker loses nothing
    // committed); FULL would fsync every claim for OS-crash durability the
    // queue does not need.
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(conn)
}

/// Apply every migration not yet recorded. Idempotent: opening a migrated
/// database is a no-op.
fn apply_migrations(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
           version INTEGER PRIMARY KEY,
           applied_at INTEGER NOT NULL
         )",
    )?;
    for m in MIGRATIONS {
        let applied: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM schema_migrations WHERE version = ?1)",
            [m.version],
            |row| row.get(0),
        )?;
        if applied {
            continue;
        }
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(m.sql)?;
        tx.execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
            rusqlite::params![m.version, now_unix()],
        )?;
        tx.commit()?;
    }
    Ok(())
}

/// Crash recovery: jobs still marked `running` belonged to a dead process —
/// no worker can be alive to finish them, so they return to `queued` with
/// their attempt count intact. Returns the number of recovered jobs.
fn recover_running_jobs(conn: &Connection) -> Result<usize> {
    let n = conn.execute(
        "UPDATE jobs SET state = 'queued', updated_at = ?1 WHERE state = 'running'",
        [now_unix()],
    )?;
    Ok(n)
}

/// All migration versions applied to this database.
pub fn applied_versions(conn: &Connection) -> Result<Vec<i32>> {
    let mut stmt = conn.prepare("SELECT version FROM schema_migrations ORDER BY version")?;
    let versions = stmt
        .query_map([], |row| row.get(0))?
        .collect::<std::result::Result<Vec<i32>, _>>()?;
    Ok(versions)
}

/// Every user table the schema owns (plus `schema_migrations`).
pub fn expected_tables() -> &'static [&'static str] {
    &[
        "roots",
        "photos",
        "items",
        "edits",
        "selections",
        "masks",
        "retouch_recipes",
        "retouch_versions",
        "detections",
        "jobs",
        "transfers",
        "iptc_templates",
        "send_recipes",
        "code_replacements",
        "ingest_batches",
        "presets",
        "settings",
        "schema_migrations",
    ]
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DB: AtomicU64 = AtomicU64::new(0);

    /// Unique scratch database path per test.
    pub fn scratch_db(name: &str) -> PathBuf {
        let n = NEXT_DB.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "edit-catalog-test-{}-{}-{n}.sqlite",
            std::process::id(),
            name
        ))
    }

    fn table_names(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .unwrap();
        stmt.query_map([], |row| row.get(0))
            .unwrap()
            .collect::<std::result::Result<Vec<String>, _>>()
            .unwrap()
    }

    #[test]
    fn migrations_apply_cleanly_on_empty_db() {
        let path = scratch_db("migrate-empty");
        let catalog = Catalog::open(&path).unwrap();
        catalog
            .with_conn(|conn| {
                let names = table_names(conn);
                for expected in expected_tables() {
                    assert!(names.iter().any(|n| n == expected), "missing {expected}");
                }
                assert_eq!(applied_versions(conn)?, vec![1]);
                Ok(())
            })
            .unwrap();
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn migrations_are_idempotent_on_premigrated_db() {
        let path = scratch_db("migrate-twice");
        {
            let catalog = Catalog::open(&path).unwrap();
            catalog
                .with_conn(|conn| {
                    conn.execute(
                        "INSERT INTO settings (key, value) VALUES ('probe', '1')",
                        [],
                    )?;
                    Ok(())
                })
                .unwrap();
        }
        // Reopen the migrated database: must succeed, keep data, stay at v1.
        let catalog = Catalog::open(&path).unwrap();
        catalog
            .with_conn(|conn| {
                let value: String = conn.query_row(
                    "SELECT value FROM settings WHERE key = 'probe'",
                    [],
                    |row| row.get(0),
                )?;
                assert_eq!(value, "1");
                assert_eq!(applied_versions(conn)?, vec![1]);
                Ok(())
            })
            .unwrap();
        std::fs::remove_file(path).ok();
    }
}
