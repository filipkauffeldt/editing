//! Live contact sheet during ingest (plan sections 2.2, PLAN core loop step
//! 2): photos appear in the grid as each copy is verified and cataloged,
//! with their thumbnail jobs queued, before the ingest finishes.

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use app::ingest_job::{IngestStart, start_ingest};
use app::views::Browser;
use edit_catalog::{Catalog, JobEvent, JobEventKind, JobKind};
use edit_ingest::{FileMetadata, IngestCopyPayload, run_ingest_copy_with};
use edit_render::RenderService;
use gpui_kit::AppContext;

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

fn scratch() -> PathBuf {
    let n = NEXT_DIR.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("edit-ingest-live-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn card(base: &std::path::Path, names: &[&str]) -> PathBuf {
    let dcim = base.join("card/DCIM/100NIKON");
    std::fs::create_dir_all(&dcim).unwrap();
    for name in names {
        std::fs::write(dcim.join(name), format!("bytes of {name}")).unwrap();
    }
    base.join("card")
}

fn open_browser(
    cx: &mut gpui_kit::TestAppContext,
    catalog: Arc<Catalog>,
) -> gpui_kit::Entity<Browser> {
    let (_, rx) = flume::unbounded();
    cx.update(|cx| {
        cx.new(|cx| {
            let mut browser = Browser::new(catalog, rx, Arc::new(RenderService::new()), cx);
            browser.set_ingest_refresh_gap(Duration::ZERO);
            browser
        })
    })
}

#[gpui_kit::test]
fn photos_appear_one_by_one_while_the_ingest_runs(cx: &mut gpui_kit::TestAppContext) {
    cx.update(gpui_kit::init);
    let base = scratch();
    let card = card(&base, &["DSC_0001.JPG", "DSC_0002.JPG", "DSC_0003.JPG"]);
    let dest = base.join("Photos");
    let dest_rule = format!("{}/{{yyyy}}-{{mm}}-{{dd}}", dest.display());
    let catalog = Arc::new(Catalog::open(&base.join("catalog.sqlite")).unwrap());
    let browser = open_browser(cx, Arc::clone(&catalog));

    let started = start_ingest(
        &catalog,
        &IngestStart {
            volume_uuid: Some("UUID"),
            volume_label: Some("NIKON Z 6"),
            source_root: &card,
            dest_rule: &dest_rule,
            filename_rule: "",
            backup_dir: "",
            session: "shoot",
            template: None,
        },
    )
    .unwrap();
    let job = catalog.claim_job().unwrap().unwrap();
    assert_eq!(job.id, started.job_id);
    let payload: IngestCopyPayload = serde_json::from_str(&job.payload_json).unwrap();

    // After every file the ingest reports progress; the app's drain loop
    // would hand that event to the browser, so do the same, synchronously.
    let seen: RefCell<Vec<(usize, usize, usize)>> = RefCell::new(Vec::new());
    let cx_cell = RefCell::new(cx.clone());
    let on_progress = |done: usize, total: usize| {
        let cx = cx_cell.borrow_mut();
        let rows = cx.update(|cx| {
            browser.update(cx, |browser, _| {
                browser.on_job_event(&JobEvent {
                    id: job.id,
                    kind: JobEventKind::Progress {
                        done: done as u64,
                        total: total as u64,
                    },
                });
                browser.row_count()
            })
        });
        let thumbs = catalog
            .list_jobs(100)
            .unwrap()
            .iter()
            .filter(|job| job.kind == JobKind::Thumb)
            .count();
        seen.borrow_mut().push((done, rows, thumbs));
    };
    run_ingest_copy_with(
        &catalog,
        &payload,
        |_| FileMetadata::default(),
        &|_| None,
        &on_progress,
    )
    .unwrap();

    // Each landed photo is on the sheet with its thumbnail queued before the
    // next one is copied.
    assert_eq!(
        *seen.borrow(),
        vec![(0, 0, 0), (1, 1, 1), (2, 2, 2), (3, 3, 3)]
    );

    cx.update(|cx| {
        browser.update(cx, |browser, _| {
            browser.on_job_event(&JobEvent {
                id: job.id,
                kind: JobEventKind::Succeeded,
            });
            assert_eq!(browser.row_count(), 3);
            assert!(browser.root_id().is_some());
        })
    });
    std::fs::remove_dir_all(&base).ok();
}

#[gpui_kit::test]
fn an_ingest_switches_the_sheet_once_and_does_not_fight_the_user(
    cx: &mut gpui_kit::TestAppContext,
) {
    cx.update(gpui_kit::init);
    let base = scratch();
    let other = base.join("other");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("OLD_0001.JPG"), b"old").unwrap();
    let card = card(&base, &["DSC_0001.JPG"]);
    let dest = base.join("Photos");
    let catalog = Arc::new(Catalog::open(&base.join("catalog.sqlite")).unwrap());
    let browser = open_browser(cx, Arc::clone(&catalog));

    // A folder is already open.
    let summary = app::browse::run_catalog_scan(&catalog, &other, &|_, _| {}).unwrap();
    cx.update(|cx| {
        browser.update(cx, |browser, cx| browser.open_folder(other.clone(), cx));
        let scan = browser.read(cx).last_scan_job().unwrap();
        browser.update(cx, |browser, _| {
            browser.on_job_event(&JobEvent {
                id: scan,
                kind: JobEventKind::Succeeded,
            })
        });
        assert_eq!(browser.read(cx).root_id(), Some(summary.root_id));
    });

    let started = start_ingest(
        &catalog,
        &IngestStart {
            volume_uuid: None,
            volume_label: None,
            source_root: &card,
            dest_rule: &dest.to_string_lossy(),
            filename_rule: "",
            backup_dir: "",
            session: "shoot",
            template: None,
        },
    )
    .unwrap();
    let job = loop {
        let job = catalog.claim_job().unwrap().unwrap();
        if job.kind == JobKind::IngestCopy {
            break job;
        }
    };
    let payload: IngestCopyPayload = serde_json::from_str(&job.payload_json).unwrap();
    let tx_events = RefCell::new(Vec::new());
    run_ingest_copy_with(
        &catalog,
        &payload,
        |_| FileMetadata::default(),
        &|_| None,
        &|done, total| tx_events.borrow_mut().push((done, total)),
    )
    .unwrap();

    // The first ingest event moves the sheet to the destination...
    cx.update(|cx| {
        browser.update(cx, |browser, _| {
            browser.on_job_event(&JobEvent {
                id: started.job_id,
                kind: JobEventKind::Progress { done: 1, total: 1 },
            });
            assert_ne!(browser.root_id(), Some(summary.root_id));
            assert_eq!(browser.row_count(), 1);
        })
    });
    // ...and a later folder open is not undone by the terminal event.
    cx.update(|cx| {
        browser.update(cx, |browser, cx| browser.open_folder(other.clone(), cx));
        let scan = browser.read(cx).last_scan_job().unwrap();
        browser.update(cx, |browser, _| {
            browser.on_job_event(&JobEvent {
                id: scan,
                kind: JobEventKind::Succeeded,
            });
            browser.on_job_event(&JobEvent {
                id: started.job_id,
                kind: JobEventKind::Succeeded,
            });
            assert_eq!(browser.root_id(), Some(summary.root_id));
        })
    });
    std::fs::remove_dir_all(&base).ok();
}
