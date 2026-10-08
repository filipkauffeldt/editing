//! GPUI browse surfaces for M1: contact sheet, single view, panels.
//!
//! The pure state lives in [`crate::browse`]; this module renders snapshots
//! of it. The catalog scan runs as a durable `catalog_scan` job and thumb
//! work as `thumb` jobs (plan section 1.2); workers report over flume and
//! [`Browser::on_job_event`] applies them to the visible rows.
//!
//! Keyboard map (plan section 2.11, M1 subset):
//!
//! | Keys | Action |
//! |---|---|
//! | `Left` / `Right` (`Up` / `Down` by row) | walk items |
//! | `Shift` + arrows | extend the selection range |
//! | `Ctrl+A` | select all |
//! | `G` | toggle contact sheet and single view |
//! | `Z` | fit to window |
//! | `Shift+Z` | 100% zoom |
//! | Scroll | zoom (from fit it starts a free scale) |
//! | Drag | pan while zoomed |
//! | `0`–`5` | rating |
//! | `O` | open a folder (native picker) |

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use edit_catalog::{Catalog, JobEvent, JobEventKind, JobId, JobKind};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::label::Label;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, IconName, Sizable as _, h_flex, status_bar::StatusBar,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::browse::{
    BrowseFilter, BrowseItem, CatalogScanPayload, MetaBadge, SelectionModel, SortOrder,
    ThumbPayload, ThumbState, UploadBadge, ZoomMode, ZoomState, apply_filter_sort, canonical_root,
    list_browse_items, pair_key, plan_template_apply, run_catalog_scan,
    template_apply_payloads_with,
};
use crate::develop::{DevelopSession, RenderQueue, SidecarQueue, SliderId};
use crate::preview::BrowsePreviewProvider;
use edit_core::{CancelToken, DevelopSettings, RenderSize};
use edit_metadata::{CodeReplacements, MetadataWritePayload, TemplateFields};
use edit_render::{PROXY_DRAG_LONG_EDGE, PROXY_RELEASE_LONG_EDGE, RenderService};
use gpui_kit::component::slider::{
    Slider as DevelopSliderElement, SliderEvent, SliderState, SliderValue,
};

/// Cells per contact-sheet row over `uniform_list`.
pub const GRID_COLS: usize = 4;
/// Downscaled thumbnails kept per item (long edge, pixels).
pub const THUMB_LONG_EDGE: u32 = 320;
/// Bounded decoded-thumbnail cache: the grid and strip share it, so a
/// 10k-photo folder cannot grow the UI process without limit.
pub const THUMB_CACHE_CAP: usize = 256;
/// New background decodes kicked off per frame at most.
const THUMB_REQUEST_BUDGET: usize = 24;

/// Painted offset of a frame's top-left within a viewport: centered on the
/// `scale`d size, shifted by the pan (fractional frame widths/heights).
/// Extracted from the single-view canvas so the zoom-applied-to-paint math
/// has a regression test (review blocking item 4).
fn paint_origin(
    frame_w: f32,
    frame_h: f32,
    vw: f32,
    vh: f32,
    scale: f32,
    zoom: ZoomState,
) -> (f32, f32) {
    let dw = frame_w * scale;
    let dh = frame_h * scale;
    let ox = (vw - dw) / 2.0 + dw * zoom.pan_x;
    let oy = (vh - dh) / 2.0 + dh * zoom.pan_y;
    (ox, oy)
}

gpui_kit::actions!(
    browser,
    [
        NextItem,
        PrevItem,
        NextRow,
        PrevRow,
        ExtendNext,
        ExtendPrev,
        ExtendNextRow,
        ExtendPrevRow,
        ToggleView,
        FitView,
        HundredView,
        OpenFolder,
        SelectAllItems,
        Rate0,
        Rate1,
        Rate2,
        Rate3,
        Rate4,
        Rate5
    ]
);

/// Grid vs single-view center mode (plan sections 2.2-2.3). `G` toggles.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Grid,
    Single,
}

/// Root browse entity: snapshot rows plus the interaction state around them.
pub struct Browser {
    catalog: Arc<Catalog>,
    events: flume::Receiver<JobEvent>,
    focus: FocusHandle,
    items: Vec<BrowseItem>,
    filter: BrowseFilter,
    sort: SortOrder,
    selection: SelectionModel,
    current: usize,
    mode: Mode,
    zoom: ZoomState,
    root_label: String,
    root_id: Option<i64>,
    scan_status: String,
    /// Display table: filtered/sorted snapshots. Rebuilt on data changes
    /// only, so per-frame rendering clones [`Arc`]s instead of items.
    rows: Arc<Vec<BrowseItem>>,
    /// Bounded decoded thumbnails shared by the grid, single view, and
    /// strip; filled on background threads, never on the UI thread.
    thumbs: HashMap<i64, Arc<RenderImage>>,
    thumb_order: VecDeque<i64>,
    thumb_pending: HashSet<i64>,
    /// Queue thumb job id → photo path, resolved from the job payload on
    /// `Started` so progress/completion can mark the right cell.
    thumb_jobs: HashMap<JobId, String>,
    /// `metadata_write` job id → item id, resolved from the payload so
    /// per-item badges flip on the right row even across restarts.
    meta_jobs: HashMap<JobId, i64>,
    develop: DevelopSession,
    develop_sliders: Vec<(SliderId, Entity<SliderState>)>,
    /// Develop render scheduling: the latest wanted size for the one worker,
    /// cancellation of stale renders, and per-item terminal failures that
    /// stop the paint-time kick.
    queue: RenderQueue,
    /// Cancels neighbor prewarms when the cursor moves.
    prewarm_token: CancelToken,
    /// Items whose neighbor prewarm already ran (bounded).
    prewarmed: HashSet<i64>,
    /// Sidecar writes, serialized per item.
    sidecars: SidecarQueue,
    /// Edits whose catalog write failed, retried at every commit point and
    /// at quit; a newer commit of the same item supersedes the entry.
    unsaved: BTreeMap<i64, String>,
    /// The resolved develop output, swapped with the store on completion
    /// (plan section 1.5). DisplaySrgb path — the canvas paints it.
    develop_image: Option<Arc<RenderImage>>,
    develop_image_item: Option<i64>,
    /// Whether the develop buffer holds unsaved slider changes.
    develop_dirty: bool,
    /// Diagnostics for the develop panel status line.
    develop_status: String,
    /// The resolved rendered develop output through `render_path`.
    renderer: Arc<RenderService>,
    scan_jobs: Vec<JobId>,
    /// Status strip line for template applies (plan section 2.1:
    /// "template changes" marker).
    template_status: String,
    drag_from: Option<Point<Pixels>>,
    strip_scroll: ScrollHandle,
    /// Fit scale painted by the last canvas frame; scroll-zoom reads it to
    /// enter a free scale from Fit. UI-thread only, hence `Rc`.
    fit_scale: Rc<Cell<f32>>,
}

