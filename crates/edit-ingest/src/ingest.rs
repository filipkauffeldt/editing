//! The durable ingest copy job body.
//!
//! One `ingest_copy` job runs one card (plan `PLAN.md` core loop: two cards
//! ingest in parallel with per-card progress). The job enumerates the card,
//! records every file, checks the Lightroom-style duplicate triple
//! (original name, capture time, size), then copies each newcomer with
//! [`crate::copy`], inserts the catalog photo, and queues a thumbnail job.
//!
//! The job never reads EXIF itself: the caller injects a metadata reader,
//! keeping `edit-ingest` free of a dependency on `edit-metadata` (services do
//! not depend on each other; the app composes them).
//!
//! Crash safety: landed paths and the verified hash are persisted (state
//! `Renaming`) before the catalog insert, and the duplicate lookup also sees
//! non-terminal rows, so a card job that died after the rename resumes by
//! completing the catalog insert instead of landing a second copy.

use std::fs;
use std::path::{Path, PathBuf};

use edit_catalog::models::{IngestBatch, IngestFile, NewIngestBatch, NewIngestFile, NewPhoto};
use edit_catalog::{Catalog, JobKind};
use edit_core::IngestFileState;
use serde::{Deserialize, Serialize};

use crate::copy::{CopyError, CopyOutcome, CopyRequest, CopyStage, copy_verified_with, hash_file};
use crate::naming::{
    NamingContext, NamingError, expand_tilde, render_dir, render_filename, static_prefix, validate,
    with_original_extension,
};

/// Raw extensions the ingest pipeline recognizes.
const RAW_EXTS: &[&str] = &[
    "NEF", "NRW", "ARW", "SRF", "SR2", "CR2", "CR3", "RAF", "ORF", "RW2", "DNG", "PEF",
];
/// Plain stills recognized on the ingest path.
const PLAIN_EXTS: &[&str] = &["JPG", "JPEG", "TIF", "TIFF"];

/// Capture metadata the naming and duplicate logic needs, read by the caller.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileMetadata {
    /// EXIF capture time in Unix seconds, when present.
    pub taken_at: Option<i64>,
    /// Camera model for the `{camera}` variable.
    pub camera: Option<String>,
}

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
    /// Files whose landed copy was found and cataloged after a crash.
    pub recovered: usize,
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

/// Inputs for planning one card's ingest.
#[derive(Clone, Debug)]
pub struct IngestPlanRequest<'a> {
    pub volume_uuid: Option<&'a str>,
    pub volume_label: Option<&'a str>,
    pub source_root: &'a Path,
    pub dest_dir_template: &'a str,
    pub filename_template: &'a str,
    pub backup_dir: Option<&'a str>,
    pub session: &'a str,
    pub start_seq: u64,
}

/// A planned batch: the `ingest_batches` row and the job payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IngestPlan {
    pub batch: NewIngestBatch,
    pub payload: IngestCopyPayload,
}

/// Validate the rules and build the batch row plus job payload. Moving this
/// out of the UI keeps naming rules and payload shape in `edit-ingest`.
pub fn plan_ingest(request: &IngestPlanRequest<'_>) -> Result<IngestPlan, NamingError> {
    let dest = request.dest_dir_template.trim();
    let filename = request.filename_template.trim();
    if dest.is_empty() {
        return Err(NamingError::EmptyName);
    }
    validate(dest)?;
    validate(filename)?;
    let backup_dir = request
        .backup_dir
        .map(str::trim)
        .filter(|dir| !dir.is_empty())
        .map(str::to_string);
    Ok(IngestPlan {
        batch: NewIngestBatch {
            volume_uuid: request.volume_uuid.map(str::to_string),
            volume_label: request.volume_label.map(str::to_string),
            session_name: Some(request.session.to_string()),
            dest_dir: static_prefix(dest),
            backup_dir: backup_dir.clone(),
            template_id: None,
            state: "running".to_string(),
        },
        payload: IngestCopyPayload {
            batch_id: 0,
            source_root: request.source_root.to_string_lossy().into_owned(),
            dest_dir_template: dest.to_string(),
            filename_template: filename.to_string(),
            backup_dir,
            session: request.session.to_string(),
            start_seq: request.start_seq,
        },
    })
}

