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

use std::path::Path;
use std::sync::Arc;

use edit_catalog::{Catalog, ClaimedJob, JobEvent, JobEventKind, JobId, JobKind};
use edit_ingest::{
    FileMetadata, FollowUpJob, IngestCopyPayload, IngestPlanRequest, IngestTemplate, LandedPhoto,
    plan_ingest, run_ingest_copy_with,
};
use edit_metadata::{CodeReplacements, MetadataWritePayload, TemplateFields};

/// Catalog setting holding the last loaded code replacement file path (the
/// template editor writes it).
pub const REPLACEMENTS_SETTING: &str = "code_replacements_path";

/// The code replacements the template editor last loaded, if any.
pub fn saved_code_replacements(catalog: &Catalog) -> Option<CodeReplacements> {
    let path = catalog.get_setting(REPLACEMENTS_SETTING).ok().flatten()?;
    CodeReplacements::from_file(Path::new(&path))
        .ok()
        .filter(|codes| !codes.is_empty())
}

/// What the dialog hands over to start one card's ingest.
#[derive(Clone, Copy, Debug)]
pub struct IngestStart<'a> {
    pub volume_uuid: Option<&'a str>,
    pub volume_label: Option<&'a str>,
    pub source_root: &'a Path,
    pub dest_rule: &'a str,
    /// Blank means the default rule.
    pub filename_rule: &'a str,
    pub backup_dir: &'a str,
    pub session: &'a str,
    /// The IPTC fields from the dialog form; frozen onto the batch as shown.
    /// `None` or an all-empty set stamps nothing.
    pub fields: Option<&'a TemplateFields>,
    /// Frozen with the fields; applied when the stamp expands.
    pub replacements: Option<&'a CodeReplacements>,
    /// Copy only these source paths (the tagged files); `None` copies all.
    pub only_sources: Option<&'a [String]>,
}

/// A queued ingest: the batch row and its `ingest_copy` job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StartedIngest {
    pub batch_id: i64,
    pub job_id: JobId,
}

