//! Ingest dialog (B3 shell + C2 live state) and the shared [`AppState`].
//!
//! Scope: volume list driven by `edit-ingest` detection events, destination
//! inputs as plain text, the IPTC template selector, and a live file table
//! with a per-file state column. C2 adds the **Start ingest** action: it opens
//! an `ingest_batches` row, enqueues one durable `ingest_copy` job per card
//! (multi-card parallel), and reflects the worker's per-file progress, live
//! file states and duplicates through the shared job-event drain.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use edit_catalog::models::{IngestFile, IptcTemplate};
use edit_catalog::{Catalog, JobEvent, JobEventKind, JobId, JobKind};
use edit_core::IngestFileState;
use edit_ingest::{IngestCopyPayload, Volume, batch_volume_identity};

use crate::assets::LOCK_ICON;
use crate::ingest_job::{IngestStart, saved_code_replacements, start_ingest};
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::alert::Alert;
use gpui_kit::component::button::{Button, ButtonGroup, ButtonVariants as _};
use gpui_kit::component::dialog::DialogClose;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::label::Label;
use gpui_kit::component::progress::Progress;
use gpui_kit::component::scroll::{ScrollableElement as _, Scrollbar};
use gpui_kit::component::searchable_list::SearchableVec;
use gpui_kit::component::select::{Select, SelectState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Selectable as _, Sizable as _,
    StyledExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

actions!(ingest, [OpenIngest]);

/// One row of the live file table, built from an `ingest_files` row.
///
/// The state is `edit-core`'s [`IngestFileState`], the same type the catalog
/// stores and the worker writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestFileRow {
    pub name: String,
    pub bytes: u64,
    pub state: IngestFileState,
}

impl IngestFileRow {
    /// Snapshot one durable ingest-history row for the dialog.
    pub fn from_db(file: &IngestFile) -> Self {
        Self {
            name: file.original_name.clone(),
            bytes: file.bytes.max(0) as u64,
            state: file.state,
        }
    }
}

/// Plain wording for a file's state in the table.
pub fn state_label(state: IngestFileState) -> &'static str {
    match state {
        IngestFileState::Queued => "Waiting",
        IngestFileState::Copying => "Copying",
        IngestFileState::Verifying => "Checking",
        IngestFileState::Backup => "Backing up",
        IngestFileState::Renaming => "Finishing",
        IngestFileState::Cataloged => "Copied",
        IngestFileState::Duplicate => "Duplicate",
        IngestFileState::Failed => "Failed",
    }
}

/// Lifecycle of one card's ingest batch, for the per-card progress bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchState {
    Running,
    Done,
    Failed,
}

/// What happened to the files of one batch, counted from the ingest history.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileTally {
    pub cataloged: usize,
    pub duplicates: usize,
    pub failed: usize,
    /// The first recorded failure reason, in file order.
    pub first_error: Option<String>,
}

impl FileTally {
    pub fn from_rows(rows: &[IngestFile]) -> Self {
        let mut tally = Self::default();
        for row in rows {
            match row.state {
                IngestFileState::Cataloged => tally.cataloged += 1,
                IngestFileState::Duplicate => tally.duplicates += 1,
                IngestFileState::Failed => {
                    tally.failed += 1;
                    if tally.first_error.is_none() {
                        tally.first_error = row.error.clone();
                    }
                }
                _ => {}
            }
        }
        tally
    }
}

/// Per-card progress shown next to a detected volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestBatchProgress {
    pub batch_id: i64,
    pub volume_identity: String,
    pub label: String,
    pub done: usize,
    pub total: usize,
    pub state: BatchState,
    pub tally: FileTally,
}

impl IngestBatchProgress {
    /// Percentage for the per-card progress bar (0 with no files planned yet).
    pub fn percent(&self) -> f32 {
        if self.total == 0 {
            0.0
        } else {
            (self.done as f32 / self.total as f32) * 100.0
        }
    }

    /// One plain line for the card: what landed, what was skipped, and what
    /// failed. A finished batch never reads as done for files that failed.
    pub fn summary(&self) -> String {
        let tally = &self.tally;
        if self.state == BatchState::Running {
            let mut line = format!("Copying {} of {}", self.done, self.total);
            if tally.failed > 0 {
                line.push_str(&format!(", {} failed", tally.failed));
            }
            return line;
        }
        let mut parts = Vec::new();
        if tally.cataloged > 0 {
            parts.push(format!("{} copied", tally.cataloged));
        }
        if tally.duplicates > 0 {
            parts.push(format!("{} duplicates", tally.duplicates));
        }
        if tally.failed > 0 {
            parts.push(format!("{} failed", tally.failed));
        }
        if parts.is_empty() {
            "Nothing to copy".to_string()
        } else {
            parts.join(", ")
        }
    }
}

