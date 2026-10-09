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
//! A caller can also hook each landed photo ([`LandedPhoto`]) to queue one
//! follow-up job for it (the app stamps the ingest IPTC snapshot this way).
//! The job is queued in the same commit that marks the file cataloged, so a
//! crash can neither lose it nor queue it for a file that is not complete.
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
    DEFAULT_DEST_RULE, DEFAULT_FILENAME_RULE, NamingContext, NamingError, expand_tilde, render_dir,
    render_filename, static_prefix, validate, validate_filename_rule, with_original_extension,
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
    /// The chosen IPTC template's fields, frozen when the batch was planned.
    /// Later edits to the template do not reach a running (or resumed)
    /// ingest; `None` ingests without stamping.
    #[serde(default)]
    pub iptc_fields_json: Option<String>,
    /// The code replacement table in force at start, frozen with the
    /// template (opaque JSON; the metadata writer reads it).
    #[serde(default)]
    pub iptc_replacements_json: Option<String>,
    /// Copy only these card files (their full source paths); `None` copies
    /// every photo. Files left out are not recorded, so they never show up
    /// as failures or duplicates.
    #[serde(default)]
    pub only_sources: Option<Vec<String>>,
}

/// One supported file found on a card.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CardFile {
    pub path: PathBuf,
    pub original_name: String,
    pub bytes: u64,
    pub mtime: i64,
    /// Protected in the camera. A locked photo is stored read-only on the
    /// card (no owner-write bit); see [`is_locked`].
    pub locked: bool,
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

/// A photo that just landed and was cataloged, handed to the landed hook.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LandedPhoto {
    pub item_id: i64,
    pub photo_id: i64,
    /// Where the file landed (never the backup copy).
    pub path: PathBuf,
    /// The batch sequence number; a RAW+JPEG pair shares one.
    pub sequence: u64,
}

/// A durable job to queue with a landed photo, as `(kind, payload_json)`.
pub type FollowUpJob = Option<(JobKind, String)>;

/// A failure that stops a whole card (a per-file failure is recorded on the
/// file row and the card continues).
#[derive(Debug, thiserror::Error)]
pub enum IngestError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("catalog: {0}")]
    Catalog(#[from] edit_catalog::Error),
}

/// The IPTC template an ingest stamps onto what it lands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IngestTemplate<'a> {
    /// `iptc_templates.id`, kept on the batch row as history. `None` when
    /// the fields were typed in rather than taken from a saved template.
    pub id: Option<i64>,
    /// The fields as JSON at the moment the ingest starts.
    pub fields_json: &'a str,
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
    pub template: Option<IngestTemplate<'a>>,
    /// Copy only these source paths; `None` copies every photo on the card.
    pub only_sources: Option<&'a [String]>,
}

/// A planned batch: the `ingest_batches` row and the job payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IngestPlan {
    pub batch: NewIngestBatch,
    pub payload: IngestCopyPayload,
}

/// Validate the rules and build the batch row plus job payload. Moving this
/// out of the UI keeps naming rules and payload shape in `edit-ingest`.
///
/// A blank filename rule means "use the default" ([`DEFAULT_FILENAME_RULE`]);
/// a rule that cannot produce a file name is rejected here, before a batch
/// is created.
pub fn plan_ingest(request: &IngestPlanRequest<'_>) -> Result<IngestPlan, NamingError> {
    let dest = match request.dest_dir_template.trim() {
        "" => DEFAULT_DEST_RULE,
        rule => rule,
    };
    let filename = match request.filename_template.trim() {
        "" => DEFAULT_FILENAME_RULE,
        rule => rule,
    };
    validate(dest)?;
    validate_filename_rule(filename)?;
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
            template_id: request.template.and_then(|template| template.id),
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
            iptc_fields_json: request
                .template
                .map(|template| template.fields_json.to_string()),
            iptc_replacements_json: None,
            only_sources: request.only_sources.map(<[String]>::to_vec),
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
        locked: is_locked(&meta),
    })
}

/// Whether a card file is locked in the camera. Cameras mark a protected
/// image with the FAT/exFAT read-only attribute, which Linux shows as a
/// missing owner-write bit. Trust it only when the volume itself is
/// writable: on a read-only mount every file looks read-only.
pub fn is_locked(meta: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o200 == 0
    }
    #[cfg(not(unix))]
    {
        meta.permissions().readonly()
    }
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
    plan_selected(root, start_seq, None)
}

