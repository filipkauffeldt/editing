mod ingest;
mod templates;

use std::path::PathBuf;
use std::sync::Arc;

use edit_catalog::{Catalog, JobKind};
use gpui_kit::component::input::InputState;
use gpui_kit::component::{TitleBar, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use ingest::{AppState, IngestDialog, OpenIngest, apply_ingest_event};
use templates::{OpenTemplates, TemplateEditor};

use app::browse::CatalogScanPayload;
use app::ingest_job::handle_job;
use app::views::Browser;
use edit_metadata::Writer;

/// Worker threads draining the durable job queue.
///
/// The pool is created before volume detection, so it cannot be sized to the
/// live card count. Three is the common card count (and comfortably covers one
/// or two), so up to three cards ingest in parallel; a fourth card's job stays
/// `queued` and starts as soon as a worker frees up. The copy stage, not the
/// database, is the bottleneck, and each worker holds its own SQLite
/// connection, so raising this is a one-line change when more cards are common.
const WORKER_THREADS: usize = 3;

fn catalog_path() -> PathBuf {
    let base = std::env::var_os("EDITING_CATALOG")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(|home| PathBuf::from(home).join(".local/share/editing/catalog.sqlite"))
        });
    base.unwrap_or_else(|| PathBuf::from("editing-catalog.sqlite"))
}

/// Window root: the browse browser owns the chrome (title + status bar) and
/// the ingest dialog overlays it when opened. The shell adds the shared
/// `Workspace` key context so `i` opens ingest wherever focus is inside the
/// browser tree, but not while typing in the dialog inputs.
struct BrowserShell {
    browser: Entity<Browser>,
    state: Entity<AppState>,
    dialog: Entity<IngestDialog>,
    templates: Entity<TemplateEditor>,
    focus_handle: FocusHandle,
}

impl BrowserShell {
    fn new(
        browser: Entity<Browser>,
        state: Entity<AppState>,
        dialog: Entity<IngestDialog>,
        templates: Entity<TemplateEditor>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            browser,
            state,
            dialog,
            templates,
            focus_handle: cx.focus_handle(),
        }
    }
}

impl Focusable for BrowserShell {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for BrowserShell {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let dialog_open = self.state.read(cx).is_ingest_open();
        let dialog = self.dialog.clone();
        let templates_open = self.state.read(cx).is_templates_open();
        let templates = self.templates.clone();
        v_flex()
            .size_full()
            .key_context("Workspace")
            .track_focus(&self.focus_handle)
            .child(self.browser.clone())
            .when(dialog_open, |root| {
                root.child(
                    div()
                        .absolute()
                        .inset_0()
                        .child(div().w(px(600.0)).mx_auto().mt_16().child(dialog)),
                )
            })
            .when(templates_open, |root| {
                root.child(
                    div()
                        .absolute()
                        .inset_0()
                        .child(div().w(px(760.0)).mx_auto().mt_8().child(templates)),
                )
            })
    }
}

