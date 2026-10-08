//! Ingest with an IPTC template, end to end against a real exiftool: a
//! folder standing in for a card goes through the durable job path (the
//! same dispatch the app runs: ingest copy, thumbnail and `metadata_write`
//! jobs over worker threads), and the template is read back from the files
//! that landed. Resume and re-apply must not change the result.
//!
//! Needs the exiftool binary (`$EXIFTOOL`, else `PATH`); a missing one
//! fails the test, see `Writer::binary_for_tests`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use app::ingest_job::{IngestStart, handle_job, start_ingest};
use edit_catalog::{Catalog, ClaimedJob, JobHandler, JobKind, JobState};
use edit_metadata::{Writer, run_metadata_write};

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

/// The template under test: a session variable, a sequence number, a
/// per-file variable and a keyword list.
const TEMPLATE: &str = r#"{"credit":"SHL {session}","caption":"{base} #{seq:3}","keywords":"ice; {base}","copyright":"(c) SHL"}"#;

struct Card {
    base: PathBuf,
    card: PathBuf,
    dest: PathBuf,
    catalog: Arc<Catalog>,
    bin: PathBuf,
    writer: Arc<Writer>,
}

impl Card {
    /// A card with one RAW+JPEG pair (`DSC_0001`) and a lone raw (`DSC_0002`).
    /// The raws are opaque bytes: the writer must never open them.
    fn new() -> Option<Self> {
        let bin = Writer::binary_for_tests()?;
        let n = NEXT_DIR.fetch_add(1, Ordering::SeqCst);
        let base = std::env::temp_dir().join(format!("edit-ingest-tpl-{}-{n}", std::process::id()));
        let dcim = base.join("card/DCIM/100NIKON");
        std::fs::create_dir_all(&dcim).unwrap();
        let img = image::RgbImage::from_pixel(8, 8, image::Rgb([200, 40, 40]));
        let mut jpeg = Vec::new();
        img.write_to(
            &mut std::io::Cursor::new(&mut jpeg),
            image::ImageFormat::Jpeg,
        )
        .unwrap();
        std::fs::write(dcim.join("DSC_0001.JPG"), jpeg).unwrap();
        std::fs::write(dcim.join("DSC_0001.NEF"), b"opaque raw container one").unwrap();
        std::fs::write(dcim.join("DSC_0002.NEF"), b"opaque raw container two").unwrap();
        let catalog = Arc::new(Catalog::open(&base.join("catalog.sqlite")).unwrap());
        let writer = Arc::new(Writer::resident(&bin).unwrap());
        Some(Self {
            card: base.join("card"),
            dest: base.join("Photos"),
            base,
            catalog,
            bin,
            writer,
        })
    }

    /// Open a batch with the template and queue its ingest job.
    fn start(&self) -> edit_catalog::JobId {
        let template = match self
            .catalog
            .list_iptc_templates()
            .unwrap()
            .into_iter()
            .next()
        {
            Some(row) => row,
            None => {
                let id = self
                    .catalog
                    .insert_iptc_template("press", TEMPLATE, "ingest")
                    .unwrap();
                self.catalog.get_iptc_template(id).unwrap()
            }
        };
        let rule = self.dest.to_string_lossy().into_owned();
        start_ingest(
            &self.catalog,
            &IngestStart {
                volume_uuid: None,
                volume_label: Some("NIKON Z 6"),
                source_root: &self.card,
                dest_rule: &rule,
                filename_rule: "",
                backup_dir: "",
                session: "Cup final",
                template: Some(&template),
            },
        )
        .unwrap()
        .job_id
    }

