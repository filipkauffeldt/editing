//! Crop mode driven the way a user drives it: keys and pointer drags on a
//! real `Browser` in a test window, with the render service decoding a real
//! JPEG in the background. Covers entering with `C`, dragging a handle and
//! the frame, an aspect lock, straightening, `Escape` cancelling, and
//! `Enter` applying and saving to the catalog.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use app::browse::run_catalog_scan;
use app::views::Browser;
use edit_catalog::{Catalog, JobEvent, JobEventKind};
use edit_core::crop::{rect_fits, rect_ratio};
use edit_core::{Aspect, Crop};
use edit_render::RenderService;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{
    AppContext, Bounds, Entity, Modifiers, MouseButton, TestAppContext, VisualTestContext,
    WindowBounds, WindowOptions, point, px, size,
};

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

/// A 600x400 JPEG with a gradient, so a render has something to show.
fn photo_dir() -> PathBuf {
    let n = NEXT_DIR.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("edit-crop-ui-{}-{n}", std::process::id()));
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
    item_id: i64,
}

fn rig(cx: &mut TestAppContext) -> Rig {
    cx.update(gpui_kit::init);
    let dir = photo_dir();
    let catalog = Arc::new(Catalog::open(&dir.join("crop-ui.sqlite")).unwrap());
    let (_, rx) = flume::unbounded();
    let (handle, browser) = cx.update(|cx| {
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                    Default::default(),
                    size(px(1200.0), px(800.0)),
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
    let item_id = cx.update(|cx| browser.read(cx).visible()[0].item_id);
    // Focus the browser so its key bindings reach it, like a click would.
    cx.update_window(handle, |_, window, cx| {
        let focus = browser.read(cx).focus_handle();
        window.focus(&focus, cx);
    })
    .unwrap();
    Rig {
        handle,
        browser,
        catalog,
        item_id,
    }
}

/// Paint frames and let background work finish until `done` holds.
fn settle(cx: &mut TestAppContext, rig: &Rig, done: impl Fn(&Browser) -> bool) {
    for _ in 0..200 {
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

fn draft(cx: &mut TestAppContext, rig: &Rig) -> Crop {
    cx.update(|cx| {
        rig.browser
            .read(cx)
            .crop_draft()
            .expect("crop mode is active")
    })
}

/// Window position of a normalized point on the painted image.
fn image_point(
    cx: &mut TestAppContext,
    rig: &Rig,
    nx: f32,
    ny: f32,
) -> gpui_kit::Point<gpui_kit::Pixels> {
    let (x, y, w, h) = cx
        .update(|cx| rig.browser.read(cx).canvas_image_frame())
        .expect("the image has painted");
    point(px(x + nx * w), px(y + ny * h))
}

fn drag(cx: &mut TestAppContext, rig: &Rig, from: (f32, f32), to: (f32, f32)) {
    let from = image_point(cx, rig, from.0, from.1);
    let to = image_point(cx, rig, to.0, to.1);
    let mut visual = VisualTestContext::from_window(rig.handle, cx);
    visual.simulate_mouse_down(from, MouseButton::Left, Modifiers::none());
    let mid = point((from.x + to.x) / 2.0, (from.y + to.y) / 2.0);
    for at in [mid, to] {
        visual.simulate_mouse_move(at, Some(MouseButton::Left), Modifiers::none());
    }
    visual.simulate_mouse_up(to, MouseButton::Left, Modifiers::none());
}

/// Open the single view and enter crop mode with the real keys.
fn enter_crop(cx: &mut TestAppContext, rig: &Rig, first: bool) {
    if first {
        cx.simulate_keystrokes(rig.handle, "g");
    }
    settle(cx, rig, |_| true);
    // The photo loads and renders in the background before crop mode opens.
    for _ in 0..200 {
        cx.simulate_keystrokes(rig.handle, "c");
        if cx.update(|cx| rig.browser.read(cx).crop_active()) {
            break;
        }
        settle(cx, rig, |_| true);
    }
    assert!(cx.update(|cx| rig.browser.read(cx).crop_active()));
    settle(cx, rig, |b| b.crop_ready());
}

#[gpui_kit::test]
fn dragging_a_handle_and_the_frame_edits_the_draft(cx: &mut TestAppContext) {
    let rig = rig(cx);
    enter_crop(cx, &rig, true);
    assert_eq!(draft(cx, &rig), Crop::default());

    // Pull the bottom-right handle in: the frame shrinks from that corner.
    drag(cx, &rig, (0.995, 0.995), (0.7, 0.6));
    let shrunk = draft(cx, &rig).rect;
    assert!(
        (shrunk.w - 0.7).abs() < 0.02 && (shrunk.h - 0.6).abs() < 0.02,
        "{shrunk:?}"
    );
    assert!(shrunk.x.abs() < 1e-3 && shrunk.y.abs() < 1e-3, "{shrunk:?}");

    // Drag the body: the frame moves and keeps its size.
    drag(cx, &rig, (0.35, 0.3), (0.55, 0.5));
    let moved = draft(cx, &rig).rect;
    assert!((moved.w - shrunk.w).abs() < 1e-4 && (moved.h - shrunk.h).abs() < 1e-4);
    assert!(
        (moved.x - 0.2).abs() < 0.02 && (moved.y - 0.2).abs() < 0.02,
        "{moved:?}"
    );

    // Dragging past the image edge stops at the edge.
    drag(cx, &rig, (0.9, 0.8), (1.6, 1.6));
    let far = draft(cx, &rig).rect;
    assert!(far.x + far.w <= 1.001 && far.y + far.h <= 1.001, "{far:?}");
}

#[gpui_kit::test]
fn an_aspect_lock_and_a_straighten_keep_the_frame_inside(cx: &mut TestAppContext) {
    let rig = rig(cx);
    enter_crop(cx, &rig, true);
    cx.update(|cx| {
        rig.browser
            .update(cx, |b, cx| b.set_crop_aspect(Some(Aspect::Square), cx))
    });
    let source = (600.0, 400.0);
    let square = draft(cx, &rig).rect;
    assert!(
        (rect_ratio(square, source) - 1.0).abs() < 0.01,
        "{square:?}"
    );

    // A locked corner drag keeps the ratio.
    drag(
        cx,
        &rig,
        (square.x + square.w, square.y + square.h),
        (0.5, 0.9),
    );
    assert!((rect_ratio(draft(cx, &rig).rect, source) - 1.0).abs() < 0.02);

    cx.update(|cx| rig.browser.update(cx, |b, cx| b.straighten_to(8.0, cx)));
    let crop = draft(cx, &rig);
    assert_eq!(crop.rotation_degrees, 8.0);
    assert!(rect_fits(crop.rect, 8.0, source), "{crop:?}");
    assert!((rect_ratio(crop.rect, source) - 1.0).abs() < 0.02);
}

#[gpui_kit::test]
fn escape_cancels_and_enter_applies_and_saves(cx: &mut TestAppContext) {
    let rig = rig(cx);
    enter_crop(cx, &rig, true);

    // Escape leaves crop mode and saves nothing.
    drag(cx, &rig, (0.995, 0.995), (0.6, 0.6));
    cx.simulate_keystrokes(rig.handle, "escape");
    settle(cx, &rig, |_| true);
    assert!(!cx.update(|cx| rig.browser.read(cx).crop_active()));
    let stored = rig.catalog.get_edit(rig.item_id).unwrap();
    assert!(stored.is_none_or(|edit| edit.crop_json.is_none()));

    // Enter applies: the crop is in the catalog row at once.
    enter_crop(cx, &rig, false);
    drag(cx, &rig, (0.995, 0.995), (0.6, 0.5));
    cx.update(|cx| rig.browser.update(cx, |b, cx| b.straighten_to(3.0, cx)));
    let want = draft(cx, &rig);
    cx.simulate_keystrokes(rig.handle, "enter");
    settle(cx, &rig, |_| true);
    assert!(!cx.update(|cx| rig.browser.read(cx).crop_active()));
    let row = rig
        .catalog
        .get_edit(rig.item_id)
        .unwrap()
        .expect("an edit row");
    let saved: Crop = serde_json::from_str(&row.crop_json.expect("a crop")).unwrap();
    assert_eq!(saved.rotation_degrees, 3.0);
    assert!((saved.rect.w - want.rect.w).abs() < 1e-4);

    // Coming back, the draft starts from what was saved.
    cx.simulate_keystrokes(rig.handle, "c");
    settle(cx, &rig, |b| b.crop_ready());
    assert_eq!(draft(cx, &rig).rotation_degrees, 3.0);
}

#[gpui_kit::test]
fn drawing_on_an_uncropped_photo_makes_a_frame_in_the_selected_aspect(cx: &mut TestAppContext) {
    let rig = rig(cx);
    enter_crop(cx, &rig, true);
    let source = (600.0, 400.0);

    // Free: the frame is the box between the press and the release.
    drag(cx, &rig, (0.2, 0.2), (0.6, 0.5));
    let free = draft(cx, &rig).rect;
    assert!(
        (free.x - 0.2).abs() < 0.01 && (free.y - 0.2).abs() < 0.01,
        "{free:?}"
    );
    assert!(
        (free.w - 0.4).abs() < 0.01 && (free.h - 0.3).abs() < 0.01,
        "{free:?}"
    );

    // Back to the whole photo, then draw with a lock: the ratio holds.
    cx.update(|cx| rig.browser.update(cx, |b, cx| b.reset_crop(cx)));
    cx.update(|cx| {
        rig.browser
            .update(cx, |b, cx| b.set_crop_aspect(Some(Aspect::ThreeTwo), cx))
    });
    drag(cx, &rig, (0.1, 0.1), (0.5, 0.9));
    let locked = draft(cx, &rig).rect;
    assert!(
        (rect_ratio(locked, source) - 1.5).abs() < 0.01,
        "{locked:?}"
    );
    assert!(
        (locked.x - 0.1).abs() < 0.01 && (locked.y - 0.1).abs() < 0.01,
        "{locked:?}"
    );

    // The frame drawn is editable: a locked handle drag keeps the ratio.
    drag(
        cx,
        &rig,
        (locked.x + locked.w - 0.002, locked.y + locked.h - 0.002),
        (0.3, 0.3),
    );
    assert!((rect_ratio(draft(cx, &rig).rect, source) - 1.5).abs() < 0.02);
}

#[gpui_kit::test]
fn dragging_outside_the_frame_turns_the_photo_and_x_flips_the_frame(cx: &mut TestAppContext) {
    let rig = rig(cx);
    enter_crop(cx, &rig, true);
    let source = (600.0, 400.0);
    drag(cx, &rig, (0.3, 0.3), (0.7, 0.7));
    let frame = draft(cx, &rig).rect;

    // A press east of the frame, pulled downward, turns the photo clockwise.
    drag(cx, &rig, (0.9, 0.5), (0.9, 0.7));
    let turned = draft(cx, &rig);
    assert!(turned.rotation_degrees > 2.0, "{turned:?}");
    assert!(
        rect_fits(turned.rect, turned.rotation_degrees, source),
        "{turned:?}"
    );
    assert!(
        turned.rect.w <= frame.w + 1e-4,
        "the frame never grows when turning"
    );

    // Pulled the other way past the limit it clamps at 45 degrees.
    drag(cx, &rig, (0.95, 0.5), (0.2, 1.6));
    assert!(draft(cx, &rig).rotation_degrees.abs() <= 45.0);

    // X turns a locked frame between landscape and portrait.
    cx.update(|cx| rig.browser.update(cx, |b, cx| b.straighten_to(0.0, cx)));
    cx.update(|cx| {
        rig.browser
            .update(cx, |b, cx| b.set_crop_aspect(Some(Aspect::ThreeTwo), cx))
    });
    let before = draft(cx, &rig);
    assert!(rect_ratio(before.rect, source) > 1.0);
    cx.simulate_keystrokes(rig.handle, "x");
    let after = draft(cx, &rig);
    assert!(after.aspect_flipped);
    assert!(
        (rect_ratio(after.rect, source) - 2.0 / 3.0).abs() < 0.02,
        "{after:?}"
    );
}
