//! The develop sidebar: a preset bar on top, then collapsible sections of
//! sliders built from the [`crate::panel`] table.
//!
//! The panel is its own entity. It never touches the edit buffer: it shows a
//! snapshot pushed in by the browser ([`DevelopPanel::set_state`]) and
//! reports what the user did as [`PanelEvent`]s, which keeps rendering and
//! persistence where they already were.

use std::collections::HashMap;

use edit_core::DevelopSettings;
use gpui_kit::component::accordion::Accordion;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::label::Label;
use gpui_kit::component::popover::Popover;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::select::{SearchableVec, Select, SelectEvent, SelectItem, SelectState};
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState, SliderValue};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, IconName, IndexPath, Sizable as _, StyledExt as _, h_flex,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use lightcraft_develop::WbMode;

use crate::develop::ControlId;
use crate::panel::{self, Item, PANEL};

/// What the user did in the panel.
#[derive(Clone, Debug, PartialEq)]
pub enum PanelEvent {
    /// A slider is moving.
    Change(ControlId, f64),
    /// A slider was let go, or a value was typed: commit it.
    Release(ControlId, f64),
    ResetSection(&'static str),
    ResetAll,
    Toggle(ControlId, bool),
    WhiteBalance(WbMode),
    /// A preset picked from the list, by its key (see [`PresetEntry`]).
    ApplyPreset(SharedString),
    /// "Save preset" confirmed with this name.
    SavePreset(String),
}

/// One line of the preset list.
#[derive(Clone, Debug, PartialEq)]
pub struct PresetEntry {
    /// `none`, `builtin:<name>` or `user:<name>`.
    pub key: SharedString,
    pub title: SharedString,
}

impl SelectItem for PresetEntry {
    type Value = SharedString;

    fn title(&self) -> SharedString {
        self.title.clone()
    }

    fn value(&self) -> &SharedString {
        &self.key
    }
}

/// White balance picker rows (sentence case, in LightCraft's order).
#[derive(Clone, Debug, PartialEq)]
struct WbEntry(WbMode, SharedString);

impl SelectItem for WbEntry {
    type Value = WbMode;

    fn title(&self) -> SharedString {
        self.1.clone()
    }

    fn value(&self) -> &WbMode {
        &self.0
    }
}

fn wb_label(mode: WbMode) -> &'static str {
    match mode {
        WbMode::AsShot => "As shot",
        WbMode::Auto => "Auto",
        WbMode::Daylight => "Daylight",
        WbMode::Cloudy => "Cloudy",
        WbMode::Shade => "Shade",
        WbMode::Tungsten => "Tungsten",
        WbMode::Fluorescent => "Fluorescent",
        WbMode::Flash => "Flash",
        WbMode::Custom => "Custom",
    }
}

/// The preset every list starts with.
pub fn none_entry() -> PresetEntry {
    PresetEntry {
        key: "none".into(),
        title: "None".into(),
    }
}

/// A slider with its number box.
struct Row {
    slider: Entity<SliderState>,
    input: Entity<InputState>,
}

pub struct DevelopPanel {
    settings: DevelopSettings,
    /// Whether the current photo can be edited right now.
    editable: bool,
    status: SharedString,
    rows: HashMap<ControlId, Row>,
    /// Section keys that are open.
    open: Vec<&'static str>,
    /// The slider being dragged, so the pushed snapshot does not fight it.
    dragging: Option<ControlId>,
    /// The number box being typed in.
    typing: Option<ControlId>,
    presets: Vec<PresetEntry>,
    preset_select: Option<Entity<SelectState<SearchableVec<PresetEntry>>>>,
    wb_select: Option<Entity<SelectState<SearchableVec<WbEntry>>>>,
    name_input: Option<Entity<InputState>>,
    save_open: bool,
    scroll_id: ElementId,
}

impl EventEmitter<PanelEvent> for DevelopPanel {}

impl Default for DevelopPanel {
    fn default() -> Self {
        Self::new()
    }
}

