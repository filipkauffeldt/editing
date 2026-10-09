//! Durable job queue: every background task is a row in `jobs`.
//!
//! Workers claim jobs with one atomic `UPDATE ... RETURNING` statement, so N
//! threads never run the same job twice. Progress flows back over a
//! [`flume`] channel that the UI drains; background threads never touch UI
//! state. A crash mid-job is safe: [`Catalog::recover_running`] (called once
//! at startup) returns `running` rows to `queued`, and the next worker picks
//! them up with the attempt count intact.
//!
//! Job kinds (plan section 1.2): `catalog_scan`, `thumb`, `preview`,
//! `develop_full`, `retouch_render`, `export`, `face_detect`,
//! `metadata_write`, `ingest_copy`, `ingest_verify`, `backup_copy`, `upload`.

use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::db::{Catalog, WorkerHandle, now_unix};
use crate::error::{Error, Result};

/// Every durable background task kind. Stored as text in `jobs.kind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum JobKind {
    CatalogScan,
    Thumb,
    Preview,
    DevelopFull,
    RetouchRender,
    Export,
    FaceDetect,
    MetadataWrite,
    IngestCopy,
    IngestVerify,
    BackupCopy,
    Upload,
    /// The persistent downscaled editing copy of a raw.
    SmartPreview,
}

impl JobKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CatalogScan => "catalog_scan",
            Self::Thumb => "thumb",
            Self::Preview => "preview",
            Self::DevelopFull => "develop_full",
            Self::RetouchRender => "retouch_render",
            Self::Export => "export",
            Self::FaceDetect => "face_detect",
            Self::MetadataWrite => "metadata_write",
            Self::IngestCopy => "ingest_copy",
            Self::IngestVerify => "ingest_verify",
            Self::BackupCopy => "backup_copy",
            Self::Upload => "upload",
            Self::SmartPreview => "smart_preview",
        }
    }
}

impl FromStr for JobKind {
    type Err = Error;

    fn from_str(s: &str) -> std::result::Result<Self, Error> {
        match s {
            "catalog_scan" => Ok(Self::CatalogScan),
            "thumb" => Ok(Self::Thumb),
            "preview" => Ok(Self::Preview),
            "develop_full" => Ok(Self::DevelopFull),
            "retouch_render" => Ok(Self::RetouchRender),
            "export" => Ok(Self::Export),
            "face_detect" => Ok(Self::FaceDetect),
            "metadata_write" => Ok(Self::MetadataWrite),
            "ingest_copy" => Ok(Self::IngestCopy),
            "ingest_verify" => Ok(Self::IngestVerify),
            "backup_copy" => Ok(Self::BackupCopy),
            "upload" => Ok(Self::Upload),
            "smart_preview" => Ok(Self::SmartPreview),
            unknown => Err(Error::InvalidValue(format!("unknown job kind: {unknown}"))),
        }
    }
}

impl JobKind {
    pub fn all() -> &'static [JobKind] {
        &[
            Self::CatalogScan,
            Self::Thumb,
            Self::Preview,
            Self::DevelopFull,
            Self::RetouchRender,
            Self::Export,
            Self::FaceDetect,
            Self::MetadataWrite,
            Self::IngestCopy,
            Self::IngestVerify,
            Self::BackupCopy,
            Self::Upload,
            Self::SmartPreview,
        ]
    }
}

/// Lifecycle state of a job row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobState {
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
}

impl JobState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

impl FromStr for JobState {
    type Err = Error;

    fn from_str(s: &str) -> std::result::Result<Self, Error> {
        match s {
            "queued" => Ok(Self::Queued),
            "running" => Ok(Self::Running),
            "done" => Ok(Self::Done),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            unknown => Err(Error::InvalidValue(format!("unknown job state: {unknown}"))),
        }
    }
}

/// Opaque handle for a row in `jobs`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct JobId(pub i64);

/// A job row as observed by the queue owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Job {
    pub id: JobId,
    pub kind: JobKind,
    pub payload_json: String,
    pub state: JobState,
    pub attempts: i64,
    pub error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// A job owned by one worker. The worker must finish it with
/// [`Catalog::complete_job`] or [`Catalog::fail_job`]; if the process dies
/// first, the next startup's [`Catalog::recover_running`] re-queues it.
#[derive(Clone, Debug)]
pub struct ClaimedJob {
    pub id: JobId,
    pub kind: JobKind,
    pub payload_json: String,
    pub attempts: i64,
}

