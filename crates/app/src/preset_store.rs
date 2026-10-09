//! User presets: Lightroom-readable `.xmp` files in one folder (default
//! `presets` next to the catalog) and an index of them in the catalog's
//! `presets` table. Subfolders are preset groups. The scan reads only files
//! whose modified time or size changed and drops rows of deleted files, so a
//! re-scan of an unchanged folder reads nothing.
//!
//! A preset is the controls that differ from the defaults, so applying it
//! sets only those and leaves the rest of an edit alone.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use edit_catalog::Catalog;
use edit_catalog::models::Preset;
use edit_core::DevelopSettings;
use edit_metadata::preset_xmp::{self, PresetInfo};

use crate::panel::PresetValues;

/// `source` column value of presets saved here.
pub const USER_SOURCE: &str = "user";
/// `source` column value of presets brought in from another file.
pub const IMPORTED_SOURCE: &str = "imported";
/// Group (folder) imported presets without a group of their own go to.
pub const IMPORT_GROUP: &str = "Imported";

const FOLDER_SETTING: &str = "preset_folder";

/// The preset folder: the chosen one, else `presets` next to the catalog.
pub fn preset_dir(catalog: &Catalog) -> PathBuf {
    if let Ok(Some(dir)) = catalog.get_setting(FOLDER_SETTING)
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    catalog
        .path()
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("presets")
}

/// Use `dir` as the preset folder from now on.
pub fn set_preset_dir(catalog: &Catalog, dir: &Path) -> Result<(), String> {
    catalog
        .set_setting(FOLDER_SETTING, &dir.to_string_lossy())
        .map_err(|e| e.to_string())
}

fn file_stem(name: &str) -> String {
    let stem: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '-' | '_' | ' ') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let stem = stem.trim().to_owned();
    if stem.is_empty() {
        "preset".into()
    } else {
        stem
    }
}

/// A preset file found on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub path: String,
    pub mtime: i64,
    pub size: i64,
    /// Folders between the preset folder and the file, joined with `/`.
    pub group: Option<String>,
}

/// What a scan has to do.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// New or changed files, to read and index.
    pub read: Vec<Found>,
    /// Index rows whose file is gone.
    pub remove: Vec<i64>,
    pub unchanged: usize,
}

/// Compare the index with the files found. Pure.
pub fn plan_scan(indexed: &[Preset], found: &[Found]) -> Plan {
    let by_path: HashMap<&str, &Preset> = indexed.iter().map(|p| (p.path.as_str(), p)).collect();
    let mut plan = Plan::default();
    for file in found {
        match by_path.get(file.path.as_str()) {
            Some(row)
                if row.mtime == file.mtime
                    && row.size == file.size
                    && row.group_name == file.group =>
            {
                plan.unchanged += 1;
            }
            _ => plan.read.push(file.clone()),
        }
    }
    let present: std::collections::HashSet<&str> = found.iter().map(|f| f.path.as_str()).collect();
    plan.remove = indexed
        .iter()
        .filter(|p| !present.contains(p.path.as_str()))
        .map(|p| p.id)
        .collect();
    plan
}

/// Every `.xmp` under `dir`, sorted by path.
pub fn walk(dir: &Path) -> Vec<Found> {
    fn visit(root: &Path, dir: &Path, out: &mut Vec<Found>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                visit(root, &path, out);
            } else if path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("xmp"))
            {
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_secs() as i64);
                let group = path
                    .parent()
                    .and_then(|p| p.strip_prefix(root).ok())
                    .map(|rel| {
                        rel.components()
                            .map(|c| c.as_os_str().to_string_lossy())
                            .collect::<Vec<_>>()
                            .join("/")
                    })
                    .filter(|g| !g.is_empty());
                out.push(Found {
                    path: path.to_string_lossy().into_owned(),
                    mtime,
                    size: meta.len() as i64,
                    group,
                });
            }
        }
    }
    let mut out = Vec::new();
    visit(dir, dir, &mut out);
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// What a scan did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScanReport {
    pub read: usize,
    pub removed: usize,
    pub unchanged: usize,
}