impl DevelopPanel {
    pub fn new() -> Self {
        Self {
            settings: DevelopSettings::default(),
            editable: false,
            status: SharedString::default(),
            rows: HashMap::new(),
            open: PANEL.iter().filter(|s| s.open).map(|s| s.key).collect(),
            dragging: None,
            typing: None,
            presets: vec![none_entry()],
            preset_select: None,
            wb_select: None,
            name_input: None,
            save_open: false,
            scroll_id: "develop-scroll".into(),
        }
    }

    /// Push the browser's snapshot in: the edit buffer, whether it can be
    /// edited, the status line and the preset list.
    pub fn set_state(
        &mut self,
        settings: &DevelopSettings,
        editable: bool,
        status: &str,
        presets: &[PresetEntry],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.ensure_chrome(window, cx);
        let mut changed = self.editable != editable || self.status.as_ref() != status;
        self.editable = editable;
        self.status = status.to_owned().into();
        if self.settings != *settings {
            self.settings = settings.clone();
            changed = true;
        }
        if self.presets != presets {
            self.presets = presets.to_vec();
            if let Some(select) = &self.preset_select {
                let items = SearchableVec::new(self.presets.clone());
                select.update(cx, |state, cx| state.set_items(items, window, cx));
            }
            changed = true;
        }
        self.sync_rows(window, cx);
        if let Some(select) = &self.wb_select {
            let mode = self.settings.wb.mode;
            select.update(cx, |state, cx| {
                if state.selected_value() != Some(&mode) {
                    state.set_selected_value(&mode, window, cx);
                }
            });
        }
        if changed {
            cx.notify();
        }
    }

    /// Show a preset as the chosen one (after applying it).
    pub fn show_preset(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(select) = &self.preset_select {
            let key: SharedString = key.to_owned().into();
            select.update(cx, |state, cx| state.set_selected_value(&key, window, cx));
        }
    }

    /// The slider widget behind a control, once its section has shown.
    pub fn slider(&self, id: &str) -> Option<Entity<SliderState>> {
        self.rows.get(id).map(|row| row.slider.clone())
    }

    /// The number box behind a control.
    pub fn number_input(&self, id: &str) -> Option<Entity<InputState>> {
        self.rows.get(id).map(|row| row.input.clone())
    }

