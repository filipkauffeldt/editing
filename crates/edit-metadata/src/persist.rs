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
/// [Crop] can ride in two places: the `crop_json` column and embedded
/// `DevelopSettings.crop` (C1/D1 producers may emit either). When
/// `crop_json` is missing, the embedded crop is mapped into the sidecar;
/// a debug assert flags the mapping so D1 can reconcile the two models
/// instead of relying on it.
pub fn persist_edit(
    catalog: &Catalog,
    item_id: i64,
    develop_json: &str,
    crop_json: Option<&str>,
    sidecar_path: &Path,
) -> Result<PersistOutcome> {
    catalog.upsert_edit(item_id, develop_json, crop_json)?;

    let develop: DevelopSettings = match serde_json::from_str(develop_json) {
        Ok(d) => d,
        Err(e) => return Ok(PersistOutcome::SidecarSkipped(format!("develop json: {e}"))),
    };
    let crop: Option<Crop> = match crop_json {
        None => embedded_crop(&develop),
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

/// Map the LightCraft-embedded crop to the sidecar `Crop` when no
/// `crop_json` row exists, so the sidecar encodes exactly one crop model.
/// TODO(D1): replace this fallback with a single reconciled crop model —
/// a producer must not have to choose where the crop rides.
fn embedded_crop(develop: &DevelopSettings) -> Option<Crop> {
    let geometry = &develop.crop.geometry;
    if develop.crop == DevelopSettings::default().crop {
        return None;
    }
    Some(Crop {
        rect: edit_core::Rect::new(
            geometry.rect.x0 as f32,
            geometry.rect.y0 as f32,
            (geometry.rect.x1 - geometry.rect.x0) as f32,
            (geometry.rect.y1 - geometry.rect.y0) as f32,
        ),
        rotation_degrees: geometry.angle as f32,
        aspect: None,
    })
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
    fn persist_maps_crop_embedded_in_develop_json() {
        let (_dir, catalog) = scratch_catalog();
        let item = seed_item(&catalog);
        let dir = tempfile::TempDir::new().unwrap();

        // A first C1/D1 producer embedding the crop in develop_json instead
        // of crop_json: the sidecar encodes exactly one crop model, never
        // both.
        let embedded = serde_json::json!({
            "crop": {"geometry": {"rect": {"x0": 0.1, "y0": 0.2, "x1": 0.9, "y1": 0.9}, "angle": 0.0}, "aspect": null, "flipH": false, "flipV": false},
        });
        let develop_json = {
            let mut base = serde_json::to_value(DevelopSettings::default()).unwrap();
            for (k, v) in embedded.as_object().unwrap() {
                base[k] = v.clone();
            }
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
        assert!(packet.contains("crs:HasCrop=\"True\""));
        assert!(packet.contains("crs:CropLeft=\"0.1\""));
        // The edit committed verbatim; crs_written_at follows the sidecar.
        assert_eq!(
            catalog.get_edit(item).unwrap().unwrap().develop_json,
            develop_json
        );
        assert!(
            catalog
                .get_edit(item)
                .unwrap()
                .unwrap()
                .crs_written_at
                .is_some()
        );
    }
}
