//! The IPTC field form: one widget per row of the field table
//! (`edit_metadata::FIELDS`), grouped in sections. The sidebar edits the
//! selection's metadata with it and the ingest dialog drafts the fields to
//! stamp, so both show the same rows with the same widgets.
//!
//! The form tracks a baseline (`loaded`) and the fields the user touched;
//! [`FieldsForm::changed`] is the difference between the widgets and that
//! baseline.

use std::collections::{BTreeMap, BTreeSet};

use edit_metadata::{FIELDS, FieldSpec, Group, Kind, Merged, TemplateFields, changed_fields};
use gpui_kit::component::accordion::Accordion;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::label::Label;
use gpui_kit::component::searchable_list::SearchableVec;
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, IconName, Sizable as _, StyledExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

type Choices = SearchableVec<String>;

enum Control {
    Line(Entity<InputState>),
    Area(Entity<TextareaState>),
    Pick(Entity<SelectState<Choices>>),
}

struct FieldWidget {
    spec: &'static FieldSpec,
    control: Control,
}

pub struct FieldsForm {
    /// Element id prefix, so two forms in one window never share ids.
    prefix: &'static str,
    widgets: Vec<FieldWidget>,
    open_groups: BTreeSet<usize>,
    loaded: BTreeMap<&'static str, Merged>,
    touched: BTreeSet<&'static str>,
}

/// Option labels of a select field, plus the current value when it is not
/// one of the options.
fn pick_labels(spec: &FieldSpec, extra: Option<&str>) -> Vec<String> {
    let mut labels: Vec<String> = match spec.kind {
        Kind::Select(options) => options.iter().map(|(l, _)| (*l).to_owned()).collect(),
        _ => Vec::new(),
    };
    if let Some(extra) = extra.filter(|v| !v.is_empty())
        && label_for(spec, extra).is_none()
    {
        labels.push(extra.to_owned());
    }
    labels
}

fn label_for(spec: &FieldSpec, value: &str) -> Option<&'static str> {
    match spec.kind {
        Kind::Select(options) => options.iter().find(|(_, v)| *v == value).map(|(l, _)| *l),
        _ => None,
    }
}

fn value_for(spec: &FieldSpec, label: &str) -> String {
    match spec.kind {
        Kind::Select(options) => options
            .iter()
            .find(|(l, _)| *l == label)
            .map_or_else(|| label.to_owned(), |(_, v)| (*v).to_owned()),
        _ => label.to_owned(),
    }
}

impl FieldsForm {
    pub fn new(prefix: &'static str, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let widgets = FIELDS
            .iter()
            .map(|spec| FieldWidget {
                spec,
                control: Self::make_control(spec, window, cx),
            })
            .collect();
        Self {
            prefix,
            widgets,
            open_groups: Group::ALL
                .iter()
                .enumerate()
                .filter(|(_, g)| g.open_by_default())
                .map(|(i, _)| i)
                .collect(),
            loaded: BTreeMap::new(),
            touched: BTreeSet::new(),
        }
    }

    fn make_control(
        spec: &'static FieldSpec,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Control {
        let id = spec.id;
        match spec.kind {
            Kind::Multi(_) | Kind::List => {
                let state = cx.new(|cx| TextareaState::new(window, cx));
                cx.subscribe(&state, move |this, _, event: &InputEvent, cx| {
                    this.edited(id, event, cx)
                })
                .detach();
                Control::Area(state)
            }
            Kind::Select(_) => {
                let state = cx.new(|cx| {
                    SelectState::new(
                        SearchableVec::new(pick_labels(spec, None)),
                        None,
                        window,
                        cx,
                    )
                });
                cx.subscribe(&state, move |this, _, _: &SelectEvent<Choices>, cx| {
                    this.touched.insert(id);
                    cx.notify();
                })
                .detach();
                Control::Pick(state)
            }
            _ => {
                let state = cx.new(|cx| InputState::new(window, cx));
                cx.subscribe(&state, move |this, _, event: &InputEvent, cx| {
                    this.edited(id, event, cx)
                })
                .detach();
                Control::Line(state)
            }
        }
    }

