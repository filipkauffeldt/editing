//! IPTC template editor and apply flow (C3 phase 1).
//!
//! The editor is the `Form`/`Input`/`Textarea`/`Select` surface over the
//! phase-1 field set (`edit_metadata::TemplateFields`), stored in the
//! catalog `iptc_templates` table. "Apply to selection" applies what the
//! form shows right now, saved or not, and hands it to
//! [`Browser::apply_template`], which enqueues one durable
//! `metadata_write` job per selected item (plan sections 2.1, 2.7). `T`
//! opens the editor from anywhere in the shell.
//!
//! Fields may contain `{…}` variables. The Preview rows expand every
//! field against the focused photo's capture metadata off the UI thread
//! ([`edit_metadata::preview_fields`]) before apply, so users see the
//! resolved text (plan section 2.7). A loaded Photo Mechanic code
//! replacement file adds `=code=` / hot-code expansion to Preview and to
//! the durable apply payloads.

use std::sync::Arc;

use edit_catalog::Catalog;
use edit_catalog::models::IptcTemplate;
use edit_metadata::{CodeReplacements, FIELDS, FieldSpec, Group, Kind, TemplateFields};
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::form::{Field, Form};
use gpui_kit::component::input::{Input, InputState, Textarea, TextareaState};
use gpui_kit::component::label::Label;
use gpui_kit::component::searchable_list::SearchableVec;
use gpui_kit::component::select::{Select, SelectState};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::views::Browser;

actions!(templates, [OpenTemplates]);

use crate::ingest_job::REPLACEMENTS_SETTING;

