//! Left sidebar: the IPTC metadata form of the current selection.
//!
//! The rows come from the field table (`edit_metadata::FIELDS`). Values are
//! read off the UI thread through exiftool and cached per photo. Editing
//! builds a draft; Save hands only the changed fields to the browser, which
//! writes them through the durable `metadata_write` jobs. With several
//! photos selected a field shows its value when they agree and "Mixed"
//! when they differ.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

use edit_catalog::Catalog;
use edit_catalog::models::IptcTemplate;
use edit_metadata::{
    FIELDS, FieldSpec, FieldValues, Group, Kind, Merged, TemplateFields, Writer, changed_fields,
    merge_values, read_fields,
};
use gpui_kit::component::accordion::Accordion;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::label::Label;
use gpui_kit::component::searchable_list::SearchableVec;
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::browse::{BrowseItem, item_photo_paths};

gpui_kit::actions!(metadata_sidebar, [SaveMetadata]);

/// Photos whose values stay cached; past this the cache starts over.
const CACHE_CAP: usize = 4096;
const LABEL_WIDTH: f32 = 92.0;

type Choices = SearchableVec<String>;

/// What the sidebar asks of the browser.
pub enum SidebarEvent {
    /// Write `fields` (changed ones only) to every photo of `items`.
    Save {
        items: Vec<BrowseItem>,
        fields: TemplateFields,
    },
}

impl EventEmitter<SidebarEvent> for MetadataSidebar {}

enum Control {
    Line(Entity<InputState>),
    Area(Entity<TextareaState>),
    Pick(Entity<SelectState<Choices>>),
}

struct FieldWidget {
    spec: &'static FieldSpec,
    control: Control,
}

enum Load {
    Empty,
    Loading,
    Ready,
    Failed(String),
}

pub struct MetadataSidebar {
    catalog: Arc<Catalog>,
    browser_focus: FocusHandle,
    widgets: Vec<FieldWidget>,
    open_groups: BTreeSet<usize>,
    items: Vec<BrowseItem>,
    key: Vec<String>,
    cache: HashMap<String, Arc<FieldValues>>,
    load: Load,
    loaded: BTreeMap<&'static str, Merged>,
    generation: u64,
    /// Widgets need to be reset from `loaded` (and a stashed draft).
    pending_sync: bool,
    touched: BTreeSet<&'static str>,
    /// Unsaved edits of selections the user moved away from.
    drafts: HashMap<Vec<String>, TemplateFields>,
    templates: Entity<SelectState<Choices>>,
    template_rows: Vec<IptcTemplate>,
    badge: String,
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

fn read_items(
    catalog: &Catalog,
    items: &[BrowseItem],
) -> Result<Vec<(String, FieldValues)>, String> {
    let bin = Writer::find_binary().ok_or("exiftool not found")?;
    let paths: Vec<PathBuf> = items
        .iter()
        .map(|item| {
            item_photo_paths(catalog, item)
                .into_iter()
                .next()
                .map_or_else(|| PathBuf::from(&item.display_path), PathBuf::from)
        })
        .collect();
    let values = read_fields(&bin, &paths).map_err(|e| e.to_string())?;
    Ok(items
        .iter()
        .map(|item| item.display_path.clone())
        .zip(values)
        .collect())
}

impl MetadataSidebar {
    pub fn new(
        catalog: Arc<Catalog>,
        browser_focus: FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let widgets = FIELDS
            .iter()
            .map(|spec| FieldWidget {
                spec,
                control: Self::make_control(spec, window, cx),
            })
            .collect();
        let templates = cx
            .new(|cx| SelectState::new(SearchableVec::new(Vec::<String>::new()), None, window, cx));
        cx.subscribe_in(
            &templates,
            window,
            |this, _, event: &SelectEvent<Choices>, window, cx| {
                if let SelectEvent::Confirm(Some(name)) = event {
                    this.fill_from_template(name, window, cx);
                }
            },
        )
        .detach();
        let mut sidebar = Self {
            catalog,
            browser_focus,
            widgets,
            open_groups: Group::ALL
                .iter()
                .enumerate()
                .filter(|(_, g)| g.open_by_default())
                .map(|(i, _)| i)
                .collect(),
            items: Vec::new(),
            key: Vec::new(),
            cache: HashMap::new(),
            load: Load::Empty,
            loaded: BTreeMap::new(),
            generation: 0,
            pending_sync: false,
            touched: BTreeSet::new(),
            drafts: HashMap::new(),
            templates,
            template_rows: Vec::new(),
            badge: String::new(),
        };
        sidebar.refresh_templates(window, cx);
        sidebar
    }

