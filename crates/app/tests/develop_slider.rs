//! A click on a develop slider track must update the preview the same way a
//! drag does: the knob jumps to the click point and a render follows.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use app::browse::run_catalog_scan;
use app::develop::SliderId;
use app::views::Browser;
use edit_catalog::{Catalog, JobEvent, JobEventKind};
use edit_render::RenderService;
use gpui_kit::component::slider::SliderValue;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{
    AppContext, Bounds, Entity, Modifiers, MouseButton, TestAppContext, VisualTestContext,
    WindowBounds, WindowOptions, point, px, size,
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
    Rig { handle, browser }
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

#[gpui_kit::test]
fn clicking_the_slider_track_renders_a_new_preview(cx: &mut TestAppContext) {
    let rig = rig(cx);
    cx.simulate_keystrokes(rig.handle, "g");
    settle(cx, &rig, |_| true);
    let slider = cx
        .update(|cx| rig.browser.read(cx).develop_slider(SliderId::Exposure))
        .unwrap();
    // The photo's edit loads in the background; wait for the panel to be live.
    settle(cx, &rig, |b| b.develop_owner().is_some());
    let before = cx.update(|cx| rig.browser.read(cx).develop_frames());
    // Opening an unedited photo shows the camera preview; nothing is developed.
    assert_eq!(before, 0, "opening an unedited photo must not render");

    let bounds = cx.update(|cx| slider.read(cx).bounds());
    assert!(
        bounds.size.width > px(10.0),
        "slider never laid out: {bounds:?}"
    );
    let click = point(
        bounds.left() + bounds.size.width * 0.8,
        bounds.top() + bounds.size.height / 2.0,
    );
    {
        let mut visual = VisualTestContext::from_window(rig.handle, cx);
        visual.simulate_mouse_down(click, MouseButton::Left, Modifiers::none());
        visual.simulate_mouse_up(click, MouseButton::Left, Modifiers::none());
    }
    let value = cx.update(|cx| match slider.read(cx).value() {
        SliderValue::Single(v) => v,
        SliderValue::Range(v, _) => v,
    });
    assert!(value > 2.0, "the knob should jump to the click: {value}");
    settle(cx, &rig, |b| b.develop_frames() > before);
    let exposure = cx.update(|cx| rig.browser.read(cx).develop_value(SliderId::Exposure));
    assert!(
        (exposure - value as f64).abs() < 0.05,
        "{exposure} vs {value}"
    );
}
