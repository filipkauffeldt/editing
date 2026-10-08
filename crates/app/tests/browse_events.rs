//! Headless regression tests for the browse event wiring: opening a folder
//! resolves the catalog root and fills the grid (review B1), and thumb job
//! events flow from payload resolution to per-cell state (review B2).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use app::browse::{MetaBadge, ThumbPayload, ThumbState, list_browse_items, run_catalog_scan};
use app::views::Browser;
use edit_catalog::{Catalog, JobEvent, JobEventKind, JobKind, JobState};
use edit_metadata::{MetadataWritePayload, TemplateFields};
use edit_render::RenderService;
use gpui_kit::AppContext;

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

fn photo_dir() -> PathBuf {
    let n = NEXT_DIR.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("edit-browse-evt-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for (name, color) in [
        ("DSC0001.JPG", [200, 40, 40]),
        ("DSC0002.JPG", [40, 120, 40]),
    ] {
        let img = image::RgbImage::from_pixel(8, 8, image::Rgb(color));
        let mut bytes = Vec::new();
        {
            let mut cursor = std::io::Cursor::new(&mut bytes);
            img.write_to(&mut cursor, image::ImageFormat::Jpeg).unwrap();
        }
        std::fs::write(dir.join(name), bytes).unwrap();
    }
    // A RAW side that completes a half-pair on rescan.
    std::fs::write(dir.join("DSC0001.NEF"), b"fake-raw-bytes").unwrap();
    dir
}

fn open_browser(
    cx: &mut gpui_kit::TestAppContext,
    catalog: Arc<Catalog>,
) -> gpui_kit::Entity<Browser> {
    let (_, rx) = flume::unbounded();
    cx.update(|cx| cx.new(|cx| Browser::new(catalog, rx, Arc::new(RenderService::new()), cx)))
}

#[gpui_kit::test]
fn scan_events_set_root_and_fill_the_grid(cx: &mut gpui_kit::TestAppContext) {
    cx.update(gpui_kit::init);
    let dir = photo_dir();
    let db = dir.join("evt.sqlite");
    let catalog = Arc::new(Catalog::open(&db).unwrap());
    let browser = open_browser(cx, Arc::clone(&catalog));

    // Folder open enqueues the durable job; the root row does not exist yet.
    cx.update(|cx| {
        browser.update(cx, |b, cx| b.open_folder(dir.clone(), cx));
    });
    let scan_id = cx.update(|cx| browser.read(cx).last_scan_job().unwrap());
    assert_eq!(cx.update(|cx| browser.read(cx).row_count()), 0);

    // The worker runs the same scan the handler would.
    let summary = run_catalog_scan(&catalog, &dir, &|_, _| {}).unwrap();
    assert_eq!(
        list_browse_items(&catalog, summary.root_id).unwrap().len(),
        2
    );

    // Progress resolves the root and live-fills the grid; success finishes.
    cx.update(|cx| {
        browser.update(cx, |b, _| {
            b.on_job_event(&JobEvent {
                id: scan_id,
                kind: JobEventKind::Progress { done: 1, total: 3 },
            });
            assert!(b.root_id().is_some());
            assert_eq!(b.row_count(), 2);
            b.on_job_event(&JobEvent {
                id: scan_id,
                kind: JobEventKind::Succeeded,
            });
            assert_eq!(b.row_count(), 2);
        });
    });
    std::fs::remove_dir_all(&dir).ok();
}

