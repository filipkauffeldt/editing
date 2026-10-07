//! IPTC template editor and apply flow (C3 phase 1).
//!
//! The editor is the `Form`/`Input`/`Textarea`/`Select` surface over the
//! phase-1 field set (`edit_metadata::TemplateFields`), stored in the
//! catalog `iptc_templates` table. "Apply to selection" reads the
//! template currently picked in the Select and hands it to
//! [`Browser::apply_template`], which enqueues one durable
//! `metadata_write` job per selected item (plan sections 2.1, 2.7). `T`
//! opens the editor from anywhere in the shell.
//!
//! Fields may contain `{…}` variables. The Preview row expands the
//! caption against the focused photo's capture metadata off the UI
//! thread ([`edit_metadata::preview_field`]) before apply, so users see
//! the resolved text (plan section 2.7).

use std::sync::Arc;

use edit_catalog::Catalog;
use edit_catalog::models::IptcTemplate;
use edit_metadata::TemplateFields;
use gpui_kit::component::button::Button;
use gpui_kit::component::form::{Field, Form};
use gpui_kit::component::input::{Input, InputState, Textarea, TextareaState};
use gpui_kit::component::searchable_list::SearchableVec;
use gpui_kit::component::select::{Select, SelectState};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::*;

use crate::ingest::AppState;
use app::views::Browser;

actions!(templates, [OpenTemplates]);

/// The overlay dialog. Owns the form and the catalog rows; the browser
/// owns the job queue and badges, so apply is a thin read + enqueue.
pub struct TemplateEditor {
    state: Entity<AppState>,
    browser: Entity<Browser>,
    catalog: Arc<Catalog>,
    /// Template row being edited; `None` means the next Save inserts.
    editing_id: Option<i64>,
    /// Visible template rows (picker + apply share the same list).
    templates: Vec<IptcTemplate>,
    names: Entity<SelectState<SearchableVec<String>>>,
    name: Entity<InputState>,
    caption: Entity<TextareaState>,
    keywords: Entity<InputState>,
    headline: Entity<InputState>,
    byline: Entity<InputState>,
    byline_title: Entity<InputState>,
    credit: Entity<InputState>,
    source: Entity<InputState>,
    copyright: Entity<InputState>,
    instructions: Entity<InputState>,
    edit_status: Entity<InputState>,
    rights: Entity<InputState>,
    preview: String,
    status: String,
}

/// Values collected from one input; an empty box registers no field.
fn value_of(state: &Entity<InputState>, cx: &App) -> Option<String> {
    let text: String = state.read(cx).value().into();
    let trimmed = text.trim().to_owned();
    (!trimmed.is_empty()).then_some(trimmed)
}

fn textarea_value_of(state: &Entity<TextareaState>, cx: &App) -> Option<String> {
    let text: String = state.read(cx).value().into();
    let trimmed = text.trim().to_owned();
    (!trimmed.is_empty()).then_some(trimmed)
}

impl TemplateEditor {
    pub fn new(
        state: Entity<AppState>,
        browser: Entity<Browser>,
        catalog: Arc<Catalog>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let names = cx
            .new(|cx| SelectState::new(SearchableVec::new(Vec::<String>::new()), None, window, cx));
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("Press pass"));
        let caption = cx.new(|cx| {
            TextareaState::new(window, cx).placeholder("{date} {seq:3} — caption with {camera}")
        });
        let keywords = cx.new(|cx| InputState::new(window, cx).placeholder("ice; {session}"));
        let headline = cx.new(|cx| InputState::new(window, cx).placeholder("Season opener"));
        let byline = cx.new(|cx| InputState::new(window, cx).placeholder("Anna Foto"));
        let byline_title =
            cx.new(|cx| InputState::new(window, cx).placeholder("staff photographer"));
        let credit = cx.new(|cx| InputState::new(window, cx).placeholder("SHL"));
        let source = cx.new(|cx| InputState::new(window, cx).placeholder("Agency"));
        let copyright = cx.new(|cx| InputState::new(window, cx).placeholder("(c) {yyyy} SHL"));
        let instructions =
            cx.new(|cx| InputState::new(window, cx).placeholder("raw key; not for publication"));
        let edit_status = cx.new(|cx| InputState::new(window, cx).placeholder("New"));
        let rights =
            cx.new(|cx| InputState::new(window, cx).placeholder("free use, credit required"));

