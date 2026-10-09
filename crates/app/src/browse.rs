//! Browse helpers for M1: folder scan, RAW+JPEG pairing, filter/sort,
//! selection, zoom, and thumbnail state.
//!
//! Everything here is UI-agnostic so `cargo test` covers it without a
//! display. The GPUI views in [`crate::views`] render snapshots of these
//! types; the durable rows stay in `edit-catalog` (migration 001 is frozen,
//! so pairing lookups go through the existing stores plus the side-fill
//! `UPDATE`).

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Photo file kinds the browser understands: raws through the
/// embedded-preview path, plain stills decoded directly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PhotoKind {
    Raw,
    Jpeg,
}

/// Raw extensions served through the embedded-preview path
/// (`lightcraft-raw`; NEF/ARW/CR2/DNG plus the preview-grade rest).
pub const RAW_EXTS: &[&str] = &[
    "NEF", "NRW", "ARW", "SRF", "SR2", "CR2", "CR3", "RAF", "ORF", "RW2", "DNG", "PEF",
];
/// Plain stills decoded directly (JPEG photos and TIFFs).
pub const PLAIN_EXTS: &[&str] = &["JPG", "JPEG", "TIF", "TIFF"];

/// Classify a file by extension (case-insensitive). `None` means skipped.
pub fn classify_file(path: &Path) -> Option<PhotoKind> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_uppercase())?;
    if RAW_EXTS.contains(&ext.as_str()) {
        Some(PhotoKind::Raw)
    } else if PLAIN_EXTS.contains(&ext.as_str()) {
        Some(PhotoKind::Jpeg)
    } else {
        None
    }
}

/// One supported file found during a folder walk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEntry {
    pub path: PathBuf,
    pub dir: String,
    pub base_name: String,
    pub ext: String,
    pub kind: PhotoKind,
    pub bytes: u64,
    pub mtime: i64,
}

fn entry_for(path: PathBuf) -> Option<FileEntry> {
    let kind = classify_file(&path)?;
    let meta = std::fs::metadata(&path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_uppercase();
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    let dir = path
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Some(FileEntry {
        path,
        dir,
        base_name: stem,
        ext,
        kind,
        bytes: meta.len(),
        mtime,
    })
}

/// Recursively collect supported files under `root`, sorted by path so scans
/// are deterministic and progress events read in order. Symlinked
/// directories are never descended into (a link cycle would loop forever);
/// symlinked files are still collected under the link path.
pub fn collect_entries(root: &Path) -> Vec<FileEntry> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let read = match std::fs::read_dir(&dir) {
            Ok(read) => read,
            Err(_) => continue,
        };
        let mut children: Vec<PathBuf> = read.filter_map(|e| e.ok().map(|e| e.path())).collect();
        children.sort();
        for child in children.into_iter().rev() {
            let is_link = std::fs::symlink_metadata(&child)
                .map(|meta| meta.file_type().is_symlink())
                .unwrap_or(false);
            if is_link && child.is_dir() {
                continue;
            }
            if child.is_dir() {
                stack.push(child);
            } else if let Some(entry) = entry_for(child) {
                out.push(entry);
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Pair key for grouping files into contact-sheet cells: the directory
/// verbatim (case-sensitive on Linux) plus a case-folded stem, so
/// `DSC0001.NEF` and `dsc0001.JPG` land in one cell.
pub fn pair_key(dir: &str, base_name: &str) -> (String, String) {
    (dir.to_string(), base_name.to_lowercase())
}

/// One contact-sheet cell: a RAW+JPEG pair (or lone file) sharing
/// `(dir, base_name)`, matching the `items` table grain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairGroup {
    pub dir: String,
    pub base_name: String,
    pub raw: Option<FileEntry>,
    pub jpeg: Option<FileEntry>,
}

/// Group entries into pair groups. The first file per side wins the pair
/// slot; a second file on the same side (e.g. `DSC1.NEF` plus `DSC1.ARW`)
/// becomes its own lone cell keyed `{stem} ({ext})` instead of silently
/// dropping a photo. Groups sort by the folded pair key.
///
/// The `{stem} ({ext})` key can theoretically collide with a real file of
/// that stem (e.g. `DSC1 (ARW).NEF`); both would then share one cell. No
/// photo is lost either way.
pub fn pair_entries(entries: Vec<FileEntry>) -> Vec<PairGroup> {
    let mut map: BTreeMap<(String, String), PairGroup> = BTreeMap::new();
    for entry in entries {
        let key = pair_key(&entry.dir, &entry.base_name);
        let group = map.entry(key).or_insert_with(|| PairGroup {
            dir: entry.dir.clone(),
            base_name: entry.base_name.clone(),
            raw: None,
            jpeg: None,
        });
        let slot = match entry.kind {
            PhotoKind::Raw => &mut group.raw,
            PhotoKind::Jpeg => &mut group.jpeg,
        };
        if slot.is_none() {
            *slot = Some(entry);
        } else {
            // Same-side collision: first free `{stem} ({ext}[ n])` key wins
            // its own lone cell, so no photo is ever silently dropped.
            let mut n = 0;
            loop {
                let lone_base = if n == 0 {
                    format!("{} ({})", entry.base_name, entry.ext.to_ascii_uppercase())
                } else {
                    format!(
                        "{} ({} {n})",
                        entry.base_name,
                        entry.ext.to_ascii_uppercase()
                    )
                };
                n += 1;
                let lone_key = pair_key(&entry.dir, &lone_base);
                let lone = map.entry(lone_key).or_insert_with(|| PairGroup {
                    dir: entry.dir.clone(),
                    base_name: lone_base.clone(),
                    raw: None,
                    jpeg: None,
                });
                let lone_slot = match entry.kind {
                    PhotoKind::Raw => &mut lone.raw,
                    PhotoKind::Jpeg => &mut lone.jpeg,
                };
                if lone_slot.is_none() {
                    *lone_slot = Some(entry);
                    break;
                }
            }
        }
    }
    map.into_values().collect()
}

// ---- browse item snapshot (what the views render) ----

/// Per-photo upload badge states (plan section 2.2). C3 writes metadata
/// jobs, not transfers; the scan path leaves both at `None`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum UploadBadge {
    #[default]
    None,
    Queued,
    Uploading,
    Done,
    Failed,
}

/// Per-item IPTC template apply state (plan sections 2.1, 2.7): the
/// status strip's "template changes" marker, rendered on cells like the
/// upload badge. `metadata_write` jobs (plan section 1.2) drive it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum MetaBadge {
    #[default]
    None,
    Queued,
    Writing,
    Done,
    Failed,
}

