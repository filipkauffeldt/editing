//! Ingest dialog (B3 shell + C2 live state) and the shared [`AppState`].
//!
//! Scope: volume list driven by `edit-ingest` detection events, destination
//! inputs as plain text, the IPTC template selector, and a live file table
//! with a per-file state column. C2 adds the **Start ingest** action: it opens
//! an `ingest_batches` row, enqueues one durable `ingest_copy` job per card
//! (multi-card parallel), and reflects the worker's per-file progress, live
//! file states and duplicates through the shared job-event drain.

use std::collections::HashMap;
use std::sync::Arc;

use edit_catalog::models::{IngestFile, IptcTemplate};
use edit_catalog::{Catalog, JobEvent, JobEventKind, JobId, JobKind};
use edit_core::IngestFileState;
use edit_ingest::{
    DEFAULT_DEST_RULE, DEFAULT_FILENAME_RULE, IngestCopyPayload, Volume, batch_volume_identity,
};

use app::ingest_job::{IngestStart, saved_code_replacements, start_ingest};
use gpui_kit::component::alert::Alert;
use gpui_kit::component::button::Button;
use gpui_kit::component::dialog::DialogTitle;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::label::Label;
use gpui_kit::component::progress::Progress;
use gpui_kit::component::searchable_list::SearchableVec;
use gpui_kit::component::select::{Select, SelectState};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, StyledExt as _, h_flex, v_flex};
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
    ingest_open: bool,
    templates_open: bool,
    batches: Vec<IngestBatchProgress>,
    files_by_batch: HashMap<i64, Vec<IngestFileRow>>,
    job_batches: HashMap<JobId, i64>,
    /// Bumped each time the dialog opens, so it reloads its template list.
    ingest_generation: u64,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            volumes: Vec::new(),
            ingest_open: false,
            templates_open: false,
            batches: Vec::new(),
            files_by_batch: HashMap::new(),
            job_batches: HashMap::new(),
            ingest_generation: 0,
        }
    }

    pub fn volumes(&self) -> &[Volume] {
        &self.volumes
    }

    pub fn is_ingest_open(&self) -> bool {
        self.ingest_open
    }

    pub fn is_templates_open(&self) -> bool {
        self.templates_open
    }

    /// All live file rows, batches in registration order.
    pub fn files(&self) -> Vec<IngestFileRow> {
        let mut rows: Vec<IngestFileRow> = Vec::new();
        for batch in &self.batches {
            if let Some(files) = self.files_by_batch.get(&batch.batch_id) {
                rows.extend(files.iter().cloned());
            }
        }
        rows
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

    pub fn set_ingest_open(&mut self, open: bool, cx: &mut Context<Self>) {
        if open != self.ingest_open {
            self.ingest_open = open;
            if open {
                self.ingest_generation += 1;
            }
            cx.notify();
        }
    }

    pub fn ingest_generation(&self) -> u64 {
        self.ingest_generation
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

    pub fn set_templates_open(&mut self, open: bool, cx: &mut Context<Self>) {
        if open != self.templates_open {
            self.templates_open = open;
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

/// Ingest dialog (plan section 2.8): card list with per-card progress,
/// destination folder + filename rule inputs, backup destination input, the
/// IPTC template selector, the live file table, and the Start / Retry actions.
pub struct IngestDialog {
    state: Entity<AppState>,
    catalog: Arc<Catalog>,
    /// Volume identity of the selected card, if any.
    pub selected: Option<String>,
    pub dest_dir: Entity<InputState>,
    pub file_rule: Entity<InputState>,
    pub backup_dir: Entity<InputState>,
    template_select: Entity<SelectState<SearchableVec<String>>>,
    /// Template rows behind the selector, refreshed each time the dialog opens.
    templates: Vec<IptcTemplate>,
    seen_generation: u64,
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
    ) -> Self {
        Self {
            state,
            catalog,
            selected: None,
            dest_dir,
            file_rule,
            backup_dir,
            template_select,
            templates: Vec::new(),
            // Differs from the state's generation, so the first render loads.
            seen_generation: u64::MAX,
            status: None,
            status_is_error: false,
        }
    }

    fn set_status(&mut self, is_error: bool, message: String) {
        self.status = Some(message);
        self.status_is_error = is_error;
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            state.set_ingest_open(false, cx);
        });
    }

    /// Reload the template rows and rebuild the selector. The default is the
    /// newest ingest-scoped template (the ingest snapshot), else "no
    /// template"; a still-existing earlier pick is kept.
    fn reload_templates(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.templates = self.catalog.list_iptc_templates().unwrap_or_default();
        self.templates.sort_by_key(|row| row.name.to_lowercase());
        let previous = self.template_select.read(cx).selected_value().cloned();
        let default = self
            .templates
            .iter()
            .filter(|row| row.scope == "ingest")
            .max_by_key(|row| row.updated_at)
            .map(|row| row.name.clone());
        let mut names = vec![NO_TEMPLATE.to_string()];
        names.extend(self.templates.iter().map(|row| row.name.clone()));
        let pick = previous
            .filter(|name| names.contains(name))
            .or(default)
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

    /// Put the defaults back into blank rule boxes, so what the box shows is
    /// exactly what the ingest uses (a placeholder is never the rule).
    fn fill_blank_rules(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for (input, default) in [
            (&self.dest_dir, DEFAULT_DEST_RULE),
            (&self.file_rule, DEFAULT_FILENAME_RULE),
        ] {
            if input.read(cx).value().trim().is_empty() {
                input.update(cx, |input, cx| input.set_value(default, window, cx));
            }
        }
    }

    /// Open an ingest batch for the selected card and queue its copy job.
    /// Clicking it again after a failure retries: files already ingested are
    /// skipped as duplicates and failed ones are copied again.
    ///
    /// Enumeration, copying and dedupe all run inside the `ingest_copy` job,
    /// so this never blocks the UI thread.
    fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(identity) = self.selected.clone() else {
            self.set_status(true, "Select a card.".to_string());
            cx.notify();
            return;
        };
        let volume = self
            .state
            .read(cx)
            .volumes()
            .iter()
            .find(|volume| volume.identity == identity)
            .cloned();
        let Some(volume) = volume else {
            self.set_status(true, "The card was removed.".to_string());
            cx.notify();
            return;
        };
        if self.start_card(&volume, window, cx) {
            self.set_status(false, format!("Copying from {}…", volume.display_name()));
        }
        cx.notify();
    }

    /// Open one batch per detected card, so every card ingests in parallel.
    fn start_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let volumes = self.state.read(cx).volumes().to_vec();
        if volumes.is_empty() {
            self.set_status(true, "No cards found.".to_string());
            cx.notify();
            return;
        }
        let mut started = 0;
        for volume in &volumes {
            if self.start_card(volume, window, cx) {
                started += 1;
            }
        }
        if started > 0 {
            self.set_status(
                false,
                format!("Copying from {started} of {} cards…", volumes.len()),
            );
        }
        cx.notify();
    }

    /// Validate the inputs and queue the card. Returns false (with `status`
    /// set) when the card cannot be started; a rejected request creates no
    /// batch.
    fn start_card(&mut self, volume: &Volume, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.fill_blank_rules(window, cx);
        let dest = self.dest_dir.read(cx).value().to_string();
        let file_rule = self.file_rule.read(cx).value().to_string();
        let backup = self.backup_dir.read(cx).value().to_string();
        let session = volume.display_name();
        let template = self.selected_template(cx);
        let replacements = saved_code_replacements(&self.catalog);
        let started = start_ingest(
            &self.catalog,
            &IngestStart {
                volume_uuid: volume.fs_uuid.as_deref(),
                volume_label: volume.label.as_deref(),
                source_root: &volume.mount_point,
                dest_rule: &dest,
                filename_rule: &file_rule,
                backup_dir: &backup,
                session: &session,
                template: template.as_ref(),
                replacements: replacements.as_ref(),
                only_sources: None,
            },
        );
        match started {
            Ok(started) => {
                let identity = volume.identity.clone();
                self.state.update(cx, |state, cx| {
                    state.register_batch(started.batch_id, started.job_id, identity, session, cx);
                });
                true
            }
            Err(message) => {
                self.set_status(true, message);
                false
            }
        }
    }

    fn render_cards(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let volumes = self.state.read(cx).volumes().to_vec();
        let selected = self.selected.clone();
        let progress: Vec<(String, Option<IngestBatchProgress>)> = volumes
            .iter()
            .map(|volume| {
                let batch = self
                    .state
                    .read(cx)
                    .batch_for_volume(&volume.identity)
                    .cloned();
                (volume.identity.clone(), batch)
            })
            .collect();
        v_flex().gap_2().children(volumes.into_iter().map(|volume| {
            let identity = volume.identity.clone();
            let is_selected = selected.as_deref() == Some(identity.as_str());
            let label = volume.display_name();
            let detail = volume.mount_point.display().to_string();
            let batch = progress
                .iter()
                .find(|(id, _)| id == &identity)
                .and_then(|(_, batch)| batch.clone());
            let (percent, progress_text, failure) = match batch {
                Some(batch) => (
                    batch.percent(),
                    batch.summary(),
                    batch
                        .tally
                        .first_error
                        .clone()
                        .map(|error| format!("First error: {error}")),
                ),
                None => (
                    volume.used_percent(),
                    format!(
                        "{} free of {}",
                        format_bytes(volume.free_bytes),
                        format_bytes(volume.capacity_bytes)
                    ),
                    None,
                ),
            };
            let progress_id = format!("card-progress-{}", volume.identity);
            let row_id = format!("card-row-{}", volume.identity);
            let identity_for_click = identity.clone();
            v_flex()
                .id(row_id)
                .gap_1()
                .p_2()
                .rounded_md()
                .border_1()
                .cursor_pointer()
                .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                    this.selected = Some(identity_for_click.clone());
                    cx.notify();
                }))
                .when(is_selected, |row| {
                    row.border_color(cx.theme().tokens.list_active_border.color)
                })
                .child(
                    h_flex()
                        .w_full()
                        .justify_between()
                        .child(div().font_semibold().child(label))
                        .child(div().text_sm().child(detail)),
                )
                .child(Progress::new(progress_id).value(percent))
                .child(Label::new(progress_text).text_xs())
                .when_some(failure, |row, failure| {
                    row.child(Alert::error(
                        format!("card-failure-{}", volume.identity),
                        failure,
                    ))
                })
        }))
    }

    fn render_field(
        &self,
        label: &str,
        input: &Entity<InputState>,
        id: &'static str,
    ) -> impl IntoElement {
        v_flex()
            .gap_1()
            .child(Label::new(label.to_string()).text_sm())
            .child(Input::new(input).id(id))
    }

    fn render_template_picker(&self) -> impl IntoElement {
        v_flex()
            .gap_1()
            .child(Label::new("IPTC template").text_sm())
            .child(Select::new(&self.template_select).placeholder(NO_TEMPLATE))
    }

    fn render_file_table(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let files = self.state.read(cx).files();
        v_flex()
            .gap_1()
            .child(Label::new("Files").text_sm().font_semibold())
            .child(
                h_flex()
                    .w_full()
                    .gap_4()
                    .text_sm()
                    .font_semibold()
                    .child(Label::new("Name").w_1_2())
                    .child(Label::new("Size").w_1_4())
                    .child(Label::new("State").w_1_4()),
            )
            .children(files.iter().map(|row| {
                h_flex()
                    .w_full()
                    .gap_4()
                    .text_sm()
                    .child(Label::new(row.name.clone()).w_1_2())
                    .child(Label::new(format_bytes(row.bytes)).w_1_4())
                    .child(Label::new(state_label(row.state)).w_1_4())
            }))
            .when(files.is_empty(), |table| {
                table.child(Label::new("No files yet.").text_sm())
            })
    }
}

