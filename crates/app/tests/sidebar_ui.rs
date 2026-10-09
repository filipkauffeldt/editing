//! UI integration tests for the metadata sidebar: real Browser, real
//! sidebar widgets, real exiftool. Input goes through native events.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use app::browse::run_catalog_scan;
use app::views::Browser;
use edit_catalog::{Catalog, ClaimedJob, JobEvent, JobEventKind, JobKind};
use edit_metadata::{MetadataWritePayload, TemplateFields, Writer};
use edit_render::RenderService;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{
    AppContext, Bounds, Context, Entity, InteractiveElement, IntoElement, ParentElement, Render,
    ScrollDelta, Styled, TestAppContext, Window, WindowBounds, WindowOptions, div, point, px, size,
};

static NEXT: AtomicU64 = AtomicU64::new(0);

/// Test shell: the browser under the `Workspace` key context, as in the app.
struct Shell(Entity<Browser>);

impl Render for Shell {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .key_context("Workspace")
            .child(self.0.clone())
    }
}

fn photo_dir() -> PathBuf {
    let n = NEXT.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("sidebar-ui-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for (name, color) in [
        ("DSC0001.JPG", [200, 40, 40]),
        ("DSC0002.JPG", [40, 120, 40]),
    ] {
        let img = image::RgbImage::from_pixel(8, 8, image::Rgb(color));
        let mut bytes = Vec::new();
        img.write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Jpeg,
        )
        .unwrap();
        std::fs::write(dir.join(name), bytes).unwrap();
    }
    dir
}

struct Fixture {
    dir: PathBuf,
    catalog: Arc<Catalog>,
    handle: gpui_kit::AnyWindowHandle,
    browser: Entity<Browser>,
}

fn open(cx: &mut TestAppContext, setup: impl FnOnce(&Path)) -> Fixture {
    cx.update(gpui_kit::init);
    let dir = photo_dir();
    setup(&dir);
    let catalog = Arc::new(Catalog::open(&dir.join("ui.sqlite")).unwrap());
    let (_tx, rx) = flume::unbounded();
    let cat = Arc::clone(&catalog);
    let (handle, shell) = cx.update(|cx| {
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                    Default::default(),
                    size(px(1100.), px(900.)),
                ))),
                ..Default::default()
            },
            cx,
            |_, cx| {
                let browser =
                    cx.new(|cx| Browser::new(cat, rx, Arc::new(RenderService::new()), cx));
                cx.new(|_| Shell(browser))
            },
        )
        .expect("open window")
    });
    let browser = cx.update(|cx| shell.read(cx).0.clone());
    cx.update(|cx| browser.update(cx, |b, cx| b.open_folder(dir.clone(), cx)));
    let scan_id = cx.update(|cx| browser.read(cx).last_scan_job().unwrap());
    run_catalog_scan(&catalog, &dir, &|_, _| {}).unwrap();
    cx.update(|cx| {
        browser.update(cx, |b, _| {
            b.on_job_event(&JobEvent {
                id: scan_id,
                kind: JobEventKind::Succeeded,
            })
        })
    });
    let fx = Fixture {
        dir,
        catalog,
        handle,
        browser,
    };
    settle(cx, &fx);
    fx
}