/// Validate the inputs, open the batch, and queue its `ingest_copy` job.
///
/// Every check that can reject the request (naming rules, the template)
/// runs before the batch row exists, so a bad request leaves nothing behind.
/// Errors are user-facing messages.
pub fn start_ingest(catalog: &Catalog, start: &IngestStart<'_>) -> Result<StartedIngest, String> {
    // Fields left empty write nothing, so an empty set is no stamp at all.
    let fields_json = start
        .fields
        .filter(|fields| !fields.is_empty())
        .map(serde_json::to_string)
        .transpose()
        .map_err(|e| format!("Could not encode the IPTC fields: {e}"))?;
    let plan = plan_ingest(&IngestPlanRequest {
        volume_uuid: start.volume_uuid,
        volume_label: start.volume_label,
        source_root: start.source_root,
        dest_dir_template: start.dest_rule,
        filename_template: start.filename_rule,
        backup_dir: Some(start.backup_dir),
        session: start.session,
        start_seq: 1,
        template: fields_json.as_deref().map(|fields_json| IngestTemplate {
            id: None,
            fields_json,
        }),
        only_sources: start.only_sources,
    })
    .map_err(|e| format!("Bad naming rule: {e}"))?;
    let batch_id = catalog
        .create_ingest_batch(&plan.batch)
        .map_err(|e| format!("Could not open batch: {e}"))?;
    let payload = IngestCopyPayload {
        batch_id,
        iptc_replacements_json: start
            .replacements
            .filter(|_| fields_json.is_some())
            .and_then(|codes| serde_json::to_string(codes).ok()),
        ..plan.payload
    };
    let encoded =
        serde_json::to_string(&payload).map_err(|e| format!("Could not encode ingest job: {e}"))?;
    let job_id = catalog
        .enqueue_job(JobKind::IngestCopy, &encoded)
        .map_err(|e| {
            // No job will ever fill this batch; do not leave it `running`.
            let _ = catalog.finish_ingest_batch(batch_id, "failed");
            format!("Could not queue ingest: {e}")
        })?;
    Ok(StartedIngest { batch_id, job_id })
}

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
    replacements: &Option<CodeReplacements>,
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
        replacements: replacements.clone(),
        clear_empty: false,
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
    let replacements: Option<CodeReplacements> = payload
        .iptc_replacements_json
        .as_deref()
        .and_then(|json| serde_json::from_str(json).ok());
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
        &|landed| iptc_stamp_job(&payload, captured_at, &replacements, landed),
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
    .map_err(|e| format!("ingest failed: {e}"))
    .and_then(|summary| {
        // Same rule as the batch row: nothing landed, so the job did not
        // succeed either (the first reason travels in the failure event).
        let landed = summary.copied + summary.recovered + summary.duplicates;
        if summary.failed > 0 && landed == 0 {
            let reason = catalog
                .list_ingest_files(payload.batch_id)
                .ok()
                .and_then(|rows| rows.into_iter().find_map(|row| row.error))
                .unwrap_or_default();
            return Err(format!(
                "none of {} files could be ingested: {reason}",
                summary.failed
            ));
        }
        Ok(())
    })
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
            iptc_replacements_json: None,
            only_sources: None,
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
            iptc_replacements_json: None,
            only_sources: None,
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
                &None,
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

    fn start_request<'a>(card: &'a Path, dest_rule: &'a str, rule: &'a str) -> IngestStart<'a> {
        IngestStart {
            volume_uuid: Some("UUID"),
            volume_label: Some("NIKON Z 6"),
            source_root: card,
            dest_rule,
            filename_rule: rule,
            backup_dir: "",
            session: "NIKON Z 6",
            fields: None,
            replacements: None,
            only_sources: None,
        }
    }

    /// The reported failure: Start with an empty filename rule used to queue
    /// a job whose every file rendered an empty name and failed.
    #[test]
    fn starting_with_an_empty_rule_lands_every_file_under_the_default_name() {
        let base = scratch("empty-rule");
        let card = base.join("card");
        let dcim = card.join("DCIM/100NIKON");
        std::fs::create_dir_all(&dcim).unwrap();
        std::fs::write(dcim.join("DSC_0001.NEF"), b"raw one").unwrap();
        std::fs::write(dcim.join("DSC_0001.JPG"), b"jpg one").unwrap();
        std::fs::write(dcim.join("DSC_0002.NEF"), b"raw two").unwrap();
        let dest = base.join("Photos");
        let dest_rule = dest.to_string_lossy().into_owned();

        let catalog = Arc::new(Catalog::open(&base.join("catalog.sqlite")).unwrap());
        let started = start_ingest(&catalog, &start_request(&card, &dest_rule, "")).unwrap();
        let job = catalog.claim_job().unwrap().unwrap();
        assert_eq!(job.id, started.job_id);
        let (tx, _rx) = flume::unbounded();
        ingest_job_handler(Arc::clone(&catalog), &job, &tx).unwrap();

        let rows = catalog.list_ingest_files(started.batch_id).unwrap();
        assert_eq!(rows.len(), 3);
        assert!(
            rows.iter()
                .all(|row| row.state == edit_core::IngestFileState::Cataloged),
            "{rows:?}"
        );
        let mut names: Vec<String> = std::fs::read_dir(&dest)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["DSC_0001.JPG", "DSC_0001.NEF", "DSC_0002.NEF"]);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn a_bad_request_is_rejected_before_any_batch_or_job_exists() {
        let base = scratch("reject");
        let catalog = Catalog::open(&base.join("catalog.sqlite")).unwrap();
        let card = base.join("card");
        let dest = base.join("Photos").to_string_lossy().into_owned();

        let slash = start_ingest(&catalog, &start_request(&card, &dest, "a/b")).unwrap_err();
        assert!(slash.starts_with("Bad naming rule"), "{slash}");
        let unknown = start_ingest(&catalog, &start_request(&card, &dest, "{nope}")).unwrap_err();
        assert!(unknown.contains("nope"), "{unknown}");

        assert!(catalog.list_jobs(10).unwrap().is_empty());
        assert!(
            catalog
                .list_ingest_batches_by_state("running")
                .unwrap()
                .is_empty()
        );
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn the_typed_fields_are_frozen_onto_the_job_without_a_template_id() {
        let base = scratch("freeze");
        let catalog = Catalog::open(&base.join("catalog.sqlite")).unwrap();
        let card = base.join("card");
        let dest = base.join("Photos").to_string_lossy().into_owned();
        let fields = TemplateFields::default()
            .with("credit", "SHL")
            .with("caption", "{session} {date}");
        let started = start_ingest(
            &catalog,
            &IngestStart {
                fields: Some(&fields),
                ..start_request(&card, &dest, "")
            },
        )
        .unwrap();

        assert_eq!(
            catalog
                .get_ingest_batch(started.batch_id)
                .unwrap()
                .template_id,
            None
        );
        let job = catalog.job(started.job_id).unwrap();
        let payload: IngestCopyPayload = serde_json::from_str(&job.payload_json).unwrap();
        let frozen: TemplateFields =
            serde_json::from_str(payload.iptc_fields_json.as_deref().unwrap()).unwrap();
        assert_eq!(frozen, fields);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn empty_fields_write_nothing_and_blank_values_are_not_a_stamp() {
        let base = scratch("empty-fields");
        let catalog = Catalog::open(&base.join("catalog.sqlite")).unwrap();
        let card = base.join("card");
        let dest = base.join("Photos").to_string_lossy().into_owned();
        for fields in [None, Some(TemplateFields::default())] {
            let started = start_ingest(
                &catalog,
                &IngestStart {
                    fields: fields.as_ref(),
                    ..start_request(&card, &dest, "")
                },
            )
            .unwrap();
            let job = catalog.job(started.job_id).unwrap();
            let payload: IngestCopyPayload = serde_json::from_str(&job.payload_json).unwrap();
            assert_eq!(payload.iptc_fields_json, None);
            assert_eq!(payload.iptc_replacements_json, None);
        }
        std::fs::remove_dir_all(&base).ok();
    }

    /// A batch saved before the dialog had fields: its row references a
    /// template id and its job carries the frozen fields. Resuming it stamps
    /// those fields, even after the template was edited or deleted.
    #[test]
    fn a_saved_batch_that_references_a_template_id_still_resumes_and_stamps() {
        let base = scratch("legacy");
        let card = base.join("card/DCIM/100CANON");
        std::fs::create_dir_all(&card).unwrap();
        std::fs::write(card.join("DSC0001.JPG"), b"one").unwrap();
        let dest = base.join("Photos");
        let catalog = Arc::new(Catalog::open(&base.join("catalog.sqlite")).unwrap());
        let template = catalog
            .insert_iptc_template("press", r#"{"credit":"SHL"}"#, "global")
            .unwrap();
        let batch = catalog
            .create_ingest_batch(&NewIngestBatch {
                volume_uuid: None,
                volume_label: None,
                session_name: Some("Cup final".into()),
                dest_dir: dest.to_string_lossy().into_owned(),
                backup_dir: None,
                template_id: Some(template),
                state: "running".into(),
            })
            .unwrap();
        let payload = IngestCopyPayload {
            batch_id: batch,
            source_root: base.join("card").to_string_lossy().into_owned(),
            dest_dir_template: dest.to_string_lossy().into_owned(),
            filename_template: "{session}_{seq:4}".into(),
            backup_dir: None,
            session: "Cup final".into(),
            start_seq: 1,
            iptc_fields_json: Some(r#"{"credit":"SHL"}"#.into()),
            iptc_replacements_json: None,
            only_sources: None,
        };
        catalog.delete_iptc_template(template).unwrap();
        let job = ClaimedJob {
            id: JobId(1),
            kind: JobKind::IngestCopy,
            payload_json: serde_json::to_string(&payload).unwrap(),
            attempts: 2,
        };
        let (tx, _rx) = flume::unbounded();
        ingest_job_handler(Arc::clone(&catalog), &job, &tx).unwrap();

        let stamps = stamp_payloads(&catalog);
        assert_eq!(stamps.len(), 1);
        assert_eq!(stamps[0].fields_json, r#"{"credit":"SHL"}"#);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn a_job_where_every_file_failed_fails_with_the_first_reason() {
        let base = scratch("all-failed");
        let card = base.join("card");
        let dcim = card.join("DCIM/100NIKON");
        std::fs::create_dir_all(&dcim).unwrap();
        std::fs::write(dcim.join("DSC_0001.NEF"), b"raw one").unwrap();
        std::fs::write(dcim.join("DSC_0002.NEF"), b"raw two").unwrap();
        let dest = base.join("Photos").to_string_lossy().into_owned();
        let catalog = Arc::new(Catalog::open(&base.join("catalog.sqlite")).unwrap());
        let started = start_ingest(&catalog, &start_request(&card, &dest, "")).unwrap();

        // Corrupt the durable rule the way the old empty-rule bug did.
        let job = catalog.claim_job().unwrap().unwrap();
        let mut payload: IngestCopyPayload = serde_json::from_str(&job.payload_json).unwrap();
        payload.filename_template = String::new();
        let job = ClaimedJob {
            payload_json: serde_json::to_string(&payload).unwrap(),
            ..job
        };
        let (tx, _rx) = flume::unbounded();
        let error = ingest_job_handler(Arc::clone(&catalog), &job, &tx).unwrap_err();
        assert!(error.contains("none of 2 files"), "{error}");
        assert!(error.contains("empty name"), "{error}");
        assert_eq!(
            catalog.get_ingest_batch(started.batch_id).unwrap().state,
            "failed"
        );
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn code_replacements_are_frozen_with_the_template_and_ride_the_stamp_job() {
        let base = scratch("codes");
        let catalog = Catalog::open(&base.join("catalog.sqlite")).unwrap();
        let card = base.join("card");
        let dest = base.join("Photos").to_string_lossy().into_owned();
        let fields = TemplateFields::default().with("caption", "\\h1\\ win");
        let codes = CodeReplacements::parse("h1\tSHL\n");
        let started = start_ingest(
            &catalog,
            &IngestStart {
                fields: Some(&fields),
                replacements: Some(&codes),
                ..start_request(&card, &dest, "")
            },
        )
        .unwrap();
        let job = catalog.job(started.job_id).unwrap();
        let payload: IngestCopyPayload = serde_json::from_str(&job.payload_json).unwrap();
        let frozen: Option<CodeReplacements> =
            serde_json::from_str(payload.iptc_replacements_json.as_deref().unwrap()).ok();
        assert_eq!(frozen.as_ref(), Some(&codes));

        let (_, stamp) = iptc_stamp_job(
            &payload,
            0,
            &frozen,
            &LandedPhoto {
                item_id: 1,
                photo_id: 1,
                path: "/p/a.JPG".into(),
                sequence: 1,
            },
        )
        .unwrap();
        let stamp: MetadataWritePayload = serde_json::from_str(&stamp).unwrap();
        assert_eq!(stamp.replacements, Some(codes));
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
