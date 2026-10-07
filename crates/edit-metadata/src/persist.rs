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
}