fn row_job(row: &Row) -> rusqlite::Result<Job> {
    let kind_str: String = row.get(1)?;
    let state_str: String = row.get(3)?;
    let kind = JobKind::from_str(&kind_str).map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            1,
            rusqlite::types::Type::Text,
            kind_str.clone().into(),
        )
    })?;
    let state = JobState::from_str(&state_str).map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            3,
            rusqlite::types::Type::Text,
            state_str.clone().into(),
        )
    })?;
    Ok(Job {
        id: JobId(row.get(0)?),
        kind,
        payload_json: row.get(2)?,
        state,
        attempts: row.get(4)?,
        error: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

// ---- free functions over one connection (shared by Catalog + workers) ----

pub(crate) fn enqueue(conn: &Connection, kind: JobKind, payload_json: &str) -> Result<JobId> {
    let now = now_unix();
    conn.execute(
        "INSERT INTO jobs (kind, payload_json, state, created_at, updated_at)
         VALUES (?1, ?2, 'queued', ?3, ?3)",
        params![kind.as_str(), payload_json, now],
    )?;
    Ok(JobId(conn.last_insert_rowid()))
}

/// Atomically claim the oldest queued job. One statement, so concurrent
/// workers serialize inside SQLite and never receive the same row.
fn claim(conn: &Connection) -> Result<Option<ClaimedJob>> {
    let job = conn
        .query_row(
            "UPDATE jobs SET state = 'running', attempts = attempts + 1, updated_at = ?1
             WHERE id = (SELECT id FROM jobs WHERE state = 'queued' ORDER BY id LIMIT 1)
             RETURNING id, kind, payload_json, attempts",
            [now_unix()],
            |row| {
                let kind_str: String = row.get(1)?;
                let kind = JobKind::from_str(&kind_str).map_err(|_| {
                    rusqlite::Error::FromSqlConversionFailure(
                        1,
                        rusqlite::types::Type::Text,
                        kind_str.clone().into(),
                    )
                })?;
                Ok(ClaimedJob {
                    id: JobId(row.get(0)?),
                    kind,
                    payload_json: row.get(2)?,
                    attempts: row.get(3)?,
                })
            },
        )
        .optional()?;
    Ok(job)
}

fn finish(conn: &Connection, id: JobId, state: JobState, error: Option<&str>) -> Result<()> {
    let n = conn.execute(
        "UPDATE jobs SET state = ?1, error = ?2, updated_at = ?3
         WHERE id = ?4 AND state = 'running'",
        params![state.as_str(), error, now_unix(), id.0],
    )?;
    if n == 0 {
        return Err(Error::NotFound(format!("running job {}", id.0)));
    }
    Ok(())
}

impl Catalog {
    /// Durably enqueue a job. Returns its id.
    pub fn enqueue_job(&self, kind: JobKind, payload_json: &str) -> Result<JobId> {
        self.with_conn(|conn| enqueue(conn, kind, payload_json))
    }

    /// Enqueue unless an identical job (same kind and payload) is already
    /// queued or running, so repeated requests for the same work collapse
    /// into one. `None` when one was already pending.
    pub fn enqueue_job_if_absent(
        &self,
        kind: JobKind,
        payload_json: &str,
    ) -> Result<Option<JobId>> {
        self.with_conn(|conn| {
            let pending: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM jobs WHERE kind = ?1 AND payload_json = ?2
                               AND state IN ('queued', 'running'))",
                params![kind.as_str(), payload_json],
                |row| row.get(0),
            )?;
            if pending {
                return Ok(None);
            }
            enqueue(conn, kind, payload_json).map(Some)
        })
    }

    /// Queued jobs of the given kinds.
    pub fn count_queued_jobs(&self, kinds: &[JobKind]) -> Result<i64> {
        self.with_conn(|conn| {
            let mut total = 0;
            for kind in kinds {
                total += conn.query_row(
                    "SELECT COUNT(*) FROM jobs WHERE state = 'queued' AND kind = ?1",
                    [kind.as_str()],
                    |row| row.get::<_, i64>(0),
                )?;
            }
            Ok(total)
        })
    }

    /// Claim the oldest queued job (see [`claim`]).
    pub fn claim_job(&self) -> Result<Option<ClaimedJob>> {
        self.with_conn(claim)
    }

    /// Mark a claimed job done.
    pub fn complete_job(&self, id: JobId) -> Result<()> {
        self.with_conn(|conn| finish(conn, id, JobState::Done, None))
    }

    /// Mark a claimed job failed with a diagnostic.
    pub fn fail_job(&self, id: JobId, error: &str) -> Result<()> {
        self.with_conn(|conn| finish(conn, id, JobState::Failed, Some(error)))
    }

    /// Cancel a queued or running job. Finished jobs stay finished.
    pub fn cancel_job(&self, id: JobId) -> Result<()> {
        self.with_conn(|conn| {
            let n = conn.execute(
                "UPDATE jobs SET state = 'cancelled', updated_at = ?1
                 WHERE id = ?2 AND state IN ('queued', 'running')",
                params![now_unix(), id.0],
            )?;
            if n == 0 {
                return Err(Error::NotFound(format!("cancellable job {}", id.0)));
            }
            Ok(())
        })
    }

    /// Read one job row.
    pub fn job(&self, id: JobId) -> Result<Job> {
        self.with_conn(|conn| {
            conn.query_row("SELECT id, kind, payload_json, state, attempts, error, created_at, updated_at FROM jobs WHERE id = ?1", [id.0], row_job)
                .optional()?
                .ok_or_else(|| Error::NotFound(format!("job {}", id.0)))
        })
    }

    /// Count jobs in a state.
    pub fn count_jobs_by_state(&self, state: JobState) -> Result<i64> {
        self.with_conn(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM jobs WHERE state = ?1",
                [state.as_str()],
                |row| row.get(0),
            )?)
        })
    }

    /// Queued or running `ingest_copy` jobs. The app rebuilds its card/job
    /// routing from these on startup, so a crash-resumed ingest is visible.
    pub fn list_active_ingest_jobs(&self) -> Result<Vec<Job>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, kind, payload_json, state, attempts, error, created_at, updated_at
                   FROM jobs WHERE kind = 'ingest_copy' AND state IN ('queued', 'running')
                   ORDER BY id",
            )?;
            Ok(stmt
                .query_map([], row_job)?
                .collect::<rusqlite::Result<_>>()?)
        })
    }

    /// Newest jobs first, at most `limit`.
    pub fn list_jobs(&self, limit: i64) -> Result<Vec<Job>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare("SELECT id, kind, payload_json, state, attempts, error, created_at, updated_at FROM jobs ORDER BY id DESC LIMIT ?1")?;
            Ok(stmt
                .query_map([limit], row_job)?
                .collect::<rusqlite::Result<_>>()?)
        })
    }
}