/// Shared application state (plan section 2.10): detected volumes, the ingest
/// dialog visibility, per-card batch progress, and the live file rows.
pub struct AppState {
    volumes: Vec<Volume>,
    batches: Vec<IngestBatchProgress>,
    files_by_batch: HashMap<i64, Vec<IngestFileRow>>,
    job_batches: HashMap<JobId, i64>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            volumes: Vec::new(),
            batches: Vec::new(),
            files_by_batch: HashMap::new(),
            job_batches: HashMap::new(),
        }
    }

    pub fn volumes(&self) -> &[Volume] {
        &self.volumes
    }

    /// File rows of the newest batch for one source.
    pub fn files_for_volume(&self, identity: &str) -> Vec<IngestFileRow> {
        self.batch_for_volume(identity)
            .and_then(|batch| self.files_by_batch.get(&batch.batch_id))
            .cloned()
            .unwrap_or_default()
    }

    /// Whether any card is still ingesting.
    pub fn has_running_batch(&self) -> bool {
        self.batches
            .iter()
            .any(|batch| batch.state == BatchState::Running)
    }

    /// Progress of the card's newest batch (a retry opens a new one).
    pub fn batch_for_volume(&self, identity: &str) -> Option<&IngestBatchProgress> {
        self.batches
            .iter()
            .rev()
            .find(|batch| batch.volume_identity == identity)
    }

    /// Replace the volume list from a detection event. Volumes are keyed by
    /// filesystem UUID (never the label); see `edit-ingest`.
    pub fn set_volumes(&mut self, volumes: Vec<Volume>, cx: &mut Context<Self>) {
        if volumes != self.volumes {
            self.volumes = volumes;
            cx.notify();
        }
    }

    /// Remember the batch a queued `ingest_copy` job belongs to, so progress
    /// events can be routed to the right card.
    pub fn register_batch(
        &mut self,
        batch_id: i64,
        job_id: JobId,
        volume_identity: String,
        label: String,
        cx: &mut Context<Self>,
    ) {
        self.job_batches.insert(job_id, batch_id);
        self.batches.push(IngestBatchProgress {
            batch_id,
            volume_identity,
            label,
            done: 0,
            total: 0,
            state: BatchState::Running,
            tally: FileTally::default(),
        });
        cx.notify();
    }

    /// The batch a job id belongs to, if it is an ingest job.
    pub fn batch_for_job(&self, job_id: JobId) -> Option<i64> {
        self.job_batches.get(&job_id).copied()
    }

    pub fn set_progress(
        &mut self,
        batch_id: i64,
        done: usize,
        total: usize,
        cx: &mut Context<Self>,
    ) {
        if let Some(batch) = self.batches.iter_mut().find(|b| b.batch_id == batch_id) {
            batch.done = done;
            batch.total = total;
            cx.notify();
        }
    }

    pub fn finish_batch(&mut self, batch_id: i64, failed: bool, cx: &mut Context<Self>) {
        if let Some(batch) = self.batches.iter_mut().find(|b| b.batch_id == batch_id) {
            batch.state = if failed {
                BatchState::Failed
            } else {
                BatchState::Done
            };
            if failed {
                batch.total = batch.total.max(batch.done);
            }
            cx.notify();
        }
    }

    pub fn set_files(
        &mut self,
        batch_id: i64,
        rows: Vec<IngestFileRow>,
        tally: FileTally,
        cx: &mut Context<Self>,
    ) {
        self.files_by_batch.insert(batch_id, rows);
        if let Some(batch) = self.batches.iter_mut().find(|b| b.batch_id == batch_id) {
            batch.tally = tally;
        }
        cx.notify();
    }

    /// Rebuild the card/job routing from durable state on startup, so an
    /// ingest that was `running` when the app died shows up and its batch
    /// leaves `running` when the resumed job finishes.
    pub fn resume_batches(&mut self, catalog: &Catalog, cx: &mut Context<Self>) {
        let jobs = match catalog.list_active_ingest_jobs() {
            Ok(jobs) => jobs,
            Err(_) => return,
        };
        let mut changed = false;
        for job in jobs {
            if self.job_batches.contains_key(&job.id) {
                continue;
            }
            let Ok(payload) = serde_json::from_str::<IngestCopyPayload>(&job.payload_json) else {
                continue;
            };
            let (identity, label) = batch_card_label(catalog, payload.batch_id);
            self.job_batches.insert(job.id, payload.batch_id);
            self.batches.push(IngestBatchProgress {
                batch_id: payload.batch_id,
                volume_identity: identity,
                label,
                done: 0,
                total: 0,
                state: BatchState::Running,
                tally: FileTally::default(),
            });
            changed = true;
        }
        if changed {
            cx.notify();
        }
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

/// Apply one durable job event to the ingest UI.
///
/// An unknown ingest job (a crash-resumed job from a previous process) is
/// registered here from its durable payload, so a resumed ingest is visible
/// without a startup scan. `ingest_files` is only re-read on Started, on the
/// first and final progress event, and on the terminal event, never on every
/// progress tick.
pub fn apply_ingest_event(
    state: &mut AppState,
    catalog: &Catalog,
    event: &JobEvent,
    cx: &mut Context<AppState>,
) {
    let Some(batch_id) = batch_for_event(state, catalog, event.id, cx) else {
        return;
    };
    let reload = match &event.kind {
        JobEventKind::Started { .. } => true,
        JobEventKind::Progress { done, total } => {
            state.set_progress(batch_id, *done as usize, *total as usize, cx);
            *done == 0 || done == total
        }
        JobEventKind::Succeeded => {
            state.finish_batch(batch_id, false, cx);
            true
        }
        JobEventKind::Failed { .. } => {
            state.finish_batch(batch_id, true, cx);
            true
        }
    };
    if reload && let Ok(files) = catalog.list_ingest_files(batch_id) {
        let rows: Vec<IngestFileRow> = files.iter().map(IngestFileRow::from_db).collect();
        state.set_files(batch_id, rows, FileTally::from_rows(&files), cx);
    }
}

/// The batch a job event belongs to, registering an ingest job seen for the
/// first time (e.g. resumed from a previous process).
fn batch_for_event(
    state: &mut AppState,
    catalog: &Catalog,
    job_id: JobId,
    cx: &mut Context<AppState>,
) -> Option<i64> {
    if let Some(batch_id) = state.batch_for_job(job_id) {
        return Some(batch_id);
    }
    let job = catalog.job(job_id).ok()?;
    if job.kind != JobKind::IngestCopy {
        return None;
    }
    let payload: IngestCopyPayload = serde_json::from_str(&job.payload_json).ok()?;
    let (identity, label) = batch_card_label(catalog, payload.batch_id);
    state.register_batch(payload.batch_id, job_id, identity, label, cx);
    Some(payload.batch_id)
}

/// Volume identity and display label for a batch, used when registering a
/// resumed job.
fn batch_card_label(catalog: &Catalog, batch_id: i64) -> (String, String) {
    match catalog.get_ingest_batch(batch_id) {
        Ok(batch) => (
            batch_volume_identity(&batch),
            batch
                .session_name
                .clone()
                .or_else(|| batch.volume_label.clone())
                .unwrap_or_else(|| "card".to_string()),
        ),
        Err(_) => (String::new(), "card".to_string()),
    }
}

/// Format a byte count for the card list and file table (binary units).
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Template choice meaning "ingest without stamping".
const NO_TEMPLATE: &str = "None";

/// What to copy from the card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyScope {
    All,
    /// Only photos locked (protected) in the camera.
    Locked,
}