/// Enumerate supported photo files on a card.
///
/// A camera card's photos live under `DCIM`; any other folder is walked
/// directly so a plain folder can be ingested too. Symlinked directories are
/// never descended into (a link cycle would loop forever). An unreadable card
/// root, or a walk that yields nothing but read errors, is an error so the
/// batch is failed instead of silently reporting "done, 0 files".
pub fn enumerate_card(root: &Path) -> std::io::Result<Vec<CardFile>> {
    let start = if root.join("DCIM").is_dir() {
        root.join("DCIM")
    } else {
        root.to_path_buf()
    };
    let mut out = Vec::new();
    let mut stack = vec![start.clone()];
    let mut errors = 0usize;
    while let Some(dir) = stack.pop() {
        let read = match fs::read_dir(&dir) {
            Ok(read) => read,
            Err(error) => {
                if dir == start {
                    return Err(error);
                }
                errors += 1;
                continue;
            }
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
    if out.is_empty() && errors > 0 {
        return Err(std::io::Error::other(format!(
            "card root {} is unreadable ({errors} directories failed)",
            start.display()
        )));
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

/// Plan a card into the ordered file list and sequence numbers.
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
/// `metadata` reads the capture fields for the duplicate check, the date
/// variables, and `{camera}`. `on_progress` fires once with `(0, total)`
/// after enumeration and then after every file, so the dialog gets live
/// counts. Any card-level error marks the batch `failed` before returning.
pub fn run_ingest_copy<F>(
    catalog: &Catalog,
    payload: &IngestCopyPayload,
    metadata: F,
    on_progress: &dyn Fn(usize, usize),
) -> Result<IngestSummary, IngestError>
where
    F: Fn(&Path) -> FileMetadata,
{
    match run_card(catalog, payload, &metadata, on_progress) {
        Ok(summary) => {
            let state = if summary.failed > 0
                && summary.copied == 0
                && summary.recovered == 0
                && summary.duplicates == 0
            {
                "failed"
            } else {
                "done"
            };
            catalog.finish_ingest_batch(payload.batch_id, state)?;
            Ok(summary)
        }
        Err(error) => {
            // Never leave a card batch `running` after a card-level failure.
            let _ = catalog.finish_ingest_batch(payload.batch_id, "failed");
            Err(error)
        }
    }
}

fn run_card<F>(
    catalog: &Catalog,
    payload: &IngestCopyPayload,
    metadata: &F,
    on_progress: &dyn Fn(usize, usize),
) -> Result<IngestSummary, IngestError>
where
    F: Fn(&Path) -> FileMetadata,
{
    let (files, seqs) = plan_card(Path::new(&payload.source_root), payload.start_seq)?;
    let dest_root = resolve_dest_root(&payload.dest_dir_template)?;
    // Get-or-create races safely when two cards land in a fresh destination.
    let root_id = catalog.ensure_root(&dest_root, "folder")?;

    let total = files.len();
    on_progress(0, total);
    let mut summary = IngestSummary {
        files: total,
        ..IngestSummary::default()
    };

    for (index, file) in files.iter().enumerate() {
        let meta = metadata(&file.path);
        // One capture time: EXIF when present, else the card mtime. Dedupe
        // and the date variables must agree on it.
        let taken_at = meta.taken_at.unwrap_or(file.mtime);
        let context = NamingContext {
            session: payload.session.clone(),
            original_name: file.original_name.clone(),
            seq: seqs[index],
            taken_at: Some(taken_at),
            camera: meta.camera.clone(),
        };

        let existing = catalog.find_ingest_file_by_key(
            &file.original_name,
            Some(taken_at),
            file.bytes as i64,
        )?;
        let registered =
            catalog.find_photo_duplicate(&file.original_name, Some(taken_at), file.bytes as i64)?;

        let row_id = match classify(existing, registered) {
            Existing::Duplicate => {
                // Record the duplicate so the dialog can report it.
                catalog.record_ingest_file(&NewIngestFile {
                    batch_id: payload.batch_id,
                    original_name: file.original_name.clone(),
                    taken_at: Some(taken_at),
                    bytes: file.bytes as i64,
                    source_path: file.path.to_string_lossy().into_owned(),
                    state: IngestFileState::Duplicate,
                })?;
                summary.duplicates += 1;
                on_progress(index + 1, total);
                continue;
            }
            Existing::Recoverable(row) => {
                match recover(catalog, file, &row, root_id, taken_at) {
                    Ok(()) => summary.recovered += 1,
                    Err(error) => {
                        summary.failed += 1;
                        catalog.set_ingest_file_state(
                            row.id,
                            IngestFileState::Failed,
                            Some(&error),
                        )?;
                    }
                }
                on_progress(index + 1, total);
                continue;
            }
            Existing::Resume(id) => id,
            Existing::New => catalog.record_ingest_file(&NewIngestFile {
                batch_id: payload.batch_id,
                original_name: file.original_name.clone(),
                taken_at: Some(taken_at),
                bytes: file.bytes as i64,
                source_path: file.path.to_string_lossy().into_owned(),
                state: IngestFileState::Queued,
            })?,
        };

        match land_one(
            catalog, payload, &dest_root, root_id, file, row_id, &context, taken_at,
        ) {
            Ok(()) => summary.copied += 1,
            Err(error) => {
                summary.failed += 1;
                catalog.set_ingest_file_state(row_id, IngestFileState::Failed, Some(&error))?;
            }
        }
        on_progress(index + 1, total);
    }

    Ok(summary)
}

/// What an existing history/photo row means for this file.
enum Existing {
    /// Already cataloged; do not copy.
    Duplicate,
    /// A previous run landed bytes; finish cataloging them.
    Recoverable(Box<IngestFile>),
    /// A previous run recorded a row but landed nothing; reuse its id.
    Resume(i64),
    /// No history; record a fresh row and copy.
    New,
}

/// Decide whether to skip, recover, or copy based on the ingest history and
/// the photos table.
fn classify(
    existing: Option<IngestFile>,
    registered: Option<edit_catalog::models::Photo>,
) -> Existing {
    if let Some(row) = existing {
        if row.state == IngestFileState::Cataloged {
            return Existing::Duplicate;
        }
        if row.state.has_landed_bytes() && row.dest_path.is_some() {
            return Existing::Recoverable(Box::new(row));
        }
        return Existing::Resume(row.id);
    }
    if registered.is_some() {
        return Existing::Duplicate;
    }
    Existing::New
}

/// Copy one file, persist the landed paths, catalog the photo, and complete
/// the row. Errors are strings: one bad file must not abort the card.
#[allow(clippy::too_many_arguments)]
fn land_one(
    catalog: &Catalog,
    payload: &IngestCopyPayload,
    dest_root: &str,
    root_id: i64,
    file: &CardFile,
    row_id: i64,
    context: &NamingContext,
    taken_at: i64,
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
                CopyStage::Copying => IngestFileState::Copying,
                CopyStage::Verifying => IngestFileState::Verifying,
                CopyStage::BackingUp => IngestFileState::Backup,
                CopyStage::Renaming => IngestFileState::Renaming,
            };
            let _ = catalog.set_ingest_file_state(row_id, state, None);
        },
    )
    .map_err(copy_error)?;

    // Persist the landed paths and hash before the catalog insert, so a
    // crash here is recoverable instead of a second copy.
    catalog
        .mark_ingest_file_landed(
            row_id,
            &outcome.dest_path.to_string_lossy(),
            outcome
                .backup_path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .as_deref(),
            &outcome.blake3,
        )
        .map_err(|e| e.to_string())?;

    let photo_id =
        catalog_photo(catalog, root_id, &outcome, file, taken_at).map_err(|e| e.to_string())?;
    catalog
        .complete_ingest_file(
            row_id,
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

/// Finish cataloging a copy that a crash interrupted after the rename.
fn recover(
    catalog: &Catalog,
    file: &CardFile,
    row: &IngestFile,
    root_id: i64,
    taken_at: i64,
) -> Result<(), String> {
    let dest = row.dest_path.clone().ok_or("no landed path")?;
    let actual = hash_file(Path::new(&dest)).map_err(|e| e.to_string())?;
    if row.blake3.as_deref() != Some(actual.as_str()) {
        return Err(format!("landed file at {dest} changed"));
    }
    let outcome = CopyOutcome {
        dest_path: PathBuf::from(&dest),
        backup_path: row.backup_path.as_ref().map(PathBuf::from),
        bytes: file.bytes,
        blake3: actual,
    };
    let photo_id = match row.photo_id {
        Some(id) => Some(id),
        None => {
            catalog_photo(catalog, root_id, &outcome, file, taken_at).map_err(|e| e.to_string())?
        }
    };
    catalog
        .complete_ingest_file(
            row.id,
            &dest,
            row.backup_path.as_deref(),
            &outcome.blake3,
            photo_id,
        )
        .map_err(|e| e.to_string())?;
    Ok(())
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

/// Insert the landed photo (or reuse the row a crash left behind), pair it
/// into an `items` row, and queue its thumbnail job.
fn catalog_photo(
    catalog: &Catalog,
    root_id: i64,
    outcome: &CopyOutcome,
    file: &CardFile,
    taken_at: i64,
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
        .unwrap_or(file.mtime);

    let photo_id = match catalog.insert_photo(&NewPhoto {
        root_id,
        path: path.clone(),
        dir: dir.clone(),
        base_name: stem.clone(),
        ext,
        kind: kind.to_string(),
        bytes: outcome.bytes as i64,
        mtime,
        original_name: Some(file.original_name.clone()),
        taken_at: Some(taken_at),
    }) {
        Ok(id) => Some(id),
        // A crash may already have inserted the photo; reuse that row.
        Err(e) if e.is_unique_violation() => {
            catalog.get_photo_by_path(&path)?.map(|photo| photo.id)
        }
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

/// Convenience for the app: derive the volume identity a batch is keyed by.
pub fn batch_volume_identity(batch: &IngestBatch) -> String {
    match batch.volume_uuid.as_deref().filter(|uuid| !uuid.is_empty()) {
        Some(uuid) => format!("uuid:{uuid}"),
        None => String::new(),
    }
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
        crate::testutil::scratch_dir(&format!("ingest-{name}"))
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

    fn fixed_metadata(_: &Path) -> FileMetadata {
        FileMetadata {
            taken_at: Some(1_791_374_400),
            camera: Some("Canon EOS R6".into()),
        }
    }

    fn new_batch(catalog: &Catalog, dest: &Path, backup: Option<&Path>) -> i64 {
        catalog
            .create_ingest_batch(&NewIngestBatch {
                volume_uuid: Some("UUID".into()),
                volume_label: Some("CARD".into()),
                session_name: Some("shoot".into()),
                dest_dir: dest.to_string_lossy().into_owned(),
                backup_dir: backup.map(|p| p.to_string_lossy().into_owned()),
                template_id: None,
                state: "running".into(),
            })
            .unwrap()
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
    fn enumerate_fails_on_an_unreadable_card_root() {
        let missing = scratch("gone").join("not-mounted");
        assert!(enumerate_card(&missing).is_err());
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
        let catalog = Catalog::open(&base.join("catalog.sqlite")).unwrap();
        let batch = new_batch(&catalog, &dest, Some(&backup));

        let progress = std::cell::RefCell::new(Vec::new());
        let summary = run_ingest_copy(
            &catalog,
            &payload(batch, &card, &dest, Some(&backup)),
            fixed_metadata,
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
            assert_eq!(row.state, IngestFileState::Cataloged);
            let dest_path = PathBuf::from(row.dest_path.as_ref().unwrap());
            let backup_path = PathBuf::from(row.backup_path.as_ref().unwrap());
            assert!(dest_path.is_file());
            assert!(backup_path.is_file());
            assert_eq!(
                fs::read(&dest_path).unwrap(),
                fs::read(&backup_path).unwrap()
            );
            assert_eq!(
                row.blake3.as_deref(),
                Some(hash_file(&dest_path).unwrap().as_str())
            );
            assert!(row.photo_id.is_some());
        }
        assert!(dest.join("2026-10-07/shoot_0001.JPG").is_file());
        assert!(dest.join("2026-10-07/shoot_0002.NEF").is_file());
        assert!(backup.join("2026-10-07/shoot_0001.JPG").is_file());

        let root = catalog
            .get_root_by_path(&dest.canonicalize().unwrap().to_string_lossy())
            .unwrap()
            .unwrap();
        assert_eq!(catalog.list_photos_by_root(root.id).unwrap().len(), 2);
        let jobs = catalog.list_jobs(50).unwrap();
        assert_eq!(jobs.iter().filter(|j| j.kind == JobKind::Thumb).count(), 2);
        assert_eq!(catalog.get_ingest_batch(batch).unwrap().state, "done");

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
        let catalog = Catalog::open(&base.join("catalog.sqlite")).unwrap();
        let first_batch = new_batch(&catalog, &dest, None);
        let first = run_ingest_copy(
            &catalog,
            &payload(first_batch, &card, &dest, None),
            fixed_metadata,
            &|_, _| {},
        )
        .unwrap();
        assert_eq!(first.copied, 1);

        let second_batch = new_batch(&catalog, &dest, None);
        let second = run_ingest_copy(
            &catalog,
            &payload(second_batch, &card, &dest, None),
            fixed_metadata,
            &|_, _| {},
        )
        .unwrap();
        assert_eq!(second.copied, 0);
        assert_eq!(second.duplicates, 1);

        let rows = catalog.list_ingest_files(second_batch).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, IngestFileState::Duplicate);

        fs::remove_dir_all(&base).ok();
        fs::remove_dir_all(&card).ok();
    }

    #[test]
    fn a_crash_after_rename_recovers_instead_of_recopying() {
        let card = card_with(&[("DSC0001.JPG", b"recover me")]);
        let base = scratch("recover");
        let dest = base.join("Photos");
        let catalog = Catalog::open(&base.join("catalog.sqlite")).unwrap();
        let batch = new_batch(&catalog, &dest, None);
        let payload = payload(batch, &card, &dest, None);

        // Simulate: the previous run landed the file and recorded the paths,
        // then died before the catalog insert (state `Renaming`).
        let landed = dest.join("2026-10-07/shoot_0001.JPG");
        fs::create_dir_all(landed.parent().unwrap()).unwrap();
        fs::write(&landed, b"recover me").unwrap();
        let hash = hash_file(&landed).unwrap();
        let row = catalog
            .record_ingest_file(&NewIngestFile {
                batch_id: batch,
                original_name: "DSC0001.JPG".into(),
                taken_at: Some(1_791_374_400),
                bytes: 10,
                source_path: "/card/DCIM/DSC0001.JPG".into(),
                state: IngestFileState::Queued,
            })
            .unwrap();
        catalog
            .mark_ingest_file_landed(row, &landed.to_string_lossy(), None, &hash)
            .unwrap();

        let summary = run_ingest_copy(&catalog, &payload, fixed_metadata, &|_, _| {}).unwrap();
        assert_eq!(summary.copied, 0);
        assert_eq!(summary.recovered, 1);
        assert_eq!(summary.duplicates, 0);

        // One landed file only, one cataloged row, one photo.
        let files: Vec<_> = fs::read_dir(landed.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".JPG"))
            .collect();
        assert_eq!(files.len(), 1);
        let rows = catalog.list_ingest_files(batch).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, IngestFileState::Cataloged);
        assert!(rows[0].photo_id.is_some());

        fs::remove_dir_all(&base).ok();
        fs::remove_dir_all(&card).ok();
    }

    #[test]
    fn a_bad_rule_fails_only_its_file_and_the_batch() {
        let card = card_with(&[("DSC0001.JPG", b"a"), ("DSC0002.JPG", b"b")]);
        let base = scratch("badrule");
        let dest = base.join("Photos");
        let catalog = Catalog::open(&base.join("catalog.sqlite")).unwrap();
        let batch = new_batch(&catalog, &dest, None);
        let mut p = payload(batch, &card, &dest, None);
        p.filename_template = "{nope}_{seq}".to_string();
        let summary = run_ingest_copy(&catalog, &p, fixed_metadata, &|_, _| {}).unwrap();
        assert_eq!(summary.failed, 2);
        assert_eq!(summary.copied, 0);
        let rows = catalog.list_ingest_files(batch).unwrap();
        assert!(
            rows.iter()
                .all(|r| r.state == IngestFileState::Failed && r.error.is_some())
        );
        assert_eq!(catalog.get_ingest_batch(batch).unwrap().state, "failed");

        fs::remove_dir_all(&base).ok();
        fs::remove_dir_all(&card).ok();
    }

    #[test]
    fn unreadable_card_marks_the_batch_failed() {
        let base = scratch("unplugged");
        let dest = base.join("Photos");
        let catalog = Catalog::open(&base.join("catalog.sqlite")).unwrap();
        let batch = new_batch(&catalog, &dest, None);
        let p = IngestCopyPayload {
            batch_id: batch,
            source_root: base.join("gone").to_string_lossy().into_owned(),
            dest_dir_template: dest.join("{yyyy}").to_string_lossy().into_owned(),
            filename_template: "{seq}".into(),
            backup_dir: None,
            session: "shoot".into(),
            start_seq: 1,
        };
        assert!(run_ingest_copy(&catalog, &p, fixed_metadata, &|_, _| {}).is_err());
        assert_eq!(catalog.get_ingest_batch(batch).unwrap().state, "failed");
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn plan_ingest_validates_rules_and_builds_the_batch() {
        let plan = plan_ingest(&IngestPlanRequest {
            volume_uuid: Some("UUID"),
            volume_label: Some("CARD"),
            source_root: Path::new("/media/card"),
            dest_dir_template: "  ~/Photos/{yyyy}/{mm}-{dd}  ",
            filename_template: "{session}_{seq:4}",
            backup_dir: Some("  /mnt/backup  "),
            session: "shoot",
            start_seq: 1,
        })
        .unwrap();
        assert_eq!(plan.batch.volume_label.as_deref(), Some("CARD"));
        assert_eq!(plan.batch.dest_dir, "~/Photos");
        assert_eq!(plan.batch.backup_dir.as_deref(), Some("/mnt/backup"));
        assert_eq!(plan.payload.filename_template, "{session}_{seq:4}");
        assert_eq!(plan.payload.backup_dir.as_deref(), Some("/mnt/backup"));

        assert!(matches!(
            plan_ingest(&IngestPlanRequest {
                dest_dir_template: "{nope}",
                ..plan_request_defaults()
            })
            .unwrap_err(),
            NamingError::UnknownVariable(_)
        ));
        assert!(matches!(
            plan_ingest(&IngestPlanRequest {
                dest_dir_template: "   ",
                ..plan_request_defaults()
            })
            .unwrap_err(),
            NamingError::EmptyName
        ));
    }

    fn plan_request_defaults<'a>() -> IngestPlanRequest<'a> {
        IngestPlanRequest {
            volume_uuid: None,
            volume_label: None,
            source_root: Path::new("/media/card"),
            dest_dir_template: "~/Photos/{yyyy}",
            filename_template: "{seq}",
            backup_dir: None,
            session: "shoot",
            start_seq: 1,
        }
    }
}