fn main() {
    let catalog = Arc::new(Catalog::open(&catalog_path()).expect("open catalog"));
    catalog.recover_running().expect("recover running jobs");

    let worker_catalog = Arc::clone(&catalog);
    // The resident exiftool writer lives once for the process; job grain
    // serializes through the transport mutex. Missing binary: template
    // jobs report the failure on their durable rows (degrade gracefully).
    let writer = Writer::find_binary().and_then(|bin| Writer::resident(&bin).ok());
    let handler: edit_catalog::JobHandler = Arc::new(move |job, tx| match job.kind {
        JobKind::MetadataWrite => match &writer {
            Some(writer) => edit_metadata::run_metadata_write(job, writer, tx),
            None => Err("exiftool unavailable (install libimage-exiftool-perl)".into()),
        },
        _ => handle_job(worker_catalog.clone(), job, tx),
    });
    let pool =
        edit_catalog::spawn_workers(&catalog, WORKER_THREADS, handler).expect("spawn workers");
    let events = pool.events().clone();
    let browser_events = events.clone();
    // The pool owns the worker threads; leak it so background jobs keep
    // draining for the life of the window.
    std::mem::forget(pool);

    // Open the demo folder from argv[1] through the real job path when given.
    let pending_folder: Option<PathBuf> = std::env::args().nth(1).map(PathBuf::from);

    let app = gpui_kit::application().with_assets(gpui_kit::assets::Assets);
    app.run(move |cx| {
        gpui_kit::init(cx);

        // Keyboard shortcut target: focus lands in browse views too, so the
        // ingest / template shortcuts are gated on `Workspace && !Input` to
        // leave typing in the dialog inputs alone.
        cx.bind_keys([
            KeyBinding::new("i", OpenIngest, Some("Workspace && !Input")),
            KeyBinding::new("t", OpenTemplates, Some("Workspace && !Input")),
        ]);

        if let Some(folder) = pending_folder.clone() {
            let payload = serde_json::to_string(&CatalogScanPayload {
                root: folder.to_string_lossy().into_owned(),
            })
            .unwrap_or_else(|_| "{}".to_string());
            catalog.enqueue_job(JobKind::CatalogScan, &payload).ok();
        }

        let window_options = TitleBar::window_options();
        let (window_handle, shell) = gpui_kit::open_window(window_options, cx, |window, cx| {
            let browser_catalog = Arc::clone(&catalog);
            let browser = cx.new(|cx| Browser::new(browser_catalog, browser_events.clone(), cx));
            // Ingest dialog state + inputs. Rebuild the card/job routing from
            // durable state so a crash-resumed ingest is visible.
            let state = cx.new(|_| AppState::new());
            state.update(cx, |state, cx| state.resume_batches(&catalog, cx));
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
            let dialog = cx.new(|_| {
                IngestDialog::new(
                    state.clone(),
                    Arc::clone(&catalog),
                    dest_dir,
                    file_rule,
                    backup_dir,
                )
            });
            let templates = cx.new(|cx| {
                TemplateEditor::new(
                    state.clone(),
                    browser.clone(),
                    Arc::clone(&catalog),
                    window,
                    cx,
                )
            });
            cx.new(|cx| BrowserShell::new(browser, state.clone(), dialog, templates, cx))
        })
        .expect("Failed to open window");

        // Keyboard shortcut target: focus the shell so `i` dispatches.
        let focus_handle = shell.read(cx).focus_handle.clone();
        let _ = window_handle.update(cx, |_view, window, cx| {
            window.focus(&focus_handle, cx);
        });

        // Global shortcut handler: the action can come from anywhere in the
        // shell focus tree, so act on the shared ingest state directly.
        let shortcut_state = shell.read(cx).state.clone();
        cx.on_action(move |_action: &OpenIngest, cx: &mut App| {
            shortcut_state.update(cx, |state, cx| {
                state.set_ingest_open(true, cx);
            });
        });

        // One drain loop for every durable job event, so background threads
        // never touch entities. It fans out to the contact sheet and to the
        // ingest dialog (per-card progress, live file rows, duplicates).
        let drain_browser = shell.read(cx).browser.clone();
        let drain_ingest_state = shell.read(cx).state.clone();
        let drain_catalog = Arc::clone(&catalog);
        cx.spawn(async move |cx| {
            while let Ok(event) = events.recv_async().await {
                let ingest_event = event.clone();
                let browser = drain_browser.clone();
                let state = drain_ingest_state.clone();
                let catalog = Arc::clone(&drain_catalog);
                cx.update(|cx| {
                    browser.update(cx, |browser, cx| {
                        browser.on_job_event(&event);
                        cx.notify();
                    });
                    state.update(cx, |state, cx| {
                        apply_ingest_event(state, &catalog, &ingest_event, cx);
                    });
                });
            }
        })
        .detach();

        let shortcuts_state = shell.read(cx).state.clone();
        cx.on_action(move |_action: &OpenTemplates, cx: &mut App| {
            shortcuts_state.update(cx, |state, cx| {
                state.set_templates_open(true, cx);
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