/// One photo on the chosen card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardPhoto {
    pub path: String,
    pub name: String,
    pub bytes: u64,
    /// Locked in the camera; always false when the card is read-only, as
    /// every file looks read-only then and the lock cannot be told.
    pub locked: bool,
}

/// Reading the chosen card's file list happens off the UI thread.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CardScan {
    Idle,
    Reading,
    Failed(String),
}

/// Photos the ingest would copy: every photo (`None`), or only the locked ones.
pub fn chosen_sources(photos: &[CardPhoto], scope: CopyScope) -> Option<Vec<String>> {
    match scope {
        CopyScope::All => None,
        CopyScope::Locked => Some(
            photos
                .iter()
                .filter(|photo| photo.locked)
                .map(|photo| photo.path.clone())
                .collect(),
        ),
    }
}

/// Ingest dialog body (plan section 2.8), hosted by a modal `Dialog`: the
/// source (card or folder), what to copy (all, or only photos locked in the
/// camera), where it goes, optional renaming, an optional IPTC template, and
/// the file list. The footer is
/// [`IngestFooter`].
pub struct IngestDialog {
    state: Entity<AppState>,
    catalog: Arc<Catalog>,
    /// Identity of the chosen source, if any.
    pub selected: Option<String>,
    /// Folders the user picked with "Choose folder…".
    folders: Vec<Volume>,
    photos: Vec<CardPhoto>,
    /// Whether lock marks on this source can be trusted (not a read-only card).
    locks_known: bool,
    scan: CardScan,
    scan_task: Option<Task<()>>,
    pub scope: CopyScope,
    pub rename: bool,
    pub dest_dir: Entity<InputState>,
    pub file_rule: Entity<InputState>,
    pub backup_dir: Entity<InputState>,
    template_select: Entity<SelectState<SearchableVec<String>>>,
    /// Template rows behind the selector, refreshed each time the dialog opens.
    templates: Vec<IptcTemplate>,
    files_scroll: UniformListScrollHandle,
    /// After Start the list shows the ingest's progress instead of tags.
    show_progress: bool,
    status: Option<String>,
    /// Whether `status` reports a rejected request (shown as an error).
    status_is_error: bool,
}