impl Browser {
    pub fn new(
        catalog: Arc<Catalog>,
        events: flume::Receiver<JobEvent>,
        renderer: Arc<RenderService>,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        let develop_sliders: Vec<(SliderId, Entity<SliderState>)> = SliderId::ALL
            .iter()
            .map(|id| {
                // Widget ranges come from LightCraft's own control table, so
                // the panel can never disagree with the edit model.
                let control = match id.spec() {
                    Some(spec) => cx.new(|_| {
                        SliderState::new()
                            .min(spec.min as f32)
                            .max(spec.max as f32)
                            .step(spec.step as f32)
                            .default_value(spec.default as f32)
                    }),
                    None => cx.new(|_| SliderState::new()),
                };
                (*id, control)
            })
            .collect();
        for (id, control) in &develop_sliders {
            let id: SliderId = *id;
            cx.subscribe(control, move |this, _, event: &SliderEvent, cx| {
                let value = match event {
                    SliderEvent::Change(value) | SliderEvent::Release(value) => Some(*value),
                }
                .and_then(|value| match value {
                    SliderValue::Single(v) => Some(v),
                    _ => None,
                });
                match (event, value) {
                    (SliderEvent::Change(_), Some(v)) => this.on_develop_change(id, v, cx),
                    (SliderEvent::Release(_), Some(v)) => this.on_develop_release(id, v, cx),
                    _ => {}
                }
                cx.notify();
            })
            .detach();
        }
        cx.bind_keys([
            KeyBinding::new("right", NextItem, None),
            KeyBinding::new("left", PrevItem, None),
            KeyBinding::new("down", NextRow, None),
            KeyBinding::new("up", PrevRow, None),
            KeyBinding::new("shift-right", ExtendNext, None),
            KeyBinding::new("shift-left", ExtendPrev, None),
            KeyBinding::new("shift-down", ExtendNextRow, None),
            KeyBinding::new("shift-up", ExtendPrevRow, None),
            KeyBinding::new("g", ToggleView, None),
            KeyBinding::new("z", FitView, None),
            KeyBinding::new("shift-z", HundredView, None),
            KeyBinding::new("ctrl-a", SelectAllItems, None),
            KeyBinding::new("0", Rate0, None),
            KeyBinding::new("1", Rate1, None),
            KeyBinding::new("2", Rate2, None),
            KeyBinding::new("3", Rate3, None),
            KeyBinding::new("4", Rate4, None),
            KeyBinding::new("5", Rate5, None),
        ]);
        let browser = Self {
            catalog,
            events,
            focus,
            items: Vec::new(),
            filter: BrowseFilter::default(),
            sort: SortOrder::IngestOrder,
            selection: SelectionModel::default(),
            current: 0,
            mode: Mode::Grid,
            zoom: ZoomState::default(),
            root_label: "No folder open".to_string(),
            root_id: None,
            scan_status: "Ready".to_string(),
            rows: Arc::new(Vec::new()),
            thumbs: HashMap::new(),
            thumb_order: VecDeque::new(),
            thumb_pending: HashSet::new(),
            thumb_jobs: HashMap::new(),
            meta_jobs: HashMap::new(),
            develop: DevelopSession::new(DevelopSettings::default()),
            develop_sliders,
            queue: RenderQueue::default(),
            prewarm_token: CancelToken::new(),
            prewarmed: HashSet::new(),
            sidecars: SidecarQueue::default(),
            unsaved: BTreeMap::new(),
            develop_image: None,
            develop_image_item: None,
            develop_dirty: false,
            develop_status: "Ready".to_string(),
            renderer,
            scan_jobs: Vec::new(),
            template_status: String::new(),
            drag_from: None,
            strip_scroll: ScrollHandle::new(),
            fit_scale: Rc::new(Cell::new(1.0)),
        };
        cx.on_app_quit(|this, cx| this.flush_for_quit(cx)).detach();
        browser
    }

    /// The handle the browse key bindings hang off. Focus has to sit on it
    /// (or below it): actions only reach handlers on the focused element and
    /// its ancestors.
    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    /// Catalog root currently shown, if a scan has landed one.
    pub fn root_id(&self) -> Option<i64> {
        self.root_id
    }

    /// Number of display rows after filter/sort.
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// Thumbnail state of one display item, for tests and badges.
    pub fn thumb_state_for(&self, item_id: i64) -> Option<ThumbState> {
        self.rows
            .iter()
            .find(|row| row.item_id == item_id)
            .map(|row| row.thumb)
    }

    /// Most recently enqueued scan job, for tests.
    pub fn last_scan_job(&self) -> Option<JobId> {
        self.scan_jobs.last().copied()
    }

    /// Start the flume drain loop: background threads never touch entities.
    pub fn start_event_drain(&self, cx: &mut Context<Self>) {
        let rx = self.events.clone();
        cx.spawn(async move |this, cx| {
            while let Ok(event) = rx.recv_async().await {
                let applied = this
                    .update(cx, |browser, cx| {
                        browser.on_job_event(&event);
                        cx.notify();
                    })
                    .ok();
                if applied.is_none() {
                    break;
                }
            }
        })
        .detach();
    }

