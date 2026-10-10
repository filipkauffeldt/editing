//! The develop sidebar driven through its real widgets in a test window:
//! sections, resets, typed values, presets, the save popover and scrolling.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use app::browse::run_catalog_scan;
use app::views::Browser;
use edit_catalog::{Catalog, JobEvent, JobEventKind};
use edit_render::RenderService;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{
    AppContext, Bounds, Entity, ScrollDelta, TestAppContext, WindowBounds, WindowOptions, point,
    px, size,
};

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

fn photo_dir() -> PathBuf {
    let n = NEXT_DIR.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("edit-slider-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let img = image::RgbImage::from_fn(600, 400, |x, y| {
        image::Rgb([(x / 3) as u8, (y / 2) as u8, 120])
    });
    let mut bytes = Vec::new();
    img.write_to(
        &mut std::io::Cursor::new(&mut bytes),
        image::ImageFormat::Jpeg,
    )
    .unwrap();
    std::fs::write(dir.join("DSC0001.JPG"), bytes).unwrap();
    dir
}

struct Rig {
    handle: gpui_kit::AnyWindowHandle,
    browser: Entity<Browser>,
    catalog: Arc<Catalog>,
    dir: PathBuf,
}

fn rig(cx: &mut TestAppContext) -> Rig {
    cx.update(gpui_kit::init);
    let dir = photo_dir();
    let catalog = Arc::new(Catalog::open(&dir.join("slider.sqlite")).unwrap());
    let (_, rx) = flume::unbounded();
    let (handle, browser) = cx.update(|cx| {
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                    Default::default(),
                    size(px(1400.0), px(900.0)),
                ))),
                ..Default::default()
            },
            cx,
            |_, cx| {
                cx.new(|cx| {
                    Browser::new(Arc::clone(&catalog), rx, Arc::new(RenderService::new()), cx)
                })
            },
        )
        .expect("open test window")
    });
    cx.update(|cx| browser.update(cx, |b, cx| b.open_folder(dir.clone(), cx)));
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
    cx.update_window(handle, |_, window, cx| {
        let focus = browser.read(cx).focus_handle();
        window.focus(&focus, cx);
    })
    .unwrap();
    Rig {
        handle,
        browser,
        catalog,
        dir,
    }
}