impl IngestDialog {
    pub fn new(
        state: Entity<AppState>,
        catalog: Arc<Catalog>,
        dest_dir: Entity<InputState>,
        file_rule: Entity<InputState>,
        backup_dir: Entity<InputState>,
        template_select: Entity<SelectState<SearchableVec<String>>>,
        cx: &mut Context<Self>,
    ) -> Self {
        // Cards come and go and progress events arrive; repaint on both.
        cx.observe(&state, |_, _, cx| cx.notify()).detach();
        Self {
            state,
            catalog,
            selected: None,
            folders: Vec::new(),
            photos: Vec::new(),
            locks_known: true,
            scan: CardScan::Idle,
            scan_task: None,
            scope: CopyScope::All,
            rename: false,
            dest_dir,
            file_rule,
            backup_dir,
            template_select,
            templates: Vec::new(),
            files_scroll: UniformListScrollHandle::new(),
            show_progress: false,
            status: None,
            status_is_error: false,
        }
    }

    /// Called each time the dialog opens: fresh template list, no stale
    /// message, and a lone card is chosen for the user.
    pub fn prepare(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.reload_templates(window, cx);
        self.status = None;
        self.show_progress = false;
        let sources = self.sources(cx);
        let still_there = self
            .selected
            .as_deref()
            .is_some_and(|id| sources.iter().any(|source| source.identity == id));
        if !still_there {
            self.selected = None;
            self.photos.clear();
            self.scan = CardScan::Idle;
            if let [only] = sources.as_slice() {
                self.select(only.identity.clone(), cx);
            }
        }
        cx.notify();
    }

    /// Cards and external drives, plus folders chosen by hand.
    fn sources(&self, cx: &App) -> Vec<Volume> {
        let mut sources: Vec<Volume> = self
            .state
            .read(cx)
            .volumes()
            .iter()
            .filter(|volume| volume.is_ingest_source())
            .cloned()
            .collect();
        sources.extend(self.folders.iter().cloned());
        sources
    }

    fn set_status(&mut self, is_error: bool, message: String) {
        self.status = Some(message);
        self.status_is_error = is_error;
    }

    /// Photos locked in the camera.
    pub fn locked_count(&self) -> usize {
        self.photos.iter().filter(|photo| photo.locked).count()
    }