    /// Ask the platform for a folder (native picker) and open the choice.
    /// A cancelled or failed picker leaves the current folder alone.
    pub fn prompt_open_folder(&mut self, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Open folder".into()),
        });
        cx.spawn(async move |this, cx| match picked.await {
            Ok(Ok(Some(paths))) => {
                if let Some(path) = paths.into_iter().next() {
                    this.update(cx, |this, cx| this.open_folder(path, cx)).ok();
                }
            }
            Ok(Ok(None)) => {}
            Ok(Err(e)) => {
                this.update(cx, |this, cx| {
                    this.scan_status = format!("Folder picker failed: {e}");
                    cx.notify();
                })
                .ok();
            }
            Err(_) => {}
        })
        .detach();
    }

    /// Pick a folder: enqueue a `catalog_scan` job and show live progress.
    /// Workers do the catalog writes; progress events refresh the rows.
    /// The path is canonicalized first, so `photos` and `photos/` share one
    /// root row instead of two competing roots.
    pub fn open_folder(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        // Reopening is an explicit reload: failed renders get another try.
        self.queue.navigate();
        // A folder opens on its contact sheet with the first photo selected.
        self.mode = Mode::Grid;
        self.current = 0;
        self.selection = SelectionModel::new(self.rows.len());
        self.zoom.reset();
        let canon = canonical_root(&path);
        let label = canon.to_string_lossy().into_owned();
        let payload = serde_json::to_string(&CatalogScanPayload {
            root: label.clone(),
        })
        .unwrap_or_else(|_| "{}".to_string());
        match self.catalog.enqueue_job(JobKind::CatalogScan, &payload) {
            Ok(id) => {
                self.scan_jobs.push(id);
                self.root_label = label.clone();
                self.scan_status = format!("Scan queued: {label}");
                // The root row may already exist from an earlier open.
                self.resolve_scan_root(id);
                self.refresh_items();
            }
            Err(e) => self.scan_status = format!("Scan failed to queue: {e}"),
        }
        cx.notify();
    }

    /// Point `root_id` at the root a scan job is filling (roots appear when
    /// the worker runs, so this runs on every scan event, not just enqueue).
    fn resolve_scan_root(&mut self, job_id: JobId) {
        if let Ok(job) = self.catalog.job(job_id)
            && let Ok(payload) = serde_json::from_str::<CatalogScanPayload>(&job.payload_json)
        {
            let root = canonical_root(Path::new(&payload.root))
                .to_string_lossy()
                .into_owned();
            if let Ok(Some(row)) = self.catalog.get_root_by_path(&root) {
                self.root_id = Some(row.id);
            }
        }
    }

    fn refresh_items(&mut self) {
        if let Some(root_id) = self.root_id
            && let Ok(items) = list_browse_items(&self.catalog, root_id)
        {
            self.merge_thumbs(items);
        }
    }

    fn merge_thumbs(&mut self, fresh: Vec<BrowseItem>) {
        // Keep per-photo thumb/upload/meta state across rescans keyed by
        // item id so a refresh never forgets what the worker reported.
        let kept: HashMap<i64, (ThumbState, UploadBadge, bool, MetaBadge)> = self
            .items
            .iter()
            .map(|item| {
                (
                    item.item_id,
                    (item.thumb, item.upload, item.ingest_fresh, item.meta),
                )
            })
            .collect();
        self.items = fresh;
        for item in &mut self.items {
            if let Some((thumb, upload, fresh_flag, meta)) = kept.get(&item.item_id) {
                item.thumb = *thumb;
                item.upload = *upload;
                item.ingest_fresh = *fresh_flag;
                item.meta = *meta;
            } else {
                item.ingest_fresh = true;
            }
        }
        self.sort.apply(&mut self.items);
        self.rebuild_rows();
    }

    /// Rebuild the display table from the store, then clamp the focus.
    fn rebuild_rows(&mut self) {
        let rows: Vec<BrowseItem> = apply_filter_sort(&self.items, &self.filter, self.sort)
            .into_iter()
            .cloned()
            .collect();
        self.rows = Arc::new(rows);
        self.clamp_to_visible();
    }

    /// Clamp cursor, anchor, and current item to the visible table.
    fn clamp_to_visible(&mut self) {
        let len = self.rows.len();
        self.selection.clamp(len);
        self.current = self.current.min(len.saturating_sub(1));
    }

    /// Replace the header filter and refocus the view on what remains.
    pub fn set_filter(&mut self, filter: BrowseFilter) {
        self.filter = filter;
        self.rebuild_rows();
    }

    /// Replace the header sort order and refocus the view on what remains.
    pub fn set_sort(&mut self, sort: SortOrder) {
        self.sort = sort;
        self.rebuild_rows();
    }

    /// Display rows after filter/sort.
    pub fn visible(&self) -> Vec<&BrowseItem> {
        self.rows.iter().collect()
    }

    fn current_item(&self) -> Option<BrowseItem> {
        self.rows.get(self.current).cloned()
    }

    /// Display rows of the multi-selection (plan section 2.7: apply a
    /// template to the selection). The focused row is always included, so
    /// a fresh single-cell browser still applies.
    pub fn selected_items(&self) -> Vec<BrowseItem> {
        self.selection
            .selected
            .iter()
            .filter_map(|index| self.rows.get(*index))
            .cloned()
            .collect()
    }

    /// Display path of the focused row (drive-through for previews).
    pub fn current_display_path(&self) -> Option<String> {
        self.current_item().map(|item| item.display_path)
    }

    /// Which display index holds `item_id` (tests read back by id).
    pub fn item_meta(&self, item_id: i64) -> Option<MetaBadge> {
        self.items
            .iter()
            .find(|row| row.item_id == item_id)
            .map(|row| row.meta)
    }

    /// Current template status strip text.
    pub fn template_status(&self) -> String {
        self.template_status.clone()
    }

    /// Queue the template over the multi-selection as durable
    /// `metadata_write` jobs (plan section 2.7). Variables expand inside
    /// the worker per photo; sequence numbers are display order, 1-based.
    ///
    /// Cells flip to `Queued` right away and the whole batch commits in
    /// one background transaction, so the UI thread never waits on the
    /// catalog. Job ids are not tracked here: worker events resolve their
    /// item from the durable payload (`meta_item_id`), which also keeps an
    /// event that beats the enqueue result from being overwritten.
    pub fn apply_template(
        &mut self,
        fields: TemplateFields,
        template_name: &str,
        cx: &mut Context<Self>,
    ) {
        self.apply_template_with(fields, template_name, None, cx);
    }

    /// [`Self::apply_template`] with a parsed code replacement set baked
    /// into each durable payload, so workers expand `=code=` / hot codes.
    pub fn apply_template_with(
        &mut self,
        fields: TemplateFields,
        template_name: &str,
        replacements: Option<CodeReplacements>,
        cx: &mut Context<Self>,
    ) {
        if fields == TemplateFields::default() {
            self.template_status = "Apply skipped: template is empty.".into();
            cx.notify();
            return;
        }
        let plan = plan_template_apply(&self.selected_items(), &self.catalog);
        if plan.is_empty() {
            self.template_status = "Apply skipped: no cataloged item in the selection.".into();
            cx.notify();
            return;
        }
        let item_ids: Vec<i64> = plan.iter().map(|step| step.item_id).collect();
        for item_id in &item_ids {
            self.set_meta_quiet(*item_id, MetaBadge::Queued);
        }
        self.rebuild_rows();
        self.template_status = format!(
            "Applying template \"{template_name}\" to {} photos…",
            item_ids.len()
        );
        cx.notify();

        let catalog = Arc::clone(&self.catalog);
        let fields_json = serde_json::to_string(&fields).unwrap_or_else(|_| "{}".to_string());
        cx.spawn(async move |this, cx| {
            let enqueue = cx.background_executor().spawn(async move {
                let payloads = template_apply_payloads_with(
                    &plan,
                    &fields_json,
                    edit_catalog::db::now_unix(),
                    replacements,
                );
                catalog.enqueue_jobs_batch(JobKind::MetadataWrite, &payloads)
            });
            if let Err(e) = enqueue.await {
                let _ = this.update(cx, |this, cx| {
                    for item_id in &item_ids {
                        this.set_meta_quiet(*item_id, MetaBadge::Failed);
                    }
                    this.rebuild_rows();
                    this.template_status = format!("Apply failed to queue: {e}");
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn meta_item_id(&mut self, id: JobId) -> Option<i64> {
        if let Some(item_id) = self.meta_jobs.get(&id) {
            return Some(*item_id);
        }
        let job = self.catalog.job(id).ok()?;
        if job.kind != JobKind::MetadataWrite {
            return None;
        }
        let payload: MetadataWritePayload = serde_json::from_str(&job.payload_json).ok()?;
        if payload.item_id < 0 {
            return None;
        }
        self.meta_jobs.insert(id, payload.item_id);
        Some(payload.item_id)
    }

    /// Flip an item's badge without rebuilding rows (callers batch).
    /// Negative ids are transient display cells with no durable identity:
    /// writing them would collapse every unmatched row's state.
    fn set_meta_quiet(&mut self, item_id: i64, badge: MetaBadge) -> bool {
        if item_id < 0 {
            return false;
        }
        let mut hit = false;
        for item in &mut self.items {
            if item.item_id == item_id {
                item.meta = badge;
                hit = true;
            }
        }
        hit
    }

    fn set_meta(&mut self, item_id: i64, badge: MetaBadge) {
        if self.set_meta_quiet(item_id, badge) {
            self.rebuild_rows();
        }
    }

    /// Apply one worker event to the visible rows.
    pub fn on_job_event(&mut self, event: &JobEvent) {
        match &event.kind {
            JobEventKind::Started { .. } => {
                if self.scan_jobs.contains(&event.id) {
                    self.resolve_scan_root(event.id);
                    self.scan_status = "Scanning folder…".to_string();
                } else if let Some(path) = self.thumb_path_for(event.id) {
                    self.mark_thumb(&path, ThumbState::Running);
                } else if let Some(item_id) = self.meta_item_id(event.id) {
                    self.set_meta(item_id, MetaBadge::Writing);
                    if self.template_status.is_empty() {
                        self.template_status = "Writing template metadata…".into();
                    }
                }
            }
            JobEventKind::Progress { done, total } => {
                if self.scan_jobs.contains(&event.id) {
                    self.resolve_scan_root(event.id);
                    self.scan_status = format!("Scanning: {done}/{total}");
                    self.refresh_items();
                } else if self.meta_jobs.contains_key(&event.id) {
                    self.template_status = format!("Writing template metadata: {done}/{total}");
                }
            }
            JobEventKind::Succeeded => {
                if self.scan_jobs.contains(&event.id) {
                    self.resolve_scan_root(event.id);
                    self.scan_status = "Scan complete".to_string();
                    self.refresh_items();
                } else if let Some(path) = self.thumb_jobs.remove(&event.id) {
                    self.mark_thumb(&path, ThumbState::Ready);
                    self.rebuild_rows();
                } else if let Some(item_id) = self.meta_item_id(event.id) {
                    self.meta_jobs.remove(&event.id);
                    self.set_meta(item_id, MetaBadge::Done);
                    let in_flight = self
                        .items
                        .iter()
                        .filter(|i| matches!(i.meta, MetaBadge::Queued | MetaBadge::Writing))
                        .count();
                    let done_count = self
                        .items
                        .iter()
                        .filter(|i| i.meta == MetaBadge::Done)
                        .count();
                    self.template_status = if in_flight == 0 {
                        format!(
                            "Template applied: {done_count} photo{}.",
                            if done_count == 1 { "" } else { "s" }
                        )
                    } else {
                        format!("Template applied: {done_count} done, {in_flight} in flight…")
                    };
                }
            }
            JobEventKind::Failed { error } => {
                if self.scan_jobs.contains(&event.id) {
                    self.scan_status = format!("Scan failed: {error}");
                } else if let Some(path) = self.thumb_jobs.remove(&event.id) {
                    self.mark_thumb(&path, ThumbState::Failed);
                    self.rebuild_rows();
                } else if let Some(item_id) = self.meta_item_id(event.id) {
                    self.meta_jobs.remove(&event.id);
                    self.set_meta(item_id, MetaBadge::Failed);
                    self.template_status = format!("Template apply failed: {error}");
                }
            }
        }
    }

    /// Photo path for a thumb job: remembered, or resolved from the durable
    /// job payload (the worker claims before its `Started` event lands, so
    /// the row is always readable here).
    fn thumb_path_for(&mut self, id: JobId) -> Option<String> {
        if let Some(path) = self.thumb_jobs.get(&id) {
            return Some(path.clone());
        }
        let job = self.catalog.job(id).ok()?;
        let payload: ThumbPayload = serde_json::from_str(&job.payload_json).ok()?;
        self.thumb_jobs.insert(id, payload.path.clone());
        Some(payload.path)
    }

    fn mark_thumb(&mut self, path: &str, state: ThumbState) {
        let mut hit = false;
        for item in &mut self.items {
            if item.display_path == path {
                item.thumb = state;
                item.ingest_fresh = false;
                hit = true;
            }
        }
        // The display table is a separate snapshot; refresh it so the cell
        // repaints on the same event (tests read it back immediately).
        if hit {
            self.rebuild_rows();
        }
    }

    fn walk(&mut self, delta: isize, extend: bool, cx: &mut Context<Self>) {
        let len = self.rows.len();
        if len == 0 {
            return;
        }
        // An outgoing item commits its edit buffer before the cursor moves.
        self.persist_current_edit(cx);
        self.selection.move_cursor(delta, extend, len);
        self.current = self.selection.cursor;
        self.zoom.reset();
        self.on_navigated();
        self.preload_neighbors(cx);
        self.strip_scroll.scroll_to_item(self.current);
        cx.notify();
    }

    fn goto(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.rows.is_empty() {
            return;
        }
        // An outgoing item commits its edit buffer before the cursor moves.
        self.persist_current_edit(cx);
        self.selection.select_single(index, self.rows.len());
        self.current = self.selection.cursor;
        self.zoom.reset();
        self.on_navigated();
        self.preload_neighbors(cx);
        self.strip_scroll.scroll_to_item(self.current);
        cx.notify();
    }

    fn goto_item(&mut self, item_id: i64, cx: &mut Context<Self>) {
        if let Some(index) = self.rows.iter().position(|row| row.item_id == item_id) {
            self.goto(index, cx);
        }
    }

    /// The cursor moved: the in-flight render and prewarms belong to the item
    /// left behind, and failures anchored there get another chance.
    fn on_navigated(&mut self) {
        self.queue.navigate();
        self.prewarm_token.cancel();
        self.prewarm_token = CancelToken::new();
        if self.unsaved.is_empty() {
            self.develop_status = "Ready".to_string();
        }
    }

    /// Queue neighbor thumbnails on the background executor so Left/Right
    /// never waits on disk (and never blocks the UI thread decoding).
    fn preload_neighbors(&mut self, cx: &mut Context<Self>) {
        let rows = Arc::clone(&self.rows);
        for offset in 1..=2 {
            for neighbor in [
                self.current.checked_add(offset),
                self.current.checked_sub(offset),
            ]
            .into_iter()
            .flatten()
            {
                if let Some(item) = rows.get(neighbor) {
                    self.request_thumb(cx, item.item_id, item.display_path.clone());
                }
            }
        }
    }

    /// Queue a downscaled decode for one item unless cached or already
    /// queued. Reads and decodes off-thread; the entity inserts the frame
    /// and re-renders on completion.
    fn request_thumb(&mut self, cx: &mut Context<Self>, item_id: i64, path: String) {
        if self.thumbs.contains_key(&item_id) || !self.thumb_pending.insert(item_id) {
            return;
        }
        cx.spawn(async move |this, cx| {
            let rgba = cx
                .background_executor()
                .spawn(async move {
                    let bytes = BrowsePreviewProvider::thumbnail_bytes(Path::new(&path))?;
                    let decoded = image::load_from_memory(&bytes).ok()?;
                    Some(
                        decoded
                            .thumbnail(THUMB_LONG_EDGE, THUMB_LONG_EDGE)
                            .to_rgba8(),
                    )
                })
                .await;
            if let Some(rgba) = rgba {
                this.update(cx, |this, cx| {
                    this.insert_thumb(item_id, rgba);
                    cx.notify();
                })
                .ok();
            } else {
                this.update(cx, |this, cx| {
                    this.thumb_pending.remove(&item_id);
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    /// Insert a decoded frame, evicting the oldest entries past the cap.
    /// Stored BGRA, matching what `paint_image` expects.
    fn insert_thumb(&mut self, item_id: i64, mut rgba: image::RgbaImage) {
        for px in rgba.as_chunks_mut::<4>().0 {
            px.swap(0, 2);
        }
        if !self.thumbs.contains_key(&item_id)
            && self.thumbs.len() >= THUMB_CACHE_CAP
            && let Some(old) = self.thumb_order.pop_front()
        {
            self.thumbs.remove(&old);
        }
        if let Some(pos) = self.thumb_order.iter().position(|id| *id == item_id) {
            self.thumb_order.remove(pos);
        }
        self.thumb_order.push_back(item_id);
        self.thumbs.insert(
            item_id,
            Arc::new(RenderImage::new([image::Frame::new(rgba)])),
        );
        self.thumb_pending.remove(&item_id);
    }

    /// Read the current item's edit row into the develop session on the
    /// background executor, then render its selection proxy. A photo with no
    /// row yet starts from the default settings (raw import defaults and the
    /// as-shot white balance arrive with the D2 presets). A failed read is
    /// not an absent row: the previous buffer stays, nothing is saved over
    /// the unreadable row, and the error shows in the panel (review M6).
    fn schedule_current_edit_load(&mut self, cx: &mut Context<Self>) {
        let Some(item) = self.current_item() else {
            return;
        };
        let catalog = Arc::clone(&self.catalog);
        let item_id = item.item_id;
        // A commit that failed to reach the catalog is newer than the row.
        let unsaved = self.unsaved.get(&item_id).cloned();
        cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    let stored = match unsaved {
                        Some(json) => Some(json),
                        None => match catalog.get_edit(item_id) {
                            Ok(edit) => edit.map(|edit| edit.develop_json),
                            Err(e) => return Err(format!("Edit read failed: {e}")),
                        },
                    };
                    match stored {
                        None => Ok(DevelopSettings::default()),
                        Some(json) => serde_json::from_str::<DevelopSettings>(&json)
                            .map_err(|e| format!("Edit is unreadable: {e}")),
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                this.apply_edit_load(item_id, outcome, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn apply_edit_load(
        &mut self,
        item_id: i64,
        outcome: std::result::Result<DevelopSettings, String>,
        cx: &mut Context<Self>,
    ) {
        // The cursor moved on while the row was being read.
        if self.develop_item_id() != item_id {
            self.queue.finish(item_id);
            return;
        }
        match outcome {
            Ok(buffer) => {
                // A buffer already owned by this item may hold live edits.
                if self.develop.owner() != Some(item_id) {
                    self.develop.load_edit(item_id, buffer);
                    self.develop_dirty = false;
                }
                self.develop_status = "Ready".to_string();
                self.request_render(
                    RenderSize::Proxy(PROXY_DRAG_LONG_EDGE),
                    Some(RenderSize::Proxy(PROXY_RELEASE_LONG_EDGE)),
                    cx,
                );
            }
            Err(diagnostic) => {
                self.develop.disown();
                self.develop_dirty = false;
                self.develop_status = diagnostic;
                self.queue.mark_failed(item_id);
            }
        }
    }

    /// The develop panel (plan sections 2.1 and 2.3): Presets placeholder on
    /// top, then the six LR-style tone sliders driving `DevelopSettings`
    /// through the render service.
    fn render_develop_panel(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (bg, fg, border, muted) = (
            theme.sidebar,
            theme.sidebar_foreground,
            theme.sidebar_border,
            theme.muted_foreground,
        );
        let status = self.develop_status.clone();
        v_flex()
            .w(px(240.0))
            .h_full()
            .p_3()
            .gap_3()
            .bg(bg)
            .text_color(fg)
            .border_l_1()
            .border_color(border)
            .child(Label::new("Develop").text_sm())
            .child(
                Label::new("Presets land in D2.")
                    .text_xs()
                    .text_color(muted),
            )
            .children(self.render_develop_rows(cx))
            .child(Label::new(status).text_xs().text_color(muted))
    }

    /// One block per slider: name, live value readout, a reset button, then
    /// the slider (which dispatches actions from its own listeners against
    /// the browser entity). Double-clicking the name row resets too.
    fn render_develop_rows(&mut self, cx: &mut Context<Self>) -> Vec<impl IntoElement> {
        let muted = cx.theme().muted_foreground;
        let mut rows = Vec::with_capacity(SliderId::ALL.len());
        for (id, control) in &self.develop_sliders {
            let id = *id;
            let Some(spec) = id.spec() else { continue };
            let value = self.develop.slider_value(id);
            let at_default = (value - spec.default).abs() < 1e-6;
            let entity_id = control.entity_id().as_u64();
            rows.push(
                v_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        h_flex()
                            .id(("develop-row", entity_id))
                            .w_full()
                            .items_center()
                            .justify_between()
                            .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                                if event.click_count() == 2 {
                                    this.reset_develop_slider(id, cx);
                                }
                            }))
                            .child(Label::new(spec.label).text_xs())
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap_1()
                                    .child(
                                        Label::new(spec.format(value)).text_xs().text_color(muted),
                                    )
                                    .child(
                                        Button::new(("develop-reset", entity_id))
                                            .ghost()
                                            .xsmall()
                                            .icon(IconName::Undo2)
                                            .tooltip("Reset")
                                            .disabled(at_default)
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.reset_develop_slider(id, cx);
                                            })),
                                    ),
                            ),
                    )
                    .child(DevelopSliderElement::new(control)),
            );
        }
        rows
    }

    /// Put one slider back to its default: the same commit a released drag
    /// makes, so the render and the saved edit follow.
    fn reset_develop_slider(&mut self, id: SliderId, cx: &mut Context<Self>) {
        if let Some(spec) = id.spec() {
            self.on_develop_release(id, spec.default as f32, cx);
            cx.notify();
        }
    }

    /// Sync the edit buffer's slider values into the widget states. A live
    /// drag on one slider keeps its thumb where the gesture put it; the rest
    /// track the buffer.
    fn sync_sliders(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for (id, control) in &self.develop_sliders {
            let session_value = self.develop.slider_value(*id) as f32;
            let matches_slider = matches!(control.read(cx).value(), SliderValue::Single(v) if (v - session_value).abs() < 1e-3);
            let dragging_this = self.develop.dragging == Some(*id);
            if !matches_slider && !dragging_this {
                control.update(cx, |control, cx| {
                    control.set_value(session_value, window, cx);
                });
            }
        }
    }

    /// Subscribe-backed live slider change (`SliderEvent::Change`): write the
    /// change to the edit buffer, then render the downscaled proxy live
    /// through the coalescing worker.
    pub fn on_develop_change(&mut self, id: SliderId, value: f32, cx: &mut Context<Self>) {
        let item = self.develop_item_id();
        if self.develop.owner() != Some(item) {
            return;
        }
        self.develop.set_slider(id, value as f64);
        self.develop_dirty = true;
        self.queue.clear_failed(item);
        self.request_render(RenderSize::Proxy(PROXY_DRAG_LONG_EDGE), None, cx);
    }

    /// Drag released (`SliderEvent::Release`): final value in the buffer, the
    /// bounded release proxy renders in the background, and the edit persists.
    pub fn on_develop_release(&mut self, id: SliderId, value: f32, cx: &mut Context<Self>) {
        let item = self.develop_item_id();
        if self.develop.owner() != Some(item) {
            return;
        }
        self.develop.end_drag(id, value as f64);
        self.develop_dirty = true;
        self.queue.clear_failed(item);
        self.request_render(RenderSize::Proxy(PROXY_RELEASE_LONG_EDGE), None, cx);
        self.persist_current_edit(cx);
    }

    /// The catalog row id of the item the develop panel is editing.
    fn develop_item_id(&self) -> i64 {
        self.current_item().map(|row| row.item_id).unwrap_or(-1)
    }

    /// Request a render; the worker takes the latest request only, so a
    /// slider drag coalesces into one render at a time (review B2).
    fn request_render(
        &mut self,
        size: RenderSize,
        then: Option<RenderSize>,
        cx: &mut Context<Self>,
    ) {
        if self.queue.request(size, then) {
            self.spawn_render_worker(cx);
        }
    }

    /// The worker's next job against the current item, or `None` to rest.
    fn take_render_job(
        &mut self,
    ) -> Option<(i64, String, RenderSize, DevelopSettings, CancelToken)> {
        loop {
            let (size, token) = self.queue.take()?;
            if let Some(item) = self.current_item() {
                return Some((
                    item.item_id,
                    item.display_path.clone(),
                    size,
                    self.develop.buffer().clone(),
                    token,
                ));
            }
            self.queue.complete(false);
        }
    }

    /// The latest-request worker loop: each pass renders what is wanted now,
    /// so slider changes during a render supersede it instead of piling up.
    /// Rendering runs on the background executor, never the UI thread.
    fn spawn_render_worker(&mut self, cx: &mut Context<Self>) {
        let renderer = Arc::clone(&self.renderer);
        cx.spawn(async move |this, cx| {
            loop {
                let job = this
                    .update(cx, |this, _| this.take_render_job())
                    .ok()
                    .flatten();
                let Some((item_id, path, size, develop, token)) = job else {
                    break;
                };
                let frame = {
                    let renderer = Arc::clone(&renderer);
                    cx.background_executor()
                        .spawn(async move {
                            renderer.render_path(Path::new(&path), size, &develop, None, &token)
                        })
                        .await
                };
                let applied = this
                    .update(cx, |this, cx| {
                        this.finish_render(item_id, frame, cx);
                        cx.notify();
                    })
                    .is_ok();
                if !applied {
                    break;
                }
            }
        })
        .detach();
    }

    /// A render ended. A cancelled one was superseded and the worker's next
    /// pass reads whatever superseded it. Any other failure is terminal for
    /// the paint-time kick (review B1): the panel shows the error and a paint
    /// frame does not re-kick the request; a new edit, navigation away and
    /// back, or reopening the folder retries.
    fn finish_render(
        &mut self,
        item_id: i64,
        frame: edit_core::Result<edit_core::Frame>,
        cx: &mut Context<Self>,
    ) {
        match frame {
            Ok(frame) => {
                self.queue.complete(true);
                self.queue.finish(item_id);
                if self.develop_item_id() == item_id {
                    self.insert_develop_frame(item_id, frame);
                    self.prewarm_neighbors(cx);
                }
            }
            Err(edit_core::Error::Cancelled) => {
                self.queue.complete(false);
                self.queue.finish(item_id);
            }
            Err(e) => {
                self.queue.complete(false);
                self.queue.mark_failed(item_id);
                self.develop_status = format!("Render failed: {e}");
            }
        }
    }

    fn insert_develop_frame(&mut self, item_id: i64, frame: edit_core::Frame) {
        if let Some(rgba) = image::RgbaImage::from_raw(frame.width, frame.height, frame.pixels) {
            self.develop_image = Some(Arc::new(RenderImage::new([image::Frame::new(rgba)])));
            self.develop_image_item = Some(item_id);
        }
    }

    /// Warm the drag-edge proxy decode for the current item's neighbors
    /// (current ± 1..2, plan section 2.3) once the panel is idle, so
    /// Left/Right renders hit the render service's bounded decode cache
    /// instead of waiting on disk. One prewarm per item.
    fn prewarm_neighbors(&mut self, cx: &mut Context<Self>) {
        if self.queue.busy() {
            return;
        }
        if self.prewarmed.len() >= THUMB_CACHE_CAP {
            self.prewarmed.clear();
        }
        let rows = Arc::clone(&self.rows);
        for offset in 1..=2 {
            for neighbor in [
                self.current.checked_add(offset),
                self.current.checked_sub(offset),
            ]
            .into_iter()
            .flatten()
            {
                let Some(item) = rows.get(neighbor) else {
                    continue;
                };
                if !self.prewarmed.insert(item.item_id) {
                    continue;
                }
                let renderer = Arc::clone(&self.renderer);
                let token = self.prewarm_token.clone();
                let path = item.display_path.clone();
                let item_id = item.item_id;
                cx.spawn(async move |this, cx| {
                    let warmed = cx
                        .background_executor()
                        .spawn(async move {
                            renderer.prewarm(Path::new(&path), PROXY_DRAG_LONG_EDGE, &token)
                        })
                        .await;
                    if warmed.is_err() {
                        // Cancelled or unreadable: a later pass may retry.
                        this.update(cx, |this, _| {
                            this.prewarmed.remove(&item_id);
                        })
                        .ok();
                    }
                })
                .detach();
            }
        }
    }

    /// Commit the edit buffer: the catalog row writes synchronously, so a
    /// release or a navigation is durable the moment it returns, and the
    /// `crs:` sidecar fans out best-effort in the background, serialized per
    /// item (review M1).
    fn persist_current_edit(&mut self, cx: &mut Context<Self>) {
        for (item_id, json) in self.commit_catalog() {
            self.write_sidecar(item_id, json, cx);
        }
        cx.notify();
    }

    /// Write every edit not yet in the catalog: earlier failures first, then
    /// the dirty buffer. Returns what landed. A failed write stays recorded
    /// (retried at the next commit point and at quit) and surfaces in the
    /// panel; nothing is marked clean before the catalog acknowledged it.
    fn commit_catalog(&mut self) -> Vec<(i64, String)> {
        if self.develop_dirty
            && let Some(item_id) = self.develop.owner()
        {
            match serde_json::to_string(self.develop.buffer()) {
                Ok(json) => {
                    self.unsaved.insert(item_id, json);
                    self.develop_dirty = false;
                }
                Err(e) => self.develop_status = format!("Edit save failed: {e}"),
            }
        }
        let mut landed = Vec::new();
        for (item_id, json) in std::mem::take(&mut self.unsaved) {
            match self.catalog.upsert_edit(item_id, &json, None) {
                Ok(()) => landed.push((item_id, json)),
                Err(e) => {
                    self.develop_status = format!("Edit save failed: {e}");
                    self.unsaved.insert(item_id, json);
                }
            }
        }
        landed
    }

    fn sidecar_path(&self, item_id: i64) -> Option<PathBuf> {
        let item = self.items.iter().find(|row| row.item_id == item_id)?;
        Some(PathBuf::from(&item.display_path).with_extension("xmp"))
    }

    /// Best-effort `crs:` sidecar write; the newest edit per item wins.
    fn write_sidecar(&mut self, item_id: i64, develop_json: String, cx: &mut Context<Self>) {
        if let Some(json) = self.sidecars.submit(item_id, develop_json) {
            self.spawn_sidecar(item_id, json, cx);
        }
    }

    /// One sidecar write on the background executor. Its ack starts the
    /// item's queued write, if any, and the outcome surfaces in the panel.
    fn spawn_sidecar(&mut self, item_id: i64, json: String, cx: &mut Context<Self>) {
        let Some(sidecar) = self.sidecar_path(item_id) else {
            if let Some(next) = self.sidecars.ack(item_id) {
                self.spawn_sidecar(item_id, next, cx);
            }
            return;
        };
        let catalog = Arc::clone(&self.catalog);
        cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    edit_metadata::persist_sidecar(&catalog, item_id, &json, None, &sidecar)
                })
                .await;
            this.update(cx, |this, cx| {
                // A catalog failure on record is the more important message.
                if this.unsaved.is_empty() {
                    this.develop_status = match outcome {
                        Ok(edit_metadata::PersistOutcome::Written) => "Edit saved".to_string(),
                        Ok(edit_metadata::PersistOutcome::SidecarSkipped(reason)) => {
                            format!("Edit saved (sidecar skipped: {reason})")
                        }
                        Err(e) => format!("Edit saved (sidecar failed: {e})"),
                    };
                }
                if let Some(next) = this.sidecars.ack(item_id) {
                    this.spawn_sidecar(item_id, next, cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Quit: commit what the catalog has not seen, then give the sidecars of
    /// those commits and of the queued writes the shutdown window.
    fn flush_for_quit(
        &mut self,
        cx: &mut Context<Self>,
    ) -> impl Future<Output = ()> + 'static + use<> {
        let mut writes = self.commit_catalog();
        writes.extend(self.sidecars.drain_queued());
        let jobs: Vec<(i64, String, PathBuf)> = writes
            .into_iter()
            .filter_map(|(item_id, json)| Some((item_id, json, self.sidecar_path(item_id)?)))
            .collect();
        let catalog = Arc::clone(&self.catalog);
        cx.background_executor().spawn(async move {
            for (item_id, json, sidecar) in jobs {
                edit_metadata::persist_sidecar(&catalog, item_id, &json, None, &sidecar).ok();
            }
        })
    }

    fn set_rating(&mut self, rating: i64, cx: &mut Context<Self>) {
        let rating = rating.clamp(0, 5);
        let current = match self.rows.get(self.current) {
            Some(item) => item.clone(),
            None => return,
        };
        let want = pair_key(&current.dir, &current.base_name);
        // Ratings fan out to every photo in the pair cell.
        if let Some(root_id) = self.root_id
            && let Ok(photos) = self.catalog.list_photos_by_root(root_id)
        {
            for photo in photos {
                if pair_key(&photo.dir, &photo.base_name) == want {
                    self.catalog.set_photo_rating(photo.id, rating).ok();
                }
            }
        }
        if let Some(item) = self.items.iter_mut().find(|i| i.item_id == current.item_id) {
            item.rating = rating;
        }
        // A rating can filter its own cell out; refocus after rebuild.
        self.rebuild_rows();
        cx.notify();
    }

    fn render_header(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, border) = (theme.muted_foreground, theme.border);
        let count = self.rows.len();
        let status = self.scan_status.clone();
        let root = self.root_label.clone();
        let mode_label = match self.mode {
            Mode::Grid => "Grid (G for single)",
            Mode::Single => "Single (G for grid)",
        };
        h_flex()
            .gap_2()
            .items_center()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(border)
            .child(div().text_sm().child(format!("Session: {root}")))
            .child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child(format!("{count} items")),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child(format!("Sort: {}", self.sort_label())),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child(mode_label.to_string()),
            )
            .child(div().text_sm().text_color(muted).child(status))
    }

    fn sort_label(&self) -> &'static str {
        match self.sort {
            SortOrder::IngestOrder => "ingest order",
            SortOrder::FileName => "filename",
        }
    }

    /// Badge line for a cell: rating stars, thumb state, upload state, and
    /// the amber "new" marker while ingest is still landing.
    fn cell_badges(item: &BrowseItem) -> String {
        let stars = "★".repeat(item.rating.clamp(0, 5) as usize);
        let thumb = match item.thumb {
            ThumbState::Queued => "queued",
            ThumbState::Running => "…",
            ThumbState::Ready => "ready",
            ThumbState::Failed => "failed",
        };
        let upload = match item.upload {
            UploadBadge::None => String::new(),
            UploadBadge::Queued => " ● queued".to_string(),
            UploadBadge::Uploading => " ● uploading".to_string(),
            UploadBadge::Done => " ● done".to_string(),
            UploadBadge::Failed => " ● failed".to_string(),
        };
        let meta = match item.meta {
            MetaBadge::None => String::new(),
            MetaBadge::Queued => " ⊘ meta:queued".to_string(),
            MetaBadge::Writing => " ⊘ meta:writing".to_string(),
            MetaBadge::Done => " ⊘ meta:done".to_string(),
            MetaBadge::Failed => " ⊘ meta:failed".to_string(),
        };
        let fresh = if item.ingest_fresh { " ◌ new" } else { "" };
        format!("{stars} {} · {thumb}{upload}{meta}{fresh}", item.base_name)
    }

    fn render_grid(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        // Cheap per-frame snapshots: the row table and thumb cache travel
        // by Arc; selection is a small index set. No item is cloned here.
        let rows = Arc::clone(&self.rows);
        let selected: Arc<BTreeSet<usize>> = Arc::new(self.selection.selected.clone());
        let current = self.current;
        // Queue decodes for the first cells still missing art (bounded per
        // frame); completed frames arrive via insert_thumb + notify.
        let mut budget = THUMB_REQUEST_BUDGET;
        for row in rows.iter() {
            if budget == 0 {
                break;
            }
            if !self.thumbs.contains_key(&row.item_id) && !self.thumb_pending.contains(&row.item_id)
            {
                self.request_thumb(cx, row.item_id, row.display_path.clone());
                budget -= 1;
            }
        }
        let thumbs: Arc<HashMap<i64, Arc<RenderImage>>> = Arc::new(
            self.thumbs
                .iter()
                .map(|(id, art)| (*id, Arc::clone(art)))
                .collect(),
        );
        let row_count = rows.len().div_ceil(GRID_COLS);
        // The row closure is 'static, so cell clicks update through an
        // owned entity handle instead of a borrowed context.
        let entity = cx.entity();
        let theme = cx.theme();
        let (current_border, selected_border, idle_border) =
            (theme.primary, theme.muted_foreground, theme.border);
        let (cell_bg, empty_bg, empty_fg, text_fg) = (
            theme.secondary,
            theme.muted,
            theme.muted_foreground,
            theme.foreground,
        );
        div().size_full().overflow_hidden().child(
            uniform_list("contact-sheet", row_count, move |range, _window, _cx| {
                let mut cells = Vec::new();
                for row in range {
                    let span = row * GRID_COLS..((row + 1) * GRID_COLS).min(rows.len());
                    if span.is_empty() {
                        continue;
                    }
                    let mut row_el = h_flex().w_full().justify_start();
                    for index in span {
                        let item = &rows[index];
                        let border = if index == current {
                            current_border
                        } else if selected.contains(&index) {
                            selected_border
                        } else {
                            idle_border
                        };
                        let art: AnyElement = match thumbs.get(&item.item_id) {
                            Some(art) => {
                                img(Arc::clone(art)).w_full().h(px(96.0)).into_any_element()
                            }
                            None => div()
                                .w_full()
                                .h(px(96.0))
                                .flex()
                                .items_center()
                                .justify_center()
                                .bg(empty_bg)
                                .text_color(empty_fg)
                                .text_sm()
                                .child(item.label())
                                .into_any_element(),
                        };
                        let item_id = item.item_id;
                        let badges = Self::cell_badges(item);
                        let entity = entity.clone();
                        row_el = row_el.child(
                            div()
                                .id(("grid-cell", index))
                                .w(px(180.0))
                                .h(px(150.0))
                                .m_1()
                                .p_1()
                                .border_2()
                                .border_color(border)
                                .rounded_md()
                                .bg(cell_bg)
                                .cursor_pointer()
                                .on_click(move |_, _, cx| {
                                    entity.update(cx, |this, cx| this.goto_item(item_id, cx));
                                })
                                .child(art)
                                .child(div().text_xs().text_color(text_fg).child(badges)),
                        );
                    }
                    cells.push(row_el);
                }
                cells
            })
            .size_full(),
        )
    }

    fn render_single(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let item = self.current_item();
        let zoom_label = match self.zoom.mode {
            ZoomMode::Fit => "fit".to_string(),
            ZoomMode::Hundred => "100%".to_string(),
            ZoomMode::Scale(s) => format!("{:.0}%", s * 100.0),
        };
        if let Some(row) = item.as_ref() {
            self.request_thumb(cx, row.item_id, row.display_path.clone());
            // Kick the develop load and render for a photo with no resolved
            // art yet, unless it already failed (failures are terminal here).
            if self.queue.should_kick(row.item_id, self.develop_image_item) {
                self.develop_image = None;
                self.develop_image_item = None;
                self.queue.begin(row.item_id);
                self.schedule_current_edit_load(cx);
            }
        }
        let image = item.as_ref().and_then(|row| {
            self.develop_image
                .as_ref()
                .filter(|_| self.develop_image_item == Some(row.item_id))
                .map(Arc::clone)
                .or_else(|| self.thumbs.get(&row.item_id).map(Arc::clone))
        });
        let zoom = self.zoom;
        let fit_cell = Rc::clone(&self.fit_scale);
        let title = item
            .as_ref()
            .map(|row| row.label())
            .unwrap_or_else(|| "No item".to_string());
        v_flex()
            .size_full()
            .child(
                div()
                    .px_2()
                    .py_1()
                    .text_sm()
                    .child(format!("{title} · {zoom_label}")),
            )
            .child(
                div()
                    .id("single-canvas")
                    .flex_1()
                    .size_full()
                    .overflow_hidden()
                    .bg(cx.theme().background)
                    .child(
                        canvas(
                            |bounds, _, _| bounds,
                            move |bounds, bounds_state, window, _| {
                                if let Some(image) = image.clone() {
                                    let img_size = image.size(0);
                                    let w = img_size.width.0 as f32;
                                    let h = img_size.height.0 as f32;
                                    if w > 0.0 && h > 0.0 {
                                        let bw: f32 = bounds.size.width.into();
                                        let bh: f32 = bounds.size.height.into();
                                        let fit = (bw / w).min(bh / h).max(0.01);
                                        fit_cell.set(fit);
                                        let scale = zoom.scale_in(w, h, bw, bh);
                                        let (ox, oy) = paint_origin(w, h, bw, bh, scale, zoom);
                                        let dw = px(w * scale);
                                        let dh = px(h * scale);
                                        let origin = point(
                                            bounds.origin.x + ox.into(),
                                            bounds.origin.y + oy.into(),
                                        );
                                        let image_bounds =
                                            Bounds::new(origin, gpui_kit::size(dw, dh));
                                        let _ = window.paint_image(
                                            bounds_state,
                                            image_bounds,
                                            Corners::default(),
                                            image,
                                            0,
                                            false,
                                        );
                                    }
                                }
                            },
                        )
                        // A canvas lays out as 0x0 unless told otherwise, which
                        // handed the paint closure empty bounds.
                        .size_full(),
                    )
                    .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                        let dy: f32 = match &event.delta {
                            ScrollDelta::Pixels(p) => p.y.into(),
                            ScrollDelta::Lines(p) => p.y * 20.0,
                        };
                        let fit = this.fit_scale.get();
                        if dy < 0.0 {
                            this.zoom.scroll_zoom(1.1, fit);
                        } else if dy > 0.0 {
                            this.zoom.scroll_zoom(1.0 / 1.1, fit);
                        }
                        cx.notify();
                    }))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, _, _| {
                            this.drag_from = Some(event.position);
                        }),
                    )
                    .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                        if let Some(from) = this.drag_from {
                            let dx_pixels: f32 = (event.position.x - from.x).into();
                            let dy_pixels: f32 = (event.position.y - from.y).into();
                            this.zoom.enter_free(this.fit_scale.get());
                            this.zoom.drag_pan(dx_pixels / 200.0, dy_pixels / 200.0);
                            this.drag_from = Some(event.position);
                            cx.notify();
                        }
                    }))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseUpEvent, _, _| {
                            this.drag_from = None;
                        }),
                    ),
            )
            .child(self.render_strip(cx))
    }

    /// Bottom film strip (plan section 2.3): thumbnails with badge
    /// overlays, the current item outlined, click to navigate. A scroll
    /// handle keeps the current cell in view on walk/goto.
    fn render_strip(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.current;
        let rows = Arc::clone(&self.rows);
        let mut budget = THUMB_REQUEST_BUDGET;
        for (index, row) in rows.iter().enumerate() {
            if budget == 0 {
                break;
            }
            // Warm the neighborhood of the current cell first.
            if index.abs_diff(current) > 12 {
                continue;
            }
            if !self.thumbs.contains_key(&row.item_id) && !self.thumb_pending.contains(&row.item_id)
            {
                self.request_thumb(cx, row.item_id, row.display_path.clone());
                budget -= 1;
            }
        }
        let theme = cx.theme();
        let (current_outline, idle_outline, cell_bg) =
            (theme.primary, theme.border, theme.secondary);
        let (empty_bg, empty_fg, badge_fg) =
            (theme.muted, theme.muted_foreground, theme.muted_foreground);
        let strip_bg = theme.sidebar;
        let cells: Vec<_> = rows
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let outline = if index == current {
                    current_outline
                } else {
                    idle_outline
                };
                let item_id = item.item_id;
                let badges = Self::cell_badges(item);
                let art: AnyElement = match self.thumbs.get(&item_id) {
                    Some(art) => img(Arc::clone(art)).w_full().h(px(44.0)).into_any_element(),
                    None => div()
                        .w_full()
                        .h(px(44.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .bg(empty_bg)
                        .text_xs()
                        .text_color(empty_fg)
                        .child(item.base_name.clone())
                        .into_any_element(),
                };
                div()
                    .id(("strip", index))
                    .w(px(96.0))
                    .h(px(72.0))
                    .m(px(2.0))
                    .border_2()
                    .border_color(outline)
                    .rounded_sm()
                    .bg(cell_bg)
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.goto_item(item_id, cx);
                    }))
                    .child(art)
                    .child(
                        div()
                            .text_xs()
                            .text_color(badge_fg)
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .child(badges),
                    )
            })
            .collect();
        div()
            .id("film-strip")
            .w_full()
            .h(px(96.0))
            .flex()
            .flex_row()
            .overflow_x_scroll()
            .track_scroll(&self.strip_scroll)
            .bg(strip_bg)
            .children(cells)
    }

    /// Left metadata panel: item summary plus the template entry point
    /// (plan section 2.7: the template editor opens over the shell).
    fn render_metadata(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (panel_bg, panel_fg, panel_border, muted, warning) = (
            theme.sidebar,
            theme.sidebar_foreground,
            theme.sidebar_border,
            theme.muted_foreground,
            theme.warning,
        );
        let (title, dir, rating, meta_state) = match self.current_item() {
            Some(row) => (
                row.base_name.clone(),
                row.dir.clone(),
                row.rating,
                Self::meta_text(row.meta),
            ),
            None => ("—".to_string(), String::new(), 0, String::new()),
        };
        v_flex()
            .w(px(220.0))
            .h_full()
            .px_2()
            .py_1()
            .gap_1()
            .bg(panel_bg)
            .text_color(panel_fg)
            .border_r_1()
            .border_color(panel_border)
            .child(div().text_sm().child("Metadata".to_string()))
            .child(div().text_xs().child(format!("File: {title}")))
            .child(div().text_xs().child(format!("Dir: {dir}")))
            .child(div().text_xs().child(format!("Rating: {rating}/5")))
            .when(!meta_state.is_empty(), |panel| {
                panel.child(div().text_xs().text_color(warning).child(meta_state))
            })
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child("T — open the IPTC template editor.".to_string()),
            )
    }

    fn meta_text(meta: MetaBadge) -> String {
        match meta {
            MetaBadge::None => String::new(),
            MetaBadge::Queued => "Template: queued".to_string(),
            MetaBadge::Writing => "Template: writing…".to_string(),
            MetaBadge::Done => "Template: applied".to_string(),
            MetaBadge::Failed => "Template: failed".to_string(),
        }
    }
}