    /// Open the sections whose keys are listed (used by tests and restore).
    pub fn set_open(&mut self, keys: &[&'static str], cx: &mut Context<Self>) {
        self.open = keys.to_vec();
        cx.notify();
    }

    pub fn open_sections(&self) -> &[&'static str] {
        &self.open
    }

    fn ensure_chrome(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.preset_select.is_none() {
            let items = SearchableVec::new(self.presets.clone());
            let select =
                cx.new(|cx| SelectState::new(items, Some(IndexPath::default()), window, cx));
            cx.subscribe(
                &select,
                |_, _, event: &SelectEvent<SearchableVec<PresetEntry>>, cx| {
                    if let SelectEvent::Confirm(Some(key)) = event {
                        cx.emit(PanelEvent::ApplyPreset(key.clone()));
                    }
                },
            )
            .detach();
            self.preset_select = Some(select);
        }
        if self.wb_select.is_none() {
            let items = SearchableVec::new(
                WbMode::ALL
                    .iter()
                    .map(|mode| WbEntry(*mode, wb_label(*mode).into()))
                    .collect::<Vec<_>>(),
            );
            let select =
                cx.new(|cx| SelectState::new(items, Some(IndexPath::default()), window, cx));
            cx.subscribe(
                &select,
                |_, _, event: &SelectEvent<SearchableVec<WbEntry>>, cx| {
                    if let SelectEvent::Confirm(Some(mode)) = event {
                        cx.emit(PanelEvent::WhiteBalance(*mode));
                    }
                },
            )
            .detach();
            self.wb_select = Some(select);
        }
        if self.name_input.is_none() {
            let input = cx.new(|cx| InputState::new(window, cx).placeholder("Preset name"));
            cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.confirm_save(window, cx);
                }
            })
            .detach();
            self.name_input = Some(input);
        }
    }

    /// Create the slider and number box of a row the first time it shows.
    fn ensure_row(&mut self, id: ControlId, window: &mut Window, cx: &mut Context<Self>) {
        if self.rows.contains_key(id) {
            return;
        }
        let Some(spec) = panel::spec(id) else { return };
        let value = lightcraft_develop::controls::get(&self.settings, id).unwrap_or(spec.default);
        let slider = cx.new(|_| {
            SliderState::new()
                // Raise the ceiling first: a floor above the default ceiling
                // (white balance starts at 2000) would panic the widget.
                .max(spec.max as f32)
                .min(spec.min as f32)
                .step(spec.step as f32)
                .default_value(value as f32)
        });
        cx.subscribe(&slider, move |this, _, event: &SliderEvent, cx| {
            let (SliderEvent::Change(SliderValue::Single(v))
            | SliderEvent::Release(SliderValue::Single(v))) = event
            else {
                return;
            };
            let v = *v as f64;
            match event {
                SliderEvent::Change(_) => {
                    this.dragging = Some(id);
                    cx.emit(PanelEvent::Change(id, v));
                }
                SliderEvent::Release(_) => {
                    this.dragging = None;
                    cx.emit(PanelEvent::Release(id, v));
                }
            }
            cx.notify();
        })
        .detach();
        let text = spec.format(value);
        let input = cx.new(|cx| InputState::new(window, cx).default_value(text));
        cx.subscribe_in(
            &input,
            window,
            move |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::Focus => this.typing = Some(id),
                InputEvent::PressEnter { .. } | InputEvent::Blur => {
                    this.typing = None;
                    this.commit_text(id, window, cx);
                }
                InputEvent::Change => {}
            },
        )
        .detach();
        self.rows.insert(id, Row { slider, input });
    }

    /// A typed number: clamp it, move the slider and commit.
    fn commit_text(&mut self, id: ControlId, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(row), Some(spec)) = (self.rows.get(id), panel::spec(id)) else {
            return;
        };
        let typed = row
            .input
            .read(cx)
            .value()
            .trim()
            .trim_start_matches('+')
            .to_owned();
        let current = lightcraft_develop::controls::get(&self.settings, id).unwrap_or(spec.default);
        match typed.parse::<f64>() {
            Ok(value) if self.editable => {
                let value = spec.clamp(value);
                if (value - current).abs() > 1e-9 {
                    cx.emit(PanelEvent::Release(id, value));
                }
                self.show_value(id, value, window, cx);
            }
            _ => self.show_value(id, current, window, cx),
        }
    }

    fn show_value(
        &mut self,
        id: ControlId,
        value: f64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (Some(row), Some(spec)) = (self.rows.get(id), panel::spec(id)) else {
            return;
        };
        let text = spec.format(value);
        row.input.update(cx, |input, cx| {
            if input.value().as_ref() != text {
                input.set_value(text, window, cx);
            }
        });
        row.slider.update(cx, |slider, cx| {
            let moved = !matches!(slider.value(), SliderValue::Single(v) if (v as f64 - value).abs() < 1e-4);
            if moved {
                slider.set_value(value as f32, window, cx);
            }
        });
    }

    /// Bring every created row in line with the pushed snapshot, except the
    /// one being dragged or typed in.
    fn sync_rows(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ids: Vec<ControlId> = self.rows.keys().copied().collect();
        for id in ids {
            let typing = self.typing == Some(id)
                || self
                    .rows
                    .get(id)
                    .is_some_and(|row| row.input.focus_handle(cx).is_focused(window));
            if self.dragging == Some(id) || typing {
                continue;
            }
            if let Some(value) = lightcraft_develop::controls::get(&self.settings, id) {
                self.show_value(id, value, window, cx);
            }
        }
    }

    fn open_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.save_open = true;
        if let Some(input) = &self.name_input {
            input.update(cx, |input, cx| {
                input.set_value("", window, cx);
                input.focus(window, cx);
            });
        }
        cx.notify();
    }

    fn confirm_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(input) = &self.name_input else {
            return;
        };
        let name = input.read(cx).value().trim().to_owned();
        if name.is_empty() {
            return;
        }
        self.save_open = false;
        input.update(cx, |input, cx| input.set_value("", window, cx));
        cx.emit(PanelEvent::SavePreset(name));
        cx.notify();
    }

    fn render_preset_bar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity();
        let save_open = self.save_open;
        let name_input = self.name_input.clone();
        let spacing = cx.theme().spacing_tokens();
        v_flex()
            .w_full()
            .gap(spacing.sm)
            .when_some(self.preset_select.clone(), |bar, select| {
                bar.child(Select::new(&select).small().placeholder("Preset"))
            })
            .child(
                h_flex()
                    .w_full()
                    .gap(spacing.sm)
                    .child(
                        Popover::new("develop-save-preset")
                            .open(save_open)
                            .on_open_change({
                                let this = this.clone();
                                move |open, window, cx| {
                                    let open = *open;
                                    this.update(cx, |panel, cx| {
                                        if open {
                                            panel.open_save(window, cx);
                                        } else {
                                            panel.save_open = false;
                                            cx.notify();
                                        }
                                    });
                                }
                            })
                            .trigger(
                                Button::new("develop-save-preset-button")
                                    .small()
                                    .outline()
                                    .label("Save preset…")
                                    .disabled(!self.editable),
                            )
                            .content({
                                let this = this.clone();
                                move |_, _, cx| {
                                    let spacing = cx.theme().spacing_tokens();
                                    let save = this.clone();
                                    let cancel = this.clone();
                                    v_flex()
                                        .id("develop-save-preset-form")
                                        .w_64()
                                        .gap(spacing.sm)
                                        .when_some(name_input.clone(), |form, input| {
                                            form.child(Input::new(&input).small())
                                        })
                                        .child(
                                            h_flex()
                                                .justify_end()
                                                .gap(spacing.sm)
                                                .child(
                                                    Button::new("develop-save-preset-cancel")
                                                        .small()
                                                        .label("Cancel")
                                                        .on_click(move |_, _, cx| {
                                                            cancel.update(cx, |panel, cx| {
                                                                panel.save_open = false;
                                                                cx.notify();
                                                            });
                                                        }),
                                                )
                                                .child(
                                                    Button::new("develop-save-preset-confirm")
                                                        .small()
                                                        .primary()
                                                        .label("Save")
                                                        .on_click(move |_, window, cx| {
                                                            save.update(cx, |panel, cx| {
                                                                panel.confirm_save(window, cx);
                                                            });
                                                        }),
                                                ),
                                        )
                                }
                            }),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("develop-reset-all")
                            .small()
                            .ghost()
                            .label("Reset all")
                            .disabled(!self.editable || self.settings.is_unedited())
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(PanelEvent::ResetAll))),
                    ),
            )
    }

    fn render_slider(
        &mut self,
        id: ControlId,
        label: &'static str,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let spec = panel::spec(id)?;
        let row = self.rows.get(id)?;
        let (slider, input) = (row.slider.clone(), row.input.clone());
        let value = lightcraft_develop::controls::get(&self.settings, id).unwrap_or(spec.default);
        let at_default = (value - spec.default).abs() < 1e-9;
        let spacing = cx.theme().spacing_tokens();
        let editable = self.editable;
        Some(
            v_flex()
                .w_full()
                .gap(spacing.xs)
                .child(
                    h_flex()
                        .id(SharedString::from(format!("develop-row-{id}")))
                        .w_full()
                        .h_6()
                        .items_center()
                        .justify_between()
                        .gap(spacing.sm)
                        .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                            if event.click_count() == 2 && this.editable {
                                cx.emit(PanelEvent::Release(id, spec.default));
                            }
                        }))
                        .child(Label::new(label).text_xs())
                        .child(
                            h_flex()
                                .items_center()
                                .gap(spacing.xs)
                                .child(
                                    Input::new(&input)
                                        .id(SharedString::from(format!("develop-value-{id}")))
                                        .xsmall()
                                        .w_16()
                                        .text_right()
                                        .disabled(!editable),
                                )
                                .child(
                                    Button::new(SharedString::from(format!("develop-reset-{id}")))
                                        .ghost()
                                        .xsmall()
                                        .icon(IconName::Undo2)
                                        .tooltip("Reset")
                                        .disabled(at_default || !editable)
                                        .on_click(cx.listener(move |_, _, _, cx| {
                                            cx.emit(PanelEvent::Release(id, spec.default));
                                        })),
                                ),
                        ),
                )
                .child(Slider::new(&slider).disabled(!editable))
                .into_any_element(),
        )
    }

    fn render_items(
        &mut self,
        section: &'static panel::PanelSection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let spacing = cx.theme().spacing_tokens();
        let muted = cx.theme().muted_foreground;
        let mut out = Vec::new();
        for item in section.items {
            match *item {
                Item::Heading(text) => out.push(
                    div()
                        .pt(spacing.sm)
                        .child(Label::new(text).text_xs().text_color(muted).font_semibold())
                        .into_any_element(),
                ),
                Item::Slider(id, label) => {
                    self.ensure_row(id, window, cx);
                    out.extend(self.render_slider(id, label, cx));
                }
                Item::Toggle(id, label) => {
                    let on = panel::toggle_get(&self.settings, id);
                    let this = cx.entity();
                    out.push(
                        Checkbox::new(SharedString::from(format!("develop-toggle-{id}")))
                            .label(label)
                            .checked(on)
                            .disabled(!self.editable)
                            .on_click(move |checked, _, cx| {
                                let checked = *checked;
                                this.update(cx, |_, cx| cx.emit(PanelEvent::Toggle(id, checked)));
                            })
                            .into_any_element(),
                    );
                }
                Item::WhiteBalanceMode => {
                    if let Some(select) = &self.wb_select {
                        out.push(
                            Select::new(select)
                                .small()
                                .disabled(!self.editable)
                                .into_any_element(),
                        );
                    }
                }
            }
        }
        out
    }
}