    /// Whether "Only locked photos" can be chosen, and why not.
    pub fn locked_unavailable(&self) -> Option<&'static str> {
        if !self.locks_known {
            Some("Locks can't be read from a read-only card.")
        } else if self.locked_count() == 0 {
            Some("No photos are locked in the camera.")
        } else {
            None
        }
    }

    /// How many photos the primary button would copy.
    pub fn photo_count(&self) -> usize {
        match self.scope {
            CopyScope::All => self.photos.len(),
            CopyScope::Locked => self.locked_count(),
        }
    }

    /// Label and enabled state of the primary footer button.
    pub fn primary(&self) -> (String, bool) {
        let count = self.photo_count();
        let busy = matches!(self.scan, CardScan::Reading);
        let label = match count {
            _ if busy || self.selected.is_none() || count == 0 => "Ingest".to_string(),
            1 => "Ingest 1 photo".to_string(),
            n => format!("Ingest {n} photos"),
        };
        (label, self.selected.is_some() && !busy && count > 0)
    }

    /// Whether the chosen source has a finished batch with failed files.
    pub fn can_retry(&self, cx: &App) -> bool {
        self.selected.as_deref().is_some_and(|identity| {
            self.state
                .read(cx)
                .batch_for_volume(identity)
                .is_some_and(|batch| batch.state != BatchState::Running && batch.tally.failed > 0)
        })
    }

    /// Whether the chosen source is still being copied.
    pub fn is_running(&self, cx: &App) -> bool {
        self.selected.as_deref().is_some_and(|identity| {
            self.state
                .read(cx)
                .batch_for_volume(identity)
                .is_some_and(|batch| batch.state == BatchState::Running)
        })
    }

    /// Choose a source and read its photo list in the background.
    pub fn select(&mut self, identity: String, cx: &mut Context<Self>) {
        let Some(source) = self
            .sources(cx)
            .into_iter()
            .find(|source| source.identity == identity)
        else {
            return;
        };
        self.selected = Some(identity);
        self.photos.clear();
        self.locks_known = !source.read_only;
        self.scan = CardScan::Reading;
        self.show_progress = false;
        let root = source.mount_point.clone();
        let wanted = source.identity.clone();
        self.scan_task = Some(cx.spawn(async move |this, cx| {
            let found = cx
                .background_executor()
                .spawn(async move { edit_ingest::enumerate_card(&root) })
                .await;
            this.update(cx, |this, cx| {
                // A newer choice replaced this one while it was reading.
                if this.selected.as_deref() != Some(wanted.as_str()) {
                    return;
                }
                match found {
                    Ok(files) => {
                        let known = this.locks_known;
                        this.photos = files
                            .into_iter()
                            .map(|file| CardPhoto {
                                path: file.path.to_string_lossy().into_owned(),
                                name: file.original_name,
                                bytes: file.bytes,
                                locked: known && file.locked,
                            })
                            .collect();
                        this.scan = CardScan::Idle;
                        if this.locked_unavailable().is_some() {
                            this.scope = CopyScope::All;
                        }
                    }
                    Err(error) => this.scan = CardScan::Failed(error.to_string()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Pick any folder as an ingest source.
    fn choose_folder(&mut self, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose folder".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = picked.await
                && let Some(path) = paths.into_iter().next()
            {
                this.update(cx, |this, cx| {
                    let folder = Volume::folder(&path);
                    let identity = folder.identity.clone();
                    if !this.folders.iter().any(|known| known.identity == identity) {
                        this.folders.push(folder);
                    }
                    this.select(identity, cx);
                })
                .ok();
            }
        })
        .detach();
    }

    pub fn set_scope(&mut self, scope: CopyScope, cx: &mut Context<Self>) {
        if scope == CopyScope::Locked && self.locked_unavailable().is_some() {
            return;
        }
        self.scope = scope;
        cx.notify();
    }

    pub fn set_rename(&mut self, rename: bool, cx: &mut Context<Self>) {
        self.rename = rename;
        cx.notify();
    }

    /// Reload the template rows and rebuild the selector. No template is
    /// required: the choice is "None" unless the user picked one earlier
    /// and it still exists.
    fn reload_templates(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.templates = self.catalog.list_iptc_templates().unwrap_or_default();
        self.templates.sort_by_key(|row| row.name.to_lowercase());
        let previous = self.template_select.read(cx).selected_value().cloned();
        let mut names = vec![NO_TEMPLATE.to_string()];
        names.extend(self.templates.iter().map(|row| row.name.clone()));
        let pick = previous
            .filter(|name| names.contains(name))
            .unwrap_or_else(|| NO_TEMPLATE.to_string());
        self.template_select.update(cx, |select, cx| {
            select.set_items(SearchableVec::new(names), window, cx);
            select.set_selected_value(&pick, window, cx);
        });
    }

    fn selected_template(&self, cx: &App) -> Option<IptcTemplate> {
        let name = self.template_select.read(cx).selected_value().cloned()?;
        self.templates.iter().find(|row| row.name == name).cloned()
    }

    /// Open an ingest batch for the chosen source and queue its copy job.
    /// Clicking Retry after a failure runs this again: files already
    /// ingested are skipped as duplicates and failed ones are copied again.
    ///
    /// Enumeration, copying and dedupe all run inside the `ingest_copy` job,
    /// so this never blocks the UI thread.
    pub fn start(&mut self, cx: &mut Context<Self>) {
        let Some(identity) = self.selected.clone() else {
            self.set_status(true, "Choose a card or folder first.".to_string());
            cx.notify();
            return;
        };
        if self.photo_count() == 0 {
            self.set_status(true, "Tag at least one photo to copy.".to_string());
            cx.notify();
            return;
        }
        if self.is_running(cx) {
            return;
        }
        let Some(source) = self
            .sources(cx)
            .into_iter()
            .find(|source| source.identity == identity)
        else {
            self.set_status(true, "The card was removed.".to_string());
            cx.notify();
            return;
        };
        let dest = self.dest_dir.read(cx).value().to_string();
        // Without "Rename files" the card's own names are kept.
        let file_rule = if self.rename {
            self.file_rule.read(cx).value().to_string()
        } else {
            String::new()
        };
        let backup = self.backup_dir.read(cx).value().to_string();
        let session = source.display_name();
        let template = self.selected_template(cx);
        let replacements = saved_code_replacements(&self.catalog);
        let only = chosen_sources(&self.photos, self.scope);
        let started = start_ingest(
            &self.catalog,
            &IngestStart {
                volume_uuid: source.fs_uuid.as_deref(),
                volume_label: source.label.as_deref(),
                source_root: &source.mount_point,
                dest_rule: &dest,
                filename_rule: &file_rule,
                backup_dir: &backup,
                session: &session,
                template: template.as_ref(),
                replacements: replacements.as_ref(),
                only_sources: only.as_deref(),
            },
        );
        match started {
            Ok(started) => {
                self.state.update(cx, |state, cx| {
                    state.register_batch(started.batch_id, started.job_id, identity, session, cx);
                });
                self.show_progress = true;
                self.set_status(false, format!("Copying from {}…", source.display_name()));
            }
            Err(message) => self.set_status(true, message),
        }
        cx.notify();
    }

    fn render_sources(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let sources = self.sources(cx);
        let selected = self.selected.clone();
        let state = self.state.read(cx);
        let border = cx.theme().border;
        let accent = cx.theme().tokens.list_active_border.color;
        let rows = sources.into_iter().map(|source| {
            let identity = source.identity.clone();
            let is_selected = selected.as_deref() == Some(identity.as_str());
            let batch = state.batch_for_volume(&identity).cloned();
            let (percent, caption, failure) = match batch {
                Some(batch) => (
                    Some(batch.percent()),
                    batch.summary(),
                    batch
                        .tally
                        .first_error
                        .clone()
                        .map(|error| format!("First error: {error}")),
                ),
                None if source.capacity_bytes > 0 => (
                    None,
                    format!(
                        "{} free of {}",
                        format_bytes(source.free_bytes),
                        format_bytes(source.capacity_bytes)
                    ),
                    None,
                ),
                None => (None, String::new(), None),
            };
            let click_identity = identity.clone();
            v_flex()
                .id(format!("source-{identity}"))
                .gap_1()
                .p_2()
                .rounded_md()
                .border_1()
                .border_color(if is_selected { accent } else { border })
                .cursor_pointer()
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.select(click_identity.clone(), cx);
                }))
                .child(
                    h_flex()
                        .w_full()
                        .justify_between()
                        .child(Label::new(source.display_name()).text_sm().font_semibold())
                        .child(
                            Label::new(source.mount_point.display().to_string())
                                .text_xs()
                                .text_color(cx.theme().muted_foreground),
                        ),
                )
                .when_some(percent, |row, percent| {
                    row.child(Progress::new(format!("progress-{identity}")).value(percent))
                })
                .when(!caption.is_empty(), |row| {
                    row.child(
                        Label::new(caption)
                            .text_xs()
                            .text_color(cx.theme().muted_foreground),
                    )
                })
                .when_some(failure, |row, failure| {
                    row.child(Alert::error(format!("failure-{identity}"), failure))
                })
        });
        let empty =
            state.volumes().iter().all(|v| !v.is_ingest_source()) && self.folders.is_empty();
        v_flex()
            .gap_2()
            .children(rows)
            .when(empty, |list| {
                list.child(
                    Label::new("No cards found. Insert a card or choose a folder.")
                        .text_sm()
                        .text_color(cx.theme().muted_foreground),
                )
            })
            .child(
                h_flex().child(
                    Button::new("choose-folder")
                        .small()
                        .outline()
                        .icon(IconName::FolderOpen)
                        .label("Choose folder…")
                        .on_click(cx.listener(|this, _, _, cx| this.choose_folder(cx))),
                ),
            )
    }

    fn render_scope(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let locked_on = self.scope == CopyScope::Locked;
        let unavailable = self.locked_unavailable();
        // Each button handles its own click, so Space and Enter work too.
        let locked = Button::new("scope-locked")
            .label("Only locked photos")
            .selected(locked_on)
            .disabled(unavailable.is_some())
            .on_click(cx.listener(|this, _, _, cx| this.set_scope(CopyScope::Locked, cx)));
        let locked = match unavailable {
            Some(why) => locked.tooltip(why),
            None => locked,
        };
        ButtonGroup::new("copy-scope")
            .outline()
            .child(
                Button::new("scope-all")
                    .label("All photos")
                    .selected(!locked_on)
                    .on_click(cx.listener(|this, _, _, cx| this.set_scope(CopyScope::All, cx))),
            )
            .child(locked)
    }

    /// The photo list with lock marks, or the ingest's progress after Start.
    fn render_files(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        if self.show_progress {
            let files = self
                .selected
                .as_deref()
                .map(|identity| self.state.read(cx).files_for_volume(identity))
                .unwrap_or_default();
            return v_flex()
                .gap_2()
                .child(Label::new("Files").text_sm().font_semibold())
                .child(
                    v_flex()
                        .h(rems(14.))
                        .overflow_y_scrollbar()
                        .gap_1()
                        .children(files.into_iter().map(|row| {
                            h_flex()
                                .w_full()
                                .gap_4()
                                .child(Label::new(row.name).text_sm().w_1_2())
                                .child(
                                    Label::new(format_bytes(row.bytes))
                                        .text_sm()
                                        .text_color(muted)
                                        .w_1_4(),
                                )
                                .child(Label::new(state_label(row.state)).text_sm().w_1_4())
                        })),
                )
                .into_any_element();
        }
        match &self.scan {
            CardScan::Reading => {
                return Label::new("Reading the card…")
                    .text_sm()
                    .text_color(muted)
                    .into_any_element();
            }
            CardScan::Failed(error) => {
                return Alert::error("scan-failed", format!("Could not read the card: {error}"))
                    .into_any_element();
            }
            CardScan::Idle if self.selected.is_none() => {
                return Label::new("Choose a card or folder to see its photos.")
                    .text_sm()
                    .text_color(muted)
                    .into_any_element();
            }
            CardScan::Idle => {}
        }
        let count = self.photos.len();
        let summary = if self.locks_known {
            format!("{} of {count} locked in camera", self.locked_count())
        } else {
            let total: u64 = self.photos.iter().map(|photo| photo.bytes).sum();
            format!("{count} photos, {}", format_bytes(total))
        };
        let only_locked = self.scope == CopyScope::Locked;
        v_flex()
            .gap_2()
            .child(Label::new(summary).text_sm().font_semibold())
            .child(
                div()
                    .relative()
                    .h(rems(14.))
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded_md()
                    .child(
                        uniform_list(
                            "card-photos",
                            count,
                            cx.processor(move |this, range: Range<usize>, _window, cx| {
                                let muted = cx.theme().muted_foreground;
                                range
                                    .filter_map(|index| {
                                        let photo = this.photos.get(index)?;
                                        // Photos the scope leaves out read as muted.
                                        let skipped = only_locked && !photo.locked;
                                        Some(
                                            h_flex()
                                                .id(("photo", index))
                                                .h_8()
                                                .px_2()
                                                .gap_2()
                                                .items_center()
                                                .child(div().w_4().when(photo.locked, |slot| {
                                                    slot.child(
                                                        Icon::empty()
                                                            .path(LOCK_ICON)
                                                            .small()
                                                            .text_color(muted),
                                                    )
                                                }))
                                                .child(
                                                    Label::new(photo.name.clone())
                                                        .text_sm()
                                                        .flex_1()
                                                        .when(skipped, |label| {
                                                            label.text_color(muted)
                                                        }),
                                                )
                                                .child(
                                                    Label::new(format_bytes(photo.bytes))
                                                        .text_sm()
                                                        .text_color(muted),
                                                ),
                                        )
                                    })
                                    .collect::<Vec<_>>()
                            }),
                        )
                        .size_full()
                        .track_scroll(&self.files_scroll),
                    )
                    .child(Scrollbar::vertical(&self.files_scroll)),
            )
            .into_any_element()
    }

    fn section(&self, title: &str, child: impl IntoElement) -> impl IntoElement {
        v_flex()
            .gap_2()
            .child(Label::new(title.to_string()).text_sm().font_semibold())
            .child(child)
    }

    fn field(&self, label: &str, input: &Entity<InputState>, id: &'static str) -> impl IntoElement {
        v_flex()
            .gap_1()
            .child(Label::new(label.to_string()).text_sm())
            .child(Input::new(input).id(id))
    }
}

impl Render for IngestDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // A progress note is only true while something is ingesting; a
        // rejection stays until the next action.
        let busy = self.state.read(cx).has_running_batch();
        let status = self
            .status
            .clone()
            .filter(|_| self.status_is_error || busy)
            .map(|message| (self.status_is_error, message));
        let rename_on = self.rename;
        let sources = self.render_sources(cx);
        let scope = self.render_scope(cx);
        v_flex()
            .w_full()
            .gap_5()
            .pt_3()
            .pb_2()
            .child(self.section("Source", sources))
            .child(self.section("Copy", scope))
            .child(self.render_files(cx))
            .child(
                v_flex()
                    .gap_3()
                    .child(self.field("Destination folder", &self.dest_dir, "ingest-dest"))
                    .child(
                        Switch::new("rename-files")
                            .label("Rename files")
                            .checked(rename_on)
                            .on_change(cx.listener(|this, next: &bool, _, cx| {
                                this.set_rename(*next, cx);
                            })),
                    )
                    .when(rename_on, |col| {
                        col.child(self.field("Filename rule", &self.file_rule, "ingest-rule"))
                    })
                    .child(self.field("Back up to (optional)", &self.backup_dir, "ingest-backup")),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(Label::new("IPTC template").text_sm())
                    .child(Select::new(&self.template_select).placeholder(NO_TEMPLATE)),
            )
            .when_some(status, |panel, (is_error, message)| {
                panel.child(if is_error {
                    Alert::error("ingest-status", message)
                } else {
                    Alert::info("ingest-status", message)
                })
            })
    }
}

