//! Metadata I/O: EXIF/XMP read, resident exiftool writer, `crs:` sidecars.
//!
//! Read path is native Rust (kamadak-exif + quick-xml). Write path is a
//! resident `exiftool -stay_open` process for tag writes, plus a direct
//! quick-xml `crs:` sidecar writer for Lightroom interop.

pub mod crs;
pub mod error;
pub mod persist;
pub mod photo;
pub mod writer;

pub use crs::{crop_attributes, develop_attributes, tone_curve_points, write_sidecar};
pub use error::{Error, Result};
pub use persist::{PersistOutcome, persist_edit};
pub use photo::{
    PhotoMetadata, read_exif, read_photo_metadata, read_xmp_file, read_xmp_sidecar_for,
};
pub use writer::{ProcessTransport, Transport, Writer};