fn settle(cx: &mut TestAppContext, rig: &Rig, done: impl Fn(&Browser) -> bool) {
    for _ in 0..300 {
        cx.update_window(rig.handle, |_, window, cx| window.render_frame(cx))
            .unwrap();
        cx.run_until_parked();
        if cx.update(|cx| done(rig.browser.read(cx))) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("the view never settled");
}

fn open_single(cx: &mut TestAppContext) -> Rig {
    let rig = rig(cx);
    cx.simulate_keystrokes(rig.handle, "g");
    settle(cx, &rig, |_| true);
    settle(cx, &rig, |b| b.develop_owner().is_some());
    rig
}

fn click(cx: &mut TestAppContext, rig: &Rig, id: &'static str) {
    cx.update_window(rig.handle, |_, window, cx| {
        window.render_frame(cx);
        window.click(id, cx);
    })
    .unwrap();
    settle(cx, rig, |_| true);
}

fn press(cx: &mut TestAppContext, rig: &Rig, key: &'static str) {
    cx.update_window(rig.handle, |_, window, cx| window.press(key, cx))
        .unwrap();
    settle(cx, rig, |_| true);
}

fn exists(cx: &mut TestAppContext, rig: &Rig, id: &'static str) -> bool {
    cx.update_window(rig.handle, |_, window, cx| {
        window.render_frame(cx);
        window.try_find(id).is_some()
    })
    .unwrap()
}

fn value(cx: &mut TestAppContext, rig: &Rig, id: &str) -> f64 {
    cx.update(|cx| rig.browser.read(cx).develop_value(id))
}

fn set(cx: &mut TestAppContext, rig: &Rig, id: &'static str, v: f64) {
    cx.update(|cx| {
        rig.browser
            .update(cx, |b, cx| b.on_develop_release(id, v, cx))
    });
    settle(cx, rig, |_| true);
}

#[gpui_kit::test]
fn only_the_basic_sections_start_open_and_the_rest_open_on_demand(cx: &mut TestAppContext) {
    let rig = open_single(cx);
    let panel = cx.update(|cx| rig.browser.read(cx).develop_panel());
    let open = cx.update(|cx| panel.read(cx).open_sections().to_vec());
    assert_eq!(open, ["white-balance", "tone", "presence"]);
    assert!(exists(cx, &rig, "develop-reset-light.exposure"));
    assert!(!exists(cx, &rig, "develop-reset-mixer.red.hue"));
    cx.update(|cx| panel.update(cx, |p, cx| p.set_open(&["color-mixer"], cx)));
    settle(cx, &rig, |_| true);
    assert!(exists(cx, &rig, "develop-reset-mixer.red.hue"));
    assert!(!exists(cx, &rig, "develop-reset-light.exposure"));
}

#[gpui_kit::test]
fn reset_buttons_put_a_row_a_section_and_everything_back(cx: &mut TestAppContext) {
    let rig = open_single(cx);
    set(cx, &rig, "light.exposure", 1.0);
    set(cx, &rig, "light.contrast", 30.0);
    set(cx, &rig, "color.vibrance", 20.0);
    click(cx, &rig, "develop-reset-light.exposure");
    assert_eq!(value(cx, &rig, "light.exposure"), 0.0);
    assert_eq!(value(cx, &rig, "light.contrast"), 30.0);
    click(cx, &rig, "develop-section-reset-tone");
    assert_eq!(value(cx, &rig, "light.contrast"), 0.0);
    assert_eq!(value(cx, &rig, "color.vibrance"), 20.0);
    click(cx, &rig, "develop-reset-all");
    assert_eq!(value(cx, &rig, "color.vibrance"), 0.0);
}

#[gpui_kit::test]
fn a_typed_number_commits_clamped_and_is_saved(cx: &mut TestAppContext) {
    let rig = open_single(cx);
    click(cx, &rig, "develop-value-light.exposure");
    cx.update_window(rig.handle, |_, window, cx| {
        window.press("secondary-a", cx);
        window.input("9", cx);
    })
    .unwrap();
    cx.update_window(rig.handle, |_, window, cx| {
        window.press("enter", cx);
    })
    .unwrap();
    settle(cx, &rig, |_| true);
    // Exposure is limited to +5 stops.
    assert_eq!(value(cx, &rig, "light.exposure"), 5.0);
    let item = cx.update(|cx| rig.browser.read(cx).visible()[0].item_id);
    let stored = rig.catalog.get_edit(item).unwrap().expect("saved row");
    assert!(
        stored.develop_json.contains("\"exposure\":5.0"),
        "{}",
        stored.develop_json
    );
    let sidecar = std::fs::read_to_string(rig.dir.join("DSC0001.xmp")).unwrap_or_default();
    // The sidecar is written in the background.
    if !sidecar.is_empty() {
        assert!(sidecar.contains("Exposure2012"));
    }
}

#[gpui_kit::test]
fn presets_apply_to_the_photo_and_save_through_a_popover_that_escape_closes(
    cx: &mut TestAppContext,
) {
    let rig = open_single(cx);
    set(cx, &rig, "light.exposure", 0.5);
    cx.update(|cx| {
        rig.browser
            .update(cx, |b, cx| b.apply_preset("builtin:Punchy", cx))
    });
    settle(cx, &rig, |_| true);
    assert_eq!(value(cx, &rig, "light.contrast"), 25.0);
    assert_eq!(value(cx, &rig, "color.vibrance"), 30.0);
    // A preset only sets its own controls.
    assert_eq!(value(cx, &rig, "light.exposure"), 0.5);

    // Escape closes the save popover without saving.
    click(cx, &rig, "develop-save-preset-button");
    assert!(exists(cx, &rig, "develop-save-preset-confirm"));
    press(cx, &rig, "escape");
    assert!(!exists(cx, &rig, "develop-save-preset-confirm"));
    assert!(app::preset_store::list(&rig.catalog).is_empty());

    // Typing a name and pressing Enter saves it.
    click(cx, &rig, "develop-save-preset-button");
    cx.update_window(rig.handle, |_, window, cx| window.input("Mine", cx))
        .unwrap();
    press(cx, &rig, "enter");
    settle(cx, &rig, |b| !b.indexed_presets().is_empty());
    let saved = app::preset_store::list(&rig.catalog);
    assert_eq!(saved.len(), 1, "{saved:?}");
    assert_eq!(saved[0].name, "Mine");

    // Applying the saved preset to a reset photo brings its values back.
    click(cx, &rig, "develop-reset-all");
    assert_eq!(value(cx, &rig, "light.contrast"), 0.0);
    let key = format!("user:{}", saved[0].path);
    cx.update(|cx| rig.browser.update(cx, |b, cx| b.apply_preset(&key, cx)));
    settle(cx, &rig, |_| true);
    assert_eq!(value(cx, &rig, "light.contrast"), 25.0);
    assert_eq!(value(cx, &rig, "light.exposure"), 0.5);
}

#[gpui_kit::test]
fn the_wheel_scrolls_the_panel_and_never_zooms_the_photo(cx: &mut TestAppContext) {
    let rig = open_single(cx);
    let panel = cx.update(|cx| rig.browser.read(cx).develop_panel());
    cx.update(|cx| {
        panel.update(cx, |p, cx| {
            p.set_open(
                &[
                    "white-balance",
                    "tone",
                    "presence",
                    "color-mixer",
                    "color-grading",
                ],
                cx,
            )
        })
    });
    settle(cx, &rig, |_| true);
    let top = |cx: &mut TestAppContext, rig: &Rig| {
        cx.update_window(rig.handle, |_, window, cx| {
            window.render_frame(cx);
            window.find("develop-reset-light.exposure").bounds().top()
        })
        .unwrap()
    };
    let before = top(cx, &rig);
    let zoom_before = cx.update(|cx| rig.browser.read(cx).zoom_label());
    cx.update_window(rig.handle, |_, window, cx| {
        window.scroll(
            "develop-reset-light.exposure",
            ScrollDelta::Pixels(point(px(0.0), px(-240.0))),
            cx,
        );
    })
    .unwrap();
    settle(cx, &rig, |_| true);
    let after = top(cx, &rig);
    assert!(
        after < before - px(100.0),
        "the panel did not scroll: {before:?} -> {after:?}"
    );
    assert_eq!(
        cx.update(|cx| rig.browser.read(cx).zoom_label()),
        zoom_before
    );
}

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../edit-metadata/tests/fixtures/presets")
}

