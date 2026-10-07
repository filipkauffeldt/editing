//! Ingest dialog shell (B3) and the shared [`AppState`].
//!
//! Scope: volume list driven by `edit-ingest` detection events, destination
//! inputs as plain text, an IPTC template selector stub, and a live file table
//! with a per-file state column. No copying, backup, rename, or template
//! application — the copy pipeline lands in C2 and template application in D3.
//!
//! The dialog is an inline panel built from gpui-component `DialogTitle`,
//! `Progress`, `Input`, and `Button` parts and rendered as an entity-driven
//! overlay, so insert/remove events refresh it while it is open. A full modal
//! `window.open_dialog` overlay arrives with the C2 pipeline.

use edit_ingest::Volume;
use gpui_kit::component::button::Button;
use gpui_kit::component::dialog::DialogTitle;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::progress::Progress;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

actions!(ingest, [OpenIngest]);

/// Per-file ingest state shown in the live file table.
///
/// Produced by the C2 copy pipeline; the B3 shell only renders the column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code, reason = "states are produced by the C2 copy pipeline")]
pub enum IngestFileState {
    Queued,
    Copying,
    Verifying,
    Renaming,
    Backup,
    Cataloged,
    Failed,
}

impl std::fmt::Display for IngestFileState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::Queued => "queued",
            Self::Copying => "copying",
            Self::Verifying => "verifying",
            Self::Renaming => "renaming",
            Self::Backup => "backup",
            Self::Cataloged => "cataloged",
            Self::Failed => "failed",
        };
        f.write_str(text)
    }
}

/// One row of the live file table. Empty in B3; C2 fills it per copied file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestFileRow {
    pub name: String,
    pub bytes: u64,
    pub state: IngestFileState,
}

/// Shared application state (plan section 2.10): currently detected volumes,
/// whether the ingest dialog is open, and the stub file rows.
///
/// Volume detection events replace the whole list; [`IngestDialog`] reads this
/// entity in `render`, so insert/remove refreshes the open dialog.
pub struct AppState {
    volumes: Vec<Volume>,
    ingest_open: bool,
    files: Vec<IngestFileRow>,
    /// IPTC template selector stub: `None` until M3/D3 populate templates.
    pub iptc_template: Option<String>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            volumes: Vec::new(),
            ingest_open: false,
            files: Vec::new(),
            iptc_template: None,
        }
    }
    pub fn volumes(&self) -> &[Volume] {
        &self.volumes
    }

    pub fn is_ingest_open(&self) -> bool {
        self.ingest_open
    }

    pub fn files(&self) -> &[IngestFileRow] {
        &self.files
    }

    /// Replace the volume list from a detection event. Volumes are keyed by
    /// filesystem UUID (never the label); see `edit-ingest`.
    pub fn set_volumes(&mut self, volumes: Vec<Volume>, cx: &mut Context<Self>) {
        if volumes != self.volumes {
            self.volumes = volumes;
            cx.notify();
        }
    }

    pub fn set_ingest_open(&mut self, open: bool, cx: &mut Context<Self>) {
        if open != self.ingest_open {
            self.ingest_open = open;
            cx.notify();
        }
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

/// Format a byte count for the card list and file table (binary units).
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Ingest dialog shell (plan section 2.8): card list with per-card progress,
/// destination folder + filename rule inputs, backup destination input, IPTC
/// template selector stub, and the live file table.
pub struct IngestDialog {
    state: Entity<AppState>,
    /// Volume identity of the selected card, if any.
    pub selected: Option<String>,
    pub dest_dir: Entity<InputState>,
    pub file_rule: Entity<InputState>,
    pub backup_dir: Entity<InputState>,
}

impl IngestDialog {
    pub fn new(
        state: Entity<AppState>,
        dest_dir: Entity<InputState>,
        file_rule: Entity<InputState>,
        backup_dir: Entity<InputState>,
    ) -> Self {
        Self {
            state,
            selected: None,
            dest_dir,
            file_rule,
            backup_dir,
        }
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            state.set_ingest_open(false, cx);
        });
    }

    fn render_cards(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let volumes = self.state.read(cx).volumes().to_vec();
        let selected = self.selected.clone();
        v_flex().gap_2().children(volumes.into_iter().map(|volume| {
            let identity = volume.identity.clone();
            let is_selected = selected.as_deref() == Some(identity.as_str());
            let label = volume.display_name();
            let detail = format!(
                "{} · {} free of {}",
                volume.mount_point.display(),
                format_bytes(volume.free_bytes),
                format_bytes(volume.capacity_bytes),
            );
            let progress_id = format!("card-progress-{}", volume.identity);
            let row_id = format!("card-row-{}", volume.identity);
            let identity_for_click = identity.clone();
            v_flex()
                .id(row_id)
                .gap_1()
                .p_2()
                .rounded_md()
                .border_1()
                .cursor_pointer()
                .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                    this.selected = Some(identity_for_click.clone());
                    cx.notify();
                }))
                .when(is_selected, |row| row.border_color(rgb(0x3399FF)))
                .child(
                    h_flex()
                        .w_full()
                        .justify_between()
                        .child(div().font_semibold().child(label))
                        .child(div().text_sm().child(detail)),
                )
                .child(Progress::new(progress_id).value(volume.used_percent()))
        }))
    }

    fn render_field(
        &self,
        label: &str,
        input: &Entity<InputState>,
        id: &'static str,
    ) -> impl IntoElement {
        v_flex()
            .gap_1()
            .child(div().text_sm().child(label.to_string()))
            .child(Input::new(input).id(id))
    }

    fn render_template_stub(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let template = self
            .state
            .read(cx)
            .iptc_template
            .clone()
            .unwrap_or_else(|| "none".to_string());
        div().text_sm().child(format!(
            "IPTC template: {template} (selector is populated in M3/D3)."
        ))
    }

    fn render_file_table(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let files = self.state.read(cx).files().to_vec();
        v_flex()
            .gap_1()
            .child(
                div()
                    .text_sm()
                    .font_semibold()
                    .child("Files (copy pipeline lands in C2)".to_string()),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_4()
                    .text_sm()
                    .font_semibold()
                    .child(div().w_1_2().child("File".to_string()))
                    .child(div().w_1_4().child("Size".to_string()))
                    .child(div().w_1_4().child("State".to_string())),
            )
            .children(files.iter().map(|row| {
                h_flex()
                    .w_full()
                    .gap_4()
                    .text_sm()
                    .child(div().w_1_2().child(row.name.clone()))
                    .child(div().w_1_4().child(format_bytes(row.bytes)))
                    .child(div().w_1_4().child(row.state.to_string()))
            }))
            .when(files.is_empty(), |table| {
                table.child(
                    div()
                        .text_sm()
                        .child("No files yet — plug in a card and start ingest in C2.".to_string()),
                )
            })
    }
}

