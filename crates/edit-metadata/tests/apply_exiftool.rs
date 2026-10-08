//! C3 demo as a running test: batch-apply an IPTC template to three photos.
//!
//! End-to-end through the real pipeline: catalog rows → durable
//! `metadata_write` jobs → worker threads → the resident exiftool writer
//! → tags land in the files. Reads verification back with the same
//! mechanism Photo Mechanic-compatible tools use: exiftool itself.
//!
//! Requires the exiftool binary (`$EXIFTOOL`, else `PATH`); a missing one
//! fails the test, see `Writer::binary_for_tests`. The selection-driven
//! scenarios (pairs, re-apply, resume, non-ASCII, multi-line) live in
//! `crates/app/tests/template_apply.rs`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use edit_catalog::db::now_unix;
use edit_catalog::{Catalog, ClaimedJob, JobKind, JobState, WorkerPool, models::NewPhoto};
use edit_metadata::{MetadataWritePayload, Writer};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("photo.jpg")
}

fn three_photos() -> (tempfile::TempDir, Vec<PathBuf>) {
    let dir = tempfile::TempDir::new().unwrap();
    // One pair cell per session folder: items key on (dir, base_name), so
    // the batch is three items in three folders (which also varies
    // `{session}`).
    let mut paths = Vec::new();
    for name in ["01", "02", "03"] {
        let folder = dir.path().join(name);
        std::fs::create_dir_all(&folder).unwrap();
        let path = folder.join("AHQ.JPG");
        std::fs::copy(fixture(), &path).unwrap();
        paths.push(path);
    }
    (dir, paths)
}

fn read_tag(exiftool: &Path, file: &Path, tag: &str) -> Option<String> {
    let out = Command::new(exiftool)
        .arg("-b")
        .arg(format!("-{tag}"))
        .arg(file)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let trimmed = text.trim_end_matches('\n').to_owned();
    (!trimmed.is_empty()).then_some(trimmed)
}

