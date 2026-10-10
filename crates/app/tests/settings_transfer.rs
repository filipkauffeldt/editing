//! Copy, paste, apply previous and sync of develop settings through the
//! real window: the group dialog, the keys and the stored edits.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use app::browse::run_catalog_scan;
use app::views::Browser;
use edit_catalog::{Catalog, JobEvent, JobEventKind};
use edit_render::RenderService;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{AppContext, Bounds, Entity, TestAppContext, WindowBounds, WindowOptions, px, size};

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

fn photo_dir() -> PathBuf {
    let n = NEXT_DIR.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("edit-transfer-{}-{n}", std::process::id()));
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
    for name in ["DSC0001.JPG", "DSC0002.JPG", "DSC0003.JPG"] {
        std::fs::write(dir.join(name), &bytes).unwrap();
    }
    dir
}

struct Rig {
    handle: gpui_kit::AnyWindowHandle,
    browser: Entity<Browser>,
    catalog: Arc<Catalog>,
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

/// A stored edit's value for one control.
fn stored(rig: &Rig, item: i64, id: &str) -> f64 {
    let edit = rig.catalog.get_edit(item).unwrap();
    let Some(edit) = edit else {
        return lightcraft_develop::controls::get(&Default::default(), id).unwrap();
    };
    let parsed =
        app::develop::LoadedEdit::parse(&edit.develop_json, edit.crop_json.as_deref()).unwrap();
    lightcraft_develop::controls::get(&parsed.develop, id).unwrap()
}

fn items(cx: &mut TestAppContext, rig: &Rig) -> Vec<i64> {
    cx.update(|cx| {
        rig.browser
            .read(cx)
            .visible()
            .iter()
            .map(|item| item.item_id)
            .collect()
    })
}

fn keys(cx: &mut TestAppContext, rig: &Rig, keys: &str) {
    cx.simulate_keystrokes(rig.handle, keys);
    settle(cx, rig, |_| true);
}

fn status(cx: &mut TestAppContext, rig: &Rig) -> String {
    cx.update(|cx| rig.browser.read(cx).develop_status().to_owned())
}

#[gpui_kit::test]
fn copy_opens_a_group_dialog_that_escape_closes_and_enter_confirms(cx: &mut TestAppContext) {
    let rig = open_single(cx);
    keys(cx, &rig, "ctrl-c");
    assert!(exists(cx, &rig, "transfer-check-all"));
    assert!(exists(cx, &rig, "transfer-group-tone"));
    assert!(exists(cx, &rig, "transfer-group-crop"));
    keys(cx, &rig, "escape");
    assert!(!exists(cx, &rig, "transfer-check-all"));

    keys(cx, &rig, "ctrl-c");
    click(cx, &rig, "transfer-check-none");
    click(cx, &rig, "transfer-group-presence");
    keys(cx, &rig, "enter");
    assert!(!exists(cx, &rig, "transfer-check-all"));
    assert_eq!(status(cx, &rig), "Copied 1 groups");
}

#[gpui_kit::test]
fn paste_applies_only_the_checked_groups_to_the_next_photo(cx: &mut TestAppContext) {
    let rig = open_single(cx);
    set(cx, &rig, "light.exposure", 0.5);
    set(cx, &rig, "color.vibrance", 30.0);
    keys(cx, &rig, "ctrl-c");
    click(cx, &rig, "transfer-group-presence");
    keys(cx, &rig, "enter");

    keys(cx, &rig, "right");
    settle(cx, &rig, |b| b.develop_owner().is_some());
    keys(cx, &rig, "ctrl-v");
    assert_eq!(value(cx, &rig, "light.exposure"), 0.5);
    assert_eq!(value(cx, &rig, "color.vibrance"), 0.0);
    let ids = items(cx, &rig);
    // Committed to the catalog and the sidecar path like any edit.
    assert_eq!(stored(&rig, ids[1], "light.exposure"), 0.5);
}

#[gpui_kit::test]
fn paste_reaches_the_whole_selection_and_apply_previous_repeats_it(cx: &mut TestAppContext) {
    let rig = open_single(cx);
    set(cx, &rig, "light.exposure", 0.5);
    keys(cx, &rig, "ctrl-c");
    keys(cx, &rig, "enter");
    let ids = items(cx, &rig);

    keys(cx, &rig, "ctrl-a");
    keys(cx, &rig, "ctrl-v");
    settle(cx, &rig, |_| stored(&rig, ids[2], "light.exposure") == 0.5);
    assert_eq!(stored(&rig, ids[1], "light.exposure"), 0.5);
    assert_eq!(stored(&rig, ids[2], "light.exposure"), 0.5);

    // Change the third photo, then repeat the last paste on it alone.
    keys(cx, &rig, "g");
    keys(cx, &rig, "g");
    cx.update(|cx| {
        rig.browser
            .update(cx, |b, cx| b.apply_preset("builtin:Punchy", cx))
    });
    settle(cx, &rig, |_| true);
    keys(cx, &rig, "ctrl-alt-v");
    assert_eq!(value(cx, &rig, "light.exposure"), 0.5);
}

#[gpui_kit::test]
fn sync_asks_for_groups_and_applies_to_the_other_selected_photos(cx: &mut TestAppContext) {
    let rig = open_single(cx);
    set(cx, &rig, "light.contrast", 20.0);
    set(cx, &rig, "color.vibrance", 30.0);
    // One photo selected: nothing to sync to.
    keys(cx, &rig, "ctrl-shift-s");
    assert!(!exists(cx, &rig, "transfer-check-all"));
    assert_eq!(status(cx, &rig), "Select more than one photo to sync");

    keys(cx, &rig, "ctrl-a");
    keys(cx, &rig, "ctrl-shift-s");
    assert!(exists(cx, &rig, "transfer-check-all"));
    click(cx, &rig, "transfer-group-presence");
    keys(cx, &rig, "enter");
    let ids = items(cx, &rig);
    settle(cx, &rig, |_| stored(&rig, ids[2], "light.contrast") == 20.0);
    assert_eq!(stored(&rig, ids[1], "light.contrast"), 20.0);
    assert_eq!(stored(&rig, ids[2], "light.contrast"), 20.0);
    assert_eq!(stored(&rig, ids[2], "color.vibrance"), 0.0);
}

#[gpui_kit::test]
fn a_run_can_be_stopped_before_it_touches_a_photo(cx: &mut TestAppContext) {
    let rig = open_single(cx);
    set(cx, &rig, "light.exposure", 0.5);
    keys(cx, &rig, "ctrl-c");
    keys(cx, &rig, "enter");
    keys(cx, &rig, "ctrl-a");
    let ids = items(cx, &rig);
    cx.update(|cx| {
        rig.browser.update(cx, |b, cx| {
            b.paste_settings(cx);
            assert!(b.cancel_transfer(cx));
        })
    });
    cx.run_until_parked();
    assert_eq!(status(cx, &rig), "Stopped after 0 of 2 photos");
    assert_eq!(stored(&rig, ids[1], "light.exposure"), 0.0);
}

#[gpui_kit::test]
fn the_preset_menu_lists_the_transfer_commands(cx: &mut TestAppContext) {
    let rig = open_single(cx);
    click(cx, &rig, "develop-preset-menu-button");
    for id in [
        "develop-copy-settings",
        "develop-paste-settings",
        "develop-apply-previous",
        "develop-sync-settings",
    ] {
        assert!(exists(cx, &rig, id), "{id}");
    }
    // Nothing is running, so there is nothing to stop.
    assert!(!exists(cx, &rig, "develop-stop-applying"));
    keys(cx, &rig, "escape");
    assert!(!exists(cx, &rig, "develop-copy-settings"));
}

#[gpui_kit::test]
fn a_photo_left_open_in_the_buffer_still_receives_a_sync_from_the_grid(cx: &mut TestAppContext) {
    let rig = open_single(cx);
    let ids = items(cx, &rig);
    // The second photo owns the buffer, then the grid shows the first.
    keys(cx, &rig, "right");
    settle(cx, &rig, |b| b.develop_owner() == Some(ids[1]));
    keys(cx, &rig, "g");
    keys(cx, &rig, "left");
    set_stored_vibrance(&rig, ids[0], 30.0);
    keys(cx, &rig, "ctrl-a");
    keys(cx, &rig, "ctrl-shift-s");
    click(cx, &rig, "transfer-check-all");
    click(cx, &rig, "transfer-confirm");
    settle(cx, &rig, |_| stored(&rig, ids[1], "color.vibrance") == 30.0);
    assert_eq!(stored(&rig, ids[2], "color.vibrance"), 30.0);
}

fn set_stored_vibrance(rig: &Rig, item: i64, value: f64) {
    let mut settings = lightcraft_develop::DevelopSettings::default();
    lightcraft_develop::controls::set(&mut settings, "color.vibrance", value);
    let json = app::develop::edit_json(&settings, None).unwrap();
    rig.catalog
        .upsert_edit(item, &json.develop, json.crop.as_deref())
        .unwrap();
}