/// Per-photo thumbnail job state shown on cells and the film strip.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThumbState {
    #[default]
    Queued,
    Running,
    Ready,
    Failed,
}

/// One row the contact sheet / single view navigates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowseItem {
    pub item_id: i64,
    pub dir: String,
    pub base_name: String,
    pub display_path: String,
    pub has_raw: bool,
    pub has_jpeg: bool,
    pub rating: i64,
    pub thumb: ThumbState,
    pub upload: UploadBadge,
    pub meta: MetaBadge,
    pub ingest_fresh: bool,
}

impl BrowseItem {
    /// Short label for cells and the film strip.
    pub fn label(&self) -> String {
        if self.has_raw && self.has_jpeg {
            format!("{} (R+J)", self.base_name)
        } else if self.has_raw {
            format!("{} (RAW)", self.base_name)
        } else {
            format!("{} (JPG)", self.base_name)
        }
    }
}

// ---- filter / sort header ----

/// Header filter controls (plan section 2.2).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BrowseFilter {
    /// Keep items rated at least this (0 disables).
    pub min_rating: i64,
    /// Keep only this file kind when set.
    pub kind: Option<PhotoKind>,
    /// Case-insensitive substring on `base_name` when non-empty.
    pub query: String,
}

/// Sort orders from the header (plan section 2.2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SortOrder {
    /// Catalog insert order (ingest order).
    #[default]
    IngestOrder,
    FileName,
}

/// Filename order over string refs: one comparator shared by every sort
/// site, so the grid and the filter helper can never disagree.
pub fn cmp_item_key(a: &BrowseItem, b: &BrowseItem) -> Ordering {
    (&a.dir, &a.base_name).cmp(&(&b.dir, &b.base_name))
}

impl SortOrder {
    pub fn apply(&self, items: &mut [BrowseItem]) {
        match self {
            Self::IngestOrder => items.sort_by_key(|item| item.item_id),
            Self::FileName => items.sort_by(cmp_item_key),
        }
    }
}

/// Filter then sort, returning borrowed rows in display order.
pub fn apply_filter_sort<'a>(
    items: &'a [BrowseItem],
    filter: &BrowseFilter,
    sort: SortOrder,
) -> Vec<&'a BrowseItem> {
    let query = filter.query.to_ascii_lowercase();
    let mut picked: Vec<&BrowseItem> = items
        .iter()
        .filter(|item| {
            if item.rating < filter.min_rating {
                return false;
            }
            match filter.kind {
                Some(PhotoKind::Raw) if !item.has_raw => return false,
                Some(PhotoKind::Jpeg) if !item.has_jpeg => return false,
                _ => {}
            }
            if !query.is_empty() && !item.base_name.to_ascii_lowercase().contains(query.as_str()) {
                return false;
            }
            true
        })
        .collect();
    match sort {
        SortOrder::IngestOrder => picked.sort_by_key(|item| item.item_id),
        SortOrder::FileName => picked.sort_by(|a, b| cmp_item_key(a, b)),
    }
    picked
}

// ---- selection model ----

/// Grid/single-view selection over display indices.
///
/// `cursor` is the focused cell (arrow keys move it); `selected` is the
/// multi-selection set. Plain arrows move both, Shift+arrows extend the range
/// from `anchor`, Ctrl+A selects all, mouse click replaces.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SelectionModel {
    pub cursor: usize,
    pub anchor: usize,
    pub selected: BTreeSet<usize>,
}

impl SelectionModel {
    pub fn new(len: usize) -> Self {
        let mut selected = BTreeSet::new();
        if len > 0 {
            selected.insert(0);
        }
        Self {
            cursor: 0,
            anchor: 0,
            selected,
        }
    }

    pub fn clamp(&mut self, len: usize) {
        if len == 0 {
            self.cursor = 0;
            self.anchor = 0;
            self.selected.clear();
            return;
        }
        let max = len - 1;
        self.cursor = self.cursor.min(max);
        self.anchor = self.anchor.min(max);
        self.selected.retain(|i| *i < len);
        if self.selected.is_empty() {
            self.selected.insert(self.cursor);
        }
    }

    pub fn select_single(&mut self, index: usize, len: usize) {
        if len == 0 {
            return;
        }
        let index = index.min(len - 1);
        self.cursor = index;
        self.anchor = index;
        self.selected.clear();
        self.selected.insert(index);
    }

