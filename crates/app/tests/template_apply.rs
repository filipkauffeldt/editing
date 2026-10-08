//! Template apply scenarios against a real exiftool: the selection is
//! planned by `plan_template_apply`, enqueued as durable `metadata_write`
//! jobs, drained by workers, and verified by reading the files back.
//!
//! Needs the exiftool binary (`$EXIFTOOL`, else `PATH`); a missing one
//! fails the test, see `Writer::binary_for_tests`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use app::browse::{
    TemplateApplyPlan, list_browse_items, plan_template_apply, run_catalog_scan,
    template_apply_payloads,
};
use edit_catalog::{Catalog, ClaimedJob, JobHandler, JobId, JobKind, JobState};
use edit_metadata::{Writer, run_metadata_write};

/// Capture time for the batch: 2026-10-07 12:00:00 UTC.
const CAPTURED_AT: i64 = 1_791_374_400;

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

/// A scanned shoot folder plus the machinery to apply a template to it.
struct Shoot {
    dir: PathBuf,
    catalog: Catalog,
    root_id: i64,
    bin: PathBuf,
    writer: Arc<Writer>,
}

impl Shoot {
    /// `session` names the folder (the `{session}` variable); `files` are
    /// created inside it, `.JPG` as real EXIF-less JPEGs and anything else
    /// as opaque bytes (a stand-in raw the writer must never open).
    fn new(session: &str, files: &[&str]) -> Option<Self> {
        let bin = Writer::binary_for_tests()?;
        let n = NEXT_DIR.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir()
            .join(format!("edit-template-apply-{}-{n}", std::process::id()))
            .join(session);
        std::fs::create_dir_all(&dir).unwrap();
        for name in files {
            if name.ends_with(".JPG") {
                let img = image::RgbImage::from_pixel(8, 8, image::Rgb([200, 40, 40]));
                let mut bytes = Vec::new();
                img.write_to(
                    &mut std::io::Cursor::new(&mut bytes),
                    image::ImageFormat::Jpeg,
                )
                .unwrap();
                std::fs::write(dir.join(name), bytes).unwrap();
            } else {
                std::fs::write(dir.join(name), b"opaque raw container bytes").unwrap();
            }
        }
        let catalog = Catalog::open(&dir.parent().unwrap().join("catalog.sqlite")).unwrap();
        let root_id = run_catalog_scan(&catalog, &dir, &|_, _| {})
            .unwrap()
            .root_id;
        // The scan queued thumbnail jobs; settle them so the queue only
        // holds the metadata jobs under test.
        while let Some(job) = catalog.claim_job().unwrap() {
            catalog.complete_job(job.id).unwrap();
        }
        let writer = Arc::new(Writer::resident(&bin).unwrap());
        Some(Self {
            dir,
            catalog,
            root_id,
            bin,
            writer,
        })
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// Select everything, plan it, and enqueue the batch like the browser
    /// does. Returns the plan and the job ids in plan order.
    fn apply(&self, fields_json: &str) -> (Vec<TemplateApplyPlan>, Vec<JobId>) {
        let items = list_browse_items(&self.catalog, self.root_id).unwrap();
        let plan = plan_template_apply(&items, &self.catalog);
        let payloads = template_apply_payloads(&plan, fields_json, CAPTURED_AT);
        let ids = self
            .catalog
            .enqueue_jobs_batch(JobKind::MetadataWrite, &payloads)
            .unwrap();
        (plan, ids)
    }

    /// Run workers until the queue is empty; returns the failed-job count.
    fn drain(&self) -> i64 {
        let writer = Arc::clone(&self.writer);
        let handler: JobHandler =
            Arc::new(move |job: &ClaimedJob, tx| run_metadata_write(job, &writer, tx));
        let pool = edit_catalog::spawn_workers(&self.catalog, 2, handler).unwrap();
        let start = Instant::now();
        loop {
            let waiting = self.count(JobState::Queued) + self.count(JobState::Running);
            if waiting == 0 {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(60),
                "jobs did not drain"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        pool.shutdown();
        self.count(JobState::Failed)
    }

    fn assert_done(&self, ids: &[JobId]) {
        for id in ids {
            let job = self.catalog.job(*id).unwrap();
            assert_eq!(job.state, JobState::Done, "job {id:?}: {:?}", job.error);
        }
    }

    fn count(&self, state: JobState) -> i64 {
        self.catalog.count_jobs_by_state(state).unwrap()
    }

    /// `exiftool -j <tag> file`: the tag's JSON value (a list stays a
    /// list, a lone value is a scalar), `None` when the tag is absent.
    fn tag(&self, file: &Path, tag: &str) -> Option<serde_json::Value> {
        let out = Command::new(&self.bin)
            .args(["-j", &format!("-{tag}")])
            .arg(file)
            .output()
            .expect("exiftool read-back runs");
        assert!(out.status.success(), "exiftool read: {out:?}");
        let rows: Vec<serde_json::Map<String, serde_json::Value>> =
            serde_json::from_slice(&out.stdout).unwrap();
        let name = tag.rsplit(':').next().unwrap();
        rows.into_iter().next().and_then(|mut row| row.remove(name))
    }

    fn text(&self, file: &Path, tag: &str) -> Option<String> {
        self.tag(file, tag).map(|v| {
            v.as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| v.to_string())
        })
    }

    /// A tag that may be a list, as a list of strings.
    fn list(&self, file: &Path, tag: &str) -> Vec<String> {
        match self.tag(file, tag) {
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

    fn sequence_of(plan: &[TemplateApplyPlan], name: &str) -> u64 {
        plan.iter()
            .find(|step| step.paths.iter().any(|p| p.ends_with(name)))
            .unwrap_or_else(|| panic!("{name} not in plan {plan:?}"))
            .sequence
    }
}

impl Drop for Shoot {
    fn drop(&mut self) {
        std::fs::remove_dir_all(self.dir.parent().unwrap()).ok();
    }
}

#[test]
fn a_pair_stamps_the_jpeg_in_place_and_the_raw_through_its_sidecar() {
    let Some(shoot) = Shoot::new("pair run", &["A.NEF", "A.JPG", "B.JPG"]) else {
        return;
    };
    let raw_before = std::fs::read(shoot.path("A.NEF")).unwrap();
    let fields = serde_json::json!({
        "caption": "{base} #{seq:1}",
        "credit": "SHL",
        "keywords": "ice; Smith, John",
    });
    let (plan, ids) = shoot.apply(&fields.to_string());
    assert_eq!(ids.len(), 2, "one job per item, the pair is one item");
    assert_eq!(shoot.drain(), 0, "no job failed");
    shoot.assert_done(&ids);

    let seq_a = Shoot::sequence_of(&plan, "A.JPG");
    let seq_b = Shoot::sequence_of(&plan, "B.JPG");
    assert_ne!(seq_a, seq_b);
    // JPEG side: in place, dual XMP + IIM.
    let jpg = shoot.path("A.JPG");
    let caption = format!("A #{seq_a}");
    assert_eq!(
        shoot.text(&jpg, "XMP-dc:Description").as_deref(),
        Some(caption.as_str())
    );
    assert_eq!(
        shoot.text(&jpg, "IPTC:Caption-Abstract").as_deref(),
        Some(caption.as_str())
    );
    assert_eq!(
        shoot.text(&shoot.path("B.JPG"), "XMP-dc:Description"),
        Some(format!("B #{seq_b}"))
    );
    // A comma is ordinary keyword text: "Smith, John" stays one keyword.
    assert_eq!(
        shoot.list(&jpg, "XMP-dc:Subject"),
        vec!["ice", "Smith, John"]
    );
    assert_eq!(
        shoot.list(&jpg, "IPTC:Keywords"),
        vec!["ice", "Smith, John"]
    );

    // Raw side: XMP sidecar only, the container is byte-identical.
    let sidecar = shoot.path("A.xmp");
    assert!(sidecar.exists(), "raw side writes its sidecar");
    assert_eq!(
        shoot.text(&sidecar, "XMP-dc:Description").as_deref(),
        Some(caption.as_str())
    );
    assert_eq!(
        shoot.text(&sidecar, "XMP-photoshop:Credit").as_deref(),
        Some("SHL")
    );
    assert_eq!(
        shoot.tag(&sidecar, "IPTC:Credit"),
        None,
        "no IIM in a sidecar"
    );
    assert_eq!(std::fs::read(shoot.path("A.NEF")).unwrap(), raw_before);
    assert!(
        !shoot.path("A.NEF_original").exists(),
        "no -overwrite_original backup next to the raw either"
    );
}

#[test]
fn reapplying_the_same_template_succeeds_as_unchanged() {
    let Some(shoot) = Shoot::new("again", &["A.NEF", "A.JPG"]) else {
        return;
    };
    let fields = serde_json::json!({"credit": "SHL", "caption": "{date}"}).to_string();
    shoot.apply(&fields);
    assert_eq!(shoot.drain(), 0);
    // exiftool 13.59 rewrites an identical value and reports it as updated
    // (the `0 updated / 1 unchanged` summary only appears for no-op
    // deletes, covered by a canned-output unit test); either way a second
    // pass over already-stamped photos must not turn any cell red.
    let (_, ids) = shoot.apply(&fields);
    assert_eq!(shoot.drain(), 0, "idempotent re-apply fails nothing");
    shoot.assert_done(&ids);
}

#[test]
fn a_batch_interrupted_after_a_write_resumes_deterministically() {
    let Some(shoot) = Shoot::new("resume", &["A.JPG", "B.JPG"]) else {
        return;
    };
    // `{date}` has no EXIF to read here, so it falls back to the clock
    // stamped into the payload at enqueue time, not the clock at resume.
    let fields = serde_json::json!({"caption": "{date} {base}"}).to_string();
    let (_, ids) = shoot.apply(&fields);

    // The "crash": every job is claimed (running), the first one wrote its
    // tags, and the process died before completing anything.
    let first = shoot.catalog.claim_job().unwrap().unwrap();
    let _second = shoot.catalog.claim_job().unwrap().unwrap();
    let (tx, _rx) = flume::unbounded();
    run_metadata_write(&first, &shoot.writer, &tx).unwrap();
    assert_eq!(shoot.count(JobState::Running), 2);

    assert_eq!(shoot.catalog.recover_running().unwrap(), 2);
    assert_eq!(
        shoot.drain(),
        0,
        "the already-written job reruns as unchanged"
    );
    shoot.assert_done(&ids);
    for name in ["A", "B"] {
        assert_eq!(
            shoot
                .text(&shoot.path(&format!("{name}.JPG")), "XMP-dc:Description")
                .as_deref(),
            Some(format!("2026-10-07 {name}").as_str()),
            "{name}: the fallback date comes from the payload"
        );
    }
}

#[test]
fn non_ascii_values_and_paths_round_trip_in_both_dialects() {
    let Some(shoot) = Shoot::new("Åre 2026", &["Ö1.JPG"]) else {
        return;
    };
    let fields = serde_json::json!({
        "caption": "Café — naïve",
        "byline": "Åke Öström",
        "credit": "Zoë 日本",
        "headline": "{session}",
        "keywords": "smörgåsbord; 氷",
    });
    shoot.apply(&fields.to_string());
    assert_eq!(shoot.drain(), 0);

    let jpg = shoot.path("Ö1.JPG");
    for (xmp, iim, want) in [
        (
            "XMP-dc:Description",
            "IPTC:Caption-Abstract",
            "Café — naïve",
        ),
        ("XMP-dc:Creator", "IPTC:By-line", "Åke Öström"),
        ("XMP-photoshop:Credit", "IPTC:Credit", "Zoë 日本"),
        ("XMP-photoshop:Headline", "IPTC:Headline", "Åre 2026"),
    ] {
        assert_eq!(shoot.text(&jpg, xmp).as_deref(), Some(want), "{xmp}");
        assert_eq!(shoot.text(&jpg, iim).as_deref(), Some(want), "{iim}");
    }
    assert_eq!(shoot.list(&jpg, "IPTC:Keywords"), vec!["smörgåsbord", "氷"]);
    assert_eq!(
        shoot.text(&jpg, "IPTC:CodedCharacterSet").as_deref(),
        Some("UTF8"),
        "IIM is flagged UTF-8, not read back as Latin-1"
    );
}

#[test]
fn a_multi_line_caption_is_one_value_and_does_not_disturb_the_other_fields() {
    let Some(shoot) = Shoot::new("lines", &["A.JPG"]) else {
        return;
    };
    let fields = serde_json::json!({
        "caption": "first line\nsecond line\n\nfourth after a blank",
        "credit": "SHL",
        "byline": "Anna Foto",
    });
    shoot.apply(&fields.to_string());
    assert_eq!(shoot.drain(), 0);

    let jpg = shoot.path("A.JPG");
    let want = "first line\nsecond line\n\nfourth after a blank";
    assert_eq!(
        shoot.text(&jpg, "XMP-dc:Description").as_deref(),
        Some(want)
    );
    assert_eq!(
        shoot.text(&jpg, "IPTC:Caption-Abstract").as_deref(),
        Some(want)
    );
    // An unescaped newline would have split the argfile line and turned
    // the tail into stray arguments; the fields after it must survive.
    assert_eq!(shoot.text(&jpg, "IPTC:Credit").as_deref(), Some("SHL"));
    assert_eq!(
        shoot.text(&jpg, "IPTC:By-line").as_deref(),
        Some("Anna Foto")
    );
    let leftovers = std::fs::read_dir(std::env::temp_dir())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with(&format!("editing-exif-{}-", std::process::id()))
        })
        .count();
    assert_eq!(leftovers, 0, "out-of-band value files are deleted");
}