/// [`plan_card`] limited to `only` (source paths) when given. Sequence
/// numbers count the chosen files only, so renamed files stay contiguous.
pub fn plan_selected(
    root: &Path,
    start_seq: u64,
    only: Option<&[String]>,
) -> std::io::Result<(Vec<CardFile>, Vec<u64>)> {
    let mut files = enumerate_card(root)?;
    if let Some(only) = only {
        let wanted: std::collections::HashSet<&str> = only.iter().map(String::as_str).collect();
        files.retain(|file| wanted.contains(file.path.to_string_lossy().as_ref()));
    }
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
    run_ingest_copy_with(catalog, payload, metadata, &|_| None, on_progress)
}

/// [`run_ingest_copy`] with a landed hook: `on_landed` runs for every photo
/// that lands (or is recovered after a crash) and may return one job to
/// queue atomically with the file's completion.
pub fn run_ingest_copy_with<F>(
    catalog: &Catalog,
    payload: &IngestCopyPayload,
    metadata: F,
    on_landed: &dyn Fn(&LandedPhoto) -> FollowUpJob,
    on_progress: &dyn Fn(usize, usize),
) -> Result<IngestSummary, IngestError>
where
    F: Fn(&Path) -> FileMetadata,
{
    let hooks = Hooks {
        on_landed,
        on_progress,
    };
    match run_card(catalog, payload, &metadata, &hooks) {
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

/// The caller's callbacks, bundled so the per-file helpers take one argument.
struct Hooks<'a> {
    on_landed: &'a dyn Fn(&LandedPhoto) -> FollowUpJob,
    on_progress: &'a dyn Fn(usize, usize),
}

fn run_card<F>(
    catalog: &Catalog,
    payload: &IngestCopyPayload,
    metadata: &F,
    hooks: &Hooks<'_>,
) -> Result<IngestSummary, IngestError>
where
    F: Fn(&Path) -> FileMetadata,
{
    let on_progress = hooks.on_progress;
    let (files, seqs) = plan_selected(
        Path::new(&payload.source_root),
        payload.start_seq,
        payload.only_sources.as_deref(),
    )?;
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
                    locked: file.locked,
                })?;
                summary.duplicates += 1;
                on_progress(index + 1, total);
                continue;
            }
            Existing::Recoverable(row) => {
                match recover(catalog, file, &row, root_id, taken_at, seqs[index], hooks) {
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
                locked: file.locked,
            })?,
        };

        match land_one(
            catalog, payload, &dest_root, root_id, file, row_id, &context, taken_at, hooks,
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
    hooks: &Hooks<'_>,
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

    let cataloged =
        catalog_photo(catalog, root_id, &outcome, file, taken_at).map_err(|e| e.to_string())?;
    complete(
        catalog,
        row_id,
        &outcome,
        &cataloged,
        context.seq,
        hooks.on_landed,
    )
}

/// Mark the file cataloged and queue the landed hook's follow-up job in one
/// commit.
fn complete(
    catalog: &Catalog,
    row_id: i64,
    outcome: &CopyOutcome,
    cataloged: &Cataloged,
    sequence: u64,
    on_landed: &dyn Fn(&LandedPhoto) -> FollowUpJob,
) -> Result<(), String> {
    let job = cataloged.photo_id.and_then(|photo_id| {
        on_landed(&LandedPhoto {
            item_id: cataloged.item_id,
            photo_id,
            path: outcome.dest_path.clone(),
            sequence,
        })
    });
    catalog
        .complete_ingest_file_with_job(
            row_id,
            &outcome.dest_path.to_string_lossy(),
            outcome
                .backup_path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .as_deref(),
            &outcome.blake3,
            cataloged.photo_id,
            job.as_ref()
                .map(|(kind, payload)| (*kind, payload.as_str())),
        )
        .map_err(|e| e.to_string())
}

/// Finish cataloging a copy that a crash interrupted after the rename.
fn recover(
    catalog: &Catalog,
    file: &CardFile,
    row: &IngestFile,
    root_id: i64,
    taken_at: i64,
    sequence: u64,
    hooks: &Hooks<'_>,
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
    // Re-run the catalog step even when the row has a photo id: it is
    // idempotent, and it yields the item the follow-up job needs.
    let cataloged =
        catalog_photo(catalog, root_id, &outcome, file, taken_at).map_err(|e| e.to_string())?;
    complete(
        catalog,
        row.id,
        &outcome,
        &cataloged,
        sequence,
        hooks.on_landed,
    )
}