    fn make_control(
        spec: &'static FieldSpec,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Control {
        let id = spec.id;
        match spec.kind {
            Kind::Multi(_) => {
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

    // ---- selection and loading ----

    /// The selection changed (or its rows were rebuilt). A new set of
    /// photos stashes the current draft, then loads the new values.
    pub fn set_target(&mut self, items: Vec<BrowseItem>, cx: &mut Context<Self>) {
        let key: Vec<String> = items.iter().map(|i| i.display_path.clone()).collect();
        if key == self.key {
            self.items = items;
            return;
        }
        let draft = self.changed(cx);
        if draft.is_empty() {
            self.drafts.remove(&self.key);
        } else {
            self.drafts.insert(std::mem::take(&mut self.key), draft);
        }
        self.key = key;
        self.items = items;
        self.touched.clear();
        self.loaded.clear();
        self.start_load(cx);
        cx.notify();
    }

    /// Photos whose metadata was just written: read them again.
    pub fn invalidate(&mut self, paths: &[String], cx: &mut Context<Self>) {
        let mut hit = false;
        for path in paths {
            self.cache.remove(path);
            hit |= self.key.contains(path);
        }
        if hit {
            self.touched.clear();
            self.drafts.remove(&self.key);
            self.start_load(cx);
            cx.notify();
        }
    }

    pub fn set_badge(&mut self, badge: String, cx: &mut Context<Self>) {
        if self.badge != badge {
            self.badge = badge;
            cx.notify();
        }
    }

    /// Number of fields with unsaved edits, over the drafts of the other
    /// selections and the current one.
    pub fn unsaved_count(&self, cx: &App) -> usize {
        self.drafts.values().map(TemplateFields::len).sum::<usize>() + self.changed(cx).len()
    }

    fn start_load(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        if self.items.is_empty() {
            self.load = Load::Empty;
            self.pending_sync = false;
            return;
        }
        let missing: Vec<BrowseItem> = self
            .items
            .iter()
            .filter(|i| !self.cache.contains_key(&i.display_path))
            .cloned()
            .collect();
        if missing.is_empty() {
            self.finish_load();
            return;
        }
        self.load = Load::Loading;
        let catalog = Arc::clone(&self.catalog);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { read_items(&catalog, &missing) })
                .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(rows) => {
                        if this.cache.len() + rows.len() > CACHE_CAP {
                            this.cache.clear();
                        }
                        for (path, values) in rows {
                            this.cache.insert(path, Arc::new(values));
                        }
                        if this.generation == generation {
                            this.finish_load();
                        }
                    }
                    Err(error) if this.generation == generation => this.load = Load::Failed(error),
                    Err(_) => {}
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn finish_load(&mut self) {
        let values: Vec<FieldValues> = self
            .items
            .iter()
            .map(|i| {
                self.cache
                    .get(&i.display_path)
                    .map_or_else(FieldValues::new, |v| (**v).clone())
            })
            .collect();
        self.loaded = merge_values(&values);
        self.load = Load::Ready;
        self.pending_sync = true;
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

    /// Reset every widget to the loaded values, then lay a stashed draft
    /// for this selection on top.
    fn sync_widgets(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.pending_sync = false;
        self.touched.clear();
        let draft = self.drafts.remove(&self.key);
        for widget in &self.widgets {
            let (text, mixed) = match self.loaded.get(widget.spec.id) {
                Some(Merged::Same(value)) => (value.clone(), false),
                _ => (String::new(), true),
            };
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
    }

    fn changed(&self, cx: &App) -> TemplateFields {
        if !matches!(self.load, Load::Ready) || self.pending_sync {
            return TemplateFields::default();
        }
        let current: BTreeMap<&'static str, String> = self
            .widgets
            .iter()
            .map(|w| (w.spec.id, self.text_of(w, cx)))
            .collect();
        changed_fields(&self.loaded, &current, &self.touched)
    }

    // ---- commands ----

    pub fn save(&mut self, cx: &mut Context<Self>) {
        let fields = self.changed(cx);
        if fields.is_empty() {
            return;
        }
        cx.emit(SidebarEvent::Save {
            items: self.items.clone(),
            fields,
        });
    }

    fn revert(&mut self, cx: &mut Context<Self>) {
        self.drafts.remove(&self.key);
        self.pending_sync = matches!(self.load, Load::Ready);
        cx.notify();
    }

    fn refresh_templates(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.template_rows = self.catalog.list_iptc_templates().unwrap_or_default();
        self.template_rows
            .sort_by_key(|row| row.name.to_lowercase());
        let names: Vec<String> = self.template_rows.iter().map(|r| r.name.clone()).collect();
        self.templates.update(cx, |select, cx| {
            select.set_items(SearchableVec::new(names), window, cx);
            select.set_selected_index(None, window, cx);
        });
    }

    /// Put a saved template's values into the draft. Variables stay as
    /// typed and expand per photo when the draft is saved.
    fn fill_from_template(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.template_rows.iter().find(|r| r.name == name) else {
            return;
        };
        let fields: TemplateFields = serde_json::from_str(&row.fields_json).unwrap_or_default();
        if matches!(self.load, Load::Ready) && !self.pending_sync {
            for widget in &self.widgets {
                let Some(text) = fields.get(widget.spec.id) else {
                    continue;
                };
                if matches!(widget.control, Control::Pick(_))
                    && label_for(widget.spec, text).is_none()
                {
                    continue;
                }
                self.set_text(widget, text, false, window, cx);
                self.touched.insert(widget.spec.id);
            }
        }
        self.templates
            .update(cx, |select, cx| select.set_selected_index(None, window, cx));
        cx.notify();
    }

    // ---- rendering ----

    fn title(&self) -> String {
        match self.items.as_slice() {
            [] => String::new(),
            [one] => one.base_name.clone(),
            many => format!("{} photos", many.len()),
        }
    }

    fn render_control(&self, widget: &FieldWidget) -> AnyElement {
        let spec = widget.spec;
        let id = SharedString::from(format!("meta-{}", spec.id));
        match &widget.control {
            Control::Line(state) => Input::new(state).small().id(id).into_any_element(),
            Control::Area(state) => {
                let rows = match spec.kind {
                    Kind::Multi(rows) => rows,
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
        h_flex()
            .w_full()
            .gap_2()
            .items_start()
            .child(
                div()
                    .w(px(LABEL_WIDTH))
                    .flex_none()
                    .pt(px(5.0))
                    .child(Label::new(widget.spec.label).text_xs().text_color(color)),
            )
            .child(div().flex_1().min_w_0().child(self.render_control(widget)))
    }

    fn render_form(&self, edited: &TemplateFields, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity().downgrade();
        let mut accordion = Accordion::new("metadata-groups")
            .multiple(true)
            .small()
            .h_auto()
            .on_toggle_click(move |open, _, cx| {
                let open: BTreeSet<usize> = open.iter().copied().collect();
                this.update(cx, |this, cx| {
                    this.open_groups = open;
                    cx.notify();
                })
                .ok();
            });
        for (index, group) in Group::ALL.iter().enumerate() {
            let rows: Vec<_> = self
                .widgets
                .iter()
                .filter(|w| w.spec.group == *group)
                .collect();
            let edits = rows.iter().filter(|w| edited.contains(w.spec.id)).count();
            let content = v_flex().gap_1p5().pb_2().children(
                rows.into_iter()
                    .map(|w| self.render_row(w, edited.contains(w.spec.id), cx)),
            );
            let primary = cx.theme().primary;
            let open = self.open_groups.contains(&index);
            accordion = accordion.item(move |item| {
                item.open(open)
                    .title(
                        h_flex()
                            .gap_2()
                            .child(Label::new(group.label()).text_sm())
                            .when(edits > 0, |row| {
                                row.child(
                                    Label::new(edits.to_string()).text_xs().text_color(primary),
                                )
                            }),
                    )
                    .child(content)
            });
        }
        accordion
    }
}

impl Render for MetadataSidebar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.pending_sync && matches!(self.load, Load::Ready) {
            self.sync_widgets(window, cx);
        }
        let edited = self.changed(cx);
        let dirty = !edited.is_empty();
        let ready = matches!(self.load, Load::Ready);
        let theme = cx.theme();
        let (bg, fg, border, muted, warning) = (
            theme.sidebar,
            theme.sidebar_foreground,
            theme.sidebar_border,
            theme.muted_foreground,
            theme.warning,
        );
        let note = match &self.load {
            Load::Empty => "No photo selected".to_owned(),
            Load::Loading => "Reading metadata".to_owned(),
            Load::Failed(error) => error.clone(),
            Load::Ready => String::new(),
        };
        let status = if dirty {
            "Unsaved".to_owned()
        } else {
            self.badge.clone()
        };

        v_flex()
            .id("metadata-sidebar")
            .w(px(300.0))
            .h_full()
            .flex_none()
            .bg(bg)
            .text_color(fg)
            .border_r_1()
            .border_color(border)
            .on_action(cx.listener(|this, _: &SaveMetadata, _, cx| this.save(cx)))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" {
                    window.focus(&this.browser_focus, cx);
                }
            }))
            .child(
                h_flex()
                    .px_3()
                    .pt_2()
                    .pb_1()
                    .justify_between()
                    .child(Label::new(self.title()).text_sm())
                    .child(Label::new(status).text_xs().text_color(warning)),
            )
            .child(
                v_flex()
                    .px_3()
                    .pb_2()
                    .gap_1p5()
                    .child(
                        Select::new(&self.templates)
                            .small()
                            .placeholder("Fill from template")
                            .id("meta-template"),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("meta-save")
                                    .primary()
                                    .small()
                                    .label("Save")
                                    .disabled(!dirty)
                                    .on_click(
                                        cx.listener(|this, _: &ClickEvent, _, cx| this.save(cx)),
                                    ),
                            )
                            .child(
                                Button::new("meta-revert")
                                    .small()
                                    .label("Revert")
                                    .disabled(!dirty)
                                    .on_click(
                                        cx.listener(|this, _: &ClickEvent, _, cx| this.revert(cx)),
                                    ),
                            ),
                    ),
            )
            .child(div().h(px(1.0)).w_full().bg(border))
            .child(if ready {
                div()
                    .id("metadata-scroll")
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .overflow_y_scroll()
                    .px_3()
                    .child(self.render_form(&edited, cx))
                    .into_any_element()
            } else {
                div()
                    .px_3()
                    .py_2()
                    .child(Label::new(note).text_xs().text_color(muted))
                    .into_any_element()
            })
    }
}