    fn edited(&mut self, id: &'static str, event: &InputEvent, cx: &mut Context<Self>) {
        if matches!(event, InputEvent::Change) {
            self.touched.insert(id);
            cx.notify();
        }
    }

    // ---- widgets <-> values ----

    fn text_of(&self, widget: &FieldWidget, cx: &App) -> String {
        match &widget.control {
            Control::Line(state) => state.read(cx).value().to_string(),
            Control::Area(state) => state.read(cx).value().to_string(),
            Control::Pick(state) => match state.read(cx).selected_value() {
                Some(label) => value_for(widget.spec, label),
                None => String::new(),
            },
        }
    }

    fn set_text(
        &self,
        widget: &FieldWidget,
        text: &str,
        mixed: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match &widget.control {
            Control::Line(state) => state.update(cx, |state, cx| {
                state.set_placeholder(if mixed { "Mixed" } else { "" }, window, cx);
                state.set_value(text.to_owned(), window, cx);
            }),
            Control::Area(state) => state.update(cx, |state, cx| {
                state.set_placeholder(if mixed { "Mixed" } else { "" }, window, cx);
                state.set_value(text.to_owned(), window, cx);
            }),
            Control::Pick(state) => {
                let spec = widget.spec;
                state.update(cx, |state, cx| {
                    state.set_items(
                        SearchableVec::new(pick_labels(spec, Some(text))),
                        window,
                        cx,
                    );
                    if mixed {
                        state.set_selected_index(None, window, cx);
                    } else {
                        let label =
                            label_for(spec, text).map_or_else(|| text.to_owned(), str::to_owned);
                        state.set_selected_value(&label, window, cx);
                    }
                });
            }
        }
    }

    /// Make `loaded` the baseline, reset every widget to it, then lay a
    /// draft on top (its fields count as touched).
    pub fn reset(
        &mut self,
        loaded: BTreeMap<&'static str, Merged>,
        draft: Option<&TemplateFields>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.loaded = loaded;
        self.touched.clear();
        for widget in &self.widgets {
            let (text, mixed) = self.baseline(widget.spec.id);
            self.set_text(widget, &text, mixed, window, cx);
        }
        if let Some(draft) = draft {
            for widget in &self.widgets {
                if let Some(text) = draft.get(widget.spec.id) {
                    self.set_text(widget, text, false, window, cx);
                    self.touched.insert(widget.spec.id);
                }
            }
        }
        cx.notify();
    }

    /// Empty every field; the baseline is "nothing set".
    pub fn clear(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let empty = FIELDS
            .iter()
            .map(|spec| (spec.id, Merged::Same(String::new())))
            .collect();
        self.reset(empty, None, window, cx);
    }

    /// What a field shows at the baseline: its text and whether it is mixed.
    fn baseline(&self, id: &str) -> (String, bool) {
        match self.loaded.get(id) {
            Some(Merged::Same(value)) => (value.clone(), false),
            _ => (String::new(), true),
        }
    }

    /// Put a template's values into the form. Variables stay as typed.
    /// Select values that are not options are skipped.
    pub fn fill(&mut self, fields: &TemplateFields, window: &mut Window, cx: &mut Context<Self>) {
        for widget in &self.widgets {
            let Some(text) = fields.get(widget.spec.id) else {
                continue;
            };
            if matches!(widget.control, Control::Pick(_)) && label_for(widget.spec, text).is_none()
            {
                continue;
            }
            self.set_text(widget, text, false, window, cx);
            self.touched.insert(widget.spec.id);
        }
        cx.notify();
    }

    /// The fields whose text differs from the baseline.
    pub fn changed(&self, cx: &App) -> TemplateFields {
        let current: BTreeMap<&'static str, String> = self
            .widgets
            .iter()
            .map(|w| (w.spec.id, self.text_of(w, cx)))
            .collect();
        changed_fields(&self.loaded, &current, &self.touched)
    }

