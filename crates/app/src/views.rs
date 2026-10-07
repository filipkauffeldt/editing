//! GPUI browse surfaces for M1: contact sheet, single view, panels.
//!
//! The pure state lives in [`crate::browse`]; this module renders snapshots
//! of it. The catalog scan runs as a durable `catalog_scan` job and thumb
//! work as `thumb` jobs (plan section 1.2); workers report over flume and
//! [`Browser::on_job_event`] applies them to the visible rows.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use edit_catalog::{Catalog, JobEvent, JobEventKind, JobId, JobKind};
use gpui_kit::component::{TitleBar, h_flex, status_bar::StatusBar, v_flex};
use gpui_kit::*;

use crate::browse::{
    BrowseFilter, BrowseItem, CatalogScanPayload, SelectionModel, SortOrder, ThumbPayload,
    ThumbState, UploadBadge, ZoomState, apply_filter_sort, list_browse_items, run_catalog_scan,
};
use crate::preview::BrowsePreviewProvider;

/// Cells per contact-sheet row over `uniform_list`.
pub const GRID_COLS: usize = 4;

gpui_kit::actions!(
    browser,
    [
        NextItem,
        PrevItem,
        NextRow,
        PrevRow,
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
    thumb_images: HashMap<i64, Arc<RenderImage>>,
    thumb_jobs: HashMap<JobId, String>,
    scan_jobs: Vec<JobId>,
    drag_from: Option<Point<Pixels>>,
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
            KeyBinding::new("shift-right", NextItem, None),
            KeyBinding::new("shift-left", PrevItem, None),
            KeyBinding::new("g", ToggleView, None),
            KeyBinding::new("z", FitView, None),
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
            thumb_images: HashMap::new(),
            thumb_jobs: HashMap::new(),
            scan_jobs: Vec::new(),
            drag_from: None,
        }
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
    pub fn open_folder(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let payload = serde_json::to_string(&CatalogScanPayload {
            root: path.to_string_lossy().into_owned(),
        })
        .unwrap_or_else(|_| "{}".to_string());
        match self.catalog.enqueue_job(JobKind::CatalogScan, &payload) {
            Ok(id) => {
                self.scan_jobs.push(id);
                self.root_label = path.to_string_lossy().into_owned();
                self.scan_status = format!("Scan queued: {}", self.root_label);
            }
            Err(e) => self.scan_status = format!("Scan failed to queue: {e}"),
        }
        cx.notify();
    }

    /// Synchronous scan used by headless flows and tests: runs the same
    /// catalog writes the `catalog_scan` worker would, without job events.
    pub fn scan_folder_sync(catalog: &Catalog, path: &std::path::Path) -> Vec<String> {
        let summary = run_catalog_scan(catalog, path, &|_, _| {}).unwrap_or_default();
        catalog
            .list_photos_by_root(summary.root_id)
            .unwrap_or_default()
            .into_iter()
            .map(|photo| photo.path)
            .collect()
    }

    fn refresh_items(&mut self) {
        if let Some(root_id) = self.root_id
            && let Ok(items) = list_browse_items(&self.catalog, root_id)
        {
            self.merge_thumbs(items);
        }
    }

    fn merge_thumbs(&mut self, fresh: Vec<BrowseItem>) {
        // Keep per-photo thumb/upload state across rescans keyed by item id.
        let kept: HashMap<i64, (ThumbState, UploadBadge, bool)> = self
            .items
            .iter()
            .map(|item| (item.item_id, (item.thumb, item.upload, item.ingest_fresh)))
            .collect();
        self.items = fresh;
        for item in &mut self.items {
            if let Some((thumb, upload, fresh_flag)) = kept.get(&item.item_id) {
                item.thumb = *thumb;
                item.upload = *upload;
                item.ingest_fresh = *fresh_flag;
            } else {
                item.ingest_fresh = true;
            }
        }
        self.sort.apply(&mut self.items);
        self.selection.clamp(self.visible().len());
        self.current = self.current.min(self.visible().len().saturating_sub(1));
    }

    /// Display rows after filter/sort.
    pub fn visible(&self) -> Vec<&BrowseItem> {
        apply_filter_sort(&self.items, &self.filter, self.sort)
    }

    fn current_item(&self) -> Option<BrowseItem> {
        self.visible().get(self.current).map(|item| (*item).clone())
    }

    /// Apply one worker event to the visible rows.
    pub fn on_job_event(&mut self, event: &JobEvent) {
        match &event.kind {
            JobEventKind::Started { .. } => {
                if self.scan_jobs.contains(&event.id) {
                    self.scan_status = "Scanning folder…".to_string();
                } else if let Some(path) = self.thumb_jobs.get(&event.id).cloned() {
                    self.mark_thumb(&path, ThumbState::Running);
                }
            }
            JobEventKind::Progress { done, total } => {
                if self.scan_jobs.contains(&event.id) {
                    self.scan_status = format!("Scanning: {done}/{total}");
                    self.refresh_items();
                }
            }
            JobEventKind::Succeeded => {
                if self.scan_jobs.contains(&event.id) {
                    self.scan_status = "Scan complete".to_string();
                    self.refresh_items();
                } else if let Some(path) = self.thumb_jobs.remove(&event.id) {
                    self.mark_thumb(&path, ThumbState::Ready);
                    self.refresh_items();
                }
            }
            JobEventKind::Failed { error } => {
                if self.scan_jobs.contains(&event.id) {
                    self.scan_status = format!("Scan failed: {error}");
                } else if let Some(path) = self.thumb_jobs.remove(&event.id) {
                    self.mark_thumb(&path, ThumbState::Failed);
                }
            }
        }
    }

    fn mark_thumb(&mut self, path: &str, state: ThumbState) {
        for item in &mut self.items {
            if item.display_path == path {
                item.thumb = state;
                item.ingest_fresh = false;
            }
        }
    }

    fn walk(&mut self, delta: isize, extend: bool, cx: &mut Context<Self>) {
        let len = self.visible().len();
        if len == 0 {
            return;
        }
        self.selection.move_cursor(delta, extend, len);
        self.current = self.selection.cursor;
        self.zoom.reset();
        self.preload_neighbors();
        cx.notify();
    }

    fn goto(&mut self, index: usize, cx: &mut Context<Self>) {
        let len = self.visible().len();
        if len == 0 {
            return;
        }
        self.selection.select_single(index, len);
        self.current = self.selection.cursor;
        self.zoom.reset();
        self.preload_neighbors();
        cx.notify();
    }

    /// Decode neighbor proxies so Left/Right never waits on disk.
    fn preload_neighbors(&mut self) {
        let neighbors: Vec<BrowseItem> = self
            .visible()
            .into_iter()
            .cloned()
            .enumerate()
            .filter(|(index, _)| {
                *index == self.current.saturating_add(1)
                    || *index == self.current.saturating_add(2)
                    || Some(*index) == self.current.checked_sub(1)
                    || Some(*index) == self.current.checked_sub(2)
            })
            .map(|(_, item)| item)
            .collect();
        for item in &neighbors {
            self.cached_image(item);
        }
    }

    fn cached_image(&mut self, item: &BrowseItem) -> Option<Arc<RenderImage>> {
        if let Some(hit) = self.thumb_images.get(&item.item_id) {
            return Some(Arc::clone(hit));
        }
        let bytes =
            BrowsePreviewProvider::thumbnail_bytes(PathBuf::from(&item.display_path).as_path())?;
        let decoded = image::load_from_memory(&bytes).ok()?.to_rgba8();
        let frame = image::Frame::new(decoded);
        let image = Arc::new(RenderImage::new([frame]));
        self.thumb_images.insert(item.item_id, Arc::clone(&image));
        Some(image)
    }

    fn set_rating(&mut self, rating: i64, cx: &mut Context<Self>) {
        let rating = rating.clamp(0, 5);
        let current = match self.visible().get(self.current) {
            Some(item) => (*item).clone(),
            None => return,
        };
        // Ratings fan out to every photo in the pair cell.
        if let Some(root_id) = self.root_id
            && let Ok(photos) = self.catalog.list_photos_by_root(root_id)
        {
            for photo in photos {
                if photo.dir == current.dir && photo.base_name == current.base_name {
                    self.catalog.set_photo_rating(photo.id, rating).ok();
                }
            }
        }
        if let Some(item) = self.items.iter_mut().find(|i| i.item_id == current.item_id) {
            item.rating = rating;
        }
        cx.notify();
    }

    fn render_header(&mut self, _cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.visible().len();
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

    fn render_grid(&mut self, _cx: &mut Context<Self>) -> impl IntoElement {
        // Snapshot everything the 'static row closure needs: rows with their
        // global display indices, plus selection/current for outlines.
        let flat: Vec<(BrowseItem, usize)> = self
            .visible()
            .into_iter()
            .cloned()
            .enumerate()
            .map(|(index, item)| (item, index))
            .collect();
        let selected: std::collections::BTreeSet<usize> = self.selection.selected.clone();
        let current = self.current;
        let rows: Vec<Vec<(BrowseItem, usize)>> =
            flat.chunks(GRID_COLS).map(|chunk| chunk.to_vec()).collect();
        let row_count = rows.len();
        let rows = Arc::new(rows);
        let selected = Arc::new(selected);
        div().size_full().overflow_hidden().child(uniform_list(
            "contact-sheet",
            row_count,
            move |range, _window, _cx| {
                let mut cells = Vec::new();
                for row in range {
                    if let Some(chunk) = rows.get(row) {
                        let mut row_el = h_flex().w_full().justify_start();
                        for (item, index) in chunk.clone() {
                            let border = if index == current {
                                rgb(0x4C9AFF)
                            } else if selected.contains(&index) {
                                rgb(0x8A8A8A)
                            } else {
                                rgb(0x2A2A2A)
                            };
                            let stars = "★".repeat(item.rating.clamp(0, 5) as usize);
                            let thumb_label = match item.thumb {
                                ThumbState::Queued => "queued",
                                ThumbState::Running => "…",
                                ThumbState::Ready => "ready",
                                ThumbState::Failed => "failed",
                            };
                            let thumb: AnyElement = if item.has_jpeg {
                                img(PathBuf::from(item.display_path.clone()))
                                    .w_full()
                                    .h(px(96.0))
                                    .into_any_element()
                            } else {
                                div()
                                    .w_full()
                                    .h(px(96.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .bg(rgb(0x111111))
                                    .text_color(rgb(0x999999))
                                    .text_sm()
                                    .child(item.label())
                                    .into_any_element()
                            };
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
                                    .child(thumb)
                                    .child(div().text_xs().text_color(rgb(0xCCCCCC)).child(
                                        format!(
                                            "{stars} {} · {thumb_label}",
                                            item.base_name.clone()
                                        ),
                                    )),
                            );
                        }
                        cells.push(row_el);
                    }
                }
                cells
            },
        ))
    }

    fn render_single(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let item = self.current_item();
        let zoom_label = match self.zoom.mode {
            crate::browse::ZoomMode::Fit => "fit".to_string(),
            crate::browse::ZoomMode::Hundred => "100%".to_string(),
            crate::browse::ZoomMode::Scale(s) => format!("{:.0}%", s * 100.0),
        };
        let image = item.as_ref().and_then(|row| self.cached_image(row));
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
                                    let scale = (bw / w).min(bh / h).max(0.01);
                                    let dw = px(w * scale);
                                    let dh = px(h * scale);
                                    let origin = point(
                                        bounds.origin.x + (bounds.size.width - dw) / 2.0,
                                        bounds.origin.y + (bounds.size.height - dh) / 2.0,
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
                        if dy < 0.0 {
                            this.zoom.scroll_zoom(1.1);
                        } else if dy > 0.0 {
                            this.zoom.scroll_zoom(1.0 / 1.1);
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
                            this.zoom.enter_free(1.0);
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

    fn render_strip(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.current;
        let cells: Vec<_> = self
            .visible()
            .into_iter()
            .cloned()
            .enumerate()
            .map(|(index, item)| {
                let outline = if index == current {
                    rgb(0x4C9AFF)
                } else {
                    rgb(0x2A2A2A)
                };
                let path = item.display_path.clone();
                let cell_id = item.item_id as usize;
                div()
                    .id(("strip", cell_id))
                    .w(px(96.0))
                    .h(px(72.0))
                    .m(px(2.0))
                    .border_2()
                    .border_color(outline)
                    .rounded_sm()
                    .bg(rgb(0x1A1A1A))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_xs()
                    .text_color(rgb(0xAAAAAA))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        let pos = this
                            .visible()
                            .iter()
                            .position(|row| row.display_path == path);
                        if let Some(pos) = pos {
                            this.goto(pos, cx);
                        }
                    }))
                    .child(item.base_name.clone())
            })
            .collect();
        div()
            .id("film-strip")
            .w_full()
            .h(px(84.0))
            .flex()
            .flex_row()
            .overflow_x_scroll()
            .bg(rgb(0x141414))
            .children(cells)
    }

    /// Left metadata placeholder bound to the current item (M3 fills it).
    fn render_metadata(&self, _cx: &mut Context<Self>) -> impl IntoElement {
        let (title, dir, rating) = match self.current_item() {
            Some(row) => (row.base_name, row.dir, row.rating),
            None => ("—".to_string(), String::new(), 0),
        };
        v_flex()
            .w(px(220.0))
            .h_full()
            .px_2()
            .py_1()
            .gap_1()
            .bg(rgb(0x181818))
            .child(div().text_sm().child("Metadata (M3)".to_string()))
            .child(div().text_xs().child(format!("File: {title}")))
            .child(div().text_xs().child(format!("Dir: {dir}")))
            .child(div().text_xs().child(format!("Rating: {rating}/5")))
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(0x888888))
                    .child("Caption, keywords and the template editor land in M3.".to_string()),
            )
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
                        this.selection.select_all(this.visible().len());
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
                    .left(self.scan_status.clone())
                    .right(format!("{} shown", self.visible().len())),
            )
    }
}

/// Worker body for both browse job kinds. Progress events drive the contact
/// sheet; the terminal event is sent by the worker loop itself.
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
            let total = crate::browse::collect_entries(&root).len() as u64;
            let tx_each = tx.clone();
            let id = job.id;
            run_catalog_scan(&catalog, &root, &|done, _| {
                tx_each
                    .send(JobEvent {
                        id,
                        kind: JobEventKind::Progress {
                            done: done as u64,
                            total,
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