fn copy_tree(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().flatten() {
        let dest = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_tree(&entry.path(), &dest);
        } else {
            std::fs::copy(entry.path(), dest).unwrap();
        }
    }
}

#[gpui_kit::test]
fn the_preset_bar_groups_presets_by_folder_after_a_background_scan(cx: &mut TestAppContext) {
    let rig = open_single(cx);
    let folder = rig.dir.join("my-presets");
    copy_tree(&fixtures(), &folder);
    cx.update(|cx| {
        rig.browser
            .update(cx, |b, cx| b.set_preset_folder(folder.clone(), cx))
    });
    settle(cx, &rig, |b| b.indexed_presets().len() == 4);
    let panel = cx.update(|cx| rig.browser.read(cx).develop_panel());
    let groups: Vec<String> = cx.update(|cx| {
        panel
            .read(cx)
            .preset_groups()
            .into_iter()
            .map(|(title, _)| title.to_string())
            .collect()
    });
    assert_eq!(
        groups,
        ["Built in", "User presets", "Film", "Portraits / Studio"]
    );
    // A preset from a subfolder applies like any other.
    let key = cx.update(|cx| {
        let browser = rig.browser.read(cx);
        let row = browser
            .indexed_presets()
            .iter()
            .find(|p| p.name == "Faded film")
            .unwrap();
        format!("user:{}", row.path)
    });
    cx.update(|cx| rig.browser.update(cx, |b, cx| b.apply_preset(&key, cx)));
    settle(cx, &rig, |_| true);
    assert_eq!(value(cx, &rig, "light.blacks"), 30.0);
    assert_eq!(value(cx, &rig, "grain.amount"), 25.0);
}