    /// Run workers like the app does until the queue is empty.
    fn drain(&self) {
        let writer = Arc::clone(&self.writer);
        let catalog = Arc::clone(&self.catalog);
        let handler: JobHandler = Arc::new(move |job: &ClaimedJob, tx| match job.kind {
            JobKind::MetadataWrite => run_metadata_write(job, &writer, tx),
            _ => handle_job(Arc::clone(&catalog), job, tx),
        });
        let pool = edit_catalog::spawn_workers(&self.catalog, 3, handler).unwrap();
        let start = Instant::now();
        while self.count(JobState::Queued) + self.count(JobState::Running) > 0 {
            assert!(
                start.elapsed() < Duration::from_secs(60),
                "jobs did not drain"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        pool.shutdown();
    }

    fn count(&self, state: JobState) -> i64 {
        self.catalog.count_jobs_by_state(state).unwrap()
    }

    fn failed_stamp_jobs(&self) -> Vec<String> {
        self.catalog
            .list_jobs(200)
            .unwrap()
            .into_iter()
            .filter(|job| job.kind == JobKind::MetadataWrite && job.state == JobState::Failed)
            .map(|job| job.error.unwrap_or_default())
            .collect()
    }

    fn stamp_jobs(&self) -> usize {
        self.catalog
            .list_jobs(200)
            .unwrap()
            .iter()
            .filter(|job| job.kind == JobKind::MetadataWrite)
            .count()
    }

    /// Where a card file landed (the date folder depends on the clock).
    fn landed(&self, name: &str) -> PathBuf {
        let mut found = Vec::new();
        for day in walk(&self.dest) {
            if day.file_name().is_some_and(|n| n.to_string_lossy() == name) {
                found.push(day);
            }
        }
        assert_eq!(found.len(), 1, "{name} landed once: {found:?}");
        found.remove(0)
    }

    /// A tag as a list of strings (`-j` gives scalars for single values).
    fn list(&self, file: &Path, tag: &str) -> Vec<String> {
        let out = Command::new(&self.bin)
            .args(["-j", &format!("-{tag}")])
            .arg(file)
            .output()
            .expect("exiftool read-back runs");
        assert!(out.status.success(), "exiftool read: {out:?}");
        let rows: Vec<serde_json::Map<String, serde_json::Value>> =
            serde_json::from_slice(&out.stdout).unwrap();
        let name = tag.rsplit(':').next().unwrap();
        match rows.into_iter().next().and_then(|mut row| row.remove(name)) {
            Some(serde_json::Value::Array(values)) => values
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| v.to_string())
                })
                .collect(),
            Some(serde_json::Value::String(one)) => vec![one],
            Some(other) => vec![other.to_string()],
            None => vec![],
        }
    }

    /// Everything the template wrote to one file or sidecar, to compare runs.
    fn stamp_of(&self, file: &Path, xmp_only: bool) -> Vec<(String, Vec<String>)> {
        let mut tags = vec![
            "XMP-photoshop:Credit",
            "XMP-dc:Description",
            "XMP-dc:Subject",
            "XMP-dc:Rights",
        ];
        if !xmp_only {
            tags.extend(["IPTC:Credit", "IPTC:Caption-Abstract", "IPTC:Keywords"]);
        }
        tags.into_iter()
            .map(|tag| (tag.to_owned(), self.list(file, tag)))
            .collect()
    }
}

impl Drop for Card {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.base).ok();
    }
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }
    out
}

#[test]
fn an_ingest_stamps_the_template_on_what_lands_without_touching_raw_bytes() {
    let Some(card) = Card::new() else { return };
    let ingest = card.start();
    card.drain();

    assert_eq!(card.catalog.job(ingest).unwrap().state, JobState::Done);
    assert_eq!(card.stamp_jobs(), 3, "one stamp job per landed photo");
    assert_eq!(card.failed_stamp_jobs(), Vec::<String>::new());

    // Raws: bytes identical to the card, the template lives in the sidecar.
    for (name, card_bytes) in [
        ("DSC_0001.NEF", &b"opaque raw container one"[..]),
        ("DSC_0002.NEF", &b"opaque raw container two"[..]),
    ] {
        let landed = card.landed(&format!("Cup final_{}.NEF", &name[4..8]));
        assert_eq!(
            std::fs::read(&landed).unwrap(),
            card_bytes,
            "{name} untouched"
        );
        assert!(
            landed.with_extension("xmp").is_file(),
            "{name} has a sidecar"
        );
    }

    // The pair shares sequence 1; the lone raw is 2. `{session}` is the
    // ingest session, not the date folder.
    let jpg = card.landed("Cup final_0001.JPG");
    let raw1_sidecar = card.landed("Cup final_0001.xmp");
    let raw2_sidecar = card.landed("Cup final_0002.xmp");
    for (file, base, seq) in [
        (&jpg, "Cup final_0001", 1),
        (&raw1_sidecar, "Cup final_0001", 1),
        (&raw2_sidecar, "Cup final_0002", 2),
    ] {
        assert_eq!(
            card.list(file, "XMP-photoshop:Credit"),
            ["SHL Cup final"],
            "{file:?}"
        );
        assert_eq!(
            card.list(file, "XMP-dc:Description"),
            [format!("{base} #{seq:03}")]
        );
        assert_eq!(card.list(file, "XMP-dc:Subject"), ["ice", base]);
        assert_eq!(card.list(file, "XMP-dc:Rights"), ["(c) SHL"]);
    }
    // The JPEG master also carries the IIM copy, written in place.
    assert_eq!(card.list(&jpg, "IPTC:Credit"), ["SHL Cup final"]);
    assert_eq!(card.list(&jpg, "IPTC:Keywords"), ["ice", "Cup final_0001"]);

    // Every file is on the contact sheet's catalog with its thumbnail queued.
    let root = card
        .catalog
        .get_root_by_path(&card.dest.canonicalize().unwrap().to_string_lossy())
        .unwrap()
        .unwrap();
    let items = app::browse::list_browse_items(&card.catalog, root.id).unwrap();
    assert_eq!(items.len(), 2, "the pair is one cell, the lone raw another");
    assert!(
        items
            .iter()
            .any(|item| item.has_raw && item.has_jpeg && item.base_name == "Cup final_0001")
    );
    let thumbs = card
        .catalog
        .list_jobs(200)
        .unwrap()
        .iter()
        .filter(|job| job.kind == JobKind::Thumb)
        .count();
    assert_eq!(thumbs, 3);
}