impl Render for DevelopPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_chrome(window, cx);
        let theme = cx.theme();
        let (bg, fg, border, muted) = (
            theme.sidebar,
            theme.sidebar_foreground,
            theme.sidebar_border,
            theme.muted_foreground,
        );
        let spacing = theme.spacing_tokens();
        let this = cx.entity();

        let mut accordion = Accordion::new("develop-sections")
            .multiple(true)
            .bordered(false)
            .small();
        for section in PANEL {
            let open = self.open.contains(&section.key);
            let edited = panel::section_is_edited(&self.settings, section.key);
            let content = if open {
                self.render_items(section, window, cx)
            } else {
                Vec::new()
            };
            let key = section.key;
            let editable = self.editable;
            let owner = this.clone();
            accordion = accordion.item(move |item| {
                item.open(open)
                    .title(
                        h_flex()
                            .w_full()
                            .items_center()
                            .justify_between()
                            .child(Label::new(section.title).text_sm().font_semibold())
                            .child(
                                Button::new(SharedString::from(format!(
                                    "develop-section-reset-{key}"
                                )))
                                .ghost()
                                .xsmall()
                                .icon(IconName::Undo2)
                                .tooltip("Reset section")
                                .disabled(!edited || !editable)
                                .on_click({
                                    move |_, _, cx| {
                                        cx.stop_propagation();
                                        owner.update(cx, |_, cx| {
                                            cx.emit(PanelEvent::ResetSection(key));
                                        });
                                    }
                                }),
                            ),
                    )
                    .child(
                        v_flex()
                            .w_full()
                            .gap(spacing.md)
                            .pb(spacing.md)
                            .children(content),
                    )
            });
        }
        accordion = accordion.on_toggle_click({
            let this = this.clone();
            move |indices, _, cx| {
                this.update(cx, |panel, cx| {
                    panel.open = indices
                        .iter()
                        .filter_map(|i| PANEL.get(*i).map(|section| section.key))
                        .collect();
                    cx.notify();
                });
            }
        });
        let status = self.status.clone();
        let preset_bar = self.render_preset_bar(cx);
        v_flex()
            .w_72()
            .h_full()
            .flex_none()
            .bg(bg)
            .text_color(fg)
            .border_l_1()
            .border_color(border)
            .child(
                v_flex()
                    .w_full()
                    .flex_none()
                    .p(spacing.md)
                    .gap(spacing.sm)
                    .border_b_1()
                    .border_color(border)
                    .child(preset_bar)
                    .when(!status.is_empty(), |header| {
                        header.child(Label::new(status).text_xs().text_color(muted))
                    }),
            )
            .child(
                div()
                    .id(self.scroll_id.clone())
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .px(spacing.md)
                    .overflow_y_scrollbar()
                    .child(accordion),
            )
    }
}
