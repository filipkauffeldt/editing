//! EXIF read against a committed fixture (`photo.jpg` carries Make, Model,
//! LensModel, ISO 400, f/5.6, 1/250, 24mm, a capture time, orientation 6,
//! and 64x48 pixels). Pure Rust, runs everywhere.

use std::path::PathBuf;

use edit_metadata::{read_exif, read_photo_metadata};

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("photo.jpg")
}

#[test]
fn exif_reads_the_catalog_fields() {
    let meta = read_exif(&fixture()).unwrap();
    assert_eq!(meta.camera.as_deref(), Some("TestCam 1"));
    assert_eq!(meta.lens.as_deref(), Some("TestLens 24mm"));
    assert_eq!(meta.iso, Some(400));
    assert_eq!(meta.aperture, Some(5.6));
    assert_eq!(meta.shutter.as_deref(), Some("1/250"));
    assert_eq!(meta.focal, Some(24));
    assert_eq!(meta.orientation, Some(6));
    assert_eq!(meta.width, Some(64));
    assert_eq!(meta.height, Some(48));
    assert_eq!(meta.taken_at, Some(1_791_374_400));
}

#[test]
fn photo_metadata_falls_back_to_sidecar() {
    // No sidecar next to the fixture: EXIF alone, still complete.
    let meta = read_photo_metadata(&fixture());
    assert!(!meta.is_empty());
    assert_eq!(meta.iso, Some(400));
}

#[test]
fn missing_file_reads_empty_without_error() {
    let meta = read_photo_metadata(std::path::Path::new("/nonexistent/photo.jpg"));
    assert!(meta.is_empty());
    assert!(read_exif(std::path::Path::new("/nonexistent/photo.jpg")).is_err());
}
