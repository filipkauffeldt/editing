//! Ingest dialog (B3 shell + C2 live state) and the shared [`AppState`].
//!
//! Scope: volume list driven by `edit-ingest` detection events, destination
//! inputs as plain text, an IPTC template selector stub, and a live file table
//! with a per-file state column. C2 adds the **Start ingest** action: it opens
//! an `ingest_batches` row, enqueues one durable `ingest_copy` job per card
//! (multi-card parallel), and reflects the worker's per-file progress, live
//! file states and duplicates through the shared job-event drain.

use std::collections::HashMap;
use std::sync::Arc;

use edit_catalog::models::IngestFile;
use edit_catalog::{Catalog, JobEvent, JobEventKind, JobId, JobKind};
use edit_core::IngestFileState;
use edit_ingest::{
    IngestCopyPayload, IngestPlanRequest, Volume, batch_volume_identity, plan_ingest,
};
use gpui_kit::component::button::Button;
use gpui_kit::component::dialog::DialogTitle;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::progress::Progress;
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

/// Lifecycle of one card's ingest batch, for the per-card progress bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchState {
    Running,
    Done,
    Failed,
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
    /// IPTC template selector stub: `None` until M3/D3 populate templates.
    pub iptc_template: Option<String>,
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
            iptc_template: None,
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

    /// Progress of the batch for one card, if it is ingesting.
    pub fn batch_for_volume(&self, identity: &str) -> Option<&IngestBatchProgress> {
        self.batches
            .iter()
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

    pub fn set_files(&mut self, batch_id: i64, rows: Vec<IngestFileRow>, cx: &mut Context<Self>) {
        self.files_by_batch.insert(batch_id, rows);
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
        state.set_files(batch_id, rows, cx);
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

/// Ingest dialog (plan section 2.8): card list with per-card progress,
/// destination folder + filename rule inputs, backup destination input, IPTC
/// template selector stub, the live file table, and the Start action.
pub struct IngestDialog {
    state: Entity<AppState>,
    catalog: Arc<Catalog>,
    /// Volume identity of the selected card, if any.
    pub selected: Option<String>,
    pub dest_dir: Entity<InputState>,
    pub file_rule: Entity<InputState>,
    pub backup_dir: Entity<InputState>,
    status: Option<String>,
}

impl IngestDialog {
    pub fn new(
        state: Entity<AppState>,
        catalog: Arc<Catalog>,
        dest_dir: Entity<InputState>,
        file_rule: Entity<InputState>,
        backup_dir: Entity<InputState>,
    ) -> Self {
        Self {
            state,
            catalog,
            selected: None,
            dest_dir,
            file_rule,
            backup_dir,
            status: None,
        }
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            state.set_ingest_open(false, cx);
        });
    }

    /// Open an ingest batch for the selected card and queue its copy job.
    ///
    /// Enumeration, copying and dedupe all run inside the `ingest_copy` job,
    /// so this never blocks the UI thread.
    fn start(&mut self, cx: &mut Context<Self>) {
        let Some(identity) = self.selected.clone() else {
            self.status = Some("Select a card first.".to_string());
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
            self.status = Some("That card is gone.".to_string());
            cx.notify();
            return;
        };
        if self.start_card(&volume, cx) {
            self.status = Some(format!("Ingesting {}…", volume.display_name()));
        }
        cx.notify();
    }

    /// Open one batch per detected card, so every card ingests in parallel.
    fn start_all(&mut self, cx: &mut Context<Self>) {
        let volumes = self.state.read(cx).volumes().to_vec();
        if volumes.is_empty() {
            self.status = Some("No cards detected.".to_string());
            cx.notify();
            return;
        }
        let mut started = 0;
        for volume in &volumes {
            if self.start_card(volume, cx) {
                started += 1;
            }
        }
        if started > 0 {
            self.status = Some(format!("Ingesting {started}/{} cards…", volumes.len()));
        }
        cx.notify();
    }

    /// Validate the inputs, open the batch, and queue its `ingest_copy` job.
    /// Returns false (with `status` set) when the card cannot be started.
    fn start_card(&mut self, volume: &Volume, cx: &mut Context<Self>) -> bool {
        let dest = self.dest_dir.read(cx).value().to_string();
        let file_rule = self.file_rule.read(cx).value().to_string();
        let backup = self.backup_dir.read(cx).value().to_string();
        let session = volume.display_name();
        let plan = match plan_ingest(&IngestPlanRequest {
            volume_uuid: volume.fs_uuid.as_deref(),
            volume_label: volume.label.as_deref(),
            source_root: &volume.mount_point,
            dest_dir_template: &dest,
            filename_template: &file_rule,
            backup_dir: Some(backup.as_str()),
            session: &session,
            start_seq: 1,
            template: None,
        }) {
            Ok(plan) => plan,
            Err(error) => {
                self.status = Some(format!("Bad naming rule: {error}"));
                return false;
            }
        };
        let batch_id = match self.catalog.create_ingest_batch(&plan.batch) {
            Ok(id) => id,
            Err(error) => {
                self.status = Some(format!("Could not open batch: {error}"));
                return false;
            }
        };
        let payload = IngestCopyPayload {
            batch_id,
            ..plan.payload
        };
        let encoded = match serde_json::to_string(&payload) {
            Ok(encoded) => encoded,
            Err(error) => {
                self.status = Some(format!("Could not encode ingest job: {error}"));
                return false;
            }
        };
        match self.catalog.enqueue_job(JobKind::IngestCopy, &encoded) {
            Ok(job_id) => {
                let identity = volume.identity.clone();
                self.state.update(cx, |state, cx| {
                    state.register_batch(batch_id, job_id, identity, session, cx);
                });
                true
            }
            Err(error) => {
                self.status = Some(format!("Could not queue ingest: {error}"));
                false
            }
        }
    }

    fn render_cards(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let volumes = self.state.read(cx).volumes().to_vec();
        let selected = self.selected.clone();
        let progress: Vec<(String, Option<(f32, String)>)> = volumes
            .iter()
            .map(|volume| {
                let batch = self
                    .state
                    .read(cx)
                    .batch_for_volume(&volume.identity)
                    .cloned();
                let info = batch.map(|batch| {
                    let text = match batch.state {
                        BatchState::Running => {
                            format!("ingesting {}/{}", batch.done, batch.total)
                        }
                        BatchState::Done => format!("done · {} files", batch.total),
                        BatchState::Failed => format!("failed · {} files", batch.total),
                    };
                    (batch.percent(), text)
                });
                (volume.identity.clone(), info)
            })
            .collect();
        v_flex().gap_2().children(volumes.into_iter().map(|volume| {
            let identity = volume.identity.clone();
            let is_selected = selected.as_deref() == Some(identity.as_str());
            let label = volume.display_name();
            let detail = format!(
                "{} · {} free of {}",
                volume.mount_point.display(),
                format_bytes(volume.free_bytes),
                format_bytes(volume.capacity_bytes),
            );
            let batch_info = progress
                .iter()
                .find(|(id, _)| id == &identity)
                .and_then(|(_, info)| info.clone());
            let (percent, progress_text) = match batch_info {
                Some((percent, text)) => (percent, text),
                None => (
                    volume.used_percent(),
                    format!(
                        "{} used",
                        format_bytes(volume.capacity_bytes - volume.free_bytes)
                    ),
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
                .when(is_selected, |row| row.border_color(rgb(0x3399FF)))
                .child(
                    h_flex()
                        .w_full()
                        .justify_between()
                        .child(div().font_semibold().child(label))
                        .child(div().text_sm().child(detail)),
                )
                .child(Progress::new(progress_id).value(percent))
                .child(div().text_xs().child(progress_text))
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
            .child(div().text_sm().child(label.to_string()))
            .child(Input::new(input).id(id))
    }

    fn render_template_stub(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let template = self
            .state
            .read(cx)
            .iptc_template
            .clone()
            .unwrap_or_else(|| "none".to_string());
        div().text_sm().child(format!(
            "IPTC template: {template} (selector is populated in M3/D3)."
        ))
    }

    fn render_file_table(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let files = self.state.read(cx).files();
        v_flex()
            .gap_1()
            .child(
                div()
                    .text_sm()
                    .font_semibold()
                    .child("Files (medium: original name · bytes · state)".to_string()),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_4()
                    .text_sm()
                    .font_semibold()
                    .child(div().w_1_2().child("File".to_string()))
                    .child(div().w_1_4().child("Size".to_string()))
                    .child(div().w_1_4().child("State".to_string())),
            )
            .children(files.iter().map(|row| {
                h_flex()
                    .w_full()
                    .gap_4()
                    .text_sm()
                    .child(div().w_1_2().child(row.name.clone()))
                    .child(div().w_1_4().child(format_bytes(row.bytes)))
                    .child(div().w_1_4().child(row.state.to_string()))
            }))
            .when(files.is_empty(), |table| {
                table.child(
                    div()
                        .text_sm()
                        .child("No files yet — start an ingest to fill the table.".to_string()),
                )
            })
    }
}

impl Render for IngestDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let volumes = self.state.read(cx).volumes().to_vec();
        // Drop a selection whose card is gone so the highlight never points
        // at a removed volume.
        if let Some(selected) = self.selected.as_deref()
            && !volumes.iter().any(|volume| volume.identity == selected)
        {
            self.selected = None;
        }
        let volume_count = volumes.len();
        let dest = self.dest_dir.read(cx).value().to_string();
        let dest_ready = !dest.trim().is_empty();
        let ready = self.selected.is_some() && dest_ready;
        let all_ready = volume_count > 0 && dest_ready;
        let status = self.status.clone();
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
                    .child(DialogTitle::new().child("Ingest photos".to_string()))
                    .child(div().text_sm().child(if volume_count == 0 {
                        "No volumes detected — plug in a card.".to_string()
                    } else if volume_count == 1 {
                        "1 volume detected.".to_string()
                    } else {
                        format!("{volume_count} volumes detected.")
                    }))
                    .child(self.render_cards(cx))
                    .child(self.render_field("Destination folder", &self.dest_dir, "ingest-dest"))
                    .child(self.render_field(
                        "Filename rule (variables: {session}, {seq:4}, {filename}, {ext}, {yyyy}…)",
                        &self.file_rule,
                        "ingest-rule",
                    ))
                    .child(self.render_field(
                        "Backup destination (optional)",
                        &self.backup_dir,
                        "ingest-backup",
                    ))
                    .child(self.render_template_stub(cx))
                    .child(self.render_file_table(cx))
                    .when_some(status, |panel, status| {
                        panel.child(div().text_sm().child(status))
                    })
                    .child(
                        h_flex()
                            .w_full()
                            .justify_end()
                            .gap_2()
                            .child(
                                Button::new("ingest-start-all")
                                    .label("Ingest all cards")
                                    .disabled(!all_ready)
                                    .on_click(cx.listener(
                                        |this, _event: &ClickEvent, _window, cx| {
                                            this.start_all(cx);
                                        },
                                    )),
                            )
                            .child(
                                Button::new("ingest-start")
                                    .label("Start ingest")
                                    .disabled(!ready)
                                    .on_click(cx.listener(
                                        |this, _event: &ClickEvent, _window, cx| {
                                            this.start(cx);
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
    use super::{BatchState, IngestFileRow, format_bytes};
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
    fn batch_state_labels_are_distinct() {
        assert_ne!(BatchState::Running, BatchState::Done);
        assert_ne!(BatchState::Done, BatchState::Failed);
    }
}