#[test]
fn a_crash_and_a_second_run_leave_the_stamp_exactly_as_one_clean_run_does() {
    let Some(clean) = Card::new() else { return };
    clean.start();
    clean.drain();
    let expected_jpg = clean.stamp_of(&clean.landed("Cup final_0001.JPG"), false);
    let expected_raw = clean.stamp_of(&clean.landed("Cup final_0001.xmp"), true);
    assert_eq!(
        expected_jpg[2].1,
        ["ice", "Cup final_0001"],
        "sanity: {expected_jpg:?}"
    );

    let Some(card) = Card::new() else { return };
    let ingest = card.start();

    // The ingest runs to the end but the process dies before any stamp job
    // finishes: one is claimed (`running`), the rest are still queued.
    let job = card.catalog.claim_job().unwrap().unwrap();
    assert_eq!(job.id, ingest);
    let (tx, _rx) = flume::unbounded();
    app::ingest_job::ingest_job_handler(Arc::clone(&card.catalog), &job, &tx).unwrap();
    card.catalog.complete_job(job.id).unwrap();
    // Thumbnails are not under test; settle them to isolate the stamp jobs.
    let mut stamp = Vec::new();
    while let Some(next) = card.catalog.claim_job().unwrap() {
        if next.kind == JobKind::MetadataWrite {
            stamp.push(next.id);
            // Put it back unfinished below.
        } else {
            card.catalog.complete_job(next.id).unwrap();
        }
    }
    assert_eq!(stamp.len(), 3, "three stamp jobs were queued by the ingest");
    assert_eq!(
        card.count(JobState::Running),
        3,
        "all claimed, none finished"
    );

    // Restart: running jobs return to the queue with their payload intact.
    assert_eq!(card.catalog.recover_running().unwrap(), 3);
    card.drain();
    assert_eq!(card.failed_stamp_jobs(), Vec::<String>::new());
    assert_eq!(
        card.stamp_of(&card.landed("Cup final_0001.JPG"), false),
        expected_jpg
    );
    assert_eq!(
        card.stamp_of(&card.landed("Cup final_0001.xmp"), true),
        expected_raw
    );

    // Resuming the finished ingest again lands nothing new and queues no
    // second stamp: every file is a duplicate now.
    let again = card.start();
    card.drain();
    assert_eq!(card.catalog.job(again).unwrap().state, JobState::Done);
    assert_eq!(card.stamp_jobs(), 3);

    // Re-applying the same stamp job is idempotent: no doubled keywords.
    let payload = card
        .catalog
        .list_jobs(200)
        .unwrap()
        .into_iter()
        .find(|job| job.kind == JobKind::MetadataWrite)
        .unwrap()
        .payload_json;
    card.catalog
        .enqueue_job(JobKind::MetadataWrite, &payload)
        .unwrap();
    card.drain();
    assert_eq!(card.failed_stamp_jobs(), Vec::<String>::new());
    assert_eq!(
        card.stamp_of(&card.landed("Cup final_0001.JPG"), false),
        expected_jpg
    );
    assert_eq!(
        card.stamp_of(&card.landed("Cup final_0001.xmp"), true),
        expected_raw
    );
}
