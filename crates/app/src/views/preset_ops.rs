//! Preset files from the browser: the folder scan, save, import, export and
//! choosing the folder. File work runs on the background executor; the
//! results come back as the indexed list and a short status line.

use std::path::{Path, PathBuf};

use gpui_kit::*;

use super::Browser;
use crate::panel::PresetValues;
use crate::preset_store::{self, PresetRow};

impl Browser {
    /// The presets indexed so far.
    pub fn indexed_presets(&self) -> &[PresetRow] {
        &self.user_presets
    }

    /// The line under the preset bar.
    pub fn develop_status(&self) -> &str {
        &self.develop_status
    }

    /// Index the preset folder in the background and refresh the list.
    pub fn rescan_presets(&mut self, cx: &mut Context<Self>) {
        self.run_preset_job(|_| Ok(String::new()), cx);
    }

    /// Run `work` and then a folder scan off the UI thread; the status shows
    /// what `work` returned or why it failed.
    fn run_preset_job(
        &mut self,
        work: impl FnOnce(&edit_catalog::Catalog) -> Result<String, String> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        let catalog = self.catalog.clone();
        cx.spawn(async move |this, cx| {
            let outcome: (Result<String, String>, Vec<PresetRow>) = cx
                .background_executor()
                .spawn(async move {
                    let done = work(&catalog);
                    let dir = preset_store::preset_dir(&catalog);
                    let scanned = preset_store::scan(&catalog, &dir);
                    let done = match (done, scanned) {
                        (Ok(text), Ok(_)) => Ok(text),
                        (Err(e), _) | (Ok(_), Err(e)) => Err(e),
                    };
                    (done, preset_store::list(&catalog))
                })
                .await;
            this.update(cx, |this, cx| {
                this.user_presets = outcome.1;
                match outcome.0 {
                    Ok(text) if !text.is_empty() => this.develop_status = text,
                    Ok(_) => {}
                    Err(e) => this.develop_status = e,
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Save the current edit's changed controls as a named preset.
    pub fn save_preset(&mut self, name: &str, cx: &mut Context<Self>) {
        let values = PresetValues::from_settings(self.develop.buffer());
        let name = name.trim().to_owned();
        self.run_preset_job(
            move |catalog| {
                preset_store::save(catalog, &name, &values)
                    .map(|()| format!("Saved preset {name}"))
                    .map_err(|e| format!("Preset not saved: {e}"))
            },
            cx,
        );
    }

    /// Bring preset files or photo sidecars into the preset folder.
    pub fn import_presets(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        self.run_preset_job(move |catalog| Ok(import_summary(catalog, &paths)), cx);
    }

    pub fn prompt_import_presets(&mut self, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Import".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = picked.await {
                this.update(cx, |this, cx| this.import_presets(paths, cx))
                    .ok();
            }
        })
        .detach();
    }

    /// Write the current settings to `dest` as a Lightroom preset.
    pub fn export_settings_to(&mut self, dest: &Path, cx: &mut Context<Self>) {
        self.develop_status = match preset_store::export_settings(self.develop.buffer(), dest) {
            Ok(()) => format!("Exported {}", file_name(dest)),
            Err(e) => format!("Not exported: {e}"),
        };
        cx.notify();
    }

    pub fn prompt_export_settings(&mut self, cx: &mut Context<Self>) {
        let dir = preset_store::preset_dir(&self.catalog);
        let picked = cx.prompt_for_new_path(&dir, Some("Settings.xmp"));
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(dest))) = picked.await {
                this.update(cx, |this, cx| this.export_settings_to(&dest, cx))
                    .ok();
            }
        })
        .detach();
    }

    /// Copy the preset file behind `key` to `dest`.
    pub fn export_preset_to(&mut self, key: &str, dest: &Path, cx: &mut Context<Self>) {
        self.develop_status = match key.strip_prefix("user:") {
            Some(path) => match preset_store::export_file(Path::new(path), dest) {
                Ok(()) => format!("Exported {}", file_name(dest)),
                Err(e) => format!("Not exported: {e}"),
            },
            None => "Pick a saved preset to export".into(),
        };
        cx.notify();
    }

    pub fn prompt_export_preset(&mut self, cx: &mut Context<Self>) {
        let Some(key) = self.picked_preset.clone() else {
            return;
        };
        let name = key
            .strip_prefix("user:")
            .map(|p| file_name(Path::new(p)))
            .unwrap_or_default();
        let dir = preset_store::preset_dir(&self.catalog);
        let picked = cx.prompt_for_new_path(&dir, Some(&name));
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(dest))) = picked.await {
                this.update(cx, |this, cx| this.export_preset_to(&key, &dest, cx))
                    .ok();
            }
        })
        .detach();
    }

    /// Keep presets in `dir` from now on and index what is there.
    pub fn set_preset_folder(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        self.run_preset_job(
            move |catalog| {
                preset_store::set_preset_dir(catalog, &dir)?;
                Ok(format!("Preset folder is {}", dir.display()))
            },
            cx,
        );
    }

    pub fn prompt_preset_folder(&mut self, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Use folder".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = picked.await
                && let Some(dir) = paths.into_iter().next()
            {
                this.update(cx, |this, cx| this.set_preset_folder(dir, cx))
                    .ok();
            }
        })
        .detach();
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Import every file and describe the result in one short line.
fn import_summary(catalog: &edit_catalog::Catalog, paths: &[PathBuf]) -> String {
    let mut done = Vec::new();
    let mut skipped = 0;
    let mut failed = Vec::new();
    for path in paths {
        match preset_store::import(catalog, path) {
            Ok(imported) => {
                skipped += imported.skipped.len();
                done.push(imported.name);
            }
            Err(e) => failed.push(format!("{}: {e}", file_name(path))),
        }
    }
    let mut text = match done.as_slice() {
        [] => String::new(),
        [one] => format!("Imported {one}"),
        many => format!("Imported {} presets", many.len()),
    };
    if skipped > 0 {
        text.push_str(&format!(", {skipped} unsupported settings left out"));
    }
    if let Some(first) = failed.first() {
        if !text.is_empty() {
            text.push_str(". ");
        }
        text.push_str(&format!("Not imported: {first}"));
    }
    text
}
