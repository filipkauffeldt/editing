//! Worker body for the ingest copy job (C2).
//!
//! `edit-ingest` owns the verified copy pipeline and stays free of a
//! dependency on `edit-metadata`; the app composes them here, injecting the
//! EXIF capture-time reader the duplicate check needs. Progress flows back
//! over the same flume channel as every other job, so the dialog updates on
//! the UI thread.
//!
//! The batch's frozen IPTC template is applied as files land: each landed
//! photo queues one durable `metadata_write` job, committed together with the
//! file's completion (see [`iptc_stamp_job`]). The metadata worker routes raw
//! sides to their XMP sidecar, so a raw original is never rewritten.

use std::sync::Arc;

use edit_catalog::{Catalog, ClaimedJob, JobEvent, JobEventKind, JobKind};
use edit_ingest::{
    FileMetadata, FollowUpJob, IngestCopyPayload, LandedPhoto, run_ingest_copy_with,
};
use edit_metadata::MetadataWritePayload;

/// The single durable-job dispatch for the app: browse, thumbnail, and
/// ingest kinds in one place. Unknown kinds are a no-op.
pub fn handle_job(
    catalog: Arc<Catalog>,
    job: &ClaimedJob,
    tx: &flume::Sender<JobEvent>,
) -> Result<(), String> {
    match job.kind {
        JobKind::CatalogScan | JobKind::Thumb => crate::views::browse_job_handler(catalog, job, tx),
        JobKind::IngestCopy => ingest_job_handler(catalog, job, tx),
        _ => Ok(()),
    }
}

/// The `metadata_write` job that stamps the batch's IPTC snapshot onto one
/// landed photo, or `None` when the batch carries no template.
///
/// The job is idempotent: it only sets tags from the frozen fields, and its
/// `{date}` fallback clock is the batch start (`captured_at`), so a resumed
/// or repeated job writes exactly what the first one did. A RAW+JPEG pair
/// gets one job per side with the pair's shared sequence number.
pub fn iptc_stamp_job(
    payload: &IngestCopyPayload,
    captured_at: i64,
    landed: &LandedPhoto,
) -> FollowUpJob {
    let fields_json = payload.iptc_fields_json.as_ref()?;
    let job = MetadataWritePayload {
        item_id: landed.item_id,
        paths: vec![landed.path.to_string_lossy().into_owned()],
        sequence: landed.sequence,
        captured_at,
        session: Some(payload.session.clone()),
        fields_json: fields_json.clone(),
    };
    Some((JobKind::MetadataWrite, serde_json::to_string(&job).ok()?))
}

/// Handle one `ingest_copy` job. Returns `Ok(())` for other kinds so the
/// dispatcher can route them elsewhere.
pub fn ingest_job_handler(
    catalog: Arc<Catalog>,
    job: &ClaimedJob,
    tx: &flume::Sender<JobEvent>,
) -> Result<(), String> {
    if job.kind != JobKind::IngestCopy {
        return Ok(());
    }
    let payload: IngestCopyPayload =
        serde_json::from_str(&job.payload_json).map_err(|e| format!("bad ingest payload: {e}"))?;
    let id = job.id;
    let tx_progress = tx.clone();
    // The batch start is the stable clock for the whole ingest, resumed or not.
    let captured_at = catalog
        .get_ingest_batch(payload.batch_id)
        .map_err(|e| format!("ingest batch: {e}"))?
        .started_at;
    run_ingest_copy_with(
        &catalog,
        &payload,
        |path| {
            let meta = edit_metadata::read_photo_metadata(path);
            FileMetadata {
                taken_at: meta.taken_at,
                camera: meta.camera,
            }
        },
        &|landed| iptc_stamp_job(&payload, captured_at, landed),
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
            iptc_fields_json: None,
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
        assert!(
            rows.iter()
                .all(|row| row.state == edit_core::IngestFileState::Cataloged)
        );
        assert_eq!(catalog.get_ingest_batch(batch).unwrap().state, "done");
        std::fs::remove_dir_all(&base).ok();
    }

    fn stamp_payloads(catalog: &Catalog) -> Vec<MetadataWritePayload> {
        catalog
            .list_jobs(50)
            .unwrap()
            .into_iter()
            .filter(|job| job.kind == JobKind::MetadataWrite)
            .map(|job| serde_json::from_str(&job.payload_json).unwrap())
            .collect()
    }

    #[test]
    fn the_handler_queues_a_stamp_job_per_landed_photo_when_the_batch_has_a_template() {
        let base = scratch("stamp");
        let card = base.join("card/DCIM/100CANON");
        std::fs::create_dir_all(&card).unwrap();
        std::fs::write(card.join("DSC0001.JPG"), b"one").unwrap();
        std::fs::write(card.join("DSC0001.NEF"), b"one raw").unwrap();
        let dest = base.join("Photos");

        let catalog = Arc::new(Catalog::open(&base.join("catalog.sqlite")).unwrap());
        let batch = catalog
            .create_ingest_batch(&NewIngestBatch {
                volume_uuid: None,
                volume_label: None,
                session_name: Some("Cup final".into()),
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
            session: "Cup final".into(),
            start_seq: 1,
            iptc_fields_json: Some(r#"{"credit":"{session}"}"#.into()),
        };
        let job = ClaimedJob {
            id: JobId(1),
            kind: JobKind::IngestCopy,
            payload_json: serde_json::to_string(&payload).unwrap(),
            attempts: 1,
        };
        let (tx, _rx) = flume::unbounded();
        ingest_job_handler(Arc::clone(&catalog), &job, &tx).unwrap();

        let stamps = stamp_payloads(&catalog);
        assert_eq!(stamps.len(), 2);
        let started_at = catalog.get_ingest_batch(batch).unwrap().started_at;
        for stamp in &stamps {
            assert_eq!(stamp.paths.len(), 1);
            assert!(stamp.paths[0].starts_with(&*dest.to_string_lossy()));
            assert_eq!(stamp.sequence, 1, "the pair shares one sequence");
            assert_eq!(stamp.captured_at, started_at);
            assert_eq!(stamp.session.as_deref(), Some("Cup final"));
            assert_eq!(stamp.fields_json, r#"{"credit":"{session}"}"#);
        }
        assert_eq!(stamps[0].item_id, stamps[1].item_id);

        // Without a template nothing is queued.
        let plain = IngestCopyPayload {
            iptc_fields_json: None,
            ..payload
        };
        assert!(
            iptc_stamp_job(
                &plain,
                0,
                &LandedPhoto {
                    item_id: 1,
                    photo_id: 1,
                    path: "/p/a.JPG".into(),
                    sequence: 1,
                }
            )
            .is_none()
        );
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
