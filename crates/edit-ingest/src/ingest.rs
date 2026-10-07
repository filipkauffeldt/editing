//! The durable ingest copy job body.
//!
//! One `ingest_copy` job runs one card (plan `PLAN.md` core loop: two cards
//! ingest in parallel with per-card progress). The job enumerates the card,
//! records every file, checks the Lightroom-style duplicate triple
//! (original name, EXIF capture time, size), then copies each newcomer with
//! [`crate::copy`], inserts the catalog photo, and queues a thumbnail job.
//!
//! The job never reads EXIF itself: the caller injects a capture-time
//! reader, keeping `edit-ingest` free of a dependency on `edit-metadata`
//! (services do not depend on each other; the app composes them).

use std::fs;
use std::path::{Path, PathBuf};

use edit_catalog::models::{NewIngestFile, NewPhoto};
use edit_catalog::{Catalog, JobKind, JobState};
use serde::{Deserialize, Serialize};

use crate::copy::{CopyError, CopyOutcome, CopyRequest, CopyStage, copy_verified_with};
use crate::naming::{
    NamingContext, NamingError, expand_tilde, render_dir, render_filename, static_prefix,
    with_original_extension,
};

/// Raw extensions the ingest pipeline recognizes.
const RAW_EXTS: &[&str] = &[
    "NEF", "NRW", "ARW", "SRF", "SR2", "CR2", "CR3", "RAF", "ORF", "RW2", "DNG", "PEF",
];
/// Plain stills recognized on the ingest path.
const PLAIN_EXTS: &[&str] = &["JPG", "JPEG", "TIF", "TIFF"];

/// Durable payload for one `ingest_copy` job: one card, one batch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestCopyPayload {
    /// `ingest_batches.id` this job fills.
    pub batch_id: i64,
    /// Card root to enumerate (the mount point; `DCIM` is found under it).
    pub source_root: String,
    /// Folder rule, e.g. `~/Photos/{yyyy}/{mm}-{dd}`.
    pub dest_dir_template: String,
    /// Filename rule, e.g. `{session}_{seq:4}`.
    pub filename_template: String,
    /// Optional backup root; the same folder rule nests under it.
    pub backup_dir: Option<String>,
    /// Session name for the `{session}` variable.
    pub session: String,
    /// First sequence number of the batch.
    pub start_seq: u64,
}

/// One supported file found on a card.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CardFile {
    pub path: PathBuf,
    pub original_name: String,
    pub bytes: u64,
    pub mtime: i64,
}

/// Counts a finished card.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IngestSummary {
    pub files: usize,
    pub copied: usize,
    pub duplicates: usize,
    pub failed: usize,
}

/// A failure that stops a whole card (a per-file failure is recorded on the
/// file row and the card continues).
#[derive(Debug, thiserror::Error)]
pub enum IngestError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("catalog: {0}")]
    Catalog(#[from] edit_catalog::Error),
}

