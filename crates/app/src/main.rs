mod ingest;

use gpui_kit::component::button::Button;
use gpui_kit::component::input::InputState;
use gpui_kit::component::{TitleBar, h_flex, status_bar::StatusBar, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use ingest::{AppState, IngestDialog, OpenIngest};

struct WorkspaceShell {
    state: Entity<AppState>,
    dialog: Entity<IngestDialog>,
    focus_handle: FocusHandle,
}

impl WorkspaceShell {
    fn new(state: Entity<AppState>, dialog: Entity<IngestDialog>, cx: &mut Context<Self>) -> Self {
        Self {
            state,
            dialog,
            focus_handle: cx.focus_handle(),
        }
    }

    fn open_ingest(&mut self, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            state.set_ingest_open(true, cx);
        });
        cx.notify();
    }
}

impl Focusable for WorkspaceShell {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for WorkspaceShell {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let volume_count = self.state.read(cx).volumes().len();
        let dialog_open = self.state.read(cx).is_ingest_open();
        let dialog = self.dialog.clone();
        v_flex()
            .size_full()
            .key_context("Workspace")
            .track_focus(&self.focus_handle)
            .child(
                TitleBar::new().child(
                    h_flex()
                        .w_full()
                        .pr_2()
                        .justify_between()
                        .child("editing")
                        .child(
                            h_flex()
                                .gap_2()
                                .child(Button::new("title-ingest").label("Ingest").on_click(
                                    cx.listener(|this, _event: &ClickEvent, _window, cx| {
                                        this.open_ingest(cx);
                                    }),
                                ))
                                .child("No photos open"),
                        ),
                ),
            )
            .child(
                div()
                    .id("workspace")
                    .flex_1()
                    .size_full()
                    .items_center()
                    .justify_center()
                    .child("Workspace"),
            )
            .child(StatusBar::new().left("Ready").right(format!(
                "{volume_count} {}",
                if volume_count == 1 {
                    "volume"
                } else {
                    "volumes"
                }
            )))
            .when(dialog_open, |root| {
                root.child(
                    div()
                        .absolute()
                        .inset_0()
                        .child(div().w(px(600.0)).mx_auto().mt_16().child(dialog)),
                )
            })
    }
}

fn main() {
    let app = gpui_kit::application().with_assets(gpui_kit::assets::Assets);

    app.run(move |cx| {
        gpui_kit::init(cx);

        cx.bind_keys([KeyBinding::new(
            "i",
            OpenIngest,
            Some("Workspace && !Input"),
        )]);

        let window_options = TitleBar::window_options();

        let (window_handle, shell) = gpui_kit::open_window(window_options, cx, |window, cx| {
            let state = cx.new(|_| AppState::new());
            let dest_dir = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("~/Photos")
                    .default_value("~/Photos/{yyyy}/{mm}-{dd}")
            });
            let file_rule = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("{session}_{seq:4}")
                    .default_value("{session}_{seq:4}")
            });
            let backup_dir =
                cx.new(|cx| InputState::new(window, cx).placeholder("/mnt/backup (optional)"));
            let dialog =
                cx.new(|_| IngestDialog::new(state.clone(), dest_dir, file_rule, backup_dir));
            cx.new(|cx| WorkspaceShell::new(state.clone(), dialog, cx))
        })
        .expect("Failed to open window");

        // Keyboard shortcut target: focus the shell so `i` dispatches.
        let focus_handle = shell.read(cx).focus_handle.clone();
        let _ = window_handle.update(cx, |_view, window, cx| {
            window.focus(&focus_handle, cx);
        });

        // Global `i` shortcut opens the ingest dialog.
        let shortcut_shell = shell.clone();
        cx.on_action(move |_action: &OpenIngest, cx: &mut App| {
            shortcut_shell.update(cx, |shell, cx| {
                shell.open_ingest(cx);
            });
        });

        // Volume detection: blocking poll + udev hotplug on a worker thread,
        // snapshots drained on the GPUI executor so the dialog refreshes on
        // insert/remove while open.
        let (vol_tx, vol_rx) = flume::unbounded();
        std::thread::spawn(|| {
            edit_ingest::watch_loop(edit_ingest::SysinfoSource, vol_tx);
        });
        let drain_state = shell.read(cx).state.clone();
        cx.spawn(async move |cx| {
            while let Ok(volumes) = vol_rx.recv_async().await {
                cx.update(|cx| {
                    drain_state.update(cx, |state, cx| {
                        state.set_volumes(volumes, cx);
                    });
                });
            }
        })
        .detach();
    });
}
