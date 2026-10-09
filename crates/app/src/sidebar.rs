//! Left sidebar: the IPTC metadata form of the current selection.
//!
//! The rows come from the field table (`edit_metadata::FIELDS`). Values are
//! read off the UI thread through exiftool and cached per photo. Editing
//! builds a draft; Save hands only the changed fields to the browser, which
//! writes them through the durable `metadata_write` jobs. With several
//! photos selected a field shows its value when they agree and "Mixed"
//! when they differ.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

use edit_catalog::Catalog;
use edit_catalog::models::IptcTemplate;
use edit_metadata::{FieldValues, Merged, TemplateFields, Writer, merge_values, read_fields};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::label::Label;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::searchable_list::SearchableVec;
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, StyledExt as _, h_flex, v_flex,
};
use gpui_kit::*;

use crate::browse::{BrowseItem, item_photo_paths};
use crate::fields_form::FieldsForm;

gpui_kit::actions!(metadata_sidebar, [SaveMetadata]);

/// Photos whose values stay cached; past this the cache starts over.
const CACHE_CAP: usize = 4096;

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

enum Load {
    Empty,
    Loading,
    Ready,
    Failed(String),
}

pub struct MetadataSidebar {
    catalog: Arc<Catalog>,
    browser_focus: FocusHandle,
    form: Entity<FieldsForm>,
    items: Vec<BrowseItem>,
    key: Vec<String>,
    cache: HashMap<String, Arc<FieldValues>>,
    load: Load,
    loaded: BTreeMap<&'static str, Merged>,
    generation: u64,
    /// Widgets need to be reset from `loaded` (and a stashed draft).
    pending_sync: bool,
    /// Unsaved edits of selections the user moved away from.
    drafts: HashMap<Vec<String>, TemplateFields>,
    templates: Entity<SelectState<Choices>>,
    template_rows: Vec<IptcTemplate>,
    badge: String,
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
        let form = cx.new(|cx| FieldsForm::new("meta", window, cx));
        cx.observe(&form, |_, _, cx| cx.notify()).detach();
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
            form,
            items: Vec::new(),
            key: Vec::new(),
            cache: HashMap::new(),
            load: Load::Empty,
            loaded: BTreeMap::new(),
            generation: 0,
            pending_sync: false,
            drafts: HashMap::new(),
            templates,
            template_rows: Vec::new(),
            badge: String::new(),
        };
        sidebar.refresh_templates(window, cx);
        sidebar
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

    /// Reset the form to the loaded values, then lay a stashed draft for
    /// this selection on top.
    fn sync_widgets(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.pending_sync = false;
        let draft = self.drafts.remove(&self.key);
        let loaded = self.loaded.clone();
        self.form.update(cx, |form, cx| {
            form.reset(loaded, draft.as_ref(), window, cx)
        });
    }

    fn changed(&self, cx: &App) -> TemplateFields {
        if !matches!(self.load, Load::Ready) || self.pending_sync {
            return TemplateFields::default();
        }
        self.form.read(cx).changed(cx)
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
            self.form
                .update(cx, |form, cx| form.fill(&fields, window, cx));
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

        let spacing = theme.spacing_tokens();
        v_flex()
            .id("metadata-sidebar")
            .w_72()
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
                v_flex()
                    .w_full()
                    .flex_none()
                    .p(spacing.md)
                    .gap(spacing.sm)
                    .border_b_1()
                    .border_color(border)
                    .child(
                        h_flex()
                            .justify_between()
                            .child(Label::new(self.title()).text_sm().font_semibold())
                            .child(Label::new(status).text_xs().text_color(warning)),
                    )
                    .child(
                        Select::new(&self.templates)
                            .small()
                            .placeholder("Fill from template")
                            .id("meta-template"),
                    )
                    .child(
                        h_flex()
                            .gap(spacing.sm)
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
                                    .outline()
                                    .label("Revert")
                                    .disabled(!dirty)
                                    .on_click(
                                        cx.listener(|this, _: &ClickEvent, _, cx| this.revert(cx)),
                                    ),
                            ),
                    ),
            )
            .child(if ready {
                div()
                    .id("metadata-scroll")
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .px(spacing.md)
                    .overflow_y_scrollbar()
                    .child(self.form.clone())
                    .into_any_element()
            } else {
                div()
                    .p(spacing.md)
                    .child(Label::new(note).text_xs().text_color(muted))
                    .into_any_element()
            })
    }
}
