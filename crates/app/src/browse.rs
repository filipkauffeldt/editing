//! Browse helpers for M1: folder scan, RAW+JPEG pairing, filter/sort,
//! selection, zoom, and thumbnail state.
//!
//! Everything here is UI-agnostic so `cargo test` covers it without a
//! display. The GPUI views in [`crate::views`] render snapshots of these
//! types; the durable rows stay in `edit-catalog` (migration 001 is frozen,
//! so this module groups photos in memory instead of adding a store).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Photo file kinds the browser understands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PhotoKind {
    Raw,
    Jpeg,
}

/// Raw extensions served through the embedded-preview path
/// (`lightcraft-raw`; NEF/ARW/CR2/DNG plus the preview-grade rest).
pub const RAW_EXTS: &[&str] = &[
    "NEF", "NRW", "ARW", "SRF", "SR2", "CR2", "CR3", "RAF", "ORF", "RW2", "DNG", "PEF", "TIF",
    "TIFF",
];
/// Plain display files decoded directly.
pub const JPEG_EXTS: &[&str] = &["JPG", "JPEG"];

/// Classify a file by extension (case-insensitive). `None` means skipped.
pub fn classify_file(path: &Path) -> Option<PhotoKind> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_uppercase())?;
    if RAW_EXTS.contains(&ext.as_str()) {
        Some(PhotoKind::Raw)
    } else if JPEG_EXTS.contains(&ext.as_str()) {
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
/// are deterministic and progress events read in order.
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

/// One contact-sheet cell: a RAW+JPEG pair (or lone file) sharing
/// `(dir, base_name)`, matching the `items` table grain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairGroup {
    pub dir: String,
    pub base_name: String,
    pub raw: Option<FileEntry>,
    pub jpeg: Option<FileEntry>,
}

/// Group entries into pair groups. Later files win within one side; groups
/// sort by `(dir, base_name)`.
pub fn pair_entries(entries: Vec<FileEntry>) -> Vec<PairGroup> {
    let mut map: BTreeMap<(String, String), PairGroup> = BTreeMap::new();
    for entry in entries {
        let key = (entry.dir.clone(), entry.base_name.clone());
        let group = map.entry(key).or_insert_with(|| PairGroup {
            dir: entry.dir.clone(),
            base_name: entry.base_name.clone(),
            raw: None,
            jpeg: None,
        });
        match entry.kind {
            PhotoKind::Raw => group.raw = Some(entry),
            PhotoKind::Jpeg => group.jpeg = Some(entry),
        }
    }
    map.into_values().collect()
}

// ---- browse item snapshot (what the views render) ----

/// Upload badge states (plan section 2.2). B1 never writes transfers, so the
/// scan path leaves this at `None`; later stages fill it from the transfers
/// store per item.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum UploadBadge {
    #[default]
    None,
    Queued,
    Uploading,
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