/// The overlay dialog. Owns the form and the catalog rows; the browser
/// owns the job queue and badges, so apply is a thin read + enqueue.
pub struct TemplateEditor {
    browser: Entity<Browser>,
    catalog: Arc<Catalog>,
    /// Template row being edited; `None` means the next Save inserts.
    editing_id: Option<i64>,
    /// Visible template rows (picker + apply share the same list).
    templates: Vec<IptcTemplate>,
    names: Entity<SelectState<SearchableVec<String>>>,
    name: Entity<InputState>,
    /// One widget per row of the field table, in table order.
    widgets: Vec<(&'static FieldSpec, FieldInput)>,
    /// Path input for the PM-format code replacement file.
    replacement_path: Entity<InputState>,
    /// Parsed codes used by Preview and baked into Apply payloads.
    replacements: CodeReplacements,
    replacement_status: String,
    /// Resolved `(label, text)` rows for the focused photo.
    preview: Vec<(&'static str, String)>,
    status: String,
}

/// Values collected from one input; an empty box registers no field.
fn value_of(state: &Entity<InputState>, cx: &App) -> Option<String> {
    let text: String = state.read(cx).value().into();
    let trimmed = text.trim().to_owned();
    (!trimmed.is_empty()).then_some(trimmed)
}

/// The widget behind one table row: a text box, or a textarea for the
/// multi-line kinds. Template values are free text (selects included),
/// so `{variables}` work everywhere.
enum FieldInput {
    Line(Entity<InputState>),
    Area(Entity<TextareaState>),
}

impl FieldInput {
    fn new(spec: &FieldSpec, window: &mut Window, cx: &mut Context<TemplateEditor>) -> Self {
        match spec.kind {
            Kind::Multi(_) => Self::Area(cx.new(|cx| TextareaState::new(window, cx))),
            _ => Self::Line(cx.new(|cx| InputState::new(window, cx))),
        }
    }

    /// Trimmed text; an empty box registers no field.
    fn value(&self, cx: &App) -> Option<String> {
        let text: String = match self {
            Self::Line(state) => state.read(cx).value().into(),
            Self::Area(state) => state.read(cx).value().into(),
        };
        let trimmed = text.trim().to_owned();
        (!trimmed.is_empty()).then_some(trimmed)
    }

    fn set(&self, value: Option<&str>, window: &mut Window, cx: &mut Context<TemplateEditor>) {
        let text = value.unwrap_or_default().to_owned();
        match self {
            Self::Line(state) => state.update(cx, |state, cx| state.set_value(text, window, cx)),
            Self::Area(state) => state.update(cx, |state, cx| state.set_value(text, window, cx)),
        }
    }

    fn render(&self, spec: &FieldSpec) -> AnyElement {
        match (self, spec.kind) {
            (Self::Line(state), _) => Input::new(state).id(spec.id).into_any_element(),
            (Self::Area(state), Kind::Multi(rows)) => Textarea::new(state)
                .h(px(rows as f32 * 18.0))
                .into_any_element(),
            (Self::Area(state), _) => Textarea::new(state).into_any_element(),
        }
    }
}

impl TemplateEditor {
    pub fn new(
        browser: Entity<Browser>,
        catalog: Arc<Catalog>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let names = cx
            .new(|cx| SelectState::new(SearchableVec::new(Vec::<String>::new()), None, window, cx));
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("Press pass"));
        let widgets = FIELDS
            .iter()
            .map(|spec| (spec, FieldInput::new(spec, window, cx)))
            .collect();
        let replacement_path =
            cx.new(|cx| InputState::new(window, cx).placeholder("/path/to/code-replacements.tsv"));
        // Restore the last replacement file and parse it now, so Preview
        // and Apply have codes before the user touches the picker.
        let mut replacements = CodeReplacements::default();
        let mut replacement_status = "No code replacement file loaded.".to_string();
        if let Some(path) = catalog.get_setting(REPLACEMENTS_SETTING).ok().flatten() {
            replacement_path.update(cx, |state, cx| state.set_value(path.clone(), window, cx));
            match CodeReplacements::from_file(std::path::Path::new(&path)) {
                Ok(loaded) => {
                    replacement_status = format!("Loaded {} code(s) from {path}.", loaded.len());
                    replacements = loaded;
                }
                Err(e) => replacement_status = format!("Could not read {path}: {e}"),
            }
        }

        let mut editor = Self {
            browser,
            catalog,
            editing_id: None,
            templates: Vec::new(),
            names,
            name,
            widgets,
            replacement_path,
            replacements,
            replacement_status,
            preview: Vec::new(),
            status: "Fill the fields, Save, then pick and Apply to selection.".to_string(),
        };
        editor.reload(None, window, cx);
        editor
    }

    /// Reload template rows from the catalog and rebuild the picker,
    /// selecting `prefer` (a just-saved name) or else whatever was picked.
    /// Names are unique (catalog migration 003), so a name identifies a row.
    fn reload(&mut self, prefer: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        self.templates = self.catalog.list_iptc_templates().unwrap_or_default();
        self.templates.sort_by_key(|row| row.name.to_lowercase());
        let names: Vec<String> = self.templates.iter().map(|row| row.name.clone()).collect();
        let selected: Option<String> = prefer
            .or_else(|| self.names.read(cx).selected_value().cloned())
            .filter(|name| names.contains(name))
            .or_else(|| self.templates.first().map(|row| row.name.clone()));
        self.names.update(cx, |select, cx| {
            select.set_items(SearchableVec::new(names), window, cx);
            if let Some(selected) = selected {
                select.set_selected_value(&selected, window, cx);
            }
        });
        cx.notify();
    }

    /// Form values as the stored model. A blank box registers no field.
    fn fields(&self, cx: &App) -> TemplateFields {
        let mut fields = TemplateFields::default();
        for (spec, input) in &self.widgets {
            fields.set(spec.id, input.value(cx));
        }
        fields
    }

    fn selected_template(&self, cx: &App) -> Option<IptcTemplate> {
        let name = self.names.read(cx).selected_value().cloned()?;
        self.templates.iter().find(|row| row.name == name).cloned()
    }

    fn set_input(
        &self,
        state: &Entity<InputState>,
        value: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = value.unwrap_or_default().to_owned();
        state.update(cx, |state, cx| state.set_value(text, window, cx));
    }

    /// Fill the form from a template row and mark the editor as editing
    /// that row (Save updates; Delete removes it).
    fn fill_from(&mut self, row: &IptcTemplate, window: &mut Window, cx: &mut Context<Self>) {
        let fields: TemplateFields = serde_json::from_str(&row.fields_json).unwrap_or_default();
        self.editing_id = Some(row.id);
        self.set_input(&self.name, Some(&row.name), window, cx);
        for (spec, input) in &self.widgets {
            input.set(fields.get(spec.id), window, cx);
        }
        self.preview.clear();
        self.status = format!("Editing template \"{}\".", row.name);
        cx.notify();
    }

    /// Clear the form for a fresh template (Save inserts).
    fn start_new(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editing_id = None;
        self.set_input(&self.name, None, window, cx);
        for (_, input) in &self.widgets {
            input.set(None, window, cx);
        }
        self.preview.clear();
        self.status = "Fill the fields, Save, then pick and Apply to selection.".to_string();
        cx.notify();
    }

    /// Persist the form: update the loaded row or insert a new one.
    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = value_of(&self.name, cx).unwrap_or_default();
        if name.is_empty() {
            self.status = "Template needs a name.".to_string();
            cx.notify();
            return;
        }
        let fields_json =
            serde_json::to_string(&self.fields(cx)).unwrap_or_else(|_| "{}".to_string());
        let outcome = match self.editing_id {
            Some(id) => {
                // Editing keeps the row's scope (an ingest-scoped snapshot
                // stays ingest-scoped); only a new row starts global.
                let scope = self
                    .templates
                    .iter()
                    .find(|row| row.id == id)
                    .map_or("global", |row| row.scope.as_str());
                self.catalog
                    .update_iptc_template(id, &name, &fields_json, scope)
                    .map(|_| id)
            }
            None => self
                .catalog
                .insert_iptc_template(&name, &fields_json, "global"),
        };
        let saved_name = match outcome {
            Ok(id) => {
                self.editing_id = Some(id);
                self.status = format!("Saved template \"{name}\".");
                Some(name)
            }
            Err(e) if e.is_unique_violation() => {
                self.status = format!("A template named \"{name}\" already exists.");
                None
            }
            Err(e) => {
                self.status = format!("Save failed: {e}");
                None
            }
        };
        self.reload(saved_name, window, cx);
    }