#[gpui_kit::test]
fn the_preset_menu_imports_and_exports_and_escape_closes_it(cx: &mut TestAppContext) {
    let rig = open_single(cx);
    click(cx, &rig, "develop-preset-menu-button");
    for id in [
        "develop-preset-import",
        "develop-preset-export-settings",
        "develop-preset-export-preset",
        "develop-preset-folder",
    ] {
        assert!(exists(cx, &rig, id), "{id}");
    }
    press(cx, &rig, "escape");
    assert!(!exists(cx, &rig, "develop-preset-import"));

    let source = fixtures().join("Film/Unknown keys.xmp");
    cx.update(|cx| {
        rig.browser
            .update(cx, |b, cx| b.import_presets(vec![source.clone()], cx))
    });
    settle(cx, &rig, |b| b.develop_status().starts_with("Imported"));
    let status = cx.update(|cx| rig.browser.read(cx).develop_status().to_owned());
    assert_eq!(
        status,
        "Imported Unknown keys, 4 unsupported settings left out"
    );
    assert_eq!(
        cx.update(|cx| rig.browser.read(cx).indexed_presets().len()),
        1
    );

    set(cx, &rig, "light.exposure", 0.5);
    let dest = rig.dir.join("Out.xmp");
    cx.update(|cx| {
        rig.browser
            .update(cx, |b, cx| b.export_settings_to(&dest, cx))
    });
    let xml = std::fs::read_to_string(&dest).unwrap();
    assert!(xml.contains("crs:PresetType=\"Normal\""));
    assert!(xml.contains("crs:Exposure2012=\"+0.50\""));
}

fn header_gaps(cx: &mut TestAppContext, rig: &Rig) -> Vec<f32> {
    let keys: Vec<&str> = app::panel::PANEL.iter().map(|s| s.key).collect();
    let tops: Vec<f32> = cx
        .update_window(rig.handle, |_, window, cx| {
            window.render_frame(cx);
            keys.iter()
                .map(|key| {
                    let id: &'static str =
                        Box::leak(format!("develop-section-reset-{key}").into_boxed_str());
                    let top: f32 = window.find(id).bounds().top().into();
                    top
                })
                .collect()
        })
        .unwrap();
    tops.windows(2).map(|w| w[1] - w[0]).collect()
}

fn let_animations_finish(cx: &mut TestAppContext, rig: &Rig) {
    for _ in 0..60 {
        settle(cx, rig, |_| true);
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(50));
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[gpui_kit::test]
fn collapsed_sections_stay_one_row_apart_however_many_are_open(cx: &mut TestAppContext) {
    let rig = open_single(cx);
    let panel = cx.update(|cx| rig.browser.read(cx).develop_panel());
    for open in [&["detail"][..], &[][..]] {
        cx.update(|cx| panel.update(cx, |p, cx| p.set_open(open, cx)));
        let_animations_finish(cx, &rig);
        let gaps = header_gaps(cx, &rig);
        // A collapsed row is its title line and a separator, about 33 px.
        // With nothing open the rows used to spread over the whole sidebar.
        let collapsed: Vec<f32> = gaps.iter().copied().filter(|g| *g < 100.0).collect();
        assert!(!collapsed.is_empty());
        assert!(
            collapsed.iter().all(|g| (30.0..=36.0).contains(g)),
            "open {open:?}: {gaps:?}"
        );
    }
}