    /// Put one section's fields back to the baseline.
    fn revert_group(&mut self, group: Group, window: &mut Window, cx: &mut Context<Self>) {
        for widget in self.widgets.iter().filter(|w| w.spec.group == group) {
            let (text, mixed) = self.baseline(widget.spec.id);
            self.set_text(widget, &text, mixed, window, cx);
            self.touched.remove(widget.spec.id);
        }
        cx.notify();
    }

    // ---- rendering ----

    fn render_control(&self, widget: &FieldWidget) -> AnyElement {
        let spec = widget.spec;
        let id = SharedString::from(format!("{}-{}", self.prefix, spec.id));
        match &widget.control {
            Control::Line(state) => Input::new(state).small().id(id).into_any_element(),
            Control::Area(state) => {
                let rows = match spec.kind {
                    Kind::Multi(rows) => rows,
                    Kind::List if spec.id == "keywords" => 3,
                    Kind::List => 2,
                    _ => 3,
                };
                Textarea::new(state)
                    .h(px(rows as f32 * 18.0 + 8.0))
                    .into_any_element()
            }
            Control::Pick(state) => Select::new(state)
                .small()
                .placeholder("Mixed")
                .id(id)
                .into_any_element(),
        }
    }

    fn render_row(&self, widget: &FieldWidget, edited: bool, cx: &App) -> impl IntoElement {
        let theme = cx.theme();
        let color = if edited {
            theme.primary
        } else {
            theme.muted_foreground
        };
        v_flex()
            .w_full()
            .gap(cx.theme().spacing_tokens().xs)
            .child(
                div()
                    .w_full()
                    .flex_none()
                    .truncate()
                    .child(Label::new(widget.spec.label).text_xs().text_color(color)),
            )
            .child(self.render_control(widget))
    }
}

impl Render for FieldsForm {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let edited = self.changed(cx);
        let spacing = cx.theme().spacing_tokens();
        let this = cx.entity();
        let prefix = self.prefix;
        let mut accordion = Accordion::new(SharedString::from(format!("{prefix}-groups")))
            .multiple(true)
            .bordered(false)
            .small()
            .h_auto();
        for (index, group) in Group::ALL.iter().enumerate() {
            let rows: Vec<_> = self
                .widgets
                .iter()
                .filter(|w| w.spec.group == *group)
                .collect();
            let edits = rows.iter().filter(|w| edited.contains(w.spec.id)).count();
            let open = self.open_groups.contains(&index);
            let rows = if open { rows } else { Vec::new() };
            let content = v_flex()
                .w_full()
                .gap(spacing.md)
                .when(open, |col| col.pb(spacing.md))
                .children(
                    rows.into_iter()
                        .map(|w| self.render_row(w, edited.contains(w.spec.id), cx)),
                );
            let primary = cx.theme().primary;
            let owner = this.clone();
            accordion = accordion.item(move |item| {
                item.open(open)
                    .title(
                        h_flex()
                            .w_full()
                            .items_center()
                            .justify_between()
                            .child(
                                h_flex()
                                    .gap(spacing.sm)
                                    .child(Label::new(group.label()).text_sm().font_semibold())
                                    .when(edits > 0, |row| {
                                        row.child(
                                            Label::new(edits.to_string())
                                                .text_xs()
                                                .text_color(primary),
                                        )
                                    }),
                            )
                            .child(
                                Button::new(SharedString::from(format!(
                                    "{prefix}-group-revert-{index}"
                                )))
                                .ghost()
                                .xsmall()
                                .icon(IconName::Undo2)
                                .tooltip("Revert section")
                                .disabled(edits == 0)
                                .on_click(
                                    move |_, window, cx| {
                                        cx.stop_propagation();
                                        owner.update(cx, |form, cx| {
                                            form.revert_group(*group, window, cx);
                                        });
                                    },
                                ),
                            ),
                    )
                    .when(open, |item| item.child(content))
            });
        }
        accordion.on_toggle_click(move |open, _, cx| {
            let open: BTreeSet<usize> = open.iter().copied().collect();
            this.update(cx, |this, cx| {
                this.open_groups = open;
                cx.notify();
            });
        })
    }
}
