//! User presets: a Lightroom-readable `.xmp` file under the catalog
//! directory plus a row in the catalog's `presets` table that points at it.
//! A preset is the controls that differ from the defaults, so applying it
//! sets only those and leaves the rest of an edit alone.

use std::path::{Path, PathBuf};

use edit_catalog::Catalog;
use edit_core::DevelopSettings;
use edit_metadata::crs;

use crate::panel::PresetValues;

/// `source` column value of presets saved here.
pub const USER_SOURCE: &str = "user";

/// Where preset files live: next to the catalog database.
pub fn preset_dir(catalog: &Catalog) -> PathBuf {
    catalog
        .path()
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("presets")
}

fn file_stem(name: &str) -> String {
    let stem: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if stem.is_empty() {
        "preset".into()
    } else {
        stem
    }
}

/// Save `values` under `name`; saving the same name again replaces it.
pub fn save(catalog: &Catalog, name: &str, values: &PresetValues) -> Result<(), String> {
    let dir = preset_dir(catalog);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(format!("{}.xmp", file_stem(name)));
    let mut settings = DevelopSettings::default();
    values.apply(&mut settings);
    let packet = crs::sidecar_packet(&settings, None);
    std::fs::write(&path, packet).map_err(|e| e.to_string())?;
    let path = path.to_string_lossy().into_owned();
    let known = catalog
        .list_presets()
        .map_err(|e| e.to_string())?
        .into_iter()
        .any(|p| p.path == path);
    if !known {
        catalog
            .insert_preset(name.trim(), None, &path, USER_SOURCE)
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Read a saved preset back.
pub fn load(path: &Path) -> Result<PresetValues, String> {
    let xml = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let mut settings = DevelopSettings::default();
    crs::apply_attributes(&mut settings, &crs::parse_attributes(&xml));
    Ok(PresetValues::from_settings(&settings))
}

/// `(name, file path)` of every saved user preset, oldest first.
pub fn list(catalog: &Catalog) -> Vec<(String, String)> {
    catalog
        .list_presets()
        .unwrap_or_default()
        .into_iter()
        .filter(|p| p.source == USER_SOURCE)
        .map(|p| (p.name, p.path))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> (Catalog, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "preset-store-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        (Catalog::open(&dir.join("c.sqlite")).unwrap(), dir)
    }

    #[test]
    fn a_saved_preset_reads_back_and_lists() {
        let (catalog, dir) = catalog();
        let mut edited = DevelopSettings::default();
        lightcraft_develop::controls::set(&mut edited, "effects.texture", 30.0);
        lightcraft_develop::controls::set(&mut edited, "vignette.amount", -20.0);
        let values = PresetValues::from_settings(&edited);
        save(&catalog, "Evening look", &values).unwrap();
        // The same name again replaces the file and adds no second row.
        save(&catalog, "Evening look", &values).unwrap();
        let listed = list(&catalog);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].0, "Evening look");
        let loaded = load(Path::new(&listed[0].1)).unwrap();
        assert_eq!(loaded, values);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn names_with_odd_characters_make_safe_file_names() {
        assert_eq!(file_stem("a/b: c"), "a-b--c");
        assert_eq!(file_stem("  "), "preset");
    }
}