/// Dialog footer: Cancel and the primary action, which names what it will
/// copy. A separate view so the button follows the dialog's tag counts.
pub struct IngestFooter {
    dialog: Entity<IngestDialog>,
}

impl IngestFooter {
    pub fn new(dialog: Entity<IngestDialog>, cx: &mut Context<Self>) -> Self {
        cx.observe(&dialog, |_, _, cx| cx.notify()).detach();
        Self { dialog }
    }
}

impl Render for IngestFooter {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (label, enabled) = self.dialog.read(cx).primary();
        let running = self.dialog.read(cx).is_running(cx);
        let can_retry = self.dialog.read(cx).can_retry(cx);
        let dialog = self.dialog.clone();
        let retry_dialog = self.dialog.clone();
        h_flex()
            .w_full()
            .justify_end()
            .gap_2()
            .child(div().child(
                DialogClose::new().child(Button::new("ingest-cancel").label("Close").outline()),
            ))
            .when(can_retry, |row| {
                row.child(
                    Button::new("ingest-retry")
                        .label("Retry failed")
                        .outline()
                        .on_click(move |_, _, cx| {
                            retry_dialog.update(cx, |dialog, cx| dialog.start(cx));
                        }),
                )
            })
            .child(
                Button::new("ingest-start")
                    .label(label)
                    .primary()
                    .disabled(!enabled || running)
                    .on_click(move |_, _, cx| {
                        dialog.update(cx, |dialog, cx| dialog.start(cx));
                    }),
            )
    }
}