impl Render for Browser {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_sliders(window, cx);
        let center: AnyElement = match self.mode {
            Mode::Grid => self.render_grid(cx).into_any_element(),
            Mode::Single => self.render_single(cx).into_any_element(),
        };
        // Right column collapses in grid mode (plan section 2.1).
        let right: AnyElement = match self.mode {
            Mode::Grid => div().into_any_element(),
            Mode::Single => self.render_develop_panel(cx).into_any_element(),
        };
        v_flex()
            .size_full()
            .child(
                div()
                    .id("browser-root")
                    .track_focus(&self.focus)
                    .on_action(cx.listener(|this, _: &NextItem, _, cx| this.walk(1, false, cx)))
                    .on_action(cx.listener(|this, _: &PrevItem, _, cx| this.walk(-1, false, cx)))
                    .on_action(cx.listener(|this, _: &NextRow, _, cx| {
                        this.walk(GRID_COLS as isize, false, cx);
                    }))
                    .on_action(cx.listener(|this, _: &PrevRow, _, cx| {
                        this.walk(-(GRID_COLS as isize), false, cx);
                    }))
                    .on_action(cx.listener(|this, _: &ExtendNext, _, cx| this.walk(1, true, cx)))
                    .on_action(cx.listener(|this, _: &ExtendPrev, _, cx| this.walk(-1, true, cx)))
                    .on_action(cx.listener(|this, _: &ExtendNextRow, _, cx| {
                        this.walk(GRID_COLS as isize, true, cx);
                    }))
                    .on_action(cx.listener(|this, _: &ExtendPrevRow, _, cx| {
                        this.walk(-(GRID_COLS as isize), true, cx);
                    }))
                    .on_action(cx.listener(|this, _: &ToggleView, _, cx| {
                        this.mode = match this.mode {
                            Mode::Grid => Mode::Single,
                            Mode::Single => Mode::Grid,
                        };
                        cx.notify();
                    }))
                    .on_action(cx.listener(|this, _: &FitView, _, cx| {
                        this.zoom.reset();
                        cx.notify();
                    }))
                    .on_action(cx.listener(|this, _: &HundredView, _, cx| {
                        this.zoom.show_hundred();
                        cx.notify();
                    }))
                    .on_action(cx.listener(|this, _: &SelectAllItems, _, cx| {
                        this.selection.select_all(this.rows.len());
                        this.clamp_to_visible();
                        cx.notify();
                    }))
                    .on_action(cx.listener(|this, _: &Rate0, _, cx| this.set_rating(0, cx)))
                    .on_action(cx.listener(|this, _: &Rate1, _, cx| this.set_rating(1, cx)))
                    .on_action(cx.listener(|this, _: &Rate2, _, cx| this.set_rating(2, cx)))
                    .on_action(cx.listener(|this, _: &Rate3, _, cx| this.set_rating(3, cx)))
                    .on_action(cx.listener(|this, _: &Rate4, _, cx| this.set_rating(4, cx)))
                    .on_action(cx.listener(|this, _: &Rate5, _, cx| this.set_rating(5, cx)))
                    .flex_1()
                    .size_full()
                    .flex()
                    .flex_col()
                    .child(self.render_header(cx))
                    .child(
                        h_flex()
                            .flex_1()
                            .size_full()
                            .child(self.render_metadata(cx))
                            .child(div().flex_1().size_full().overflow_hidden().child(center))
                            .child(right),
                    ),
            )
            .child(
                StatusBar::new()
                    .left(if self.template_status.is_empty() {
                        self.scan_status.clone()
                    } else {
                        format!("{} · {}", self.template_status, self.scan_status)
                    })
                    .right(format!(
                        "{} shown · O open folder · I ingest · G grid/single · Z fit · ⇧Z 100% · T templates",
                        self.rows.len()
                    )),
            )
    }
}

