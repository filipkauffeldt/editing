//! Worker body for the ingest copy job (C2).
//!
//! `edit-ingest` owns the verified copy pipeline and stays free of a
//! dependency on `edit-metadata`; the app composes them here, injecting the
//! EXIF capture-time reader the duplicate check needs. Progress flows back
//! over the same flume channel as every other job, so the dialog updates on
//! the UI thread.

use std::sync::Arc;

use edit_catalog::{Catalog, ClaimedJob, JobEvent, JobEventKind, JobKind};
use edit_ingest::{IngestCopyPayload, run_ingest_copy};

/// Handle one durable job. Returns `Ok(())` for kinds this worker does not
/// own, so the app can chain it with the browse handler.
pub fn ingest_job_handler(
    catalog: Arc<Catalog>,
    job: &ClaimedJob,
    tx: &flume::Sender<JobEvent>,
) -> Result<(), String> {
    match job.kind {
        JobKind::IngestCopy => {
            let payload: IngestCopyPayload = serde_json::from_str(&job.payload_json)
                .map_err(|e| format!("bad ingest payload: {e}"))?;
            let id = job.id;
            let tx_progress = tx.clone();
            run_ingest_copy(
                &catalog,
                &payload,
                |path| edit_metadata::read_photo_metadata(path).taken_at,
                &|done, total| {
                    tx_progress
                        .send(JobEvent {
                            id,
                            kind: JobEventKind::Progress {
                                done: done as u64,
                                total: total as u64,
                            },
                        })
                        .ok();
                },
            )
            .map(|_| ())
            .map_err(|e| format!("ingest failed: {e}"))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use edit_catalog::JobId;
    use edit_catalog::models::NewIngestBatch;
    use std::path::PathBuf;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "edit-app-ingest-{}-{}-{name}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn ingest_job_handler_lands_a_card_and_reports_progress() {
        let base = scratch("handler");
        let card = base.join("card/DCIM/100CANON");
        std::fs::create_dir_all(&card).unwrap();
        std::fs::write(card.join("DSC0001.JPG"), b"one").unwrap();
        std::fs::write(card.join("DSC0002.NEF"), b"two").unwrap();
        let dest = base.join("Photos");

        let catalog = Arc::new(Catalog::open(&base.join("catalog.sqlite")).unwrap());
        let batch = catalog
            .create_ingest_batch(&NewIngestBatch {
                volume_uuid: Some("UUID".into()),
                volume_label: Some("CARD".into()),
                session_name: Some("shoot".into()),
                dest_dir: dest.to_string_lossy().into_owned(),
                backup_dir: None,
                template_id: None,
                state: "running".into(),
            })
            .unwrap();
        let payload = IngestCopyPayload {
            batch_id: batch,
            source_root: base.join("card").to_string_lossy().into_owned(),
            dest_dir_template: dest.join("{yyyy}-{mm}-{dd}").to_string_lossy().into_owned(),
            filename_template: "{session}_{seq:4}".into(),
            backup_dir: None,
            session: "shoot".into(),
            start_seq: 1,
        };
        let job = ClaimedJob {
            id: JobId(1),
            kind: JobKind::IngestCopy,
            payload_json: serde_json::to_string(&payload).unwrap(),
            attempts: 1,
        };
        let (tx, rx) = flume::unbounded();
        ingest_job_handler(Arc::clone(&catalog), &job, &tx).unwrap();

        let events: Vec<JobEventKind> = rx.try_iter().map(|event| event.kind).collect();
        assert!(matches!(
            events.first(),
            Some(JobEventKind::Progress { done: 0, total: 2 })
        ));
        assert!(matches!(
            events.last(),
            Some(JobEventKind::Progress { done: 2, total: 2 })
        ));

        let rows = catalog.list_ingest_files(batch).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| row.state == "cataloged"));
        assert_eq!(catalog.get_ingest_batch(batch).unwrap().state, "done");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn unrelated_kinds_are_ignored() {
        let base = scratch("ignored");
        let catalog = Arc::new(Catalog::open(&base.join("catalog.sqlite")).unwrap());
        let job = ClaimedJob {
            id: JobId(1),
            kind: JobKind::Thumb,
            payload_json: "{}".into(),
            attempts: 1,
        };
        let (tx, _rx) = flume::unbounded();
        assert!(ingest_job_handler(catalog, &job, &tx).is_ok());
        std::fs::remove_dir_all(&base).ok();
    }
}