/// Open the ingest dialog as a modal over the window: backdrop, focus trap,
/// Escape, and a scrolling body come from the library's dialog layer.
pub fn open_ingest_dialog(
    dialog: &Entity<IngestDialog>,
    footer: &Entity<IngestFooter>,
    window: &mut Window,
    cx: &mut App,
) {
    if window.has_active_dialog(cx) {
        return;
    }
    dialog.update(cx, |dialog, cx| dialog.prepare(window, cx));
    let (body, footer) = (dialog.clone(), footer.clone());
    window.open_dialog(cx, move |dialog, _, _| {
        let confirm = body.clone();
        dialog
            .title("Ingest")
            // Enter starts the ingest and the dialog stays open to show it.
            .on_ok(move |_, _, cx| {
                confirm.update(cx, |dialog, cx| dialog.start(cx));
                false
            })
            .w(px(736.))
            .margin_top(px(32.))
            .child(body.clone())
            .footer(footer.clone())
    });
}

#[cfg(test)]
mod tests {
    // NOTE: import items explicitly, never `super::*` — the `gpui_kit::*`
    // glob re-exported above includes GPUI's `test` macro, which would shadow
    // the built-in `#[test]` attribute inside this module.
    use super::{
        BatchState, FileTally, IngestBatchProgress, IngestFileRow, format_bytes, state_label,
    };
    use edit_catalog::models::IngestFile;
    use edit_core::IngestFileState;

