//! A crop and straighten set in crop mode survives a restart: the edit
//! commit writes the `edits` row and the `crs:` sidecar, and a fresh catalog
//! handle on the same file reads the same crop back.

use app::develop::{DevelopSession, LoadedEdit, SliderId, edit_json};
use edit_catalog::Catalog;
use edit_catalog::models::NewPhoto;
use edit_core::{Aspect, Crop, DevelopSettings, Rect};
use edit_metadata::{PersistOutcome, persist_edit};

fn seed(catalog: &Catalog) -> i64 {
    let root = catalog.insert_root("/photos", "folder").unwrap();
    let photo = catalog
        .insert_photo(&NewPhoto {
            root_id: root,
            path: "/photos/DSC00001.JPG".into(),
            dir: "/photos".into(),
            base_name: "DSC00001".into(),
            ext: "JPG".into(),
            kind: "jpeg".into(),
            bytes: 1024,
            mtime: 1_700_000_000,
            original_name: None,
            taken_at: None,
        })
        .unwrap();
    catalog
        .insert_item("/photos", "DSC00001", None, Some(photo))
        .unwrap()
}

#[test]
fn crop_and_straighten_survive_a_restart() {
    let dir = tempfile::TempDir::new().unwrap();
    let db = dir.path().join("catalog.sqlite");
    let sidecar = dir.path().join("DSC00001.xmp");
    let crop = Crop {
        rect: Rect::new(0.12, 0.2, 0.6, 0.4),
        rotation_degrees: 3.25,
        aspect: Some(Aspect::ThreeTwo),
    };
    let mut develop = DevelopSettings::default();
    SliderId::Exposure.set(&mut develop, 0.7);

    let item = {
        let catalog = Catalog::open(&db).unwrap();
        let item = seed(&catalog);
        let edit = edit_json(&develop, Some(&crop)).unwrap();
        let outcome = persist_edit(
            &catalog,
            item,
            &edit.develop,
            edit.crop.as_deref(),
            &sidecar,
        )
        .unwrap();
        assert_eq!(outcome, PersistOutcome::Written);
        item
    };

    // "Restart": a new handle on the same file, a new session.
    let catalog = Catalog::open(&db).unwrap();
    let row = catalog.get_edit(item).unwrap().unwrap();
    let loaded = LoadedEdit::parse(&row.develop_json, row.crop_json.as_deref()).unwrap();
    assert_eq!(loaded.crop.as_ref(), Some(&crop));
    assert_eq!(SliderId::Exposure.get(&loaded.develop), 0.7);
    let mut session = DevelopSession::new(DevelopSettings::default());
    session.load_edit(item, loaded);
    assert_eq!(session.crop(), Some(&crop));

    let packet = std::fs::read_to_string(&sidecar).unwrap();
    assert!(packet.contains("crs:HasCrop=\"True\""), "{packet}");
    assert!(packet.contains("crs:CropLeft=\"0.12\""), "{packet}");
    assert!(packet.contains("crs:CropRight=\"0.72\""), "{packet}");
    assert!(packet.contains("crs:CropAngle=\"3.25\""), "{packet}");
}

#[test]
fn resetting_the_crop_clears_the_stored_crop() {
    let dir = tempfile::TempDir::new().unwrap();
    let catalog = Catalog::open(&dir.path().join("catalog.sqlite")).unwrap();
    let item = seed(&catalog);
    let sidecar = dir.path().join("DSC00001.xmp");
    let develop = DevelopSettings::default();
    let crop = Crop {
        rect: Rect::new(0.1, 0.1, 0.5, 0.5),
        rotation_degrees: -1.0,
        aspect: None,
    };
    let first = edit_json(&develop, Some(&crop)).unwrap();
    persist_edit(
        &catalog,
        item,
        &first.develop,
        first.crop.as_deref(),
        &sidecar,
    )
    .unwrap();
    assert!(catalog.get_edit(item).unwrap().unwrap().crop_json.is_some());

    // A reset draft commits as no crop, and the row follows.
    let reset = edit_json(&develop, None).unwrap();
    persist_edit(
        &catalog,
        item,
        &reset.develop,
        reset.crop.as_deref(),
        &sidecar,
    )
    .unwrap();
    assert_eq!(catalog.get_edit(item).unwrap().unwrap().crop_json, None);
    let packet = std::fs::read_to_string(&sidecar).unwrap();
    assert!(packet.contains("crs:HasCrop=\"False\""), "{packet}");
}