    fn delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.editing_id else {
            self.status = "Nothing loaded to delete.".to_string();
            cx.notify();
            return;
        };
        let name = self
            .templates
            .iter()
            .find(|row| row.id == id)
            .map(|row| row.name.clone())
            .unwrap_or_default();
        self.editing_id = None;
        match self.catalog.delete_iptc_template(id) {
            Ok(()) => self.status = format!("Deleted template \"{name}\"."),
            Err(e) => self.status = format!("Delete failed: {e}"),
        }
        self.start_new(window, cx);
        self.reload(None, window, cx);
    }

    /// Load the template selected in the picker into the form.
    fn load_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.selected_template(cx) {
            Some(row) => self.fill_from(&row, window, cx),
            None => {
                self.status = "Select a saved template first.".to_string();
                cx.notify();
            }
        }
    }

    /// Hand what the form shows to the browser for the multi-selection,
    /// whether or not it is saved or matches the picked row: the user
    /// applies what they see. The browser enqueues and reports; the
    /// editor mirrors its status line.
    fn apply_selected(&mut self, cx: &mut Context<Self>) {
        let fields = self.fields(cx);
        if fields.is_empty() {
            self.status = "The form has no fields to write.".to_string();
            cx.notify();
            return;
        }
        let name = value_of(&self.name, cx).unwrap_or_else(|| "unsaved".to_string());
        let replacements = (!self.replacements.is_empty()).then(|| self.replacements.clone());
        self.browser.update(cx, |browser, cx| {
            browser.apply_template_with(fields, &name, replacements, cx);
        });
        self.status = self.browser.read(cx).template_status();
        cx.notify();
    }

    /// Read the PM-format file at the path input, keep the parsed codes
    /// for Preview and Apply, and remember the path for the next start.
    fn load_replacements(&mut self, cx: &mut Context<Self>) {
        let path = value_of(&self.replacement_path, cx).unwrap_or_default();
        if path.is_empty() {
            self.replacements = CodeReplacements::default();
            self.replacement_status = "No code replacement file loaded.".to_string();
            cx.notify();
            return;
        }
        match CodeReplacements::from_file(std::path::Path::new(&path)) {
            Ok(loaded) => {
                let count = loaded.len();
                self.replacements = loaded;
                self.replacement_status = format!("Loaded {count} code(s) from {path}.");
                if let Err(e) = self.catalog.set_setting(REPLACEMENTS_SETTING, &path) {
                    self.replacement_status =
                        format!("Loaded {count} code(s); saving the path failed: {e}");
                }
            }
            Err(e) => {
                self.replacements = CodeReplacements::default();
                self.replacement_status = format!("Could not read {path}: {e}");
            }
        }
        cx.notify();
    }

    /// Expand every form field against the focused photo in the
    /// background; the resolved rows land under the form (plan section
    /// 2.7).
    fn preview_fields(&mut self, cx: &mut Context<Self>) {
        let fields = self.fields(cx);
        let Some(path) = self.browser.read(cx).current_display_path() else {
            self.preview.clear();
            self.status = "Open a folder and focus a photo to preview.".to_string();
            cx.notify();
            return;
        };
        let replacements = self.replacements.clone();
        cx.spawn(async move |this, cx| {
            let resolved = cx.background_executor().spawn(async move {
                edit_metadata::preview_fields_with(
                    std::path::Path::new(&path),
                    &fields,
                    1,
                    edit_catalog::db::now_unix(),
                    Some(&replacements),
                )
            });
            let resolved = resolved.await;
            this.update(cx, |this, cx| {
                this.preview = resolved;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn render_template_picker(&self) -> impl IntoElement {
        let names = self
            .templates
            .iter()
            .map(|row| row.name.clone())
            .collect::<Vec<_>>();
        let picker = Select::new(&self.names).placeholder("Select a saved template");
        let picker_label = if names.is_empty() {
            "No templates yet — Save this one first.".to_string()
        } else {
            format!("{} saved template(s).", names.len())
        };
        v_flex().gap_1().child(picker).child(
            div()
                .text_xs()
                .text_color(rgb(0x999999))
                .child(picker_label),
        )
    }

    /// Path input for the replacement file plus its load status. The
    /// parsed set feeds Preview and the Apply payloads.
    fn render_code_replacements(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        v_flex()
            .gap_1()
            .child(Label::new(
                "Code replacement file (PM TSV; =code= and hot codes)",
            ))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .child(Input::new(&self.replacement_path).id("template-code-file")),
                    )
                    .child(
                        Button::new("template-code-load")
                            .label("Load codes")
                            .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                                this.load_replacements(cx);
                            })),
                    ),
            )
            .child(Label::new(self.replacement_status.clone()).text_color(muted))
    }

    fn render_preview(&self) -> impl IntoElement {
        let rows = self.preview.iter().map(|(label, text)| {
            let shown = if text.is_empty() {
                "(empty, skipped)".to_string()
            } else {
                text.clone()
            };
            div()
                .text_xs()
                .text_color(rgb(0xDDC999))
                .child(format!("{label}: {shown}"))
        });
        v_flex()
            .gap_1()
            .when(!self.preview.is_empty(), |col| {
                col.child(
                    div()
                        .text_xs()
                        .text_color(rgb(0x999999))
                        .child("Preview (focused photo, sequence 1):".to_string()),
                )
            })
            .children(rows)
    }

    fn render_form(&self) -> impl IntoElement {
        let name = Field::new()
            .label("Name")
            .child(Input::new(&self.name).id("template-name"));
        let groups = Group::ALL.iter().map(|group| {
            let rows = self
                .widgets
                .iter()
                .filter(|(spec, _)| spec.group == *group)
                .map(|(spec, input)| Field::new().label(spec.label).child(input.render(spec)));
            v_flex()
                .gap_2()
                .child(div().font_semibold().child(group.label()))
                .child(Form::new().columns(2).children(rows))
        });
        v_flex()
            .gap_3()
            .child(Form::new().columns(2).child(name))
            .children(groups)
    }
}