        let mut editor = Self {
            state,
            browser,
            catalog,
            editing_id: None,
            templates: Vec::new(),
            names,
            name,
            caption,
            keywords,
            headline,
            byline,
            byline_title,
            credit,
            source,
            copyright,
            instructions,
            edit_status,
            rights,
            preview: String::new(),
            status: "Fill the fields, Save, then pick and Apply to selection.".to_string(),
        };
        editor.reload(window, cx);
        editor
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            state.set_templates_open(false, cx);
        });
    }

    /// Reload template rows from the catalog and rebuild the picker,
    /// keeping whatever was selected by name.
    fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.templates = self.catalog.list_iptc_templates().unwrap_or_default();
        self.templates.sort_by_key(|row| row.name.to_lowercase());
        let names: Vec<String> = self.templates.iter().map(|row| row.name.clone()).collect();
        let selected: Option<String> = self
            .names
            .read(cx)
            .selected_value()
            .cloned()
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
        TemplateFields {
            caption: textarea_value_of(&self.caption, cx),
            keywords: value_of(&self.keywords, cx),
            headline: value_of(&self.headline, cx),
            byline: value_of(&self.byline, cx),
            byline_title: value_of(&self.byline_title, cx),
            credit: value_of(&self.credit, cx),
            source: value_of(&self.source, cx),
            copyright: value_of(&self.copyright, cx),
            instructions: value_of(&self.instructions, cx),
            edit_status: value_of(&self.edit_status, cx),
            rights: value_of(&self.rights, cx),
        }
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

    fn set_textarea(
        &self,
        state: &Entity<TextareaState>,
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
        self.set_textarea(&self.caption, fields.caption.as_deref(), window, cx);
        self.set_input(&self.keywords, fields.keywords.as_deref(), window, cx);
        self.set_input(&self.headline, fields.headline.as_deref(), window, cx);
        self.set_input(&self.byline, fields.byline.as_deref(), window, cx);
        self.set_input(
            &self.byline_title,
            fields.byline_title.as_deref(),
            window,
            cx,
        );
        self.set_input(&self.credit, fields.credit.as_deref(), window, cx);
        self.set_input(&self.source, fields.source.as_deref(), window, cx);
        self.set_input(&self.copyright, fields.copyright.as_deref(), window, cx);
        self.set_input(
            &self.instructions,
            fields.instructions.as_deref(),
            window,
            cx,
        );
        self.set_input(&self.edit_status, fields.edit_status.as_deref(), window, cx);
        self.set_input(&self.rights, fields.rights.as_deref(), window, cx);
        self.preview.clear();
        self.status = format!("Editing template \"{}\".", row.name);
        cx.notify();
    }

    /// Clear the form for a fresh template (Save inserts).
    fn start_new(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editing_id = None;
        self.set_input(&self.name, None, window, cx);
        for state in [
            &self.keywords,
            &self.headline,
            &self.byline,
            &self.byline_title,
            &self.credit,
            &self.source,
            &self.copyright,
            &self.instructions,
            &self.edit_status,
            &self.rights,
        ] {
            self.set_input(state, None, window, cx);
        }
        self.set_textarea(&self.caption, None, window, cx);
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
            Some(id) => self
                .catalog
                .update_iptc_template(id, &name, &fields_json, "global")
                .map(|_| id),
            None => self
                .catalog
                .insert_iptc_template(&name, &fields_json, "global"),
        };
        match outcome {
            Ok(id) => {
                self.editing_id = Some(id);
                self.status = format!("Saved template \"{name}\".");
            }
            Err(e) => self.status = format!("Save failed: {e}"),
        }
        self.reload(window, cx);
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
        self.reload(window, cx);
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

    /// Hand the template selected in the picker to the browser for the
    /// multi-selection. The browser enqueues and reports; the editor
    /// mirrors its status line.
    fn apply_selected(&mut self, cx: &mut Context<Self>) {
        let Some(row) = self.selected_template(cx) else {
            self.status = "Select a saved template first.".to_string();
            cx.notify();
            return;
        };
        let fields: TemplateFields = serde_json::from_str(&row.fields_json).unwrap_or_default();
        if fields == TemplateFields::default() {
            self.status = format!("Template \"{}\" has no fields to write.", row.name);
            cx.notify();
            return;
        }
        self.browser.update(cx, |browser, cx| {
            browser.apply_template(fields, &row.name, cx);
        });
        self.status = self.browser.read(cx).template_status();
        cx.notify();
    }

    /// Expand the caption against the focused photo in the background;
    /// the resolved text lands in the Preview row (plan section 2.7).
    fn preview_caption(&mut self, cx: &mut Context<Self>) {
        let caption = self.caption.read(cx).value().to_string();
        let Some(path) = self.browser.read(cx).current_display_path() else {
            self.preview.clear();
            self.status = "Open a folder and focus a photo to preview.".to_string();
            cx.notify();
            return;
        };
        cx.spawn(async move |this, cx| {
            let resolved = cx.background_executor().spawn(async move {
                edit_metadata::preview_field(
                    std::path::Path::new(&path),
                    &caption,
                    1,
                    edit_catalog::db::now_unix(),
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

    fn render_form(&self) -> impl IntoElement {
        Form::new()
            .columns(2)
            .child(
                Field::new()
                    .label("Name")
                    .child(Input::new(&self.name).id("template-name")),
            )
            .child(
                Field::new()
                    .label("Caption")
                    .child(Textarea::new(&self.caption).h(px(72.0))),
            )
            .child(
                Field::new()
                    .label("Keywords ( ; , separated)")
                    .child(Input::new(&self.keywords).id("template-keywords")),
            )
            .child(
                Field::new()
                    .label("Headline")
                    .child(Input::new(&self.headline).id("template-headline")),
            )
            .child(
                Field::new()
                    .label("Byline")
                    .child(Input::new(&self.byline).id("template-byline")),
            )
            .child(
                Field::new()
                    .label("Byline title")
                    .child(Input::new(&self.byline_title).id("template-byline-title")),
            )
            .child(
                Field::new()
                    .label("Credit")
                    .child(Input::new(&self.credit).id("template-credit")),
            )
            .child(
                Field::new()
                    .label("Source")
                    .child(Input::new(&self.source).id("template-source")),
            )
            .child(
                Field::new()
                    .label("Copyright")
                    .child(Input::new(&self.copyright).id("template-copyright")),
            )
            .child(
                Field::new()
                    .label("Instructions")
                    .child(Input::new(&self.instructions).id("template-instructions")),
            )
            .child(
                Field::new()
                    .label("Edit status")
                    .child(Input::new(&self.edit_status).id("template-edit-status")),
            )
            .child(
                Field::new()
                    .label("Rights")
                    .child(Input::new(&self.rights).id("template-rights")),
            )
    }
}

impl Render for TemplateEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let status = self.status.clone();
        let preview = self.preview.clone();
        let can_delete = self.editing_id.is_some();
        div()
            .id("template-editor")
            .w_full()
            .rounded_lg()
            .border_1()
            .p_4()
            .max_h(px(720.0))
            .overflow_y_scroll()
            .bg(theme.tokens.popover)
            .child(
                v_flex()
                    .gap_3()
                    .child(
                        h_flex()
                            .w_full()
                            .justify_between()
                            .child(
                                div()
                                    .font_semibold()
                                    .child("IPTC template editor".to_string()),
                            )
                            .child(Button::new("template-close").label("Close").on_click(
                                cx.listener(|this, _: &ClickEvent, _window, cx| {
                                    this.close(cx);
                                }),
                            )),
                    )
                    .child(self.render_template_picker())
                    .child(self.render_form())
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(0xDDC999))
                            .child(format!("Preview (focused photo, sequence 1): {preview}")),
                    )
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
                            .child(Button::new("template-load").label("Load").on_click(
                                cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.load_selected(window, cx);
                                }),
                            ))
                            .child(
                                Button::new("template-delete")
                                    .label("Delete")
                                    .disabled(!can_delete)
                                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                        this.delete(window, cx);
                                    })),
                            )
                            .child(Button::new("template-save").label("Save").on_click(
                                cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.save(window, cx);
                                }),
                            ))
                            .child(Button::new("template-preview").label("Preview").on_click(
                                cx.listener(|this, _: &ClickEvent, _window, cx| {
                                    this.preview_caption(cx);
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