    /// Move the cursor by `delta` (signed). When `extend` is set (Shift held),
    /// the range `anchor..=cursor` becomes the selection; otherwise the
    /// cursor alone is selected.
    pub fn move_cursor(&mut self, delta: isize, extend: bool, len: usize) {
        if len == 0 {
            return;
        }
        let next = (self.cursor as isize + delta).clamp(0, len as isize - 1) as usize;
        self.cursor = next;
        if extend {
            let (lo, hi) = if self.anchor <= next {
                (self.anchor, next)
            } else {
                (next, self.anchor)
            };
            self.selected = (lo..=hi).collect();
        } else {
            self.anchor = next;
            self.selected.clear();
            self.selected.insert(next);
        }
    }

    pub fn select_all(&mut self, len: usize) {
        self.selected = (0..len).collect();
        // Keep the cursor where it was (or 0 when empty) and re-anchor
        // there, so a later Shift+arrow extends from the visible focus
        // instead of a stale position.
        if len == 0 {
            self.cursor = 0;
        } else {
            self.cursor = self.cursor.min(len - 1);
        }
        self.anchor = self.cursor;
    }

    pub fn contains(&self, index: usize) -> bool {
        self.selected.contains(&index)
    }
}

// ---- zoom / pan state (single view) ----

/// Single-view zoom states: fit, 100%, or a free scale. Scroll zooms,
/// drag pans (pan clamps to ±1.5 frame units), `Z` resets to fit.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ZoomMode {
    Fit,
    Hundred,
    Scale(f32),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ZoomState {
    pub mode: ZoomMode,
    pub pan_x: f32,
    pub pan_y: f32,
}

impl Default for ZoomState {
    fn default() -> Self {
        Self {
            mode: ZoomMode::Fit,
            pan_x: 0.0,
            pan_y: 0.0,
        }
    }
}

impl ZoomState {
    /// Current display scale for a frame of `frame_width` in a viewport of
    /// `viewport_width` (heights scale proportionally).
    pub fn scale_for(&self, frame_width: f32, viewport_width: f32) -> f32 {
        if frame_width <= 0.0 || viewport_width <= 0.0 {
            return 1.0;
        }
        match self.mode {
            ZoomMode::Fit => viewport_width / frame_width,
            ZoomMode::Hundred => 1.0,
            ZoomMode::Scale(s) => s.max(0.05),
        }
    }

    /// Paint scale for a frame in a viewport, using both dimensions so the
    /// Fit state honors the constraining axis (portrait frames fit by
    /// height, not by width). Free/Hundred scales are absolute and
    /// viewport-independent — the paint closure must apply them (review B1).
    pub fn scale_in(&self, frame_w: f32, frame_h: f32, vw: f32, vh: f32) -> f32 {
        if frame_w <= 0.0 || frame_h <= 0.0 || vw <= 0.0 || vh <= 0.0 {
            return 1.0;
        }
        match self.mode {
            ZoomMode::Fit => (vw / frame_w).min(vh / frame_h),
            ZoomMode::Hundred => 1.0,
            ZoomMode::Scale(s) => s.max(0.05),
        }
    }

    /// Zoom one scroll notch by `factor`. A Fit view enters a free scale
    /// from `fit_scale` (the scale the canvas is currently painting at) so
    /// scroll-zoom works from every mode instead of no-oping on Fit.
    pub fn scroll_zoom(&mut self, factor: f32, fit_scale: f32) {
        let current = match self.mode {
            ZoomMode::Fit => fit_scale.max(0.05),
            ZoomMode::Hundred => 1.0,
            ZoomMode::Scale(s) => s,
        };
        self.mode = ZoomMode::Scale((current * factor).clamp(0.05, 32.0));
    }

    pub fn enter_free(&mut self, base_fit_scale: f32) {
        if matches!(self.mode, ZoomMode::Fit) {
            self.mode = ZoomMode::Scale(base_fit_scale.max(0.05));
        }
    }

    pub fn drag_pan(&mut self, dx: f32, dy: f32) {
        self.pan_x = (self.pan_x + dx).clamp(-1.5, 1.5);
        self.pan_y = (self.pan_y + dy).clamp(-1.5, 1.5);
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn show_hundred(&mut self) {
        self.mode = ZoomMode::Hundred;
        self.pan_x = 0.0;
        self.pan_y = 0.0;
    }
}

// ---- durable job payloads (plan section 1.2) ----

/// Payload for `catalog_scan` jobs: which folder to scan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogScanPayload {
    pub root: String,
}

/// Payload for `thumb` jobs: one photo to decode a thumbnail for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThumbPayload {
    pub photo_id: i64,
    pub path: String,
}

/// Summary of a finished folder scan.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScanSummary {
    pub root_id: i64,
    /// Files on disk that belong in the catalog (fresh + already known).
    pub files_seen: usize,
    pub photos_added: usize,
    pub items_touched: usize,
    pub thumb_jobs: usize,
    /// Files skipped because their path is already cataloged (rescan,
    /// duplicate root spellings, or a cross-root collision).
    pub skipped_existing: usize,
}