impl WorkerHandle {
    pub fn claim_job(&self) -> Result<Option<ClaimedJob>> {
        claim(self.conn())
    }

    pub fn complete_job(&self, id: JobId) -> Result<()> {
        finish(self.conn(), id, JobState::Done, None)
    }

    pub fn fail_job(&self, id: JobId, error: &str) -> Result<()> {
        finish(self.conn(), id, JobState::Failed, Some(error))
    }
}

// ---- workers ----

/// A lifecycle event for one job, sent to the UI drain loop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobEvent {
    pub id: JobId,
    pub kind: JobEventKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JobEventKind {
    Started { attempts: i64 },
    Progress { done: u64, total: u64 },
    Succeeded,
    Failed { error: String },
}

/// Job body. Returns `Ok` on success or an error diagnostic on failure.
/// Sends [`JobEventKind::Progress`] on the channel as it goes; the terminal
/// event is sent by the worker loop itself.
pub type JobHandler = Arc<
    dyn Fn(&ClaimedJob, &flume::Sender<JobEvent>) -> std::result::Result<(), String> + Send + Sync,
>;

/// N worker threads draining the queue. Every thread holds its own
/// connection; claims stay atomic through the single-statement `UPDATE`.
pub struct WorkerPool {
    shutdown: Arc<AtomicBool>,
    handles: Vec<JoinHandle<()>>,
    events: flume::Receiver<JobEvent>,
}

impl WorkerPool {
    /// Drain loop endpoint for the UI thread.
    pub fn events(&self) -> &flume::Receiver<JobEvent> {
        &self.events
    }