/// Enumerate supported photo files on a card.
///
/// A camera card's photos live under `DCIM`; any other folder is walked
/// directly so a plain folder can be ingested too. Symlinked directories are
/// never descended into (a link cycle would loop forever), matching the
/// browse scanner.
pub fn enumerate_card(root: &Path) -> std::io::Result<Vec<CardFile>> {
    let start = if root.join("DCIM").is_dir() {
        root.join("DCIM")
    } else {
        root.to_path_buf()
    };
    let mut out = Vec::new();
    let mut stack = vec![start];
    while let Some(dir) = stack.pop() {
        let read = match fs::read_dir(&dir) {
            Ok(read) => read,
            Err(_) => continue,
        };
        let mut children: Vec<PathBuf> = read.filter_map(|e| e.ok().map(|e| e.path())).collect();
        children.sort();
        for child in children.into_iter().rev() {
            let is_link = fs::symlink_metadata(&child)
                .map(|meta| meta.file_type().is_symlink())
                .unwrap_or(false);
            if is_link && child.is_dir() {
                continue;
            }
            if child.is_dir() {
                stack.push(child);
            } else if let Some(file) = card_file(&child) {
                out.push(file);
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

fn card_file(path: &Path) -> Option<CardFile> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_uppercase())?;
    if !RAW_EXTS.contains(&ext.as_str()) && !PLAIN_EXTS.contains(&ext.as_str()) {
        return None;
    }
    let meta = fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let original_name = path.file_name()?.to_str()?.to_string();
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Some(CardFile {
        path: path.to_path_buf(),
        original_name,
        bytes: meta.len(),
        mtime,
    })
}

/// Assign sequence numbers: RAW and JPEG files that share a base name get
/// the same number, so a pair lands as one `items` row with a shared stem.
pub fn assign_sequence(files: &[CardFile], start_seq: u64) -> Vec<u64> {
    let mut seqs = Vec::with_capacity(files.len());
    let mut previous_base: Option<String> = None;
    let mut seq = start_seq;
    for file in files {
        let base = base_stem(&file.original_name);
        if previous_base.as_deref() != Some(base.as_str()) {
            if previous_base.is_some() {
                seq += 1;
            }
            previous_base = Some(base);
        }
        seqs.push(seq);
    }
    seqs
}

/// Plan a card into the ordered file list the worker will process.
pub fn plan_card(root: &Path, start_seq: u64) -> std::io::Result<(Vec<CardFile>, Vec<u64>)> {
    let files = enumerate_card(root)?;
    let seqs = assign_sequence(&files, start_seq);
    Ok((files, seqs))
}

fn base_stem(name: &str) -> String {
    Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(name)
        .to_ascii_lowercase()
}

/// Run one card through the verified copy pipeline.
///
/// `capture_time` reads the EXIF capture time for the duplicate check and the
/// date variables; the caller injects it so this crate stays free of
/// `edit-metadata`. `on_progress` fires once with `(0, total)` after
/// enumeration and then after every file, so the dialog gets live counts.
pub fn run_ingest_copy<F>(
    catalog: &Catalog,
    payload: &IngestCopyPayload,
    capture_time: F,
    on_progress: &dyn Fn(usize, usize),
) -> Result<IngestSummary, IngestError>
where
    F: Fn(&Path) -> Option<i64>,
{
    let (files, seqs) = plan_card(Path::new(&payload.source_root), payload.start_seq)?;
    let dest_root = resolve_dest_root(&payload.dest_dir_template)?;
    let root_id = match catalog.get_root_by_path(&dest_root)? {
        Some(root) => root.id,
        None => catalog.insert_root(&dest_root, "folder")?,
    };

    let total = files.len();
    on_progress(0, total);
    let mut summary = IngestSummary {
        files: total,
        ..IngestSummary::default()
    };

    for (index, file) in files.iter().enumerate() {
        let taken_at = capture_time(&file.path);
        if catalog
            .find_duplicate_ingest_file(&file.original_name, taken_at, file.bytes as i64)?
            .is_some()
        {
            catalog.record_ingest_file(&NewIngestFile {
                batch_id: payload.batch_id,
                original_name: file.original_name.clone(),
                taken_at,
                bytes: file.bytes as i64,
                source_path: file.path.to_string_lossy().into_owned(),
                state: "duplicate".to_string(),
            })?;
            summary.duplicates += 1;
            on_progress(index + 1, total);
            continue;
        }

        let file_id = catalog.record_ingest_file(&NewIngestFile {
            batch_id: payload.batch_id,
            original_name: file.original_name.clone(),
            taken_at,
            bytes: file.bytes as i64,
            source_path: file.path.to_string_lossy().into_owned(),
            state: "queued".to_string(),
        })?;

        let context = NamingContext {
            session: payload.session.clone(),
            original_name: file.original_name.clone(),
            seq: seqs[index],
            // Date variables fall back to the card's mtime when EXIF has no
            // capture time, so a rule never fails on a missing tag.
            taken_at: Some(taken_at.unwrap_or(file.mtime)),
            camera: None,
        };

        match land_one(
            catalog, payload, &dest_root, root_id, file, file_id, &context,
        ) {
            Ok(()) => summary.copied += 1,
            Err(error) => {
                summary.failed += 1;
                catalog.set_ingest_file_state(file_id, "failed", Some(&error))?;
            }
        }
        on_progress(index + 1, total);
    }

    let state = if summary.failed > 0 && summary.copied == 0 {
        JobState::Failed
    } else {
        JobState::Done
    };
    catalog.finish_ingest_batch(payload.batch_id, state.as_str())?;
    Ok(summary)
}

/// Resolve the stable destination root (`static_prefix`, tilde-expanded and
/// canonicalized) so the catalog root row matches how browse canonicalizes.
fn resolve_dest_root(template: &str) -> Result<String, IngestError> {
    let prefix = static_prefix(template);
    let mut root = expand_tilde(&prefix);
    if root.as_os_str().is_empty() {
        root = std::env::current_dir()?;
    }
    fs::create_dir_all(&root)?;
    let canonical = root.canonicalize().unwrap_or(root);
    Ok(canonical.to_string_lossy().into_owned())
}

/// Copy one file, record per-stage state, and catalog the landed photo.
///
/// Errors are strings: a single bad file must not abort the card.
fn land_one(
    catalog: &Catalog,
    payload: &IngestCopyPayload,
    dest_root: &str,
    root_id: i64,
    file: &CardFile,
    file_id: i64,
    context: &NamingContext,
) -> Result<(), String> {
    let dest_folder = render_dir(&payload.dest_dir_template, context).map_err(naming_error)?;
    let dest_folder = if dest_folder.is_absolute() {
        dest_folder
    } else {
        PathBuf::from(dest_root).join(dest_folder)
    };
    let filename = with_original_extension(
        &render_filename(&payload.filename_template, context).map_err(naming_error)?,
        &file.original_name,
    );
    let backup_dir = payload.backup_dir.as_ref().map(|base| {
        let relative = dest_folder
            .strip_prefix(Path::new(dest_root))
            .unwrap_or(Path::new(""));
        expand_tilde(base).join(relative)
    });

    let outcome = copy_verified_with(
        &CopyRequest {
            source: file.path.clone(),
            dest_dir: dest_folder.clone(),
            filename: filename.clone(),
            backup_dir,
        },
        &mut |stage| {
            let state = match stage {
                CopyStage::Copying => "copying",
                CopyStage::Verifying => "verifying",
                CopyStage::BackingUp => "backup",
                CopyStage::Renaming => "renaming",
            };
            let _ = catalog.set_ingest_file_state(file_id, state, None);
        },
    )
    .map_err(copy_error)?;

    let photo_id =
        catalog_photo(catalog, root_id, &outcome, file.mtime).map_err(|e| e.to_string())?;
    catalog
        .complete_ingest_file(
            file_id,
            &outcome.dest_path.to_string_lossy(),
            outcome
                .backup_path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .as_deref(),
            &outcome.blake3,
            photo_id,
        )
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Insert the landed photo, pair it into an `items` row, and queue its
/// thumbnail job. Returns the photo id (or `None` when a row for the path
/// already exists).
fn catalog_photo(
    catalog: &Catalog,
    root_id: i64,
    outcome: &CopyOutcome,
    fallback_mtime: i64,
) -> Result<Option<i64>, edit_catalog::Error> {
    let dest = &outcome.dest_path;
    let stem = dest
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    let ext = dest
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_uppercase();
    let kind = if RAW_EXTS.contains(&ext.as_str()) {
        "raw"
    } else {
        "jpeg"
    };
    let dir = dest
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let path = dest.to_string_lossy().into_owned();
    let mtime = fs::metadata(dest)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(fallback_mtime);

    let photo_id = match catalog.insert_photo(&NewPhoto {
        root_id,
        path: path.clone(),
        dir: dir.clone(),
        base_name: stem.clone(),
        ext,
        kind: kind.to_string(),
        bytes: outcome.bytes as i64,
        mtime,
    }) {
        Ok(id) => Some(id),
        // A photo with this path already exists: keep the row, but still
        // ensure the item pairing and thumbnail job below.
        Err(e) if e.is_unique_violation() => None,
        Err(e) => return Err(e),
    };

    let folded = stem.to_ascii_lowercase();
    let (raw_id, jpeg_id) = if kind == "raw" {
        (photo_id, None)
    } else {
        (None, photo_id)
    };
    match catalog.get_item_by_dir_base(&dir, &folded)? {
        None => {
            catalog.insert_item(&dir, &folded, raw_id, jpeg_id)?;
        }
        Some(item) => {
            let occupied = if kind == "raw" {
                item.raw_photo_id
            } else {
                item.jpeg_photo_id
            };
            if occupied.is_none()
                && let Some(photo_id) = photo_id
            {
                catalog.set_item_side(item.id, photo_id, kind == "raw")?;
            }
        }
    }

    if let Some(photo_id) = photo_id {
        let payload = serde_json::json!({ "photo_id": photo_id, "path": path });
        catalog.enqueue_job(JobKind::Thumb, &payload.to_string())?;
    }
    Ok(photo_id)
}

fn naming_error(error: NamingError) -> String {
    format!("naming: {error}")
}

fn copy_error(error: CopyError) -> String {
    format!("copy: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "edit-ingest-run-{}-{}-{name}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn card_with(files: &[(&str, &[u8])]) -> PathBuf {
        let dir = scratch("card");
        let dcim = dir.join("DCIM/100CANON");
        fs::create_dir_all(&dcim).unwrap();
        for (name, bytes) in files {
            fs::write(dcim.join(name), bytes).unwrap();
        }
        dir
    }

    fn payload(
        batch_id: i64,
        card: &Path,
        dest: &Path,
        backup: Option<&Path>,
    ) -> IngestCopyPayload {
        IngestCopyPayload {
            batch_id,
            source_root: card.to_string_lossy().into_owned(),
            dest_dir_template: dest.join("{yyyy}-{mm}-{dd}").to_string_lossy().into_owned(),
            filename_template: "{session}_{seq:4}".to_string(),
            backup_dir: backup.map(|p| p.to_string_lossy().into_owned()),
            session: "shoot".to_string(),
            start_seq: 1,
        }
    }

    #[test]
    fn enumerate_finds_dcim_and_sorts() {
        let card = card_with(&[
            ("DSC0002.NEF", b"b"),
            ("DSC0001.JPG", b"a"),
            ("notes.txt", b"x"),
        ]);
        let files = enumerate_card(&card).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].original_name, "DSC0001.JPG");
        assert_eq!(files[1].original_name, "DSC0002.NEF");
        fs::remove_dir_all(&card).ok();
    }

    #[test]
    fn raw_and_jpeg_share_a_sequence_number() {
        let card = card_with(&[
            ("DSC0001.JPG", b"a"),
            ("DSC0001.NEF", b"b"),
            ("DSC0002.JPG", b"c"),
        ]);
        let files = enumerate_card(&card).unwrap();
        let seqs = assign_sequence(&files, 1);
        let by_name: Vec<(String, u64)> = files
            .iter()
            .zip(seqs)
            .map(|(f, s)| (f.original_name.clone(), s))
            .collect();
        let seq_of = |name: &str| by_name.iter().find(|(n, _)| n == name).unwrap().1;
        assert_eq!(seq_of("DSC0001.JPG"), seq_of("DSC0001.NEF"));
        assert_ne!(seq_of("DSC0001.NEF"), seq_of("DSC0002.JPG"));
        fs::remove_dir_all(&card).ok();
    }

    #[test]
    fn ingest_copies_verifies_backs_up_and_catalogs() {
        let card = card_with(&[("DSC0001.JPG", b"photo one"), ("DSC0002.NEF", b"photo two")]);
        let base = scratch("run");
        let dest = base.join("Photos");
        let backup = base.join("Backup");
        let db_path = base.join("catalog.sqlite");
        let catalog = Catalog::open(&db_path).unwrap();
        let batch = catalog
            .create_ingest_batch(&edit_catalog::models::NewIngestBatch {
                volume_uuid: Some("UUID".into()),
                volume_label: Some("CARD".into()),
                session_name: Some("shoot".into()),
                dest_dir: dest.to_string_lossy().into_owned(),
                backup_dir: Some(backup.to_string_lossy().into_owned()),
                template_id: None,
                state: "running".into(),
            })
            .unwrap();

        let progress = std::cell::RefCell::new(Vec::new());
        let summary = run_ingest_copy(
            &catalog,
            &payload(batch, &card, &dest, Some(&backup)),
            |_| Some(1_791_374_400),
            &|done, total| progress.borrow_mut().push((done, total)),
        )
        .unwrap();
        let progress = progress.into_inner();

        assert_eq!(summary.files, 2);
        assert_eq!(summary.copied, 2);
        assert_eq!(summary.duplicates, 0);
        assert_eq!(summary.failed, 0);
        assert_eq!(progress.first(), Some(&(0, 2)));
        assert_eq!(progress.last(), Some(&(2, 2)));

        let rows = catalog.list_ingest_files(batch).unwrap();
        assert_eq!(rows.len(), 2);
        for row in &rows {
            assert_eq!(row.state, "cataloged");
            let dest_path = PathBuf::from(row.dest_path.as_ref().unwrap());
            let backup_path = PathBuf::from(row.backup_path.as_ref().unwrap());
            assert!(dest_path.is_file());
            assert!(backup_path.is_file());
            assert_eq!(
                fs::read(&dest_path).unwrap(),
                fs::read(&backup_path).unwrap()
            );
            // The verified hash is recorded.
            assert_eq!(
                row.blake3.as_deref(),
                Some(crate::copy::hash_file(&dest_path).unwrap().as_str())
            );
            assert!(row.photo_id.is_some());
        }
        // Folder rule produced nested date folders under both destinations.
        assert!(dest.join("2026-10-07/shoot_0001.JPG").is_file());
        assert!(dest.join("2026-10-07/shoot_0002.NEF").is_file());
        assert!(backup.join("2026-10-07/shoot_0001.JPG").is_file());

        // Landed files are cataloged and paired into items with thumbnail jobs.
        let root = catalog
            .get_root_by_path(&dest.canonicalize().unwrap().to_string_lossy())
            .unwrap()
            .unwrap();
        assert_eq!(catalog.list_photos_by_root(root.id).unwrap().len(), 2);
        let jobs = catalog.list_jobs(50).unwrap();
        assert_eq!(jobs.iter().filter(|j| j.kind == JobKind::Thumb).count(), 2);
        let batch_row = catalog.get_ingest_batch(batch).unwrap();
        assert_eq!(batch_row.state, "done");

        // No temp files survive in the destination.
        let leftovers: Vec<_> = fs::read_dir(dest.join("2026-10-07"))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".ingest-tmp-"))
            .collect();
        assert!(leftovers.is_empty());

        fs::remove_dir_all(&base).ok();
        fs::remove_dir_all(&card).ok();
    }

    #[test]
    fn duplicates_are_reported_not_recopied() {
        let card = card_with(&[("DSC0001.JPG", b"same bytes")]);
        let base = scratch("dup");
        let dest = base.join("Photos");
        let db_path = base.join("catalog.sqlite");
        let catalog = Catalog::open(&db_path).unwrap();
        let payload = payload(
            catalog
                .create_ingest_batch(&edit_catalog::models::NewIngestBatch {
                    volume_uuid: None,
                    volume_label: None,
                    session_name: None,
                    dest_dir: dest.to_string_lossy().into_owned(),
                    backup_dir: None,
                    template_id: None,
                    state: "running".into(),
                })
                .unwrap(),
            &card,
            &dest,
            None,
        );

        let first = run_ingest_copy(&catalog, &payload, |_| Some(42), &|_, _| {}).unwrap();
        assert_eq!(first.copied, 1);

        // A second batch of the same card: the triple matches, so no recap.
        let second_batch = catalog
            .create_ingest_batch(&edit_catalog::models::NewIngestBatch {
                volume_uuid: None,
                volume_label: None,
                session_name: None,
                dest_dir: dest.to_string_lossy().into_owned(),
                backup_dir: None,
                template_id: None,
                state: "running".into(),
            })
            .unwrap();
        let mut second_payload = payload.clone();
        second_payload.batch_id = second_batch;
        let second = run_ingest_copy(&catalog, &second_payload, |_| Some(42), &|_, _| {}).unwrap();
        assert_eq!(second.copied, 0);
        assert_eq!(second.duplicates, 1);

        let rows = catalog.list_ingest_files(second_batch).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, "duplicate");

        fs::remove_dir_all(&base).ok();
        fs::remove_dir_all(&card).ok();
    }

    #[test]
    fn a_bad_rule_fails_only_its_file() {
        let card = card_with(&[("DSC0001.JPG", b"a"), ("DSC0002.JPG", b"b")]);
        let base = scratch("badrule");
        let dest = base.join("Photos");
        let db_path = base.join("catalog.sqlite");
        let catalog = Catalog::open(&db_path).unwrap();
        let batch = catalog
            .create_ingest_batch(&edit_catalog::models::NewIngestBatch {
                volume_uuid: None,
                volume_label: None,
                session_name: None,
                dest_dir: dest.to_string_lossy().into_owned(),
                backup_dir: None,
                template_id: None,
                state: "running".into(),
            })
            .unwrap();
        let mut p = payload(batch, &card, &dest, None);
        // `{ext}` is fine, `{nope}` is not: every file fails, none panic.
        p.filename_template = "{nope}_{seq}".to_string();
        let summary = run_ingest_copy(&catalog, &p, |_| None, &|_, _| {}).unwrap();
        assert_eq!(summary.failed, 2);
        assert_eq!(summary.copied, 0);
        let rows = catalog.list_ingest_files(batch).unwrap();
        assert!(
            rows.iter()
                .all(|r| r.state == "failed" && r.error.is_some())
        );
        assert_eq!(catalog.get_ingest_batch(batch).unwrap().state, "failed");

        fs::remove_dir_all(&base).ok();
        fs::remove_dir_all(&card).ok();
    }
}