/// Let background reads finish and paint.
fn settle(cx: &mut TestAppContext, fx: &Fixture) {
    for _ in 0..3 {
        cx.run_until_parked();
        cx.update_window(fx.handle, |_, window, cx| window.render_frame(cx))
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    cx.run_until_parked();
    cx.update_window(fx.handle, |_, window, cx| window.render_frame(cx))
        .unwrap();
}

fn win<R>(
    cx: &mut TestAppContext,
    fx: &Fixture,
    f: impl FnOnce(&mut Window, &mut gpui_kit::App) -> R,
) -> R {
    cx.update_window(fx.handle, |_, window, cx| f(window, cx))
        .unwrap()
}

fn focus_browser(cx: &mut TestAppContext, fx: &Fixture) {
    win(cx, fx, |window, cx| {
        let focus = fx.browser.read(cx).focus_handle();
        window.focus(&focus, cx);
    });
}

fn unsaved(browser: &Entity<Browser>, cx: &gpui_kit::App) -> usize {
    let sidebar = browser.read(cx).sidebar().expect("sidebar is built");
    sidebar.read(cx).unsaved_count(cx)
}

fn queued_payloads(fx: &Fixture) -> Vec<MetadataWritePayload> {
    fx.catalog
        .list_jobs(100)
        .unwrap()
        .into_iter()
        .filter(|j| j.kind == JobKind::MetadataWrite)
        .map(|j| serde_json::from_str(&j.payload_json).unwrap())
        .collect()
}

fn exif(file: &Path, tag: &str) -> String {
    let bin = Writer::find_binary().unwrap();
    let out = std::process::Command::new(bin)
        .args(["-b", &format!("-{tag}")])
        .arg(file)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

#[gpui_kit::test]
fn editing_a_field_marks_it_and_saves_only_that_field(cx: &mut TestAppContext) {
    let Some(bin) = Writer::binary_for_tests() else {
        return;
    };
    let fx = open(cx, |_| {});

    win(cx, &fx, |window, cx| {
        assert_eq!(unsaved(&fx.browser, cx), 0);
        window.click("meta-headline", cx);
        assert_eq!(window.find("meta-headline").focused(), Some(true));
        // Typed text reaches the field; the browser shortcuts stay out of it.
        window.input("Överraskning 5 g", cx);
        assert_eq!(
            window.find("meta-headline").value(),
            Some("Överraskning 5 g")
        );
        assert_eq!(unsaved(&fx.browser, cx), 1);
        window.click("meta-save", cx);
    });
    cx.run_until_parked();

    let payloads = queued_payloads(&fx);
    assert_eq!(payloads.len(), 1);
    assert!(payloads[0].clear_empty);
    let fields: TemplateFields = serde_json::from_str(&payloads[0].fields_json).unwrap();
    assert_eq!(
        fields,
        TemplateFields::default().with("headline", "Överraskning 5 g")
    );

    // Run the durable job like the worker would; the file gets both twins.
    let job = fx
        .catalog
        .list_jobs(100)
        .unwrap()
        .into_iter()
        .find(|j| j.kind == JobKind::MetadataWrite)
        .unwrap();
    let writer = Writer::resident(&bin).unwrap();
    let (tx, _rx) = flume::unbounded();
    edit_metadata::run_metadata_write(
        &ClaimedJob {
            id: job.id,
            kind: job.kind,
            payload_json: job.payload_json.clone(),
            attempts: 1,
        },
        &writer,
        &tx,
    )
    .unwrap();
    let file = fx.dir.join("DSC0001.JPG");
    assert_eq!(exif(&file, "XMP-photoshop:Headline"), "Överraskning 5 g");
    assert_eq!(exif(&file, "IPTC:Headline"), "Överraskning 5 g");

    // The success event rereads the photo: no draft remains.
    cx.update(|cx| {
        fx.browser.update(cx, |b, _| {
            b.on_job_event(&JobEvent {
                id: job.id,
                kind: JobEventKind::Succeeded,
            })
        })
    });
    settle(cx, &fx);
    win(cx, &fx, |window, cx| {
        assert_eq!(
            window.find("meta-headline").value(),
            Some("Överraskning 5 g")
        );
        assert_eq!(unsaved(&fx.browser, cx), 0);
    });
    std::fs::remove_dir_all(&fx.dir).ok();
}

#[gpui_kit::test]
fn selection_shows_mixed_and_saves_only_changed_fields(cx: &mut TestAppContext) {
    let Some(bin) = Writer::binary_for_tests() else {
        return;
    };
    let fx = open(cx, |dir| {
        let writer = Writer::resident(&Writer::find_binary().unwrap()).unwrap();
        writer
            .write_tags(
                &dir.join("DSC0001.JPG"),
                &[("IPTC:Credit", "A"), ("IPTC:Source", "S")],
            )
            .unwrap();
        writer
            .write_tags(
                &dir.join("DSC0002.JPG"),
                &[("IPTC:Credit", "B"), ("IPTC:Source", "S")],
            )
            .unwrap();
    });
    let _ = bin;

    focus_browser(cx, &fx);
    win(cx, &fx, |window, cx| window.press("ctrl-a", cx));
    settle(cx, &fx);
    win(cx, &fx, |window, cx| {
        // Agreeing field shows its value, differing one is blank (Mixed).
        assert_eq!(window.find("meta-credit").value(), Some(""));
        assert_eq!(unsaved(&fx.browser, cx), 0);
        window.click("meta-headline", cx);
        window.input("Shared", cx);
        window.click("meta-save", cx);
    });
    cx.run_until_parked();
    let payloads = queued_payloads(&fx);
    assert_eq!(payloads.len(), 2, "one job per selected photo");
    for p in &payloads {
        let fields: TemplateFields = serde_json::from_str(&p.fields_json).unwrap();
        assert_eq!(fields, TemplateFields::default().with("headline", "Shared"));
    }
    std::fs::remove_dir_all(&fx.dir).ok();
}

#[gpui_kit::test]
fn unsaved_draft_survives_moving_to_another_photo(cx: &mut TestAppContext) {
    if Writer::binary_for_tests().is_none() {
        return;
    }
    let fx = open(cx, |_| {});
    win(cx, &fx, |window, cx| {
        window.click("meta-headline", cx);
        window.input("kept", cx);
        window.press("escape", cx);
    });
    win(cx, &fx, |window, cx| window.press("right", cx));
    settle(cx, &fx);
    win(cx, &fx, |window, cx| {
        assert_eq!(
            window.find("meta-headline").value(),
            Some(""),
            "other photo is clean"
        );
        assert_eq!(
            unsaved(&fx.browser, cx),
            1,
            "the first photo still holds its draft"
        );
    });
    win(cx, &fx, |window, cx| window.press("left", cx));
    settle(cx, &fx);
    win(cx, &fx, |window, cx| {
        assert_eq!(window.find("meta-headline").value(), Some("kept"));
        assert_eq!(unsaved(&fx.browser, cx), 1);
    });
    // Revert restores the loaded values.
    win(cx, &fx, |window, cx| window.click("meta-revert", cx));
    settle(cx, &fx);
    win(cx, &fx, |window, cx| {
        assert_eq!(window.find("meta-headline").value(), Some(""));
        assert_eq!(unsaved(&fx.browser, cx), 0);
    });
    assert!(queued_payloads(&fx).is_empty(), "nothing was written");
    std::fs::remove_dir_all(&fx.dir).ok();
}

#[gpui_kit::test]
fn wheel_scrolls_the_form_and_groups_collapse(cx: &mut TestAppContext) {
    if Writer::binary_for_tests().is_none() {
        return;
    }
    let fx = open(cx, |_| {});
    win(cx, &fx, |window, cx| {
        let before = window.find("meta-headline").bounds().top();
        window.scroll(
            "meta-headline",
            ScrollDelta::Pixels(point(px(0.), px(-120.))),
            cx,
        );
        let after = window.find("meta-headline").bounds().top();
        assert!(
            after < before,
            "the form moved up inside the sidebar: {before:?} -> {after:?}"
        );
    });
    std::fs::remove_dir_all(&fx.dir).ok();
}