/// Canonicalize a folder for catalog identity: `photos/` and `photos`
/// resolve to one root row instead of two competing roots.
pub fn canonical_root(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Scan `root_path` into the catalog: one `folder` root, photo rows, pair
/// items, and one `thumb` job per new photo.
///
/// Idempotent: files already cataloged under this root are skipped, so a
/// second scan (or a restarted `catalog_scan` job) only adds newcomers.
/// `on_file` fires first with `(0, fresh_total)` so the worker can report a
/// correct total without a redundant pre-walk, then per new file so the job
/// handler can emit `JobEventKind::Progress` and live-update the UI.
pub fn run_catalog_scan(
    catalog: &edit_catalog::Catalog,
    root_path: &Path,
    on_file: &dyn Fn(usize, usize),
) -> Result<ScanSummary, edit_catalog::Error> {
    let canon = canonical_root(root_path);
    let root_str = canon.to_string_lossy().into_owned();
    let root_id = match catalog.get_root_by_path(&root_str)? {
        Some(root) => root.id,
        None => catalog.insert_root(&root_str, "folder")?,
    };
    let known: BTreeSet<String> = catalog
        .list_photos_by_root(root_id)?
        .into_iter()
        .map(|photo| photo.path)
        .collect();

    let fresh: Vec<FileEntry> = collect_entries(&canon)
        .into_iter()
        .filter(|entry| !known.contains(&entry.path.to_string_lossy().into_owned()))
        .collect();
    let total = fresh.len();
    on_file(0, total);
    let groups = pair_entries(fresh);

    let mut summary = ScanSummary {
        root_id,
        files_seen: total,
        ..ScanSummary::default()
    };
    let mut done = 0usize;
    for group in groups.iter() {
        // Insert the fresh sides, tolerating paths that are already
        // cataloged (unique violation) instead of aborting the scan.
        let mut inserted: Vec<(&FileEntry, i64)> = Vec::new();
        for entry in [group.raw.as_ref(), group.jpeg.as_ref()]
            .into_iter()
            .flatten()
        {
            let kind_str = match entry.kind {
                PhotoKind::Raw => "raw",
                PhotoKind::Jpeg => "jpeg",
            };
            match catalog.insert_photo(&edit_catalog::models::NewPhoto {
                root_id,
                path: entry.path.to_string_lossy().into_owned(),
                dir: entry.dir.clone(),
                base_name: entry.base_name.clone(),
                ext: entry.ext.clone(),
                kind: kind_str.to_string(),
                bytes: entry.bytes as i64,
                mtime: entry.mtime,
                // A scan has no separate card name; the on-disk name is it.
                original_name: Some(entry.base_name.clone()),
                taken_at: None,
            }) {
                Ok(photo_id) => {
                    inserted.push((entry, photo_id));
                    summary.photos_added += 1;
                    let payload = serde_json::to_string(&ThumbPayload {
                        photo_id,
                        path: entry.path.to_string_lossy().into_owned(),
                    })
                    .unwrap_or_else(|_| "{}".to_string());
                    catalog.enqueue_job(edit_catalog::JobKind::Thumb, &payload)?;
                    summary.thumb_jobs += 1;
                    done += 1;
                    on_file(done, total);
                }
                Err(e) if e.is_unique_violation() => {
                    summary.skipped_existing += 1;
                }
                Err(e) => return Err(e),
            }
        }
        if inserted.is_empty() {
            continue;
        }
        // Pair grain: item rows are keyed by the folded pair key, so
        // `DSC0001.NEF` and `dsc0001.JPG` share one cell even across
        // rescans. A rescan filling a half-pair lands in `set_item_side`;
        // otherwise the new pair cell is inserted.
        let folded_base = pair_key(&group.dir, &group.base_name).1;
        match catalog.get_item_by_dir_base(&group.dir, &folded_base)? {
            None => {
                let (raw_id, jpeg_id) = inserted_ids(&inserted);
                catalog.insert_item(&group.dir, &folded_base, raw_id, jpeg_id)?;
            }
            Some(item) => {
                for (entry, photo_id) in &inserted {
                    let (occupied, raw_flag) = match entry.kind {
                        PhotoKind::Raw => (item.raw_photo_id, true),
                        PhotoKind::Jpeg => (item.jpeg_photo_id, false),
                    };
                    match occupied {
                        None => catalog.set_item_side(item.id, *photo_id, raw_flag)?,
                        Some(_) => {
                            // Side already held (same folded stem, distinct
                            // file, e.g. a case-variant duplicate added
                            // later): the newcomer rides the existing cell,
                            // so there is nothing to update.
                        }
                    }
                }
            }
        }
        summary.items_touched += 1;
    }
    Ok(summary)
}

/// Split freshly inserted photo ids back into item sides.
fn inserted_ids(inserted: &[(&FileEntry, i64)]) -> (Option<i64>, Option<i64>) {
    let mut raw_id = None;
    let mut jpeg_id = None;
    for (entry, id) in inserted {
        match entry.kind {
            PhotoKind::Raw => raw_id = Some(*id),
            PhotoKind::Jpeg => jpeg_id = Some(*id),
        }
    }
    (raw_id, jpeg_id)
}

/// Rebuild browse snapshots for a root: photos regrouped into pair cells
/// on the folded pair key, with item ids resolved through the `items` table
/// (keyed folded since the case-folding fix). Display names stay verbatim
/// from the photo rows (JPEG side preferred).
pub fn list_browse_items(
    catalog: &edit_catalog::Catalog,
    root_id: i64,
) -> Result<Vec<BrowseItem>, edit_catalog::Error> {
    let photos = catalog.list_photos_by_root(root_id)?;
    let mut groups: BTreeMap<(String, String), Vec<&edit_catalog::models::Photo>> = BTreeMap::new();
    for photo in &photos {
        groups
            .entry(pair_key(&photo.dir, &photo.base_name))
            .or_default()
            .push(photo);
    }
    let mut items = Vec::with_capacity(groups.len());
    for ((dir, folded), members) in groups {
        let item_id = catalog
            .get_item_by_dir_base(&dir, &folded)?
            .map(|item| item.id)
            .unwrap_or(-1);
        let has_raw = members.iter().any(|p| p.kind == "raw");
        let has_jpeg = members.iter().any(|p| p.kind == "jpeg");
        let display = members
            .iter()
            .find(|p| p.kind == "jpeg")
            .or_else(|| members.first());
        let display_path = display.map(|p| p.path.clone()).unwrap_or_default();
        let base_name = display
            .map(|p| p.base_name.clone())
            .unwrap_or_else(|| folded.clone());
        let member_dir = display.map(|p| p.dir.clone()).unwrap_or(dir);
        let rating = members.iter().map(|p| p.rating).max().unwrap_or(0);
        items.push(BrowseItem {
            item_id,
            dir: member_dir,
            base_name,
            display_path,
            has_raw,
            has_jpeg,
            rating,
            thumb: ThumbState::Queued,
            upload: UploadBadge::None,
            meta: MetaBadge::None,
            ingest_fresh: false,
        });
    }
    items.sort_by_key(|item| item.item_id);
    Ok(items)
}

/// Filesystem paths of every photo side of one item (plan section 1.3:
/// "A paired RAW+JPEG item stamps both files"). Falls back to the display
/// path for an item row the catalog has not paired yet (item_id -1).
pub fn item_photo_paths(catalog: &edit_catalog::Catalog, item: &BrowseItem) -> Vec<String> {
    if item.item_id >= 0
        && let Ok(item_row) = catalog.get_item(item.item_id)
    {
        let sides = [item_row.raw_photo_id, item_row.jpeg_photo_id];
        let paths: Vec<String> = sides
            .into_iter()
            .flatten()
            .filter_map(|photo_id| catalog.get_photo(photo_id).ok().map(|photo| photo.path))
            .collect();
        if !paths.is_empty() {
            return paths;
        }
    }
    vec![item.display_path.clone()]
}

// ---- template apply plan (pure, testable) ----

/// One `metadata_write` job the apply step enqueues. Built from the
/// selection without any UI state (plan section 2.7), so the sequence
/// numbers and per-item paths are unit-testable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TemplateApplyPlan {
    pub item_id: i64,
    /// Every photo side of the pair (raw + jpeg when paired).
    pub paths: Vec<String>,
    /// 1-based, over the enqueueable items in display order.
    pub sequence: u64,
}