/// The destination root of a folder rule: its `static_prefix`, tilde-expanded
/// and canonicalized when it exists. This is the path of the catalog root row
/// the ingest fills, so the contact sheet can open it while files land.
pub fn dest_root_path(template: &str) -> PathBuf {
    let mut root = expand_tilde(&static_prefix(template));
    if root.as_os_str().is_empty() {
        root = std::env::current_dir().unwrap_or_default();
    }
    root.canonicalize().unwrap_or(root)
}

/// Create the destination root and resolve it as [`dest_root_path`] does, so
/// the catalog root row matches how browse canonicalizes.
fn resolve_dest_root(template: &str) -> Result<String, IngestError> {
    let mut root = expand_tilde(&static_prefix(template));
    if root.as_os_str().is_empty() {
        root = std::env::current_dir()?;
    }
    fs::create_dir_all(&root)?;
    Ok(dest_root_path(template).to_string_lossy().into_owned())
}

/// What [`catalog_photo`] resolved for one landed file.
struct Cataloged {
    /// `None` only when a unique violation could not be resolved to a row.
    photo_id: Option<i64>,
    item_id: i64,
}

/// Insert the landed photo (or reuse the row a crash left behind), pair it
/// into an `items` row, and queue its thumbnail job.
fn catalog_photo(
    catalog: &Catalog,
    root_id: i64,
    outcome: &CopyOutcome,
    file: &CardFile,
    taken_at: i64,
) -> Result<Cataloged, edit_catalog::Error> {
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
    let item_id = match catalog.get_item_by_dir_base(&dir, &folded)? {
        None => catalog.insert_item(&dir, &folded, raw_id, jpeg_id)?,
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
            item.id
        }
    };

    if let Some(photo_id) = photo_id {
        let payload = serde_json::json!({ "photo_id": photo_id, "path": path });
        catalog.enqueue_job(JobKind::Thumb, &payload.to_string())?;
    }
    Ok(Cataloged { photo_id, item_id })
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
            iptc_fields_json: None,
            iptc_replacements_json: None,
            only_sources: None,
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

    fn lock(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o444)).unwrap();
    }

    #[test]
    fn the_scan_reports_files_the_camera_locked() {
        let card = card_with(&[
            ("DSC0001.NEF", b"locked one"),
            ("DSC0002.NEF", b"open"),
            ("DSC0003.JPG", b"locked jpg"),
        ]);
        let dcim = card.join("DCIM/100CANON");
        lock(&dcim.join("DSC0001.NEF"));
        lock(&dcim.join("DSC0003.JPG"));
        let files = enumerate_card(&card).unwrap();
        let locked: Vec<(&str, bool)> = files
            .iter()
            .map(|f| (f.original_name.as_str(), f.locked))
            .collect();
        assert_eq!(
            locked,
            [
                ("DSC0001.NEF", true),
                ("DSC0002.NEF", false),
                ("DSC0003.JPG", true)
            ]
        );
        fs::remove_dir_all(&card).ok();
    }

    #[test]
    fn locked_cards_land_writable_and_the_lock_stays_in_the_history() {
        use std::os::unix::fs::PermissionsExt;
        let card = card_with(&[("DSC0001.NEF", b"locked raw"), ("DSC0002.NEF", b"open raw")]);
        lock(&card.join("DCIM/100CANON/DSC0001.NEF"));
        let base = scratch("locked-copy");
        let dest = base.join("Photos");
        let backup = base.join("Backup");
        let catalog = Catalog::open(&base.join("catalog.sqlite")).unwrap();
        let batch = new_batch(&catalog, &dest, Some(&backup));
        run_ingest_copy(
            &catalog,
            &payload(batch, &card, &dest, Some(&backup)),
            fixed_metadata,
            &|_, _| {},
        )
        .unwrap();
        // The read-only attribute must not follow the file, or later
        // IPTC/XMP writes on the copy would fail.
        for root in [&dest, &backup] {
            for name in ["shoot_0001.NEF", "shoot_0002.NEF"] {
                let mode = fs::metadata(root.join("2026-10-07").join(name))
                    .unwrap()
                    .permissions()
                    .mode();
                assert!(
                    mode & 0o200 != 0,
                    "{name} is writable in {root:?}: {mode:o}"
                );
            }
        }
        let rows = catalog.list_ingest_files(batch).unwrap();
        let by_name = |name: &str| rows.iter().find(|r| r.original_name == name).unwrap();
        assert!(by_name("DSC0001.NEF").locked);
        assert!(!by_name("DSC0002.NEF").locked);
        fs::remove_dir_all(&base).ok();
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
                locked: false,
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

    /// A landed hook that queues one `metadata_write`-shaped job per photo.
    fn stamp_hook(landed: &LandedPhoto) -> FollowUpJob {
        Some((
            JobKind::MetadataWrite,
            serde_json::json!({
                "item_id": landed.item_id,
                "path": landed.path,
                "sequence": landed.sequence,
            })
            .to_string(),
        ))
    }

    fn stamp_jobs(catalog: &Catalog) -> Vec<serde_json::Value> {
        catalog
            .list_jobs(100)
            .unwrap()
            .into_iter()
            .filter(|job| job.kind == JobKind::MetadataWrite)
            .map(|job| serde_json::from_str(&job.payload_json).unwrap())
            .collect()
    }

    #[test]
    fn the_landed_hook_queues_one_job_per_photo_and_pairs_share_a_sequence() {
        let card = card_with(&[
            ("DSC0001.JPG", b"one jpg"),
            ("DSC0001.NEF", b"one raw"),
            ("DSC0002.JPG", b"two jpg"),
        ]);
        let base = scratch("hook");
        let dest = base.join("Photos");
        let catalog = Catalog::open(&base.join("catalog.sqlite")).unwrap();
        let batch = new_batch(&catalog, &dest, None);
        let p = payload(batch, &card, &dest, None);

        run_ingest_copy_with(&catalog, &p, fixed_metadata, &stamp_hook, &|_, _| {}).unwrap();
        let jobs = stamp_jobs(&catalog);
        assert_eq!(jobs.len(), 3);
        let seq_of = |suffix: &str| {
            jobs.iter()
                .find(|job| job["path"].as_str().unwrap().ends_with(suffix))
                .map(|job| {
                    (
                        job["sequence"].as_u64().unwrap(),
                        job["item_id"].as_i64().unwrap(),
                    )
                })
                .unwrap()
        };
        let (jpg_seq, jpg_item) = seq_of("shoot_0001.JPG");
        let (raw_seq, raw_item) = seq_of("shoot_0001.NEF");
        assert_eq!(jpg_seq, raw_seq);
        assert_eq!(jpg_item, raw_item, "a pair stamps both sides of one item");
        assert_ne!(seq_of("shoot_0002.JPG").0, jpg_seq);

        // A second pass over the same card finds only duplicates: no hook,
        // no new jobs.
        let again = new_batch(&catalog, &dest, None);
        let summary = run_ingest_copy_with(
            &catalog,
            &payload(again, &card, &dest, None),
            fixed_metadata,
            &stamp_hook,
            &|_, _| {},
        )
        .unwrap();
        assert_eq!(summary.duplicates, 3);
        assert_eq!(stamp_jobs(&catalog).len(), 3);

        fs::remove_dir_all(&base).ok();
        fs::remove_dir_all(&card).ok();
    }

    #[test]
    fn a_crash_after_rename_queues_the_hook_job_exactly_once_on_recovery() {
        let card = card_with(&[("DSC0001.JPG", b"recover me")]);
        let base = scratch("hook-recover");
        let dest = base.join("Photos");
        let catalog = Catalog::open(&base.join("catalog.sqlite")).unwrap();
        let batch = new_batch(&catalog, &dest, None);
        let p = payload(batch, &card, &dest, None);

        // The previous run landed the file and died before completing it:
        // no hook job exists yet.
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
                locked: false,
            })
            .unwrap();
        catalog
            .mark_ingest_file_landed(row, &landed.to_string_lossy(), None, &hash)
            .unwrap();
        assert!(stamp_jobs(&catalog).is_empty());

        let summary =
            run_ingest_copy_with(&catalog, &p, fixed_metadata, &stamp_hook, &|_, _| {}).unwrap();
        assert_eq!(summary.recovered, 1);
        let jobs = stamp_jobs(&catalog);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0]["path"].as_str().unwrap(), landed.to_string_lossy());

        // Resuming the finished batch again adds nothing.
        run_ingest_copy_with(&catalog, &p, fixed_metadata, &stamp_hook, &|_, _| {}).unwrap();
        assert_eq!(stamp_jobs(&catalog).len(), 1);

        fs::remove_dir_all(&base).ok();
        fs::remove_dir_all(&card).ok();
    }

    #[test]
    fn plan_ingest_freezes_the_chosen_template_onto_batch_and_payload() {
        let plan = plan_ingest(&IngestPlanRequest {
            template: Some(IngestTemplate {
                id: Some(7),
                fields_json: r#"{"credit":"SHL"}"#,
            }),
            ..plan_request_defaults()
        })
        .unwrap();
        assert_eq!(plan.batch.template_id, Some(7));

        // Typed-in fields have no template row behind them.
        let typed = plan_ingest(&IngestPlanRequest {
            template: Some(IngestTemplate {
                id: None,
                fields_json: r#"{"credit":"SHL"}"#,
            }),
            ..plan_request_defaults()
        })
        .unwrap();
        assert_eq!(typed.batch.template_id, None);
        assert_eq!(
            typed.payload.iptc_fields_json,
            plan.payload.iptc_fields_json
        );
        assert_eq!(
            plan.payload.iptc_fields_json.as_deref(),
            Some(r#"{"credit":"SHL"}"#)
        );

        let none = plan_ingest(&plan_request_defaults()).unwrap();
        assert_eq!(none.batch.template_id, None);
        assert_eq!(none.payload.iptc_fields_json, None);

        // A payload from before the snapshot existed still decodes.
        let old = r#"{"batch_id":1,"source_root":"/c","dest_dir_template":"/d","filename_template":"{seq}","backup_dir":null,"session":"s","start_seq":1}"#;
        let decoded: IngestCopyPayload = serde_json::from_str(old).unwrap();
        assert_eq!(decoded.iptc_fields_json, None);
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
            iptc_fields_json: None,
            iptc_replacements_json: None,
            only_sources: None,
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
            template: None,
            only_sources: None,
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
        // A blank folder rule is not an error: it means the default.
        assert_eq!(
            plan_ingest(&IngestPlanRequest {
                dest_dir_template: "   ",
                ..plan_request_defaults()
            })
            .unwrap()
            .payload
            .dest_dir_template,
            DEFAULT_DEST_RULE
        );
    }

    #[test]
    fn a_blank_filename_rule_plans_the_default_and_lands_named_files_with_extensions() {
        // The reported failure: the rule box was empty, so every file
        // rendered an empty name and nothing was copied.
        let card = card_with(&[
            ("DSC0001.JPG", b"one jpg"),
            ("DSC0001.NEF", b"one raw"),
            ("DSC0002.NEF", b"two raw"),
        ]);
        let base = scratch("blank-rule");
        let dest = base.join("Photos");
        let catalog = Catalog::open(&base.join("catalog.sqlite")).unwrap();
        let dest_rule = dest.join("{yyyy}-{mm}-{dd}").to_string_lossy().into_owned();
        for blank in ["", "   ", "\t"] {
            let plan = plan_ingest(&IngestPlanRequest {
                source_root: &card,
                dest_dir_template: &dest_rule,
                filename_template: blank,
                session: "shoot",
                ..plan_request_defaults()
            })
            .unwrap();
            assert_eq!(plan.payload.filename_template, DEFAULT_FILENAME_RULE);
        }

        let plan = plan_ingest(&IngestPlanRequest {
            source_root: &card,
            dest_dir_template: &dest_rule,
            filename_template: "",
            session: "shoot",
            ..plan_request_defaults()
        })
        .unwrap();
        let batch = catalog.create_ingest_batch(&plan.batch).unwrap();
        let summary = run_ingest_copy(
            &catalog,
            &IngestCopyPayload {
                batch_id: batch,
                ..plan.payload
            },
            fixed_metadata,
            &|_, _| {},
        )
        .unwrap();
        assert_eq!((summary.copied, summary.failed), (3, 0));

        // The RAW and the JPEG of one shot share a stem and differ by
        // extension, so they do not collide; the second shot is numbered on.
        let day = dest.join("2026-10-07");
        let mut names: Vec<String> = fs::read_dir(&day)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        // Blank means "keep the card's names", extension included.
        assert_eq!(names, ["DSC0001.JPG", "DSC0001.NEF", "DSC0002.NEF"]);
        fs::remove_dir_all(&base).ok();
        fs::remove_dir_all(&card).ok();
    }

    #[test]
    fn renaming_is_optional_and_a_renamed_pair_does_not_collide() {
        let card = card_with(&[
            ("DSC0001.JPG", b"one jpg"),
            ("DSC0001.NEF", b"one raw"),
            ("DSC0002.NEF", b"two raw"),
        ]);
        let base = scratch("rename");
        let dest = base.join("Photos");
        let catalog = Catalog::open(&base.join("catalog.sqlite")).unwrap();
        let plan = plan_ingest(&IngestPlanRequest {
            source_root: &card,
            dest_dir_template: &dest.to_string_lossy(),
            filename_template: crate::naming::RENAME_EXAMPLE_RULE,
            session: "shoot",
            ..plan_request_defaults()
        })
        .unwrap();
        let batch = catalog.create_ingest_batch(&plan.batch).unwrap();
        run_ingest_copy(
            &catalog,
            &IngestCopyPayload {
                batch_id: batch,
                ..plan.payload
            },
            fixed_metadata,
            &|_, _| {},
        )
        .unwrap();
        let mut names: Vec<String> = fs::read_dir(&dest)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            ["shoot_0001.JPG", "shoot_0001.NEF", "shoot_0002.NEF"]
        );
        fs::remove_dir_all(&base).ok();
        fs::remove_dir_all(&card).ok();
    }

    #[test]
    fn only_the_chosen_files_are_copied_and_the_rest_leave_no_trace() {
        let card = card_with(&[
            ("DSC0001.JPG", b"one"),
            ("DSC0002.JPG", b"two"),
            ("DSC0003.JPG", b"three"),
        ]);
        let base = scratch("only");
        let dest = base.join("Photos");
        let catalog = Catalog::open(&base.join("catalog.sqlite")).unwrap();
        let batch = new_batch(&catalog, &dest, None);
        let mut p = payload(batch, &card, &dest, None);
        let pick = |name: &str| {
            card.join("DCIM/100CANON")
                .join(name)
                .to_string_lossy()
                .into_owned()
        };
        p.only_sources = Some(vec![pick("DSC0001.JPG"), pick("DSC0003.JPG")]);
        let progress = std::cell::RefCell::new(Vec::new());
        let summary = run_ingest_copy(&catalog, &p, fixed_metadata, &|done, total| {
            progress.borrow_mut().push((done, total))
        })
        .unwrap();
        assert_eq!((summary.files, summary.copied, summary.failed), (2, 2, 0));
        assert_eq!(progress.borrow().last(), Some(&(2, 2)));
        let rows = catalog.list_ingest_files(batch).unwrap();
        assert_eq!(rows.len(), 2, "the skipped file is not recorded at all");
        assert!(rows.iter().all(|r| r.original_name != "DSC0002.JPG"));
        // Sequence numbers count the chosen files only.
        assert!(dest.join("2026-10-07/shoot_0001.JPG").is_file());
        assert!(dest.join("2026-10-07/shoot_0002.JPG").is_file());
        assert_eq!(catalog.get_ingest_batch(batch).unwrap().state, "done");
        fs::remove_dir_all(&base).ok();
        fs::remove_dir_all(&card).ok();
    }

    #[test]
    fn the_dest_root_path_is_the_catalog_root_the_ingest_fills() {
        let card = card_with(&[("DSC0001.JPG", b"one")]);
        let base = scratch("dest-root");
        let dest = base.join("Photos");
        let rule = dest.join("{yyyy}/{mm}").to_string_lossy().into_owned();
        let catalog = Catalog::open(&base.join("catalog.sqlite")).unwrap();
        // Not created yet: the helper must not create it.
        assert!(!dest.exists());
        let before = dest_root_path(&rule);
        assert!(!dest.exists());

        let batch = new_batch(&catalog, &dest, None);
        let mut p = payload(batch, &card, &dest, None);
        p.dest_dir_template = rule.clone();
        run_ingest_copy(&catalog, &p, fixed_metadata, &|_, _| {}).unwrap();
        let after = dest_root_path(&rule);
        assert_eq!(after, dest.canonicalize().unwrap());
        assert_eq!(before, dest, "uncanonicalized while it does not exist");
        assert!(
            catalog
                .get_root_by_path(&after.to_string_lossy())
                .unwrap()
                .is_some()
        );
        fs::remove_dir_all(&base).ok();
        fs::remove_dir_all(&card).ok();
    }

    #[test]
    fn a_rule_that_cannot_name_a_file_is_rejected_before_a_batch_exists() {
        for bad in ["a/b", "{nope}", "{seq:0}"] {
            assert!(
                plan_ingest(&IngestPlanRequest {
                    filename_template: bad,
                    ..plan_request_defaults()
                })
                .is_err(),
                "{bad}"
            );
        }
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
            template: None,
            only_sources: None,
        }
    }
}