    /// Signal shutdown and join every worker.
    pub fn shutdown(self) {
        self.shutdown.store(true, Ordering::SeqCst);
        for handle in self.handles {
            handle.join().ok();
        }
    }
}

/// Spawn `count` worker threads over an open catalog.
///
/// Migrations run once on the main handle before any thread starts, so
/// worker connections never race on the schema. Every connection opens
/// before the first thread spawns, so a failure returns here instead of
/// leaking detached spinning workers.
pub fn spawn_workers(catalog: &Catalog, count: usize, handler: JobHandler) -> Result<WorkerPool> {
    catalog.ensure_migrated()?;
    let mut workers = Vec::with_capacity(count);
    for _ in 0..count {
        workers.push(catalog.open_worker_conn()?);
    }
    let (tx, rx) = flume::unbounded();
    let shutdown = Arc::new(AtomicBool::new(false));
    let mut handles = Vec::with_capacity(count);
    for worker in workers {
        let tx = tx.clone();
        let handler = Arc::clone(&handler);
        let shutdown = Arc::clone(&shutdown);
        handles.push(thread::spawn(move || {
            worker_loop(&worker, &handler, &tx, &shutdown);
        }));
    }
    Ok(WorkerPool {
        shutdown,
        handles,
        events: rx,
    })
}

fn worker_loop(
    worker: &WorkerHandle,
    handler: &JobHandler,
    tx: &flume::Sender<JobEvent>,
    shutdown: &AtomicBool,
) {
    while !shutdown.load(Ordering::SeqCst) {
        match worker.claim_job() {
            Ok(Some(job)) => {
                let id = job.id;
                send(
                    tx,
                    JobEvent {
                        id,
                        kind: JobEventKind::Started {
                            attempts: job.attempts,
                        },
                    },
                );
                match handler(&job, tx) {
                    Ok(()) => {
                        // Only report success when the row actually
                        // transitioned: a job cancelled mid-run stays
                        // `cancelled` instead of flashing a done badge.
                        if worker.complete_job(id).is_ok() {
                            send(
                                tx,
                                JobEvent {
                                    id,
                                    kind: JobEventKind::Succeeded,
                                },
                            );
                        }
                    }
                    Err(error) => {
                        if worker.fail_job(id, &error).is_ok() {
                            send(
                                tx,
                                JobEvent {
                                    id,
                                    kind: JobEventKind::Failed { error },
                                },
                            );
                        }
                    }
                }
            }
            Ok(None) => thread::sleep(Duration::from_millis(5)),
            // Transient SQLite contention or a wedged disk: back off instead
            // of hot-spinning. The job rows stay queued, so nothing is lost.
            Err(_) => thread::sleep(Duration::from_millis(50)),
        }
    }
}