impl SortOrder {
    pub fn apply(&self, items: &mut [BrowseItem]) {
        match self {
            Self::IngestOrder => items.sort_by_key(|item| item.item_id),
            Self::FileName => items.sort_by(|a, b| {
                (a.dir.clone(), a.base_name.clone()).cmp(&(b.dir.clone(), b.base_name.clone()))
            }),
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
        SortOrder::FileName => picked.sort_by(|a, b| {
            (a.dir.clone(), a.base_name.clone()).cmp(&(b.dir.clone(), b.base_name.clone()))
        }),
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

    pub fn scroll_zoom(&mut self, factor: f32) {
        let current = match self.mode {
            ZoomMode::Fit => return,
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
    pub files_seen: usize,
    pub photos_added: usize,
    pub items_touched: usize,
    pub thumb_jobs: usize,
}

/// Scan `root_path` into the catalog: one `folder` root, photo rows, pair
/// items, and one `thumb` job per new photo.
///
/// Idempotent: files already cataloged under this root are skipped, so a
/// second scan (or a restarted `catalog_scan` job) only adds newcomers.
/// `on_file` fires per new file so the job handler can emit
/// `JobEventKind::Progress` and live-update the UI.
pub fn run_catalog_scan(
    catalog: &edit_catalog::Catalog,
    root_path: &Path,
    on_file: &dyn Fn(usize, usize),
) -> Result<ScanSummary, edit_catalog::Error> {
    let root_str = root_path.to_string_lossy().into_owned();
    let root_id = match catalog.get_root_by_path(&root_str)? {
        Some(root) => root.id,
        None => catalog.insert_root(&root_str, "folder")?,
    };
    let known: BTreeSet<String> = catalog
        .list_photos_by_root(root_id)?
        .into_iter()
        .map(|photo| photo.path)
        .collect();

    let fresh: Vec<FileEntry> = collect_entries(root_path)
        .into_iter()
        .filter(|entry| !known.contains(&entry.path.to_string_lossy().into_owned()))
        .collect();
    let total = fresh.len();
    let groups = pair_entries(fresh);

    let mut summary = ScanSummary {
        root_id,
        files_seen: total,
        ..ScanSummary::default()
    };
    let mut done = 0usize;
    for group in groups.iter() {
        let mut raw_id = None;
        let mut jpeg_id = None;
        for entry in [group.raw.as_ref(), group.jpeg.as_ref()]
            .into_iter()
            .flatten()
        {
            let kind_str = match entry.kind {
                PhotoKind::Raw => "raw",
                PhotoKind::Jpeg => "jpeg",
            };
            let photo_id = catalog.insert_photo(&edit_catalog::models::NewPhoto {
                root_id,
                path: entry.path.to_string_lossy().into_owned(),
                dir: entry.dir.clone(),
                base_name: entry.base_name.clone(),
                ext: entry.ext.clone(),
                kind: kind_str.to_string(),
                bytes: entry.bytes as i64,
                mtime: entry.mtime,
            })?;
            match entry.kind {
                PhotoKind::Raw => raw_id = Some(photo_id),
                PhotoKind::Jpeg => jpeg_id = Some(photo_id),
            }
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
        // Pair grain: reuse the existing item row when a rescan completes a
        // half-pair, else insert the new pair cell.
        match catalog.get_item_by_dir_base(&group.dir, &group.base_name)? {
            Some(_) => {}
            None => {
                catalog.insert_item(&group.dir, &group.base_name, raw_id, jpeg_id)?;
            }
        }
        summary.items_touched += 1;
    }
    Ok(summary)
}

/// Rebuild browse snapshots for a root: photos regrouped into pair cells
/// with item ids resolved through the `items` table.
pub fn list_browse_items(
    catalog: &edit_catalog::Catalog,
    root_id: i64,
) -> Result<Vec<BrowseItem>, edit_catalog::Error> {
    let photos = catalog.list_photos_by_root(root_id)?;
    let mut groups: BTreeMap<(String, String), Vec<&edit_catalog::models::Photo>> = BTreeMap::new();
    for photo in &photos {
        groups
            .entry((photo.dir.clone(), photo.base_name.clone()))
            .or_default()
            .push(photo);
    }
    let mut items = Vec::with_capacity(groups.len());
    for ((dir, base_name), members) in groups {
        let item_id = catalog
            .get_item_by_dir_base(&dir, &base_name)?
            .map(|item| item.id)
            .unwrap_or(-1);
        let has_raw = members.iter().any(|p| p.kind == "raw");
        let has_jpeg = members.iter().any(|p| p.kind == "jpeg");
        let display_path = members
            .iter()
            .find(|p| p.kind == "jpeg")
            .or_else(|| members.first())
            .map(|p| p.path.clone())
            .unwrap_or_default();
        let rating = members.iter().map(|p| p.rating).max().unwrap_or(0);
        items.push(BrowseItem {
            item_id,
            dir,
            base_name,
            display_path,
            has_raw,
            has_jpeg,
            rating,
            thumb: ThumbState::Queued,
            upload: UploadBadge::None,
            ingest_fresh: false,
        });
    }
    items.sort_by_key(|item| item.item_id);
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn scratch_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("edit-browse-test-{}-{}", std::process::id(), name));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn touch(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, b"fake-image-bytes").unwrap();
        path
    }

    fn tiny_jpeg(dir: &Path, name: &str) -> PathBuf {
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
        assert!(!progress.borrow().is_empty());

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
        sel.select_all(3);
        assert_eq!(sel.selected.len(), 3);
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
        zoom.scroll_zoom(2.0);
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
