//! Best-effort edit persistence: catalog first, sidecar second.
//!
//! An edit is never lost because metadata I/O failed. [`persist_edit`]
//! always commits the catalog row; the `crs:` sidecar is a bonus —
//! `crs_written_at` records only successful sidecar writes.

use std::path::Path;

use edit_catalog::Catalog;
use edit_core::{Crop, DevelopSettings};

use crate::crs::write_sidecar;
use crate::error::Result;

/// Outcome of [`persist_edit`]. `Ok` in both cases: a failed sidecar never
/// fails the edit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PersistOutcome {
    /// Catalog and sidecar both landed; `crs_written_at` is set.
    Written,
    /// Catalog landed; the sidecar did not, with the reason attached.
    SidecarSkipped(String),
}

/// Commit one item's edit (catalog row), then try the `crs:` sidecar.
///
/// `develop_json`/`crop_json` are the exact strings stored in the `edits`
/// row, so a sidecar built from unparsable JSON is skipped but the row
/// still commits.
///
/// The crop rides only in `crop_json`. A crop embedded in the develop
/// settings is ignored here and by the render service, so the sidecar and
/// the pixels never describe two different crops.
pub fn persist_edit(
    catalog: &Catalog,
    item_id: i64,
    develop_json: &str,
    crop_json: Option<&str>,
    sidecar_path: &Path,
) -> Result<PersistOutcome> {
    catalog.upsert_edit(item_id, develop_json, crop_json)?;
    persist_sidecar(catalog, item_id, develop_json, crop_json, sidecar_path)
}