fn read_tags(exiftool: &Path, file: &Path, tag: &str) -> Vec<String> {
    read_tag(exiftool, file, tag)
        .map(|raw| {
            // Multi-value tags print one value per line (XMP bags) or a
            // ", " joined string (IIM).
            raw.split(['\n', ',', ';'])
                .map(str::trim)
                .filter(|kw| !kw.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn template_apply_recipe_writes_the_selection() {
    let Some(exiftool) = Writer::binary_for_tests() else {
        return;
    };
    let (dir, paths) = three_photos();
    let catalog = Catalog::open(&dir.path().join("catalog.sqlite")).unwrap();
    let root_id = catalog
        .insert_root(dir.path().to_str().unwrap(), "folder")
        .unwrap();

    let template = serde_json::json!({
        "caption": "{base} #{seq:2}",
        "keywords": "shooting; {camera}",
        "credit": "SHL",
        "copyright": "(c) {yyyy} SHL",
        "edit_status": "New",
    })
    .to_string();
    let template_id = catalog
        .insert_iptc_template("evening run", &template, "global")
        .unwrap();

    // One photo row + item row per display cell, as the scan would do.
    let mut item_ids = Vec::new();
    for (offset, path) in paths.iter().enumerate() {
        let photo_id = catalog
            .insert_photo(&NewPhoto {
                root_id,
                path: path.to_string_lossy().into_owned(),
                dir: path.parent().unwrap().to_string_lossy().into_owned(),
                base_name: "AHQ".into(),
                ext: "JPG".into(),
                kind: "jpeg".into(),
                bytes: 5,
                mtime: 0,
                original_name: None,
                taken_at: None,
            })
            .unwrap();
        let item_id = catalog
            .insert_item(
                path.parent().unwrap().to_str().unwrap(),
                "ahq",
                Some(photo_id),
                None,
            )
            .unwrap();
        item_ids.push((item_id, offset as u64 + 1, photo_id));
    }

    // Batch apply: select-all, one job per item, sequence in display order.
    for (item_id, sequence, photo_id) in &item_ids {
        let payload = serde_json::to_string(&MetadataWritePayload {
            item_id: *item_id,
            paths: vec![catalog.get_photo(*photo_id).unwrap().path],
            sequence: *sequence,
            captured_at: now_unix(),
            session: None,
            fields_json: catalog.get_iptc_template(template_id).unwrap().fields_json,
            replacements: None,
        })
        .unwrap();
        catalog
            .enqueue_job(JobKind::MetadataWrite, &payload)
            .unwrap();
    }

    let writer = Arc::new(Writer::resident(&exiftool).unwrap());
    let handler: edit_catalog::JobHandler =
        Arc::new(move |job: &ClaimedJob, tx| edit_metadata::run_metadata_write(job, &writer, tx));
    let pool = spawn_pool(&catalog, handler);
    wait_for(Duration::from_secs(60), || {
        catalog.count_jobs_by_state(JobState::Done).unwrap_or(0) == 3
    });
    pool.shutdown();

    // Variables expanded per photo: sequence and capture fields differ.
    assert_eq!(
        read_tag(&exiftool, &paths[1], "XMP-dc:Description").as_deref(),
        Some("AHQ #02"),
        "caption expanded with the batch sequence"
    );
    assert_eq!(
        read_tag(&exiftool, &paths[0], "XMP-dc:Description").as_deref(),
        Some("AHQ #01")
    );
    // Keywords: one typed literal plus one from the fixture's EXIF camera.
    let keywords = read_tags(&exiftool, &paths[0], "XMP-dc:Subject");
    assert_eq!(keywords, vec!["shooting", "TestCam 1"]);
    assert_eq!(
        read_tags(&exiftool, &paths[0], "IPTC:Keywords"),
        keywords,
        "IIM mirrors XMP (dual write)"
    );
    assert_eq!(
        read_tag(&exiftool, &paths[0], "IPTC:Credit").as_deref(),
        Some("SHL")
    );
    // The fixture carries 2026 EXIF, so `{yyyy}` resolves from capture.
    assert_eq!(
        read_tag(&exiftool, &paths[2], "XMP-dc:Rights").as_deref(),
        Some("(c) 2026 SHL")
    );
    assert_eq!(
        read_tag(&exiftool, &paths[2], "IPTC:CopyrightNotice").as_deref(),
        Some("(c) 2026 SHL")
    );
    assert_eq!(
        read_tag(&exiftool, &paths[2], "IPTC:EditStatus").as_deref(),
        Some("New")
    );
    std::fs::remove_dir_all(dir.path()).ok();
}

#[test]
fn code_replacements_expand_into_the_written_caption() {
    let Some(exiftool) = Writer::binary_for_tests() else {
        return;
    };
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("AHQ.JPG");
    std::fs::copy(fixture(), &path).unwrap();
    let catalog = Catalog::open(&dir.path().join("catalog.sqlite")).unwrap();
    let root_id = catalog
        .insert_root(dir.path().to_str().unwrap(), "folder")
        .unwrap();
    let photo_id = catalog
        .insert_photo(&NewPhoto {
            root_id,
            path: path.to_string_lossy().into_owned(),
            dir: dir.path().to_string_lossy().into_owned(),
            base_name: "AHQ".into(),
            ext: "JPG".into(),
            kind: "jpeg".into(),
            bytes: 5,
            mtime: 0,
            original_name: None,
            taken_at: None,
        })
        .unwrap();
    let item_id = catalog
        .insert_item(dir.path().to_str().unwrap(), "ahq", Some(photo_id), None)
        .unwrap();

    // The caption uses the demo hot-code spelling, resolved from the
    // parsed replacement set carried in the durable payload.
    let fields = serde_json::json!({"caption": "Byline: \\h1\\"}).to_string();
    let replacements = edit_metadata::CodeReplacements::parse("h1\tSHL\n");
    let payload = serde_json::to_string(&MetadataWritePayload {
        item_id,
        paths: vec![path.to_string_lossy().into_owned()],
        sequence: 1,
        captured_at: now_unix(),
        fields_json: fields,
        replacements: Some(replacements),
    })
    .unwrap();
    catalog
        .enqueue_job(JobKind::MetadataWrite, &payload)
        .unwrap();

    let writer = Arc::new(Writer::resident(&exiftool).unwrap());
    let handler: edit_catalog::JobHandler =
        Arc::new(move |job: &ClaimedJob, tx| edit_metadata::run_metadata_write(job, &writer, tx));
    let pool = spawn_pool(&catalog, handler);
    wait_for(Duration::from_secs(60), || {
        catalog.count_jobs_by_state(JobState::Done).unwrap_or(0) == 1
    });
    pool.shutdown();

    assert_eq!(
        read_tag(&exiftool, &path, "XMP-dc:Description").as_deref(),
        Some("Byline: SHL"),
        "hot code expanded in the written caption"
    );
    assert_eq!(
        read_tag(&exiftool, &path, "IPTC:Caption-Abstract").as_deref(),
        Some("Byline: SHL")
    );
    std::fs::remove_dir_all(dir.path()).ok();
}

fn spawn_pool(catalog: &Catalog, handler: edit_catalog::JobHandler) -> WorkerPool {
    edit_catalog::spawn_workers(catalog, 2, handler).unwrap()
}

fn wait_for(timeout: Duration, mut cond: impl FnMut() -> bool) {
    let start = std::time::Instant::now();
    while !cond() {
        assert!(start.elapsed() < timeout, "timed out waiting for jobs");
        std::thread::sleep(Duration::from_millis(20));
    }
}