/// Plan a template apply over a selection. An item without a catalog row
/// (the transient `item_id < 0` cells) is skipped: it has no durable
/// identity for badges or resumed queue state, so stamping it would
/// collapse the per-item state of every not-yet-matched display row.
pub fn plan_template_apply(
    selection: &[BrowseItem],
    catalog: &edit_catalog::Catalog,
) -> Vec<TemplateApplyPlan> {
    selection
        .iter()
        .filter(|item| item.item_id >= 0)
        .zip(1u64..)
        .map(|(item, sequence)| TemplateApplyPlan {
            item_id: item.item_id,
            paths: item_photo_paths(catalog, item),
            sequence,
        })
        .collect()
}

/// `metadata_write` payload JSON for each planned step. `captured_at` is
/// stamped once for the whole batch, so the `{date}` fallback is one value
/// that a crash-resumed job reproduces from its durable payload.
pub fn template_apply_payloads(
    plan: &[TemplateApplyPlan],
    fields_json: &str,
    captured_at: i64,
) -> Vec<String> {
    template_apply_payloads_with(plan, fields_json, captured_at, None)
}

/// [`template_apply_payloads`] with the code replacement set baked into
/// each payload, so the worker expands codes without the source file.
pub fn template_apply_payloads_with(
    plan: &[TemplateApplyPlan],
    fields_json: &str,
    captured_at: i64,
    replacements: Option<edit_metadata::CodeReplacements>,
) -> Vec<String> {
    template_apply_payloads_opts(plan, fields_json, captured_at, replacements, false)
}