/// The sidecar leg of [`persist_edit`], for callers that already committed
/// the catalog row: it never writes the `edits` row, so a slow sidecar write
/// cannot overwrite a newer commit. `crs_written_at` follows a successful
/// write.
pub fn persist_sidecar(
    catalog: &Catalog,
    item_id: i64,
    develop_json: &str,
    crop_json: Option<&str>,
    sidecar_path: &Path,
) -> Result<PersistOutcome> {
    let develop: DevelopSettings = match serde_json::from_str(develop_json) {
        Ok(d) => d,
        Err(e) => return Ok(PersistOutcome::SidecarSkipped(format!("develop json: {e}"))),
    };
    let crop: Option<Crop> = match crop_json {
        None => None,
        Some(json) => match serde_json::from_str(json) {
            Ok(c) => Some(c),
            Err(e) => return Ok(PersistOutcome::SidecarSkipped(format!("crop json: {e}"))),
        },
    };

    match write_sidecar(sidecar_path, &develop, crop.as_ref()) {
        Ok(()) => {
            catalog.mark_crs_written(item_id)?;
            Ok(PersistOutcome::Written)
        }
        Err(e) => Ok(PersistOutcome::SidecarSkipped(e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_catalog() -> (tempfile::TempDir, Catalog) {
        let dir = tempfile::TempDir::new().unwrap();
        let catalog = Catalog::open(&dir.path().join("catalog.sqlite")).unwrap();
        (dir, catalog)
    }

    fn seed_item(catalog: &Catalog) -> i64 {
        let root = catalog.insert_root("/photos", "folder").unwrap();
        let photo = catalog
            .insert_photo(&edit_catalog::models::NewPhoto {
                root_id: root,
                path: "/photos/DSC00001.NEF".into(),
                dir: "/photos".into(),
                base_name: "DSC00001".into(),
                ext: "NEF".into(),
                kind: "raw".into(),
                bytes: 1024,
                mtime: 1_700_000_000,
                original_name: None,
                taken_at: None,
            })
            .unwrap();
        catalog
            .insert_item("/photos", "DSC00001", Some(photo), None)
            .unwrap()
    }

    #[test]
    fn persist_writes_sidecar_and_marks_crs() {
        let (_dir, catalog) = scratch_catalog();
        let item = seed_item(&catalog);
        let sidecar = tempfile::NamedTempFile::new().unwrap().into_temp_path();
        // into_temp_path keeps the file; remove it so write_sidecar creates it.
        std::fs::remove_file(&sidecar).ok();

        let develop_json = serde_json::to_string(&DevelopSettings::default()).unwrap();
        let outcome = persist_edit(&catalog, item, &develop_json, None, &sidecar).unwrap();
        assert_eq!(outcome, PersistOutcome::Written);

        let edit = catalog.get_edit(item).unwrap().unwrap();
        assert!(edit.crs_written_at.is_some());
        let packet = std::fs::read_to_string(&sidecar).unwrap();
        assert!(packet.contains("crs:ProcessVersion=\"11.0\""));
    }

    #[test]
    fn persist_sidecar_leaves_the_edit_row_alone() {
        let (_dir, catalog) = scratch_catalog();
        let item = seed_item(&catalog);
        let dir = tempfile::TempDir::new().unwrap();
        let newer = serde_json::to_string(&DevelopSettings::default()).unwrap();
        catalog.upsert_edit(item, &newer, None).unwrap();

        // A stale sidecar write must not clobber the committed row.
        let stale = newer.replace("\"exposure\":0.0", "\"exposure\":1.0");
        assert_ne!(stale, newer);
        let outcome =
            persist_sidecar(&catalog, item, &stale, None, &dir.path().join("photo.xmp")).unwrap();
        assert_eq!(outcome, PersistOutcome::Written);
        let edit = catalog.get_edit(item).unwrap().unwrap();
        assert_eq!(edit.develop_json, newer);
        assert!(edit.crs_written_at.is_some());
    }

    #[test]
    fn persist_keeps_edit_when_sidecar_path_is_unwritable() {
        let (_dir, catalog) = scratch_catalog();
        let item = seed_item(&catalog);
        let develop_json = serde_json::to_string(&DevelopSettings::default()).unwrap();
        let crop_json = serde_json::to_string(&Crop::default()).unwrap();

        let outcome = persist_edit(
            &catalog,
            item,
            &develop_json,
            Some(&crop_json),
            Path::new("/nonexistent-dir/photo.xmp"),
        )
        .unwrap();
        assert!(matches!(outcome, PersistOutcome::SidecarSkipped(_)));

        // The edit still committed; crs_written_at stays empty.
        let edit = catalog.get_edit(item).unwrap().unwrap();
        assert_eq!(edit.develop_json, develop_json);
        assert_eq!(edit.crs_written_at, None);
    }

    #[test]
    fn persist_keeps_edit_when_develop_json_is_corrupt() {
        let (_dir, catalog) = scratch_catalog();
        let item = seed_item(&catalog);
        let dir = tempfile::TempDir::new().unwrap();

        let outcome = persist_edit(
            &catalog,
            item,
            "{not json",
            None,
            &dir.path().join("photo.xmp"),
        )
        .unwrap();
        assert!(matches!(outcome, PersistOutcome::SidecarSkipped(_)));
        // Raw string still stored verbatim.
        assert_eq!(
            catalog.get_edit(item).unwrap().unwrap().develop_json,
            "{not json"
        );
    }

    #[test]
    fn persist_writes_the_crop_json_into_the_sidecar() {
        let (_dir, catalog) = scratch_catalog();
        let item = seed_item(&catalog);
        let dir = tempfile::TempDir::new().unwrap();
        let develop_json = serde_json::to_string(&DevelopSettings::default()).unwrap();
        let crop_json = serde_json::to_string(&Crop {
            rect: edit_core::Rect::new(0.1, 0.2, 0.6, 0.5),
            rotation_degrees: 4.5,
            aspect: Some(edit_core::Aspect::FourThree),
        })
        .unwrap();

        let outcome = persist_edit(
            &catalog,
            item,
            &develop_json,
            Some(&crop_json),
            &dir.path().join("photo.xmp"),
        )
        .unwrap();
        assert_eq!(outcome, PersistOutcome::Written);
        let packet = std::fs::read_to_string(dir.path().join("photo.xmp")).unwrap();
        assert!(packet.contains("crs:HasCrop=\"True\""));
        assert!(packet.contains("crs:CropLeft=\"0.1\""));
        assert!(packet.contains("crs:CropAngle=\"4.5\""));
        // Both strings round-trip through the edits row untouched.
        let edit = catalog.get_edit(item).unwrap().unwrap();
        assert_eq!(edit.develop_json, develop_json);
        assert_eq!(edit.crop_json.as_deref(), Some(crop_json.as_str()));
        assert!(edit.crs_written_at.is_some());
    }

    #[test]
    fn persist_ignores_a_crop_embedded_in_develop_json() {
        let (_dir, catalog) = scratch_catalog();
        let item = seed_item(&catalog);
        let dir = tempfile::TempDir::new().unwrap();

        // The crop lives in crop_json only: one crop model, never two.
        let develop_json = {
            let mut base = serde_json::to_value(DevelopSettings::default()).unwrap();
            base["crop"] = serde_json::json!({
                "geometry": {"rect": {"x0": 0.1, "y0": 0.2, "x1": 0.9, "y1": 0.9}, "angle": 0.0},
                "aspect": null, "flipH": false, "flipV": false,
            });
            serde_json::to_string(&base).unwrap()
        };

        let outcome = persist_edit(
            &catalog,
            item,
            &develop_json,
            None,
            &dir.path().join("photo.xmp"),
        )
        .unwrap();
        assert_eq!(outcome, PersistOutcome::Written);
        let packet = std::fs::read_to_string(dir.path().join("photo.xmp")).unwrap();
        assert!(packet.contains("crs:HasCrop=\"False\""));
        assert!(!packet.contains("crs:CropLeft"));
        // The edit committed verbatim; crs_written_at follows the sidecar.
        let edit = catalog.get_edit(item).unwrap().unwrap();
        assert_eq!(edit.develop_json, develop_json);
        assert!(edit.crs_written_at.is_some());
    }

    #[test]
    fn persist_keeps_the_edit_when_crop_json_is_corrupt() {
        let (_dir, catalog) = scratch_catalog();
        let item = seed_item(&catalog);
        let dir = tempfile::TempDir::new().unwrap();
        let develop_json = serde_json::to_string(&DevelopSettings::default()).unwrap();
        let outcome = persist_edit(
            &catalog,
            item,
            &develop_json,
            Some("{nope"),
            &dir.path().join("photo.xmp"),
        )
        .unwrap();
        assert!(matches!(outcome, PersistOutcome::SidecarSkipped(m) if m.contains("crop json")));
        assert_eq!(
            catalog
                .get_edit(item)
                .unwrap()
                .unwrap()
                .crop_json
                .as_deref(),
            Some("{nope")
        );
    }
}