/// Bring the index in line with the preset folder. Blocking: run it on a
/// background thread.
pub fn scan(catalog: &Catalog, dir: &Path) -> Result<ScanReport, String> {
    let found = walk(dir);
    let indexed = catalog.list_presets().map_err(|e| e.to_string())?;
    let plan = plan_scan(&indexed, &found);
    for id in &plan.remove {
        catalog.delete_preset(*id).map_err(|e| e.to_string())?;
    }
    for file in &plan.read {
        let stem = Path::new(&file.path)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let info = std::fs::read_to_string(&file.path)
            .map(|xml| preset_xmp::parse_preset(&xml).info)
            .unwrap_or_default();
        let name = info.name.filter(|n| !n.is_empty()).unwrap_or(stem);
        let group = file.group.clone().or(info.group.filter(|g| !g.is_empty()));
        let source = indexed
            .iter()
            .find(|p| p.path == file.path)
            .map_or(USER_SOURCE, |p| p.source.as_str());
        catalog
            .upsert_preset(
                &name,
                group.as_deref(),
                &file.path,
                source,
                file.mtime,
                file.size,
            )
            .map_err(|e| e.to_string())?;
    }
    Ok(ScanReport {
        read: plan.read.len(),
        removed: plan.remove.len(),
        unchanged: plan.unchanged,
    })
}

/// Settings of `values` over the defaults.
fn settings_of(values: &PresetValues) -> DevelopSettings {
    let mut settings = DevelopSettings::default();
    values.apply(&mut settings);
    settings
}

/// Save `values` under `name`; saving the same name again replaces it.
pub fn save(catalog: &Catalog, name: &str, values: &PresetValues) -> Result<(), String> {
    let dir = preset_dir(catalog);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(format!("{}.xmp", file_stem(name)));
    let info = PresetInfo {
        name: Some(name.trim().to_owned()),
        ..PresetInfo::default()
    };
    std::fs::write(
        &path,
        preset_xmp::preset_packet(&settings_of(values), &info),
    )
    .map_err(|e| e.to_string())?;
    scan(catalog, &dir).map(|_| ())
}

/// What an import found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Imported {
    pub name: String,
    /// `crs:` keys the app has no control for.
    pub skipped: Vec<String>,
}

/// Bring a preset file, or a photo's sidecar, into the preset folder. The
/// copy is rewritten from the settings the app understands.
pub fn import(catalog: &Catalog, source: &Path) -> Result<Imported, String> {
    let xml = std::fs::read_to_string(source).map_err(|e| e.to_string())?;
    let parsed = preset_xmp::parse_preset(&xml);
    let settings = parsed.settings();
    if PresetValues::from_settings(&settings).is_empty() {
        return Err("No settings found in the file".into());
    }
    let stem = source
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = parsed.info.name.clone().unwrap_or(stem);
    let group = parsed
        .info
        .group
        .clone()
        .filter(|g| !g.is_empty())
        .unwrap_or_else(|| IMPORT_GROUP.to_owned());
    let dir = preset_dir(catalog);
    let target = dir.join(file_stem(&group));
    std::fs::create_dir_all(&target).map_err(|e| e.to_string())?;
    let info = PresetInfo {
        name: Some(name.clone()),
        group: Some(group),
        uuid: parsed.info.uuid.clone(),
    };
    let path = target.join(format!("{}.xmp", file_stem(&name)));
    std::fs::write(&path, preset_xmp::preset_packet(&settings, &info))
        .map_err(|e| e.to_string())?;
    scan(catalog, &dir)?;
    let path = path.to_string_lossy().into_owned();
    if let Some(row) = catalog
        .list_presets()
        .map_err(|e| e.to_string())?
        .iter()
        .find(|p| p.path == path)
        && row.source != IMPORTED_SOURCE
    {
        catalog
            .upsert_preset(
                &row.name,
                row.group_name.as_deref(),
                &row.path,
                IMPORTED_SOURCE,
                row.mtime,
                row.size,
            )
            .map_err(|e| e.to_string())?;
    }
    Ok(Imported {
        name,
        skipped: parsed.skipped,
    })
}

