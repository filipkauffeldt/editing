//! UI integration tests for the ingest modal: it opens over the window,
//! Escape closes it, the backdrop blocks clicks and the wheel for the view
//! behind it, and the copy scope follows the photos the camera locked.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use app::ingest::{AppState, CopyScope, IngestDialog, IngestFooter, open_ingest_dialog};
use edit_catalog::{Catalog, JobKind};
use edit_ingest::{IngestCopyPayload, Volume};
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::input::InputState;
use gpui_kit::component::searchable_list::SearchableVec;
use gpui_kit::component::select::SelectState;
use gpui_kit::test::{TestSupportExt as _, TestWindowExt as _};
use gpui_kit::{
    AnyWindowHandle, AppContext, Bounds, Context, Entity, ScrollDelta, ScrollHandle,
    TestAppContext, Window, WindowBounds, WindowOptions, div, point, prelude::*, px, size,
};

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

/// The view behind the dialog: a button that counts clicks and a scroll
/// area, so the tests can see whether the backdrop lets input through.
struct Behind {
    clicks: usize,
    scroll: ScrollHandle,
}

impl Render for Behind {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                Button::new("behind")
                    .label("Behind")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.clicks += 1;
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .id("behind-list")
                    .test_support()
                    .h(px(120.))
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .children((0..40).map(|n| div().h(px(24.)).child(format!("row {n}")))),
            )
    }
}

struct Fixture {
    handle: AnyWindowHandle,
    behind: Entity<Behind>,
    dialog: Entity<IngestDialog>,
    footer: Entity<IngestFooter>,
    state: Entity<AppState>,
    catalog: Arc<Catalog>,
    dir: PathBuf,
}

/// A card folder with `names`; the ones in `locked` get no write bit, the
/// way a camera marks protected photos.
fn card(names: &[&str], locked: &[&str]) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let n = NEXT_DIR.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("edit-ingest-dialog-{}-{n}", std::process::id()));
    let dcim = dir.join("card/DCIM/100NIKON");
    std::fs::create_dir_all(&dcim).unwrap();
    for name in names {
        let path = dcim.join(name);
        std::fs::write(&path, format!("bytes of {name}")).unwrap();
        let mode = if locked.contains(name) { 0o444 } else { 0o644 };
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    }
    dir
}

fn fixture(cx: &mut TestAppContext, dir: &Path) -> Fixture {
    cx.update(gpui_kit::init);
    let catalog = Arc::new(Catalog::open(&dir.join("catalog.sqlite")).unwrap());
    let (handle, built) = cx.update(|cx| {
        let mut built = None;
        let (handle, _) = gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                    Default::default(),
                    size(px(1000.), px(900.)),
                ))),
                ..Default::default()
            },
            cx,
            |window, cx| {
                let state = cx.new(|_| AppState::new());
                let dest = cx.new(|cx| InputState::new(window, cx).default_value("~/Photos"));
                let rule = cx.new(|cx| InputState::new(window, cx).default_value("{seq}"));
                let backup = cx.new(|cx| InputState::new(window, cx));
                let select = cx.new(|cx| {
                    SelectState::new(SearchableVec::new(Vec::<String>::new()), None, window, cx)
                });
                let dialog = cx.new(|cx| {
                    IngestDialog::new(
                        state.clone(),
                        Arc::clone(&catalog),
                        dest,
                        rule,
                        backup,
                        select,
                        cx,
                    )
                });
                let footer = cx.new(|cx| IngestFooter::new(dialog.clone(), cx));
                let behind = cx.new(|_| Behind {
                    clicks: 0,
                    scroll: ScrollHandle::new(),
                });
                built = Some((behind.clone(), dialog, footer, state));
                behind
            },
        )
        .expect("open test window");
        (handle, built.unwrap())
    });
    let (behind, dialog, footer, state) = built;
    Fixture {
        handle,
        behind,
        dialog,
        footer,
        state,
        catalog,
        dir: dir.to_path_buf(),
    }
}

impl Fixture {
    /// Offer the card as a source and open the dialog over the window.
    fn open(&self, cx: &mut TestAppContext, read_only: bool) {
        let volume = Volume {
            removable: true,
            read_only,
            ..Volume::folder(&self.dir.join("card"))
        };
        cx.update(|cx| {
            self.state
                .update(cx, |state, cx| state.set_volumes(vec![volume], cx));
        });
        cx.update_window(self.handle, |_, window, cx| {
            window.render_frame(cx);
            open_ingest_dialog(&self.dialog, &self.footer, window, cx);
        })
        .unwrap();
        // The card's photo list is read in the background.
        cx.run_until_parked();
        cx.update_window(self.handle, |_, window, cx| window.render_frame(cx))
            .unwrap();
    }

    fn is_open(&self, cx: &mut TestAppContext) -> bool {
        cx.update_window(self.handle, |_, window, cx| window.has_active_dialog(cx))
            .unwrap()
    }
}

#[gpui_kit::test]
fn the_dialog_opens_and_escape_closes_it(cx: &mut TestAppContext) {
    let dir = card(&["DSC_0001.NEF"], &[]);
    let fx = fixture(cx, &dir);
    assert!(!fx.is_open(cx));
    fx.open(cx, false);
    assert!(fx.is_open(cx), "the modal is on screen");
    cx.update_window(fx.handle, |_, window, cx| {
        assert!(window.try_find("ingest-start").is_some(), "footer is shown");
        window.press("escape", cx);
    })
    .unwrap();
    assert!(!fx.is_open(cx), "Escape closes the topmost surface");
    std::fs::remove_dir_all(&dir).ok();
}

