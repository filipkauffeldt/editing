//! Metadata I/O: EXIF/XMP read, resident exiftool writer, `crs:` sidecars.
//!
//! Read path is native Rust (kamadak-exif + quick-xml). Write path is a
//! resident `exiftool -stay_open` process for tag writes, plus a direct
//! quick-xml `crs:` sidecar writer for Lightroom interop.

pub mod apply;
pub mod crs;
pub mod error;
pub mod persist;
pub mod photo;
pub mod template;
pub mod writer;

pub use apply::{MetadataWritePayload, preview_fields, run_metadata_write, vars_for};
pub use crs::{
    channel_curve_points, crop_attributes, develop_attributes, exiftool_tag_args,
    tone_curve_points, write_sidecar,
};
pub use error::{Error, Result};
pub use persist::{PersistOutcome, persist_edit};
pub use photo::{
    PhotoMetadata, read_exif, read_photo_metadata, read_xmp_file, read_xmp_sidecar_for,
};
pub use template::{TemplateFields, TemplateVars, expand};
pub use writer::{ProcessTransport, Transport, Writer};