/// Write `settings` as a Lightroom preset at `dest`, named after the file.
pub fn export_settings(settings: &DevelopSettings, dest: &Path) -> Result<(), String> {
    let info = PresetInfo {
        name: dest.file_stem().map(|s| s.to_string_lossy().into_owned()),
        ..PresetInfo::default()
    };
    std::fs::write(dest, preset_xmp::preset_packet(settings, &info)).map_err(|e| e.to_string())
}

/// Copy an indexed preset file to `dest`.
pub fn export_file(path: &Path, dest: &Path) -> Result<(), String> {
    std::fs::copy(path, dest)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Read a preset file back.
pub fn load(path: &Path) -> Result<PresetValues, String> {
    let xml = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    Ok(PresetValues::from_settings(
        &preset_xmp::parse_preset(&xml).settings(),
    ))
}

/// One indexed preset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresetRow {
    pub name: String,
    pub group: Option<String>,
    pub path: String,
}

/// Every indexed preset, ungrouped first, then by group and name.
pub fn list(catalog: &Catalog) -> Vec<PresetRow> {
    let mut rows: Vec<PresetRow> = catalog
        .list_presets()
        .unwrap_or_default()
        .into_iter()
        .map(|p| PresetRow {
            name: p.name,
            group: p.group_name,
            path: p.path,
        })
        .collect();
    rows.sort_by(|a, b| {
        (a.group.is_some(), &a.group, a.name.to_lowercase()).cmp(&(
            b.group.is_some(),
            &b.group,
            b.name.to_lowercase(),
        ))
    });
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use lightcraft_develop::controls;

    fn catalog() -> (Catalog, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "preset-store-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        (Catalog::open(&dir.join("c.sqlite")).unwrap(), dir)
    }

    fn row(id: i64, path: &str, mtime: i64, size: i64, group: Option<&str>) -> Preset {
        Preset {
            id,
            name: "n".into(),
            group_name: group.map(str::to_owned),
            path: path.into(),
            source: USER_SOURCE.into(),
            updated_at: 0,
            mtime,
            size,
        }
    }

    fn found(path: &str, mtime: i64, size: i64, group: Option<&str>) -> Found {
        Found {
            path: path.into(),
            mtime,
            size,
            group: group.map(str::to_owned),
        }
    }

    #[test]
    fn the_plan_reads_new_and_changed_files_and_drops_missing_ones() {
        let indexed = [
            row(1, "/p/a.xmp", 10, 100, None),
            row(2, "/p/b.xmp", 10, 100, None),
            row(3, "/p/Film/c.xmp", 10, 100, Some("Film")),
        ];
        let files = [
            found("/p/a.xmp", 10, 100, None),
            found("/p/b.xmp", 11, 100, None),
            found("/p/Film/d.xmp", 5, 50, Some("Film")),
        ];
        let plan = plan_scan(&indexed, &files);
        assert_eq!(plan.unchanged, 1);
        assert_eq!(
            plan.read
                .iter()
                .map(|f| f.path.as_str())
                .collect::<Vec<_>>(),
            ["/p/b.xmp", "/p/Film/d.xmp"]
        );
        assert_eq!(plan.remove, [3]);
    }

    #[test]
    fn a_saved_preset_reads_back_and_lists() {
        let (catalog, dir) = catalog();
        let mut edited = DevelopSettings::default();
        controls::set(&mut edited, "effects.texture", 30.0);
        controls::set(&mut edited, "vignette.amount", -20.0);
        edited.optics.lens_profile = true;
        let values = PresetValues::from_settings(&edited);
        save(&catalog, "Evening look", &values).unwrap();
        // The same name again replaces the file and adds no second row.
        save(&catalog, "Evening look", &values).unwrap();
        let listed = list(&catalog);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "Evening look");
        assert_eq!(listed[0].group, None);
        assert_eq!(load(Path::new(&listed[0].path)).unwrap(), values);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn names_with_odd_characters_make_safe_file_names() {
        assert_eq!(file_stem("a/b: c"), "a-b- c");
        assert_eq!(file_stem("  "), "preset");
    }

    #[test]
    fn scanning_indexes_subfolders_as_groups_and_is_incremental() {
        let (catalog, dir) = catalog();
        let presets = dir.join("lib");
        set_preset_dir(&catalog, &presets).unwrap();
        assert_eq!(preset_dir(&catalog), presets);
        let fixtures =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../edit-metadata/tests/fixtures/presets");
        copy_tree(&fixtures, &presets);

        let first = scan(&catalog, &presets).unwrap();
        assert_eq!((first.read, first.removed, first.unchanged), (4, 0, 0));
        let rows = list(&catalog);
        let summary: Vec<_> = rows
            .iter()
            .map(|r| (r.name.as_str(), r.group.as_deref()))
            .collect();
        assert_eq!(
            summary,
            [
                ("Warm portrait", None),
                ("Faded film", Some("Film")),
                ("Unknown keys", Some("Film")),
                ("Grouped settings", Some("Portraits/Studio")),
            ]
        );

        let again = scan(&catalog, &presets).unwrap();
        assert_eq!((again.read, again.removed, again.unchanged), (0, 0, 4));

        std::fs::remove_file(presets.join("Film/Faded film.xmp")).unwrap();
        let after = scan(&catalog, &presets).unwrap();
        assert_eq!((after.read, after.removed, after.unchanged), (0, 1, 3));
        assert_eq!(list(&catalog).len(), 3);
        std::fs::remove_dir_all(&dir).ok();
    }

    fn copy_tree(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap().flatten() {
            let dest = to.join(entry.file_name());
            if entry.path().is_dir() {
                copy_tree(&entry.path(), &dest);
            } else {
                std::fs::copy(entry.path(), dest).unwrap();
            }
        }
    }

    #[test]
    fn import_rewrites_a_file_into_its_group_and_reports_skipped_keys() {
        let (catalog, dir) = catalog();
        let src = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../edit-metadata/tests/fixtures/presets/Film/Unknown keys.xmp");
        let done = import(&catalog, &src).unwrap();
        assert_eq!(done.name, "Unknown keys");
        assert!(done.skipped.contains(&"LookTable".to_owned()));
        let rows = list(&catalog);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].group.as_deref(), Some("Film"));
        let values = load(Path::new(&rows[0].path)).unwrap();
        assert!(values.values.contains(&("light.contrast".into(), 18.0)));
        let source = catalog.list_presets().unwrap()[0].source.clone();
        assert_eq!(source, IMPORTED_SOURCE);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_photo_sidecar_imports_into_the_import_group_and_empty_files_are_refused() {
        let (catalog, dir) = catalog();
        let mut edited = DevelopSettings::default();
        controls::set(&mut edited, "light.exposure", 0.7);
        let sidecar = dir.join("DSC0001.xmp");
        edit_metadata::crs::write_sidecar(&sidecar, &edited, None).unwrap();
        import(&catalog, &sidecar).unwrap();
        assert_eq!(list(&catalog)[0].group.as_deref(), Some(IMPORT_GROUP));
        assert_eq!(list(&catalog)[0].name, "DSC0001");

        let empty = dir.join("empty.xmp");
        edit_metadata::crs::write_sidecar(&empty, &DevelopSettings::default(), None).unwrap();
        assert!(import(&catalog, &empty).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn exported_settings_import_back_identically() {
        let (catalog, dir) = catalog();
        let mut edited = DevelopSettings::default();
        controls::set(&mut edited, "light.exposure", 0.7);
        controls::set(&mut edited, "mixer.red.hue", -12.0);
        let dest = dir.join("My look.xmp");
        export_settings(&edited, &dest).unwrap();
        import(&catalog, &dest).unwrap();
        let rows = list(&catalog);
        let loaded = load(Path::new(&rows[0].path)).unwrap();
        assert_eq!(loaded, PresetValues::from_settings(&edited));
        std::fs::remove_dir_all(&dir).ok();
    }
}
