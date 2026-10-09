//! Metadata I/O: EXIF/XMP read, resident exiftool writer, `crs:` sidecars.
//!
//! Read path is native Rust (kamadak-exif + quick-xml). Write path is a
//! resident `exiftool -stay_open` process for tag writes, plus a direct
//! quick-xml `crs:` sidecar writer for Lightroom interop.

pub mod apply;
pub mod code_replacements;
pub mod crs;
pub mod crs_keys;
pub mod draft;
pub mod error;
pub mod fields;
pub mod iptc_read;
pub mod persist;
pub mod photo;
pub mod template;
pub mod writer;

pub use apply::{
    MetadataWritePayload, preview_fields, preview_fields_with, run_metadata_write, vars_for,
};
pub use code_replacements::{CodeReplacements, HotNamespace};
pub use crs::{
    channel_curve_points, crop_attributes, develop_attributes, exiftool_tag_args,
    tone_curve_points, write_sidecar,
};
pub use draft::changed_fields;
pub use error::{Error, Result};
pub use fields::{FIELDS, FieldSpec, Group, Kind};
pub use iptc_read::{FieldValues, Merged, merge_values, read_fields};
pub use persist::{PersistOutcome, persist_edit, persist_sidecar};
pub use photo::{
    PhotoMetadata, read_exif, read_photo_metadata, read_xmp_file, read_xmp_sidecar_for,
};
pub use template::{TemplateFields, TemplateVars, expand, expand_with_codes, xmp_only};
pub use writer::{ProcessTransport, Transport, Writer};