impl Render for IngestDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let volumes = self.state.read(cx).volumes().to_vec();
        // Drop a selection whose card is gone so the highlight never points
        // at a removed volume.
        if let Some(selected) = self.selected.as_deref()
            && !volumes.iter().any(|volume| volume.identity == selected)
        {
            self.selected = None;
        }
        let volume_count = volumes.len();
        div()
            .id("ingest-dialog")
            .w_full()
            .rounded_lg()
            .border_1()
            .p_4()
            .bg(cx.theme().tokens.popover)
            .child(
                v_flex()
                    .gap_3()
                    .child(DialogTitle::new().child("Ingest photos".to_string()))
                    .child(div().text_sm().child(if volume_count == 0 {
                        "No volumes detected — plug in a card.".to_string()
                    } else if volume_count == 1 {
                        "1 volume detected.".to_string()
                    } else {
                        format!("{volume_count} volumes detected.")
                    }))
                    .child(self.render_cards(cx))
                    .child(self.render_field("Destination folder", &self.dest_dir, "ingest-dest"))
                    .child(self.render_field(
                        "Filename rule (plain text; rule engine is C2)",
                        &self.file_rule,
                        "ingest-rule",
                    ))
                    .child(self.render_field(
                        "Backup destination (optional)",
                        &self.backup_dir,
                        "ingest-backup",
                    ))
                    .child(self.render_template_stub(cx))
                    .child(self.render_file_table(cx))
                    .child(
                        h_flex()
                            .w_full()
                            .justify_end()
                            .gap_2()
                            .child(
                                Button::new("ingest-start")
                                    .label("Start ingest (C2)")
                                    .disabled(true),
                            )
                            .child(Button::new("ingest-close").label("Close").on_click(
                                cx.listener(|this, _event: &ClickEvent, _window, cx| {
                                    this.close(cx);
                                }),
                            )),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    // NOTE: import items explicitly, never `super::*` — the `gpui_kit::*`
    // glob re-exported above includes GPUI's `test` macro, which would shadow
    // the built-in `#[test]` attribute inside this module.
    use super::{IngestFileState, format_bytes};

    #[test]
    fn byte_counts_format_with_binary_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(48 * 1024 * 1024 * 1024), "48.0 GiB");
    }

    #[test]
    fn file_states_render_expected_labels() {
        assert_eq!(IngestFileState::Copying.to_string(), "copying");
        assert_eq!(IngestFileState::Cataloged.to_string(), "cataloged");
        assert_eq!(IngestFileState::Failed.to_string(), "failed");
    }
}