/// [`template_apply_payloads_with`]; `clear_empty` makes a field that
/// expands empty clear its tags (form writes) instead of being skipped.
pub fn template_apply_payloads_opts(
    plan: &[TemplateApplyPlan],
    fields_json: &str,
    captured_at: i64,
    replacements: Option<edit_metadata::CodeReplacements>,
    clear_empty: bool,
) -> Vec<String> {
    plan.iter()
        .map(|step| {
            serde_json::to_string(&edit_metadata::MetadataWritePayload {
                item_id: step.item_id,
                paths: step.paths.clone(),
                sequence: step.sequence,
                captured_at,
                session: None,
                fields_json: fields_json.to_owned(),
                replacements: replacements.clone(),
                clear_empty,
            })
            .unwrap_or_else(|_| "{}".to_string())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    pub(crate) fn scratch_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("edit-browse-test-{}-{}", std::process::id(), name));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    pub(crate) fn touch(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, b"fake-image-bytes").unwrap();
        path
    }

    pub(crate) fn tiny_jpeg(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        let img = image::RgbImage::from_pixel(4, 4, image::Rgb([200, 40, 40]));
        let mut bytes = Vec::new();
        {
            let mut cursor = std::io::Cursor::new(&mut bytes);
            img.write_to(&mut cursor, image::ImageFormat::Jpeg).unwrap();
        }
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn classify_handles_photo_extensions() {
        assert_eq!(
            classify_file(Path::new("/a/DSC0001.NEF")),
            Some(PhotoKind::Raw)
        );
        assert_eq!(
            classify_file(Path::new("/a/dsc0001.arw")),
            Some(PhotoKind::Raw)
        );
        assert_eq!(
            classify_file(Path::new("/a/IMG_1.cr2")),
            Some(PhotoKind::Raw)
        );
        assert_eq!(
            classify_file(Path::new("/a/photo.jpg")),
            Some(PhotoKind::Jpeg)
        );
        // TIFFs decode directly on the plain side, never via raw.
        assert_eq!(
            classify_file(Path::new("/a/scan.tif")),
            Some(PhotoKind::Jpeg)
        );
        assert_eq!(
            classify_file(Path::new("/a/scan.TIFF")),
            Some(PhotoKind::Jpeg)
        );
        assert_eq!(classify_file(Path::new("/a/photo.png")), None);
        assert_eq!(classify_file(Path::new("/a/notes.txt")), None);
    }

    #[test]
    fn pairing_groups_raw_and_jpeg() {
        let dir = scratch_dir("pair");
        let raw = touch(&dir, "DSC0001.NEF");
        let jpeg = touch(&dir, "DSC0001.JPG");
        let lone = touch(&dir, "DSC0002.ARW");
        let ignored = dir.join("notes.txt");
        std::fs::write(&ignored, b"nope").unwrap();

        let entries = collect_entries(&dir);
        assert_eq!(entries.len(), 3);
        let groups = pair_entries(entries);
        assert_eq!(groups.len(), 2);
        let paired = groups.iter().find(|g| g.base_name == "DSC0001").unwrap();
        assert_eq!(
            paired.raw.as_ref().unwrap().path,
            raw.canonicalize().unwrap_or(raw)
        );
        assert_eq!(
            paired.jpeg.as_ref().unwrap().path,
            jpeg.canonicalize().unwrap_or(jpeg)
        );
        assert!(
            groups
                .iter()
                .find(|g| g.base_name == "DSC0002")
                .unwrap()
                .jpeg
                .is_none()
        );
        let _ = lone;
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn pairing_folds_case() {
        // DSC0001.NEF + dsc0001.JPG share one cell despite the case gap.
        let dir = scratch_dir("pair-case");
        touch(&dir, "DSC0001.NEF");
        touch(&dir, "dsc0001.JPG");
        let groups = pair_entries(collect_entries(&dir));
        assert_eq!(groups.len(), 1);
        let group = &groups[0];
        assert!(group.raw.is_some() && group.jpeg.is_some());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn same_side_collision_becomes_a_lone_cell() {
        // DSC1.NEF + DSC1.ARW: no photo is dropped; the loser keeps its own
        // lone cell.
        let dir = scratch_dir("pair-collision");
        touch(&dir, "DSC1.NEF");
        touch(&dir, "DSC1.ARW");
        let groups = pair_entries(collect_entries(&dir));
        assert_eq!(groups.len(), 2);
        let raws: usize = groups.iter().map(|g| usize::from(g.raw.is_some())).sum();
        assert_eq!(raws, 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_dirs_are_not_descended_into() {
        use std::os::unix::fs::symlink;
        let dir = scratch_dir("pair-symlink");
        let real = dir.join("real");
        std::fs::create_dir_all(&real).unwrap();
        touch(&real, "A.JPG");
        symlink(&real, dir.join("link")).unwrap();
        symlink(&dir, real.join("cycle")).unwrap();
        // One photo, seen once: no double counts, no infinite loop.
        let entries = collect_entries(&dir);
        assert_eq!(entries.len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn scan_inserts_pairs_and_thumb_jobs_then_rescans_clean() {
        let dir = scratch_dir("scan");
        touch(&dir, "A.NEF");
        touch(&dir, "A.JPG");
        tiny_jpeg(&dir, "B.JPG");

        let db_path = dir.join("catalog.sqlite");
        let catalog = edit_catalog::Catalog::open(&db_path).unwrap();
        let progress = std::cell::RefCell::new(Vec::new());
        let summary = run_catalog_scan(&catalog, &dir, &|done, total| {
            progress.borrow_mut().push((done, total));
        })
        .unwrap();
        assert_eq!(summary.photos_added, 3);
        assert_eq!(summary.items_touched, 2);
        assert_eq!(summary.thumb_jobs, 3);
        assert_eq!(summary.files_seen, 3);
        assert_eq!(summary.skipped_existing, 0);
        // The total is announced upfront, so workers never need a pre-walk.
        assert_eq!(progress.borrow().first(), Some(&(0, 3)));
        assert_eq!(progress.borrow().last(), Some(&(3, 3)));

        let items = list_browse_items(&catalog, summary.root_id).unwrap();
        assert_eq!(items.len(), 2);
        let paired = items.iter().find(|i| i.base_name == "A").unwrap();
        assert!(paired.has_raw && paired.has_jpeg);
        assert_eq!(paired.label(), "A (R+J)");

        // Second scan is a no-op for the same files.
        let again = run_catalog_scan(&catalog, &dir, &|_, _| {}).unwrap();
        assert_eq!(again.photos_added, 0);
        assert_eq!(again.thumb_jobs, 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rescan_completes_a_half_pair_in_place() {
        // RAW first, JPEG later: one item row gains its second side instead
        // of a duplicate cell.
        let dir = scratch_dir("scan-halfpair");
        touch(&dir, "H.NEF");
        let db_path = dir.join("catalog.sqlite");
        let catalog = edit_catalog::Catalog::open(&db_path).unwrap();
        let first = run_catalog_scan(&catalog, &dir, &|_, _| {}).unwrap();
        assert_eq!(list_browse_items(&catalog, first.root_id).unwrap().len(), 1);

        touch(&dir, "h.JPG");
        let second = run_catalog_scan(&catalog, &dir, &|_, _| {}).unwrap();
        assert_eq!(second.root_id, first.root_id);
        assert_eq!(second.photos_added, 1);
        let items = list_browse_items(&catalog, second.root_id).unwrap();
        assert_eq!(items.len(), 1);
        assert!(items[0].has_raw && items[0].has_jpeg);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn root_spellings_share_one_root_row() {
        // `dir` and `dir/` canonicalize to the same root: the second scan
        // adds nothing instead of duplicating the folder.
        let dir = scratch_dir("scan-canon");
        tiny_jpeg(&dir, "C.JPG");
        let db_path = dir.join("catalog.sqlite");
        let catalog = edit_catalog::Catalog::open(&db_path).unwrap();
        let first = run_catalog_scan(&catalog, &dir, &|_, _| {}).unwrap();
        let slashed = PathBuf::from(format!("{}/", dir.to_string_lossy()));
        let second = run_catalog_scan(&catalog, &slashed, &|_, _| {}).unwrap();
        assert_eq!(second.root_id, first.root_id);
        assert_eq!(second.photos_added, 0);
        assert_eq!(catalog.list_roots().unwrap().len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn filter_sort_selection_cover_header_and_keys() {
        let items = vec![
            BrowseItem {
                item_id: 3,
                dir: "/s".into(),
                base_name: "C".into(),
                display_path: "/s/C.JPG".into(),
                has_raw: true,
                has_jpeg: false,
                rating: 5,
                thumb: ThumbState::Ready,
                meta: MetaBadge::None,
                upload: UploadBadge::None,
                ingest_fresh: false,
            },
            BrowseItem {
                item_id: 1,
                dir: "/s".into(),
                base_name: "A".into(),
                display_path: "/s/A.JPG".into(),
                has_raw: false,
                has_jpeg: true,
                rating: 1,
                thumb: ThumbState::Queued,
                meta: MetaBadge::None,
                upload: UploadBadge::None,
                ingest_fresh: true,
            },
            BrowseItem {
                item_id: 2,
                dir: "/s".into(),
                base_name: "B".into(),
                display_path: "/s/B.JPG".into(),
                has_raw: true,
                has_jpeg: true,
                rating: 3,
                thumb: ThumbState::Running,
                meta: MetaBadge::None,
                upload: UploadBadge::Queued,
                ingest_fresh: false,
            },
        ];
        let rated = apply_filter_sort(
            &items,
            &BrowseFilter {
                min_rating: 3,
                ..Default::default()
            },
            SortOrder::IngestOrder,
        );
        assert_eq!(rated.len(), 2);

        let raws = apply_filter_sort(
            &items,
            &BrowseFilter {
                kind: Some(PhotoKind::Raw),
                ..Default::default()
            },
            SortOrder::FileName,
        );
        assert_eq!(raws.len(), 2);
        assert_eq!(raws[0].base_name, "B");

        let query = apply_filter_sort(
            &items,
            &BrowseFilter {
                query: "c".into(),
                ..Default::default()
            },
            SortOrder::IngestOrder,
        );
        assert_eq!(query.len(), 1);

        let mut sel = SelectionModel::new(3);
        assert!(sel.contains(0));
        sel.move_cursor(1, false, 3);
        assert_eq!((sel.cursor, sel.selected.len()), (1, 1));
        sel.move_cursor(1, true, 3);
        assert_eq!(sel.selected.len(), 2);
        // Shift+arrows extend from the anchor instead of collapsing.
        let mut anchored = SelectionModel::new(5);
        anchored.move_cursor(2, false, 5);
        anchored.move_cursor(-1, true, 5);
        assert_eq!(anchored.selected, BTreeSet::from([1, 2]));
        assert_eq!((anchored.cursor, anchored.anchor), (1, 2));
        sel.select_all(3);
        assert_eq!(sel.selected.len(), 3);
        // Select-all re-anchors at the cursor, so the next Shift+arrow
        // extends from the visible focus.
        assert_eq!(sel.anchor, sel.cursor);
        sel.move_cursor(-1, true, 3);
        assert!(sel.selected.contains(&sel.cursor) && sel.selected.contains(&sel.anchor));
        sel.select_single(2, 3);
        assert!(sel.contains(2) && !sel.contains(0));
        sel.clamp(1);
        assert_eq!(sel.cursor, 0);
    }

    #[test]
    fn zoom_defaults_fit_and_z_resets() {
        let mut zoom = ZoomState::default();
        assert_eq!(zoom.scale_for(100.0, 200.0), 2.0);
        zoom.show_hundred();
        assert_eq!(zoom.scale_for(100.0, 200.0), 1.0);
        // Scroll-zoom from Fit enters a free scale from the painted fit
        // scale instead of ignoring the gesture.
        zoom.reset();
        zoom.scroll_zoom(2.0, 0.5);
        assert_eq!(zoom.mode, ZoomMode::Scale(1.0));
        zoom.scroll_zoom(2.0, 0.5);
        assert_eq!(zoom.mode, ZoomMode::Scale(2.0));
        assert!(zoom.scale_for(50.0, 100.0) > 1.0);
        zoom.drag_pan(0.2, -0.3);
        zoom.reset();
        assert_eq!(zoom, ZoomState::default());
    }

    #[test]
    fn payloads_round_trip() {
        let scan = CatalogScanPayload {
            root: "/photos".into(),
        };
        let json = serde_json::to_string(&scan).unwrap();
        assert_eq!(
            serde_json::from_str::<CatalogScanPayload>(&json).unwrap(),
            scan
        );
        let thumb = ThumbPayload {
            photo_id: 7,
            path: "/photos/a.jpg".into(),
        };
        let json = serde_json::to_string(&thumb).unwrap();
        assert_eq!(serde_json::from_str::<ThumbPayload>(&json).unwrap(), thumb);
    }

    #[test]
    fn walkthrough_scratch_lists_items() {
        // Mirrors the scratch walkthrough: open a folder, list items.
        let dir = scratch_dir("walkthrough");
        tiny_jpeg(&dir, "WALK_A.JPG");
        touch(&dir, "WALK_B.NEF");
        let db_path = dir.join("walk.sqlite");
        let catalog = edit_catalog::Catalog::open(&db_path).unwrap();
        let summary = run_catalog_scan(&catalog, &dir, &|_, _| {}).unwrap();
        let items: Vec<String> = list_browse_items(&catalog, summary.root_id)
            .unwrap()
            .into_iter()
            .map(|item| item.label())
            .collect();
        let mut w = Vec::new();
        writeln!(w, "items: {items:?}").unwrap();
        let text = String::from_utf8(w).unwrap();
        assert!(text.contains("WALK_A"));
        assert!(text.contains("WALK_B"));
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod meta_paths_tests {
    use super::tests::{scratch_dir, tiny_jpeg, touch};
    use super::*;

    #[test]
    fn item_photo_paths_covers_both_sides_of_a_pair() {
        let dir = scratch_dir("item-paths");
        touch(&dir, "P.NEF");
        touch(&dir, "p.JPG");
        let db_path = dir.join("catalog.sqlite");
        let catalog = edit_catalog::Catalog::open(&db_path).unwrap();
        let summary = run_catalog_scan(&catalog, &dir, &|_, _| {}).unwrap();
        let items = list_browse_items(&catalog, summary.root_id).unwrap();
        assert_eq!(items.len(), 1);
        let paths = item_photo_paths(&catalog, &items[0]);
        assert_eq!(paths.len(), 2, "pair is stamped on both files: {paths:?}");
        // And an unpaired display row still has at least one path.
        let lone = BrowseItem {
            item_id: -1,
            dir: dir.to_string_lossy().into(),
            base_name: "Q".into(),
            display_path: "/missing/Q.JPG".into(),
            has_raw: true,
            has_jpeg: true,
            rating: 0,
            thumb: ThumbState::Queued,
            upload: UploadBadge::None,
            meta: MetaBadge::None,
            ingest_fresh: false,
        };
        assert_eq!(item_photo_paths(&catalog, &lone), vec!["/missing/Q.JPG"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn plan_skips_unmatched_and_numbers_the_rest() {
        let dir = scratch_dir("apply-plan");
        touch(&dir, "A.NEF");
        touch(&dir, "a.JPG");
        tiny_jpeg(&dir, "B.JPG");
        let db_path = dir.join("plan.sqlite");
        let catalog = edit_catalog::Catalog::open(&db_path).unwrap();
        let summary = run_catalog_scan(&catalog, &dir, &|_, _| {}).unwrap();
        let items = list_browse_items(&catalog, summary.root_id).unwrap();
        assert_eq!(items.len(), 2);
        // A display row the catalog never matched (item_id -1) is planned
        // out, and the sequence numbers stay 1..N over the rest.
        let mut with_ghost = items.clone();
        with_ghost.push(BrowseItem {
            item_id: -1,
            dir: "/ghost".into(),
            base_name: "GHOST".into(),
            display_path: "/ghost/G.JPG".into(),
            has_raw: true,
            has_jpeg: false,
            rating: 0,
            thumb: ThumbState::Queued,
            upload: UploadBadge::None,
            meta: MetaBadge::None,
            ingest_fresh: false,
        });

        let planned = plan_template_apply(&items, &catalog);
        assert_eq!(planned.len(), 2, "every cataloged row plans");
        for (offset, plan) in planned.iter().enumerate() {
            assert_eq!(plan.sequence, (offset + 1) as u64);
            assert!(plan.item_id >= 0);
            let source = items
                .iter()
                .find(|item| item.item_id == plan.item_id)
                .unwrap();
            assert_eq!(plan.paths, item_photo_paths(&catalog, source));
            assert!(!plan.paths.is_empty());
        }

        let with_ghost = plan_template_apply(&with_ghost, &catalog);
        assert_eq!(with_ghost.len(), 2, "ghost row skipped");

        // One enqueue-time clock for the whole batch rides every payload.
        let payloads = template_apply_payloads(&planned, r#"{"credit":"x"}"#, 1_791_374_400);
        assert_eq!(payloads.len(), planned.len());
        for (payload, plan) in payloads.iter().zip(&planned) {
            let parsed: edit_metadata::MetadataWritePayload =
                serde_json::from_str(payload).unwrap();
            assert_eq!(parsed.item_id, plan.item_id);
            assert_eq!(parsed.paths, plan.paths);
            assert_eq!(parsed.sequence, plan.sequence);
            assert_eq!(parsed.captured_at, 1_791_374_400);
            assert_eq!(parsed.fields_json, r#"{"credit":"x"}"#);
        }

        // The parsed code replacement set rides each payload, so the
        // worker expands codes without re-reading the source file.
        let codes = edit_metadata::CodeReplacements::parse("h1\tSHL\n");
        let with_codes = template_apply_payloads_with(
            &planned,
            r#"{"caption":"\\h1\\"}"#,
            1_791_374_400,
            Some(codes.clone()),
        );
        let parsed: edit_metadata::MetadataWritePayload =
            serde_json::from_str(&with_codes[0]).unwrap();
        assert_eq!(parsed.replacements, Some(codes));
        std::fs::remove_dir_all(&dir).ok();
    }
}