/// Worker body for both browse job kinds. Progress events drive the contact
/// sheet; the terminal event is sent by the worker loop itself. The scan
/// announces its own `(0, fresh_total)` upfront, so no pre-walk is needed.
pub fn browse_job_handler(
    catalog: Arc<Catalog>,
    job: &edit_catalog::ClaimedJob,
    tx: &flume::Sender<JobEvent>,
) -> std::result::Result<(), String> {
    match job.kind {
        JobKind::CatalogScan => {
            let payload: CatalogScanPayload = serde_json::from_str(&job.payload_json)
                .map_err(|e| format!("bad scan payload: {e}"))?;
            let root = PathBuf::from(&payload.root);
            let tx_each = tx.clone();
            let id = job.id;
            run_catalog_scan(&catalog, &root, &|done, total| {
                tx_each
                    .send(JobEvent {
                        id,
                        kind: JobEventKind::Progress {
                            done: done as u64,
                            total: total as u64,
                        },
                    })
                    .ok();
            })
            .map_err(|e| format!("scan failed: {e}"))?;
            Ok(())
        }
        JobKind::Thumb => {
            let payload: ThumbPayload = serde_json::from_str(&job.payload_json)
                .map_err(|e| format!("bad thumb payload: {e}"))?;
            let path = PathBuf::from(&payload.path);
            // Validating the bytes is the thumb work in M1: full proxy
            // renders arrive with the M2 render service.
            match BrowsePreviewProvider::thumbnail_bytes(&path) {
                Some(bytes) => BrowsePreviewProvider::decode_jpeg(&bytes)
                    .map(|_| ())
                    .map_err(|e| format!("thumb failed for {}: {e}", path.display())),
                None => Err(format!("no thumbnail for {}", path.display())),
            }
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod paint_tests {
    use super::*;

    fn fit_zoom() -> ZoomState {
        ZoomState {
            mode: ZoomMode::Fit,
            pan_x: 0.0,
            pan_y: 0.0,
        }
    }

    #[::core::prelude::v1::test]
    fn paint_applies_free_zoom_not_just_fit() {
        // Regression (review B1 blocking 4): the canvas used to paint the
        // fit scale even while zoom was in a free/100% state.
        let viewport = (800.0, 600.0);
        let frame = (1000.0, 750.0);
        assert!((fit_zoom().scale_in(frame.0, frame.1, viewport.0, viewport.1) - 0.8).abs() < 1e-6);

        let mut zoomed = fit_zoom();
        zoomed.mode = ZoomMode::Scale(2.0);
        assert!((zoomed.scale_in(frame.0, frame.1, viewport.0, viewport.1) - 2.0).abs() < 1e-6);
        // The paint origin centers the scaled frame, whatever the fit was.
        let (ox, oy) = paint_origin(frame.0, frame.1, viewport.0, viewport.1, 2.0, zoomed);
        assert!((ox - (viewport.0 - frame.0 * 2.0) / 2.0).abs() < 1e-6);
        assert!((oy - (viewport.1 - frame.1 * 2.0) / 2.0).abs() < 1e-6);

        let mut hundred = fit_zoom();
        hundred.mode = ZoomMode::Hundred;
        assert!((hundred.scale_in(frame.0, frame.1, viewport.0, viewport.1) - 1.0).abs() < 1e-6);
    }

    #[::core::prelude::v1::test]
    fn paint_applies_pan_offsets() {
        let viewport = (800.0, 600.0);
        let frame = (1000.0, 750.0);
        let (cx, cy) = paint_origin(frame.0, frame.1, viewport.0, viewport.1, 0.8, fit_zoom());
        let mut panned = fit_zoom();
        panned.pan_x = 0.25;
        panned.pan_y = -0.5;
        let (px, py) = paint_origin(frame.0, frame.1, viewport.0, viewport.1, 0.8, panned);
        assert!((px - (cx + 0.25 * frame.0 * 0.8)).abs() < 1e-6);
        assert!((py - (cy - 0.5 * frame.1 * 0.8)).abs() < 1e-6);
    }

    #[::core::prelude::v1::test]
    fn fit_honors_the_constraining_axis() {
        // Portrait frame in landscape viewport: fit scales by height.
        let mut zoom = fit_zoom();
        assert!((zoom.scale_in(500.0, 1500.0, 1000.0, 1000.0) - 1000.0 / 1500.0).abs() < 1e-6);
        // Landscape frame in portrait viewport: fit scales by width.
        assert!((zoom.scale_in(1500.0, 500.0, 1000.0, 1000.0) - 1000.0 / 1500.0).abs() < 1e-6);
        // Degenerate sizes never divide by zero.
        assert_eq!(zoom.scale_in(0.0, 1500.0, 1000.0, 1000.0), 1.0);
        zoom.show_hundred();
        assert_eq!(zoom.scale_in(500.0, 1500.0, 1000.0, 1000.0), 1.0);
    }
}
