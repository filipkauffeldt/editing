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

use std::cell::Cell;
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use edit_catalog::{Catalog, JobEvent, JobEventKind, JobId, JobKind};
use gpui_kit::component::{TitleBar, h_flex, status_bar::StatusBar, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::browse::{
    BrowseFilter, BrowseItem, CatalogScanPayload, MetaBadge, SelectionModel, SortOrder,
    ThumbPayload, ThumbState, UploadBadge, ZoomMode, ZoomState, apply_filter_sort, canonical_root,
    item_photo_paths, list_browse_items, pair_key, run_catalog_scan,
};
use crate::preview::BrowsePreviewProvider;
use edit_metadata::{MetadataWritePayload, TemplateFields};

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
    /// Thumb job id → photo path, resolved from the job payload on
    /// `Started` so progress/completion can mark the right cell.
    thumb_jobs: HashMap<JobId, String>,
    /// `metadata_write` job id → item id, resolved from the payload so
    /// per-item badges flip on the right row even across restarts.
    meta_jobs: HashMap<JobId, i64>,
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
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
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
        Self {
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
            scan_jobs: Vec::new(),
            template_status: String::new(),
            drag_from: None,
            strip_scroll: ScrollHandle::new(),
            fit_scale: Rc::new(Cell::new(1.0)),
        }
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

    /// Pick a folder: enqueue a `catalog_scan` job and show live progress.
    /// Workers do the catalog writes; progress events refresh the rows.
    /// The path is canonicalized first, so `photos` and `photos/` share one
    /// root row instead of two competing roots.
    pub fn open_folder(&mut self, path: PathBuf, cx: &mut Context<Self>) {
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
    /// the worker per photo. Sequence numbers are display order, 1-based.
    pub fn apply_template(
        &mut self,
        fields: TemplateFields,
        template_name: &str,
        cx: &mut Context<Self>,
    ) {
        let selection = self.selected_items();
        if selection.is_empty() || fields == TemplateFields::default() {
            self.template_status = "Apply skipped: nothing selected or template empty.".into();
            cx.notify();
            return;
        }
        let mut results: Vec<(i64, MetaBadge)> = Vec::with_capacity(selection.len());
        for (offset, item) in selection.iter().enumerate() {
            let paths = item_photo_paths(&self.catalog, item);
            let payload = serde_json::to_string(&MetadataWritePayload {
                item_id: item.item_id,
                paths,
                sequence: (offset + 1) as u64,
                captured_at: edit_catalog::db::now_unix(),
                fields_json: serde_json::to_string(&fields).unwrap_or_else(|_| "{}".to_string()),
            })
            .unwrap_or_else(|_| "{}".to_string());
            match self.catalog.enqueue_job(JobKind::MetadataWrite, &payload) {
                Ok(job_id) => {
                    self.meta_jobs.insert(job_id, item.item_id);
                    results.push((item.item_id, MetaBadge::Queued));
                }
                Err(_) => results.push((item.item_id, MetaBadge::Failed)),
            }
        }
        let queued: usize = results
            .iter()
            .filter(|(_, meta)| *meta == MetaBadge::Queued)
            .count();
        for (item_id, meta) in &results {
            if let Some(item) = self.items.iter_mut().find(|i| i.item_id == *item_id) {
                item.meta = *meta;
            }
        }
        // Skip a re-rebuild when nothing queued: items already carry their
        // failed badges; the display table still refreshes on notify.
        if queued > 0 {
            self.rebuild_rows();
        }
        self.template_status = if queued == 0 {
            format!("Apply failed to queue for {template_name}.")
        } else {
            format!("Applying template \"{template_name}\" to {queued} photos…")
        };
        cx.notify();
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
        self.meta_jobs.insert(id, payload.item_id);
        Some(payload.item_id)
    }

    fn set_meta(&mut self, item_id: i64, badge: MetaBadge) {
        let mut hit = false;
        for item in &mut self.items {
            if item.item_id == item_id {
                item.meta = badge;
                hit = true;
            }
        }
        if hit {
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
                    let in_flight = self.meta_jobs.len();
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
        self.selection.move_cursor(delta, extend, len);
        self.current = self.selection.cursor;
        self.zoom.reset();
        self.preload_neighbors(cx);
        self.strip_scroll.scroll_to_item(self.current);
        cx.notify();
    }

    fn goto(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.rows.is_empty() {
            return;
        }
        self.selection.select_single(index, self.rows.len());
        self.current = self.selection.cursor;
        self.zoom.reset();
        self.preload_neighbors(cx);
        self.strip_scroll.scroll_to_item(self.current);
        cx.notify();
    }

    fn goto_item(&mut self, item_id: i64, cx: &mut Context<Self>) {
        if let Some(index) = self.rows.iter().position(|row| row.item_id == item_id) {
            self.goto(index, cx);
        }
    }

    /// Queue neighbor proxies on the background executor so Left/Right
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

    fn render_header(&mut self, _cx: &mut Context<Self>) -> impl IntoElement {
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
            .child(div().text_sm().child(format!("Session: {root}")))
            .child(div().text_sm().child(format!("{count} items")))
            .child(
                div()
                    .text_sm()
                    .child(format!("Sort: {:?}", self.sort_label())),
            )
            .child(div().text_sm().child(mode_label.to_string()))
            .child(div().text_sm().child(status))
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
        div().size_full().overflow_hidden().child(uniform_list(
            "contact-sheet",
            row_count,
            move |range, _window, _cx| {
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
                            rgb(0x4C9AFF)
                        } else if selected.contains(&index) {
                            rgb(0x8A8A8A)
                        } else {
                            rgb(0x2A2A2A)
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
                                .bg(rgb(0x111111))
                                .text_color(rgb(0x999999))
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
                                .bg(rgb(0x1E1E1E))
                                .cursor_pointer()
                                .on_click(move |_, _, cx| {
                                    entity.update(cx, |this, cx| this.goto_item(item_id, cx));
                                })
                                .child(art)
                                .child(div().text_xs().text_color(rgb(0xCCCCCC)).child(badges)),
                        );
                    }
                    cells.push(row_el);
                }
                cells
            },
        ))
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
        }
        let image = item
            .as_ref()
            .and_then(|row| self.thumbs.get(&row.item_id))
            .map(Arc::clone);
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
                    .bg(rgb(0x0E0E0E))
                    .child(canvas(
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
                                    let image_bounds = Bounds::new(origin, gpui_kit::size(dw, dh));
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
                    ))
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
        let cells: Vec<_> = rows
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let outline = if index == current {
                    rgb(0x4C9AFF)
                } else {
                    rgb(0x2A2A2A)
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
                        .bg(rgb(0x111111))
                        .text_xs()
                        .text_color(rgb(0x999999))
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
                    .bg(rgb(0x1A1A1A))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.goto_item(item_id, cx);
                    }))
                    .child(art)
                    .child(div().text_xs().text_color(rgb(0xAAAAAA)).child(badges))
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
            .bg(rgb(0x141414))
            .children(cells)
    }

    /// Left metadata panel: item summary plus the template entry point
    /// (plan section 2.7: the template editor opens over the shell).
    fn render_metadata(&self, _cx: &mut Context<Self>) -> impl IntoElement {
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
            .bg(rgb(0x181818))
            .child(div().text_sm().child("Metadata".to_string()))
            .child(div().text_xs().child(format!("File: {title}")))
            .child(div().text_xs().child(format!("Dir: {dir}")))
            .child(div().text_xs().child(format!("Rating: {rating}/5")))
            .when(!meta_state.is_empty(), |panel| {
                panel.child(div().text_xs().text_color(rgb(0xBBBB88)).child(meta_state))
            })
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(0x888888))
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let center: AnyElement = match self.mode {
            Mode::Grid => self.render_grid(cx).into_any_element(),
            Mode::Single => self.render_single(cx).into_any_element(),
        };
        // Right column collapses in grid mode (plan section 2.1).
        let right: AnyElement = match self.mode {
            Mode::Grid => div().into_any_element(),
            Mode::Single => v_flex()
                .w(px(200.0))
                .h_full()
                .px_2()
                .py_1()
                .bg(rgb(0x181818))
                .child(div().text_sm().child("Develop (M2)".to_string()))
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(0x888888))
                        .child("Presets and sliders appear in M2.".to_string()),
                )
                .into_any_element(),
        };
        v_flex()
            .size_full()
            .child(
                TitleBar::new().child(
                    h_flex()
                        .w_full()
                        .pr_2()
                        .justify_between()
                        .child("editing — browse (M1)")
                        .child("[Open Folder] [Ingest] [Finish & next ▾]"),
                ),
            )
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
                        "{} shown · G grid/single · Z fit · ⇧Z 100% · T templates",
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