/// Open the template editor as a modal over the window.
pub fn open_template_dialog(editor: &Entity<TemplateEditor>, window: &mut Window, cx: &mut App) {
    if window.has_active_dialog(cx) {
        return;
    }
    let body = editor.clone();
    window.open_dialog(cx, move |dialog, _, _| {
        dialog
            .title("IPTC templates")
            .w(px(820.))
            .margin_top(px(32.))
            .child(body.clone())
    });
}

impl Render for TemplateEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let status = self.status.clone();
        let can_delete = self.editing_id.is_some();
        // The body of a modal dialog (title, Escape and the backdrop come
        // from the dialog layer), so no frame of its own.
        v_flex().id("template-editor").w_full().child(
            v_flex()
                .gap_3()
                .child(self.render_template_picker())
                .child(self.render_code_replacements(cx))
                .child(self.render_form())
                .child(self.render_preview())
                .child(div().text_sm().child(status))
                .child(
                    h_flex()
                        .w_full()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("template-new")
                                .label("New")
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.start_new(window, cx);
                                })),
                        )
                        .child(
                            Button::new("template-load")
                                .label("Load")
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.load_selected(window, cx);
                                })),
                        )
                        .child(
                            Button::new("template-delete")
                                .label("Delete")
                                .disabled(!can_delete)
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.delete(window, cx);
                                })),
                        )
                        .child(
                            Button::new("template-save")
                                .label("Save")
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.save(window, cx);
                                })),
                        )
                        .child(Button::new("template-preview").label("Preview").on_click(
                            cx.listener(|this, _: &ClickEvent, _window, cx| {
                                this.preview_fields(cx);
                            }),
                        ))
                        .child(
                            Button::new("template-apply")
                                .label("Apply to selection")
                                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                                    this.apply_selected(cx);
                                })),
                        ),
                ),
        )
    }
}