fn send(tx: &flume::Sender<JobEvent>, event: JobEvent) {
    // A gone receiver means the UI went away; the DB result is already
    // durable, so dropping the event is correct.
    tx.send(event).ok();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::tests::scratch_db;
    use std::collections::{HashMap, HashSet};
    use std::sync::Mutex;
    use std::time::Instant;

    fn wait_for(timeout: Duration, mut cond: impl FnMut() -> bool) {
        let start = Instant::now();
        while !cond() {
            assert!(start.elapsed() < timeout, "timed out waiting for condition");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn drain_events(rx: &flume::Receiver<JobEvent>) -> Vec<JobEvent> {
        let mut out = Vec::new();
        while let Ok(event) = rx.try_recv() {
            out.push(event);
        }
        out
    }

    #[test]
    fn job_kinds_round_trip() {
        assert_eq!(JobKind::all().len(), 13);
        for kind in JobKind::all() {
            assert_eq!(kind.as_str().parse::<JobKind>().unwrap(), *kind);
        }
        assert!("nope".parse::<JobKind>().is_err());
        assert!("nope".parse::<JobState>().is_err());
    }

    #[test]
    fn identical_pending_smart_preview_jobs_collapse_and_resume_after_a_crash() {
        let path = scratch_db("smart-idem");
        let db = Catalog::open(&path).unwrap();
        let payload = r#"{"path":"/p/a.nef"}"#;
        let first = db
            .enqueue_job_if_absent(JobKind::SmartPreview, payload)
            .unwrap();
        assert!(first.is_some());
        assert!(
            db.enqueue_job_if_absent(JobKind::SmartPreview, payload)
                .unwrap()
                .is_none()
        );
        // Different work is not collapsed.
        assert!(
            db.enqueue_job_if_absent(JobKind::SmartPreview, r#"{"path":"/p/b.nef"}"#)
                .unwrap()
                .is_some()
        );
        assert_eq!(db.count_queued_jobs(&[JobKind::SmartPreview]).unwrap(), 2);
        assert_eq!(db.count_queued_jobs(&[JobKind::Thumb]).unwrap(), 0);
        // A claimed job still counts as pending; a crash puts it back in the queue.
        let claimed = db.claim_job().unwrap().unwrap();
        assert_eq!(Some(claimed.id), first);
        assert!(
            db.enqueue_job_if_absent(JobKind::SmartPreview, payload)
                .unwrap()
                .is_none()
        );
        assert_eq!(db.recover_running().unwrap(), 1);
        assert_eq!(db.claim_job().unwrap().unwrap().id, claimed.id);
        // Once done, the same work may be requested again.
        db.complete_job(claimed.id).unwrap();
        assert!(
            db.enqueue_job_if_absent(JobKind::SmartPreview, payload)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn queued_job_runs_to_done() {
        let path = scratch_db("job-done");
        let db = Catalog::open(&path).unwrap();
        let id = db.enqueue_job(JobKind::Thumb, r#"{"photo":1}"#).unwrap();
        assert_eq!(db.job(id).unwrap().state, JobState::Queued);

        let handler: JobHandler = Arc::new(|job, _| {
            assert_eq!(job.payload_json, r#"{"photo":1}"#);
            Ok(())
        });
        let pool = spawn_workers(&db, 1, handler).unwrap();
        wait_for(Duration::from_secs(10), || {
            db.count_jobs_by_state(JobState::Done).unwrap_or(0) == 1
        });

        let job = db.job(id).unwrap();
        assert_eq!(job.state, JobState::Done);
        assert_eq!(job.attempts, 1);

        let events = drain_events(pool.events());
        assert!(
            events
                .iter()
                .any(|e| e.id == id && matches!(e.kind, JobEventKind::Started { attempts: 1 }))
        );
        assert!(
            events
                .iter()
                .any(|e| e.id == id && e.kind == JobEventKind::Succeeded)
        );
        pool.shutdown();
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn failed_job_records_error() {
        let path = scratch_db("job-failed");
        let db = Catalog::open(&path).unwrap();
        let id = db.enqueue_job(JobKind::Export, "{}").unwrap();

        let handler: JobHandler = Arc::new(|_, _| Err("disk full".to_string()));
        let pool = spawn_workers(&db, 1, handler).unwrap();
        wait_for(Duration::from_secs(10), || {
            db.count_jobs_by_state(JobState::Failed).unwrap_or(0) == 1
        });

        let job = db.job(id).unwrap();
        assert_eq!(job.state, JobState::Failed);
        assert_eq!(job.error.as_deref(), Some("disk full"));
        pool.shutdown();
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn running_job_reverts_to_queued_on_recover() {
        let path = scratch_db("job-resume");
        let id = {
            let db = Catalog::open(&path).unwrap();
            let id = db.enqueue_job(JobKind::Upload, r#"{"to":"ftp"}"#).unwrap();
            let claimed = db.claim_job().unwrap().expect("job to claim");
            assert_eq!(claimed.id, id);
            assert_eq!(db.job(id).unwrap().state, JobState::Running);
            id
        };
        // `db` is dropped here without finishing the job: the crash.

        let db = Catalog::open(&path).unwrap();
        // A plain open never touches in-flight jobs: only the explicit
        // startup recovery does.
        assert_eq!(db.job(id).unwrap().state, JobState::Running);
        assert_eq!(db.recover_running().unwrap(), 1);
        let job = db.job(id).unwrap();
        assert_eq!(job.state, JobState::Queued);
        // The attempt is kept, so retries can back off or give up.
        assert_eq!(job.attempts, 1);
        // Recovering again is a no-op.
        assert_eq!(db.recover_running().unwrap(), 0);

        // And the job actually runs after recovery.
        let handler: JobHandler = Arc::new(|_, _| Ok(()));
        let pool = spawn_workers(&db, 1, handler).unwrap();
        wait_for(Duration::from_secs(10), || {
            db.count_jobs_by_state(JobState::Done).unwrap_or(0) == 1
        });
        assert_eq!(db.job(id).unwrap().attempts, 2);
        pool.shutdown();
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn concurrent_claim_never_double_runs_a_job() {
        let path = scratch_db("job-race");
        let db = Catalog::open(&path).unwrap();
        const JOBS: usize = 20;
        for n in 0..JOBS {
            db.enqueue_job(JobKind::Preview, &format!(r#"{{"n":{n}}}"#))
                .unwrap();
        }

        let runs: Arc<Mutex<HashMap<i64, usize>>> = Arc::new(Mutex::new(HashMap::new()));
        let handler_runs = Arc::clone(&runs);
        let handler: JobHandler = Arc::new(move |job, _| {
            // Simulate real work so claims overlap.
            thread::sleep(Duration::from_millis(10));
            *handler_runs.lock().unwrap().entry(job.id.0).or_default() += 1;
            Ok(())
        });
        let pool = spawn_workers(&db, 4, handler).unwrap();
        wait_for(Duration::from_secs(30), || {
            db.count_jobs_by_state(JobState::Done).unwrap_or(0) == JOBS as i64
        });

        let runs = runs.lock().unwrap();
        assert_eq!(runs.len(), JOBS, "every job ran");
        assert!(runs.values().all(|&n| n == 1), "no job ran twice: {runs:?}");
        // Cross-check against distinct finished ids in the database.
        let ids: HashSet<i64> = db
            .list_jobs(JOBS as i64 * 2)
            .unwrap()
            .into_iter()
            .map(|j| j.id.0)
            .collect();
        assert_eq!(ids.len(), JOBS);
        pool.shutdown();
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn cancelled_running_job_reports_no_terminal_event() {
        let path = scratch_db("job-cancel");
        let db = Catalog::open(&path).unwrap();
        let id = db.enqueue_job(JobKind::Export, "{}").unwrap();

        // Slow handler: still inside the job body when the cancel lands.
        let handler: JobHandler = Arc::new(|_, _| {
            thread::sleep(Duration::from_millis(200));
            Ok(())
        });
        let pool = spawn_workers(&db, 1, handler).unwrap();
        wait_for(Duration::from_secs(10), || {
            db.job(id).unwrap().state == JobState::Running
        });
        db.cancel_job(id).unwrap();
        // Let the worker finish the body and attempt the completion.
        thread::sleep(Duration::from_millis(500));

        // The cancel wins: no Succeeded event, no silent flip to done.
        assert_eq!(db.job(id).unwrap().state, JobState::Cancelled);
        let events = drain_events(pool.events());
        assert!(
            events
                .iter()
                .any(|e| e.id == id && matches!(e.kind, JobEventKind::Started { .. }))
        );
        assert!(
            !events.iter().any(|e| e.id == id
                && matches!(
                    e.kind,
                    JobEventKind::Succeeded | JobEventKind::Failed { .. }
                )),
            "cancelled job must not report a terminal event: {events:?}"
        );
        pool.shutdown();
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn handler_progress_events_reach_the_drain() {
        let path = scratch_db("job-progress");
        let db = Catalog::open(&path).unwrap();
        let id = db.enqueue_job(JobKind::Thumb, "{}").unwrap();

        // Handler contract: Started/Succeeded come from the worker loop;
        // Progress comes from the handler itself on the same channel.
        let handler: JobHandler = Arc::new(|job, tx| {
            for done in 1..=2 {
                tx.send(JobEvent {
                    id: job.id,
                    kind: JobEventKind::Progress { done, total: 2 },
                })
                .unwrap();
            }
            Ok(())
        });
        let pool = spawn_workers(&db, 1, handler).unwrap();
        // Started + 2x Progress + Succeeded, in order on one channel.
        wait_for(Duration::from_secs(10), || pool.events().len() >= 4);

        let kinds: Vec<JobEventKind> = drain_events(pool.events())
            .into_iter()
            .filter(|e| e.id == id)
            .map(|e| e.kind)
            .collect();
        pool.shutdown();
        assert_eq!(
            kinds,
            vec![
                JobEventKind::Started { attempts: 1 },
                JobEventKind::Progress { done: 1, total: 2 },
                JobEventKind::Progress { done: 2, total: 2 },
                JobEventKind::Succeeded,
            ]
        );
        std::fs::remove_file(path).ok();
    }
}
