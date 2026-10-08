//! Resident writer against a real exiftool: `$EXIFTOOL`, else `exiftool`
//! on `PATH`. CI installs `libimage-exiftool-perl` for this; a missing
//! binary fails the tests (see `Writer::binary_for_tests`).

use std::path::PathBuf;
use std::process::Command;

use edit_metadata::Writer;

fn exiftool() -> Option<PathBuf> {
    Writer::binary_for_tests()
}

fn fixture_copy(dir: &tempfile::TempDir) -> PathBuf {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("photo.jpg");
    let dst = dir.path().join("photo.jpg");
    std::fs::copy(src, &dst).unwrap();
    dst
}

fn exiftool_value(bin: &std::path::Path, file: &std::path::Path, tag: &str) -> String {
    let out = Command::new(bin)
        .args(["-s3", &format!("-XMP-crs:{tag}")])
        .arg(file)
        .output()
        .expect("exiftool read-back runs");
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

#[test]
fn resident_writer_sets_and_updates_tags() {
    let Some(bin) = exiftool() else { return };
    let dir = tempfile::TempDir::new().unwrap();
    let photo = fixture_copy(&dir);

    let writer = Writer::resident(&bin).unwrap();
    writer
        .write_tags(&photo, &[("XMP-crs:Exposure2012", "+0.35")])
        .unwrap();
    assert_eq!(exiftool_value(&bin, &photo, "Exposure2012"), "+0.35");

    // Same resident process serves the second command (stay-open reuse).
    writer
        .write_tags(
            &photo,
            &[
                ("XMP-crs:Exposure2012", "-0.10"),
                ("XMP-crs:Highlights2012", "-74"),
            ],
        )
        .unwrap();
    assert_eq!(exiftool_value(&bin, &photo, "Exposure2012"), "-0.10");
    assert_eq!(exiftool_value(&bin, &photo, "Highlights2012"), "-74");
}

#[test]
fn writer_surfaces_missing_binary() {
    match Writer::resident(std::path::Path::new("/nonexistent/exiftool")) {
        Err(err) => assert!(err.to_string().contains("unavailable"), "{err}"),
        Ok(_) => panic!("resident writer started from a missing binary"),
    }
}