    #[test]
    fn byte_counts_format_with_binary_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(48 * 1024 * 1024 * 1024), "48.0 GiB");
    }

    #[test]
    fn file_rows_snapshot_history() {
        let row = IngestFileRow::from_db(&IngestFile {
            id: 1,
            batch_id: 2,
            original_name: "DSC0001.NEF".into(),
            taken_at: Some(1),
            bytes: 2048,
            source_path: "/card/DCIM/DSC0001.NEF".into(),
            dest_path: None,
            backup_path: None,
            blake3: None,
            photo_id: None,
            state: IngestFileState::Duplicate,
            error: None,
            created_at: 0,
            locked: false,
        });
        assert_eq!(row.name, "DSC0001.NEF");
        assert_eq!(row.bytes, 2048);
        assert_eq!(row.state, IngestFileState::Duplicate);
    }

    #[test]
    fn every_file_state_has_a_plain_label() {
        for state in IngestFileState::all() {
            let label = state_label(*state);
            assert!(label.chars().next().unwrap().is_uppercase(), "{label}");
        }
        assert_eq!(state_label(IngestFileState::Cataloged), "Copied");
    }

    #[test]
    fn batch_state_labels_are_distinct() {
        assert_ne!(BatchState::Running, BatchState::Done);
        assert_ne!(BatchState::Done, BatchState::Failed);
    }

    fn file(name: &str, state: IngestFileState, error: Option<&str>) -> IngestFile {
        IngestFile {
            id: 1,
            batch_id: 1,
            original_name: name.into(),
            taken_at: None,
            bytes: 1,
            source_path: format!("/card/{name}"),
            dest_path: None,
            backup_path: None,
            blake3: None,
            photo_id: None,
            state,
            error: error.map(str::to_string),
            created_at: 0,
            locked: false,
        }
    }

    fn progress(state: BatchState, rows: &[IngestFile]) -> IngestBatchProgress {
        IngestBatchProgress {
            batch_id: 1,
            volume_identity: "uuid:X".into(),
            label: "card".into(),
            done: rows.len(),
            total: rows.len(),
            state,
            tally: FileTally::from_rows(rows),
        }
    }

    /// The reported card: all files failed, yet the header said "done".
    #[test]
    fn a_batch_where_every_file_failed_never_says_done() {
        let rows: Vec<IngestFile> = (0..172)
            .map(|n| {
                file(
                    &format!("DSC_{n:04}.NEF"),
                    IngestFileState::Failed,
                    Some("naming: rule rendered an empty name"),
                )
            })
            .collect();
        for state in [BatchState::Done, BatchState::Failed] {
            let batch = progress(state, &rows);
            assert_eq!(batch.summary(), "172 failed");
            assert_eq!(
                batch.tally.first_error.as_deref(),
                Some("naming: rule rendered an empty name")
            );
        }
    }

    #[test]
    fn summaries_separate_ingested_duplicates_and_failures() {
        let rows = [
            file("a.NEF", IngestFileState::Cataloged, None),
            file("b.NEF", IngestFileState::Duplicate, None),
            file("c.NEF", IngestFileState::Failed, Some("copy: disk full")),
            file("d.NEF", IngestFileState::Failed, Some("copy: other")),
        ];
        let done = progress(BatchState::Done, &rows);
        assert_eq!(done.summary(), "1 copied, 1 duplicates, 2 failed");
        assert_eq!(done.tally.first_error.as_deref(), Some("copy: disk full"));
        assert_eq!(
            progress(BatchState::Running, &rows).summary(),
            "Copying 4 of 4, 2 failed"
        );
        let clean = progress(
            BatchState::Done,
            &[file("a.NEF", IngestFileState::Cataloged, None)],
        );
        assert_eq!(clean.summary(), "1 copied");
    }
}