#[gpui_kit::test]
fn thumb_events_mark_cells_through_the_job_map(cx: &mut gpui_kit::TestAppContext) {
    cx.update(gpui_kit::init);
    let dir = photo_dir();
    let db = dir.join("evt-thumb.sqlite");
    let catalog = Arc::new(Catalog::open(&db).unwrap());
    let browser = open_browser(cx, Arc::clone(&catalog));

    cx.update(|cx| {
        browser.update(cx, |b, cx| b.open_folder(dir.clone(), cx));
    });
    let scan_id = cx.update(|cx| browser.read(cx).last_scan_job().unwrap());
    run_catalog_scan(&catalog, &dir, &|_, _| {}).unwrap();
    cx.update(|cx| {
        browser.update(cx, |b, _| {
            b.on_job_event(&JobEvent {
                id: scan_id,
                kind: JobEventKind::Succeeded,
            });
        });
    });
    let item_id = cx.update(|cx| {
        browser
            .read(cx)
            .visible()
            .first()
            .map(|row| row.item_id)
            .unwrap()
    });

    // Thumb jobs carry their photo path in the payload; Started resolves the
    // map entry (Running) and Succeeded lands it (Ready).
    let thumb_id = catalog
        .list_jobs(50)
        .unwrap()
        .into_iter()
        .find(|job| {
            job.kind == JobKind::Thumb
                && serde_json::from_str::<ThumbPayload>(&job.payload_json)
                    .is_ok_and(|p| p.path.ends_with("DSC0001.JPG"))
        })
        .map(|job| job.id)
        .unwrap();
    cx.update(|cx| {
        browser.update(cx, |b, _| {
            b.on_job_event(&JobEvent {
                id: thumb_id,
                kind: JobEventKind::Started { attempts: 1 },
            });
            assert_eq!(b.thumb_state_for(item_id), Some(ThumbState::Running));
            b.on_job_event(&JobEvent {
                id: thumb_id,
                kind: JobEventKind::Succeeded,
            });
            assert_eq!(b.thumb_state_for(item_id), Some(ThumbState::Ready));
        });
    });
    std::fs::remove_dir_all(&dir).ok();
}

#[gpui_kit::test]
fn apply_template_queues_the_selection_off_the_ui_thread(cx: &mut gpui_kit::TestAppContext) {
    cx.update(gpui_kit::init);
    let dir = photo_dir();
    let catalog = Arc::new(Catalog::open(&dir.join("evt-meta.sqlite")).unwrap());
    let browser = open_browser(cx, Arc::clone(&catalog));
    cx.update(|cx| {
        browser.update(cx, |b, cx| b.open_folder(dir.clone(), cx));
    });
    let scan_id = cx.update(|cx| browser.read(cx).last_scan_job().unwrap());
    run_catalog_scan(&catalog, &dir, &|_, _| {}).unwrap();
    cx.update(|cx| {
        browser.update(cx, |b, _| {
            b.on_job_event(&JobEvent {
                id: scan_id,
                kind: JobEventKind::Succeeded,
            });
        });
    });

    let fields = TemplateFields {
        credit: Some("SHL".into()),
        ..Default::default()
    };
    cx.update(|cx| {
        browser.update(cx, |b, cx| {
            b.apply_template(fields.clone(), "press", cx);
            // The cells flip at once; the enqueue itself is still pending.
            let item_id = b.selected_items()[0].item_id;
            assert_eq!(b.item_meta(item_id), Some(MetaBadge::Queued));
        });
    });
    cx.run_until_parked();

    let queued: Vec<_> = catalog
        .list_jobs(50)
        .unwrap()
        .into_iter()
        .filter(|job| job.kind == JobKind::MetadataWrite)
        .collect();
    assert_eq!(queued.len(), 1, "the focused cell is the whole selection");
    assert_eq!(queued[0].state, JobState::Queued);
    let payload: MetadataWritePayload = serde_json::from_str(&queued[0].payload_json).unwrap();
    assert_eq!(payload.sequence, 1);
    assert!(payload.captured_at > 0);
    assert!(!payload.paths.is_empty());
    assert_eq!(
        serde_json::from_str::<TemplateFields>(&payload.fields_json).unwrap(),
        fields
    );

    // A worker event that beats any bookkeeping resolves its item from
    // the durable payload and drives the badge.
    cx.update(|cx| {
        browser.update(cx, |b, _| {
            b.on_job_event(&JobEvent {
                id: queued[0].id,
                kind: JobEventKind::Succeeded,
            });
            assert_eq!(b.item_meta(payload.item_id), Some(MetaBadge::Done));
        });
    });
    std::fs::remove_dir_all(&dir).ok();
}
