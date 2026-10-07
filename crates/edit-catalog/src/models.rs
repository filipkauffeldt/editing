//! Typed rows for every catalog table.
//!
//! `Row` structs are what the stores return; `New*` structs (or plain
//! parameter lists for narrow tables) are what they take. Later PRs grow
//! these per feature; the shapes stay stable.

use serde::{Deserialize, Serialize};

// ---- roots ----

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Root {
    pub id: i64,
    pub path: String,
    pub kind: String,
    pub added_at: i64,
}

// ---- photos ----

#[derive(Clone, Debug, PartialEq)]
pub struct Photo {
    pub id: i64,
    pub root_id: i64,
    pub path: String,
    pub dir: String,
    pub base_name: String,
    pub ext: String,
    pub kind: String,
    pub bytes: i64,
    pub mtime: i64,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub orientation: Option<i64>,
    pub taken_at: Option<i64>,
    pub camera: Option<String>,
    pub lens: Option<String>,
    pub iso: Option<i64>,
    pub aperture: Option<f64>,
    pub shutter: Option<String>,
    pub focal: Option<i64>,
    pub rating: i64,
    pub color_label: Option<String>,
    pub ingest_batch_id: Option<i64>,
    pub created_at: i64,
    /// File name on the source (card) before the rename rule was applied.
    pub original_name: Option<String>,
}

/// Required fields for inserting a photo; EXIF-derived fields fill in later.
#[derive(Clone, Debug)]
pub struct NewPhoto {
    pub root_id: i64,
    pub path: String,
    pub dir: String,
    pub base_name: String,
    pub ext: String,
    pub kind: String,
    pub bytes: i64,
    pub mtime: i64,
    /// Source file name before renaming (duplicate-check key).
    pub original_name: Option<String>,
    /// EXIF capture time; `None` when the source has no readable tag.
    pub taken_at: Option<i64>,
}

// ---- items ----

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub id: i64,
    pub dir: String,
    pub base_name: String,
    pub raw_photo_id: Option<i64>,
    pub jpeg_photo_id: Option<i64>,
}

// ---- edits ----

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edit {
    pub item_id: i64,
    pub develop_json: String,
    pub crop_json: Option<String>,
    pub updated_at: i64,
    pub crs_written_at: Option<i64>,
}

// ---- selections / masks ----

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selection {
    pub id: i64,
    pub item_id: i64,
    pub name: Option<String>,
    pub shapes_json: String,
    pub updated_at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mask {
    pub selection_id: i64,
    pub size: String,
    pub hash: String,
    pub rle: Vec<u8>,
}

// ---- retouch ----

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetouchRecipeRow {
    pub id: i64,
    pub name: String,
    pub ops_json: String,
    pub is_sticky: bool,
    pub created_at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetouchVersion {
    pub id: i64,
    pub item_id: i64,
    pub parent_id: Option<i64>,
    pub recipe_json: String,
    pub is_top: bool,
    pub created_at: i64,
}

// ---- presets ----

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preset {
    pub id: i64,
    pub name: String,
    pub group_name: Option<String>,
    pub path: String,
    pub source: String,
    pub updated_at: i64,
}

// ---- detections ----

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Detection {
    pub id: i64,
    pub item_id: i64,
    pub region_json: String,
    pub boxes_json: String,
    pub detector: String,
    pub created_at: i64,
}

// ---- transfers ----

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transfer {
    pub id: i64,
    pub item_id: i64,
    pub recipe_id: i64,
    pub remote_path: String,
    pub state: String,
    pub bytes_done: i64,
    pub attempts: i64,
    pub error: Option<String>,
    pub updated_at: i64,
}

// ---- templates / recipes / codes ----

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IptcTemplate {
    pub id: i64,
    pub name: String,
    pub fields_json: String,
    pub scope: String,
    pub updated_at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SendRecipe {
    pub id: i64,
    pub name: String,
    pub config_json: String,
    pub is_active: bool,
    pub updated_at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodeReplacement {
    pub id: i64,
    pub group_name: String,
    pub code: String,
    pub value: String,
}

// ---- ingest ----

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IngestBatch {
    pub id: i64,
    pub volume_uuid: Option<String>,
    pub volume_label: Option<String>,
    pub session_name: Option<String>,
    pub dest_dir: String,
    pub backup_dir: Option<String>,
    pub template_id: Option<i64>,
    pub state: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
}

/// Parameters for opening an ingest batch: the volume it came from, the
/// destination and backup folders, and the selected IPTC template.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewIngestBatch {
    pub volume_uuid: Option<String>,
    pub volume_label: Option<String>,
    pub session_name: Option<String>,
    pub dest_dir: String,
    pub backup_dir: Option<String>,
    pub template_id: Option<i64>,
    pub state: String,
}

/// One file's ingest history row: the durable record of a copy, its
/// verification hash, and the Lightroom-style duplicate key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IngestFile {
    pub id: i64,
    pub batch_id: i64,
    pub original_name: String,
    pub taken_at: Option<i64>,
    pub bytes: i64,
    pub source_path: String,
    pub dest_path: Option<String>,
    pub backup_path: Option<String>,
    pub blake3: Option<String>,
    pub photo_id: Option<i64>,
    pub state: edit_core::IngestFileState,
    pub error: Option<String>,
    pub created_at: i64,
}

/// Parameters for recording a file as it enters the ingest pipeline. The
/// verification fields fill in later through
/// [`crate::stores::Catalog::complete_ingest_file`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewIngestFile {
    pub batch_id: i64,
    pub original_name: String,
    pub taken_at: Option<i64>,
    pub bytes: i64,
    pub source_path: String,
    pub state: edit_core::IngestFileState,
}