impl Render for IngestDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let generation = self.state.read(cx).ingest_generation();
        if generation != self.seen_generation {
            self.seen_generation = generation;
            self.reload_templates(window, cx);
        }
        let volumes = self.state.read(cx).volumes().to_vec();
        // Drop a selection whose card is gone so the highlight never points
        // at a removed volume.
        if let Some(selected) = self.selected.as_deref()
            && !volumes.iter().any(|volume| volume.identity == selected)
        {
            self.selected = None;
        }
        let volume_count = volumes.len();
        // Blank boxes are fine: Start refills them with the defaults.
        let ready = self.selected.is_some();
        let all_ready = volume_count > 0;
        let can_retry = self.selected.as_deref().is_some_and(|identity| {
            self.state
                .read(cx)
                .batch_for_volume(identity)
                .is_some_and(|batch| batch.state != BatchState::Running && batch.tally.failed > 0)
        });
        // A progress note is only true while something is ingesting; a
        // rejection stays until the next action.
        let busy = self.state.read(cx).has_running_batch();
        let status = self
            .status
            .clone()
            .filter(|_| self.status_is_error || busy)
            .map(|message| (self.status_is_error, message));
        div()
            .id("ingest-dialog")
            .w_full()
            .rounded_lg()
            .border_1()
            .p_4()
            .bg(cx.theme().tokens.popover)
            .child(
                v_flex()
                    .gap_3()
                    .child(DialogTitle::new().child("Ingest".to_string()))
                    .when(volume_count == 0, |panel| {
                        panel.child(Label::new("No cards found.").text_sm())
                    })
                    .child(self.render_cards(cx))
                    .child(self.render_field("Destination folder", &self.dest_dir, "ingest-dest"))
                    .child(self.render_field("Filename rule", &self.file_rule, "ingest-rule"))
                    .child(self.render_field(
                        "Backup folder (optional)",
                        &self.backup_dir,
                        "ingest-backup",
                    ))
                    .child(self.render_template_picker())
                    .child(self.render_file_table(cx))
                    .when_some(status, |panel, (is_error, message)| {
                        panel.child(if is_error {
                            Alert::error("ingest-status", message)
                        } else {
                            Alert::info("ingest-status", message)
                        })
                    })
                    .child(
                        h_flex()
                            .w_full()
                            .justify_end()
                            .gap_2()
                            .child(
                                Button::new("ingest-start-all")
                                    .label("Ingest all")
                                    .disabled(!all_ready)
                                    .on_click(cx.listener(
                                        |this, _event: &ClickEvent, window, cx| {
                                            this.start_all(window, cx);
                                        },
                                    )),
                            )
                            .child(
                                Button::new("ingest-retry")
                                    .label("Retry failed")
                                    .disabled(!can_retry)
                                    .on_click(cx.listener(
                                        |this, _event: &ClickEvent, window, cx| {
                                            this.start(window, cx);
                                        },
                                    )),
                            )
                            .child(
                                Button::new("ingest-start")
                                    .label("Start")
                                    .disabled(!ready)
                                    .on_click(cx.listener(
                                        |this, _event: &ClickEvent, window, cx| {
                                            this.start(window, cx);
                                        },
                                    )),
                            )
                            .child(Button::new("ingest-close").label("Close").on_click(
                                cx.listener(|this, _event: &ClickEvent, _window, cx| {
                                    this.close(cx);
                                }),
                            )),
                    ),
            )
    }
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
