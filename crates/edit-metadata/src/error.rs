use thiserror::Error;

/// Errors raised by metadata read and write paths.
#[derive(Debug, Error)]
pub enum Error {
    /// Filesystem or subprocess I/O failure.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// EXIF parse failure (kamadak-exif).
    #[error("exif: {0}")]
    Exif(#[from] exif::Error),
    /// XMP parse or serialize failure (quick-xml).
    #[error("xml: {0}")]
    Xml(#[from] quick_xml::Error),
    /// Develop/crop JSON in the catalog did not parse.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    /// Catalog access failure while persisting edits.
    #[error("catalog: {0}")]
    Catalog(#[from] edit_catalog::Error),
    /// The exiftool binary is missing or unusable.
    #[error("exiftool unavailable: {0}")]
    Unavailable(String),
    /// A path cannot be handed to the exiftool argfile (not UTF-8, or
    /// contains a line break) — rejected instead of lossy-mangled.
    #[error("path: {0}")]
    InvalidPath(String),
    /// The resident exiftool process reported a failure.
    #[error("exiftool: {0}")]
    ExifTool(String),
}

/// Convenience alias for metadata operations.
pub type Result<T> = std::result::Result<T, Error>;