#[gpui_kit::test]
fn the_backdrop_blocks_clicks_and_the_wheel_for_the_view_behind(cx: &mut TestAppContext) {
    let dir = card(&["DSC_0001.NEF"], &[]);
    let fx = fixture(cx, &dir);
    let wheel = ScrollDelta::Pixels(point(px(0.), px(-60.)));
    let offset = |fx: &Fixture, cx: &mut TestAppContext| {
        cx.update(|cx| fx.behind.read(cx).scroll.offset().y)
    };

    fx.open(cx, false);
    cx.update_window(fx.handle, |_, window, cx| {
        window.click("behind", cx);
        window.scroll("behind-list", wheel, cx);
    })
    .unwrap();
    assert_eq!(
        cx.update(|cx| fx.behind.read(cx).clicks),
        0,
        "click blocked"
    );
    assert_eq!(offset(&fx, cx), px(0.), "wheel blocked");

    cx.update_window(fx.handle, |_, window, cx| window.press("escape", cx))
        .unwrap();
    cx.update_window(fx.handle, |_, window, cx| {
        window.click("behind", cx);
        window.scroll("behind-list", wheel, cx);
    })
    .unwrap();
    assert_eq!(
        cx.update(|cx| fx.behind.read(cx).clicks),
        1,
        "click reaches it again"
    );
    assert!(offset(&fx, cx) < px(0.), "wheel reaches it again");
    std::fs::remove_dir_all(&dir).ok();
}

#[gpui_kit::test]
fn only_locked_photos_are_copied_and_the_rest_are_skipped(cx: &mut TestAppContext) {
    let dir = card(
        &["DSC_0001.NEF", "DSC_0002.NEF", "DSC_0003.NEF"],
        &["DSC_0001.NEF", "DSC_0003.NEF"],
    );
    let fx = fixture(cx, &dir);
    fx.open(cx, false);

    let (locked, count, primary) = cx.update(|cx| {
        let dialog = fx.dialog.read(cx);
        (
            dialog.locked_count(),
            dialog.photo_count(),
            dialog.primary(),
        )
    });
    assert_eq!((locked, count), (2, 3), "two of three are locked in camera");
    assert_eq!(primary, ("Ingest 3 photos".to_string(), true));

    cx.update_window(fx.handle, |_, window, cx| {
        // No per-file tag checkboxes any more.
        assert!(window.try_find(("tag", 0usize)).is_none());
        window.click("scope-locked", cx);
    })
    .unwrap();
    let (scope, primary) = cx.update(|cx| {
        let dialog = fx.dialog.read(cx);
        (dialog.scope, dialog.primary())
    });
    assert_eq!(scope, CopyScope::Locked);
    assert_eq!(primary, ("Ingest 2 photos".to_string(), true));

    cx.update_window(fx.handle, |_, window, cx| window.click("ingest-start", cx))
        .unwrap();
    let job = fx
        .catalog
        .list_jobs(10)
        .unwrap()
        .into_iter()
        .find(|job| job.kind == JobKind::IngestCopy)
        .expect("the ingest was queued");
    let payload: IngestCopyPayload = serde_json::from_str(&job.payload_json).unwrap();
    let only = payload
        .only_sources
        .expect("a locked-only copy lists its files");
    let names: Vec<&str> = only
        .iter()
        .map(|path| path.rsplit('/').next().unwrap())
        .collect();
    assert_eq!(names, ["DSC_0001.NEF", "DSC_0003.NEF"]);
    std::fs::remove_dir_all(&dir).ok();
}

#[gpui_kit::test]
fn the_locked_option_is_off_when_nothing_is_locked_or_the_card_is_read_only(
    cx: &mut TestAppContext,
) {
    // Nothing locked on this card.
    let open_dir = card(&["DSC_0001.NEF", "DSC_0002.NEF"], &[]);
    let fx = fixture(cx, &open_dir);
    fx.open(cx, false);
    cx.update_window(fx.handle, |_, window, cx| window.click("scope-locked", cx))
        .unwrap();
    cx.update(|cx| {
        let dialog = fx.dialog.read(cx);
        assert_eq!(
            dialog.scope,
            CopyScope::All,
            "the disabled option does nothing"
        );
        assert_eq!(
            dialog.locked_unavailable(),
            Some("No photos are locked in the camera.")
        );
    });
    std::fs::remove_dir_all(&open_dir).ok();

    // Every file looks read-only on a read-only card, so locks are unknown.
    let ro_dir = card(
        &["DSC_0001.NEF", "DSC_0002.NEF"],
        &["DSC_0001.NEF", "DSC_0002.NEF"],
    );
    let fx = fixture(cx, &ro_dir);
    fx.open(cx, true);
    cx.update(|cx| {
        let dialog = fx.dialog.read(cx);
        assert_eq!(
            dialog.locked_count(),
            0,
            "no lock marks on a read-only card"
        );
        assert_eq!(
            dialog.locked_unavailable(),
            Some("Locks can't be read from a read-only card.")
        );
        assert_eq!(dialog.primary(), ("Ingest 2 photos".to_string(), true));
    });
    std::fs::remove_dir_all(&ro_dir).ok();
}
