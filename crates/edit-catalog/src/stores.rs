//! Typed stores: one small function set per table for later PRs.
//!
//! Reads return [`crate::models`] rows; writes take plain parameters (or a
//! `New*` struct for wide tables). Timestamps use [`crate::db::now_unix`].

use rusqlite::{OptionalExtension, Row, params};

use crate::db::{Catalog, now_unix};
use crate::error::{Error, Result};
use crate::models::*;

// ---- row readers ----

fn row_root(row: &Row) -> rusqlite::Result<Root> {
    Ok(Root {
        id: row.get(0)?,
        path: row.get(1)?,
        kind: row.get(2)?,
        added_at: row.get(3)?,
    })
}

fn row_photo(row: &Row) -> rusqlite::Result<Photo> {
    Ok(Photo {
        id: row.get(0)?,
        root_id: row.get(1)?,
        path: row.get(2)?,
        dir: row.get(3)?,
        base_name: row.get(4)?,
        ext: row.get(5)?,
        kind: row.get(6)?,
        bytes: row.get(7)?,
        mtime: row.get(8)?,
        width: row.get(9)?,
        height: row.get(10)?,
        orientation: row.get(11)?,
        taken_at: row.get(12)?,
        camera: row.get(13)?,
        lens: row.get(14)?,
        iso: row.get(15)?,
        aperture: row.get(16)?,
        shutter: row.get(17)?,
        focal: row.get(18)?,
        rating: row.get(19)?,
        color_label: row.get(20)?,
        ingest_batch_id: row.get(21)?,
        created_at: row.get(22)?,
    })
}

fn row_item(row: &Row) -> rusqlite::Result<Item> {
    Ok(Item {
        id: row.get(0)?,
        dir: row.get(1)?,
        base_name: row.get(2)?,
        raw_photo_id: row.get(3)?,
        jpeg_photo_id: row.get(4)?,
    })
}

fn row_edit(row: &Row) -> rusqlite::Result<Edit> {
    Ok(Edit {
        item_id: row.get(0)?,
        develop_json: row.get(1)?,
        crop_json: row.get(2)?,
        updated_at: row.get(3)?,
        crs_written_at: row.get(4)?,
    })
}

fn row_selection(row: &Row) -> rusqlite::Result<Selection> {
    Ok(Selection {
        id: row.get(0)?,
        item_id: row.get(1)?,
        name: row.get(2)?,
        shapes_json: row.get(3)?,
        updated_at: row.get(4)?,
    })
}

fn row_mask(row: &Row) -> rusqlite::Result<Mask> {
    Ok(Mask {
        selection_id: row.get(0)?,
        size: row.get(1)?,
        hash: row.get(2)?,
        rle: row.get(3)?,
    })
}

fn row_recipe(row: &Row) -> rusqlite::Result<RetouchRecipeRow> {
    Ok(RetouchRecipeRow {
        id: row.get(0)?,
        name: row.get(1)?,
        ops_json: row.get(2)?,
        is_sticky: row.get::<_, i64>(3)? != 0,
        created_at: row.get(4)?,
    })
}

fn row_version(row: &Row) -> rusqlite::Result<RetouchVersion> {
    Ok(RetouchVersion {
        id: row.get(0)?,
        item_id: row.get(1)?,
        parent_id: row.get(2)?,
        recipe_json: row.get(3)?,
        is_top: row.get::<_, i64>(4)? != 0,
        created_at: row.get(5)?,
    })
}

fn row_preset(row: &Row) -> rusqlite::Result<Preset> {
    Ok(Preset {
        id: row.get(0)?,
        name: row.get(1)?,
        group_name: row.get(2)?,
        path: row.get(3)?,
        source: row.get(4)?,
        updated_at: row.get(5)?,
    })
}

fn row_detection(row: &Row) -> rusqlite::Result<Detection> {
    Ok(Detection {
        id: row.get(0)?,
        item_id: row.get(1)?,
        region_json: row.get(2)?,
        boxes_json: row.get(3)?,
        detector: row.get(4)?,
        created_at: row.get(5)?,
    })
}

fn row_transfer(row: &Row) -> rusqlite::Result<Transfer> {
    Ok(Transfer {
        id: row.get(0)?,
        item_id: row.get(1)?,
        recipe_id: row.get(2)?,
        remote_path: row.get(3)?,
        state: row.get(4)?,
        bytes_done: row.get(5)?,
        attempts: row.get(6)?,
        error: row.get(7)?,
        updated_at: row.get(8)?,
    })
}

fn row_template(row: &Row) -> rusqlite::Result<IptcTemplate> {
    Ok(IptcTemplate {
        id: row.get(0)?,
        name: row.get(1)?,
        fields_json: row.get(2)?,
        scope: row.get(3)?,
        updated_at: row.get(4)?,
    })
}

fn row_send_recipe(row: &Row) -> rusqlite::Result<SendRecipe> {
    Ok(SendRecipe {
        id: row.get(0)?,
        name: row.get(1)?,
        config_json: row.get(2)?,
        is_active: row.get::<_, i64>(3)? != 0,
        updated_at: row.get(4)?,
    })
}

fn row_code(row: &Row) -> rusqlite::Result<CodeReplacement> {
    Ok(CodeReplacement {
        id: row.get(0)?,
        group_name: row.get(1)?,
        code: row.get(2)?,
        value: row.get(3)?,
    })
}

fn row_batch(row: &Row) -> rusqlite::Result<IngestBatch> {
    Ok(IngestBatch {
        id: row.get(0)?,
        volume_uuid: row.get(1)?,
        volume_label: row.get(2)?,
        session_name: row.get(3)?,
        dest_dir: row.get(4)?,
        backup_dir: row.get(5)?,
        template_id: row.get(6)?,
        state: row.get(7)?,
        started_at: row.get(8)?,
        finished_at: row.get(9)?,
    })
}

fn not_found(what: &str, id: i64) -> Error {
    Error::NotFound(format!("{what} {id}"))
}

impl Catalog {
    // ---- roots ----

    pub fn insert_root(&self, path: &str, kind: &str) -> Result<i64> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO roots (path, kind, added_at) VALUES (?1, ?2, ?3)",
                params![path, kind, now_unix()],
            )?;
            Ok(conn.last_insert_rowid())
        })
    }

    pub fn get_root(&self, id: i64) -> Result<Root> {
        self.with_conn(|conn| {
            conn.query_row(
                "SELECT id, path, kind, added_at FROM roots WHERE id = ?1",
                [id],
                row_root,
            )
            .optional()?
            .ok_or_else(|| not_found("root", id))
        })
    }

    pub fn get_root_by_path(&self, path: &str) -> Result<Option<Root>> {
        self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    "SELECT id, path, kind, added_at FROM roots WHERE path = ?1",
                    [path],
                    row_root,
                )
                .optional()?)
        })
    }

    pub fn list_roots(&self) -> Result<Vec<Root>> {
        self.with_conn(|conn| {
            let mut stmt =
                conn.prepare("SELECT id, path, kind, added_at FROM roots ORDER BY id")?;
            Ok(stmt
                .query_map([], row_root)?
                .collect::<rusqlite::Result<_>>()?)
        })
    }

    // ---- photos ----

    pub fn insert_photo(&self, photo: &NewPhoto) -> Result<i64> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO photos (root_id, path, dir, base_name, ext, kind, bytes, mtime, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    photo.root_id,
                    photo.path,
                    photo.dir,
                    photo.base_name,
                    photo.ext,
                    photo.kind,
                    photo.bytes,
                    photo.mtime,
                    now_unix()
                ],
            )?;
            Ok(conn.last_insert_rowid())
        })
    }

    pub fn get_photo(&self, id: i64) -> Result<Photo> {
        self.with_conn(|conn| {
            conn.query_row("SELECT id, root_id, path, dir, base_name, ext, kind, bytes, mtime, width, height, orientation, taken_at, camera, lens, iso, aperture, shutter, focal, rating, color_label, ingest_batch_id, created_at FROM photos WHERE id = ?1", [id], row_photo)
                .optional()?
                .ok_or_else(|| not_found("photo", id))
        })
    }

    pub fn list_photos_by_root(&self, root_id: i64) -> Result<Vec<Photo>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare("SELECT id, root_id, path, dir, base_name, ext, kind, bytes, mtime, width, height, orientation, taken_at, camera, lens, iso, aperture, shutter, focal, rating, color_label, ingest_batch_id, created_at FROM photos WHERE root_id = ?1 ORDER BY id")?;
            Ok(stmt
                .query_map([root_id], row_photo)?
                .collect::<rusqlite::Result<_>>()?)
        })
    }

    pub fn set_photo_rating(&self, id: i64, rating: i64) -> Result<()> {
        self.with_conn(|conn| {
            let n = conn.execute(
                "UPDATE photos SET rating = ?1 WHERE id = ?2",
                params![rating, id],
            )?;
            if n == 0 {
                return Err(not_found("photo", id));
            }
            Ok(())
        })
    }

    // ---- items ----

    pub fn insert_item(
        &self,
        dir: &str,
        base_name: &str,
        raw_photo_id: Option<i64>,
        jpeg_photo_id: Option<i64>,
    ) -> Result<i64> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO items (dir, base_name, raw_photo_id, jpeg_photo_id)
                 VALUES (?1, ?2, ?3, ?4)",
                params![dir, base_name, raw_photo_id, jpeg_photo_id],
            )?;
            Ok(conn.last_insert_rowid())
        })
    }

    pub fn get_item(&self, id: i64) -> Result<Item> {
        self.with_conn(|conn| {
            conn.query_row(
                "SELECT id, dir, base_name, raw_photo_id, jpeg_photo_id FROM items WHERE id = ?1",
                [id],
                row_item,
            )
            .optional()?
            .ok_or_else(|| not_found("item", id))
        })
    }

    pub fn get_item_by_dir_base(&self, dir: &str, base_name: &str) -> Result<Option<Item>> {
        self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    "SELECT id, dir, base_name, raw_photo_id, jpeg_photo_id FROM items WHERE dir = ?1 AND base_name = ?2",
                    params![dir, base_name],
                    row_item,
                )
                .optional()?)
        })
    }

    /// Fill one side of an existing pair item: a rescan completing a
    /// half-pair (RAW arrived first, JPEG later, or vice versa) lands here
    /// instead of inserting a second row. No migration: plain `UPDATE`.
    pub fn set_item_side(&self, id: i64, photo_id: i64, raw: bool) -> Result<()> {
        self.with_conn(|conn| {
            let sql = if raw {
                "UPDATE items SET raw_photo_id = ?1 WHERE id = ?2"
            } else {
                "UPDATE items SET jpeg_photo_id = ?1 WHERE id = ?2"
            };
            let n = conn.execute(sql, params![photo_id, id])?;
            if n == 0 {
                return Err(not_found("item", id));
            }
            Ok(())
        })
    }

    // ---- edits ----

    pub fn upsert_edit(
        &self,
        item_id: i64,
        develop_json: &str,
        crop_json: Option<&str>,
    ) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO edits (item_id, develop_json, crop_json, updated_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(item_id) DO UPDATE SET
                   develop_json = excluded.develop_json,
                   crop_json = excluded.crop_json,
                   updated_at = excluded.updated_at",
                params![item_id, develop_json, crop_json, now_unix()],
            )?;
            Ok(())
        })
    }

    pub fn get_edit(&self, item_id: i64) -> Result<Option<Edit>> {
        self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    "SELECT item_id, develop_json, crop_json, updated_at, crs_written_at FROM edits WHERE item_id = ?1",
                    [item_id],
                    row_edit,
                )
                .optional()?)
        })
    }

    pub fn mark_crs_written(&self, item_id: i64) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE edits SET crs_written_at = ?1 WHERE item_id = ?2",
                params![now_unix(), item_id],
            )?;
            Ok(())
        })
    }

    // ---- selections / masks ----

    pub fn insert_selection(
        &self,
        item_id: i64,
        name: Option<&str>,
        shapes_json: &str,
    ) -> Result<i64> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO selections (item_id, name, shapes_json, updated_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![item_id, name, shapes_json, now_unix()],
            )?;
            Ok(conn.last_insert_rowid())
        })
    }

    pub fn get_selection(&self, id: i64) -> Result<Selection> {
        self.with_conn(|conn| {
            conn.query_row(
                "SELECT id, item_id, name, shapes_json, updated_at FROM selections WHERE id = ?1",
                [id],
                row_selection,
            )
            .optional()?
            .ok_or_else(|| not_found("selection", id))
        })
    }

    pub fn list_selections_for_item(&self, item_id: i64) -> Result<Vec<Selection>> {
        self.with_conn(|conn| {
            let mut stmt =
                conn.prepare("SELECT id, item_id, name, shapes_json, updated_at FROM selections WHERE item_id = ?1 ORDER BY id")?;
            Ok(stmt
                .query_map([item_id], row_selection)?
                .collect::<rusqlite::Result<_>>()?)
        })
    }

    pub fn upsert_mask(&self, selection_id: i64, size: &str, hash: &str, rle: &[u8]) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO masks (selection_id, size, hash, rle) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(selection_id, size) DO UPDATE SET hash = excluded.hash, rle = excluded.rle",
                params![selection_id, size, hash, rle],
            )?;
            Ok(())
        })
    }

    pub fn get_mask(&self, selection_id: i64, size: &str) -> Result<Option<Mask>> {
        self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    "SELECT selection_id, size, hash, rle FROM masks WHERE selection_id = ?1 AND size = ?2",
                    params![selection_id, size],
                    row_mask,
                )
                .optional()?)
        })
    }

    // ---- retouch recipes / versions ----

    pub fn insert_retouch_recipe(
        &self,
        name: &str,
        ops_json: &str,
        is_sticky: bool,
    ) -> Result<i64> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO retouch_recipes (name, ops_json, is_sticky, created_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![name, ops_json, is_sticky as i64, now_unix()],
            )?;
            Ok(conn.last_insert_rowid())
        })
    }

    pub fn get_retouch_recipe(&self, id: i64) -> Result<RetouchRecipeRow> {
        self.with_conn(|conn| {
            conn.query_row(
                "SELECT id, name, ops_json, is_sticky, created_at FROM retouch_recipes WHERE id = ?1",
                [id],
                row_recipe,
            )
            .optional()?
            .ok_or_else(|| not_found("retouch_recipe", id))
        })
    }

    pub fn list_retouch_recipes(&self) -> Result<Vec<RetouchRecipeRow>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, name, ops_json, is_sticky, created_at FROM retouch_recipes ORDER BY id",
            )?;
            Ok(stmt
                .query_map([], row_recipe)?
                .collect::<rusqlite::Result<_>>()?)
        })
    }

    pub fn insert_retouch_version(
        &self,
        item_id: i64,
        parent_id: Option<i64>,
        recipe_json: &str,
        is_top: bool,
    ) -> Result<i64> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO retouch_versions (item_id, parent_id, recipe_json, is_top, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![item_id, parent_id, recipe_json, is_top as i64, now_unix()],
            )?;
            Ok(conn.last_insert_rowid())
        })
    }

    pub fn list_retouch_versions_for_item(&self, item_id: i64) -> Result<Vec<RetouchVersion>> {
        self.with_conn(|conn| {
            let mut stmt =
                conn.prepare("SELECT id, item_id, parent_id, recipe_json, is_top, created_at FROM retouch_versions WHERE item_id = ?1 ORDER BY id")?;
            Ok(stmt
                .query_map([item_id], row_version)?
                .collect::<rusqlite::Result<_>>()?)
        })
    }

    /// The newest version flagged as the export top. Callers keep at most
    /// one `is_top` row per item; this is a convention, not a constraint —
    /// follow-up: a partial unique index on (item_id) WHERE is_top = 1.
    pub fn top_retouch_version(&self, item_id: i64) -> Result<Option<RetouchVersion>> {
        self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    "SELECT id, item_id, parent_id, recipe_json, is_top, created_at FROM retouch_versions WHERE item_id = ?1 AND is_top = 1 ORDER BY id DESC LIMIT 1",
                    [item_id],
                    row_version,
                )
                .optional()?)
        })
    }

    // ---- presets ----

    pub fn insert_preset(
        &self,
        name: &str,
        group: Option<&str>,
        path: &str,
        source: &str,
    ) -> Result<i64> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO presets (name, group_name, path, source, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![name, group, path, source, now_unix()],
            )?;
            Ok(conn.last_insert_rowid())
        })
    }

    pub fn list_presets(&self) -> Result<Vec<Preset>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, name, group_name, path, source, updated_at FROM presets ORDER BY id",
            )?;
            Ok(stmt
                .query_map([], row_preset)?
                .collect::<rusqlite::Result<_>>()?)
        })
    }

    // ---- detections ----

    pub fn insert_detection(
        &self,
        item_id: i64,
        region_json: &str,
        boxes_json: &str,
        detector: &str,
    ) -> Result<i64> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO detections (item_id, region_json, boxes_json, detector, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![item_id, region_json, boxes_json, detector, now_unix()],
            )?;
            Ok(conn.last_insert_rowid())
        })
    }

    pub fn list_detections_for_item(&self, item_id: i64) -> Result<Vec<Detection>> {
        self.with_conn(|conn| {
            let mut stmt =
                conn.prepare("SELECT id, item_id, region_json, boxes_json, detector, created_at FROM detections WHERE item_id = ?1 ORDER BY id")?;
            Ok(stmt
                .query_map([item_id], row_detection)?
                .collect::<rusqlite::Result<_>>()?)
        })
    }

    // ---- transfers ----

    /// States a transfer row may hold (plan section 1.3).
    const TRANSFER_STATES: &[&str] = &["queued", "uploading", "done", "failed"];

    fn check_transfer_state(state: &str) -> Result<()> {
        if Self::TRANSFER_STATES.contains(&state) {
            Ok(())
        } else {
            Err(Error::InvalidValue(format!(
                "unknown transfer state: {state}"
            )))
        }
    }

    pub fn insert_transfer(&self, item_id: i64, recipe_id: i64, remote_path: &str) -> Result<i64> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO transfers (item_id, recipe_id, remote_path, state, updated_at)
                 VALUES (?1, ?2, ?3, 'queued', ?4)",
                params![item_id, recipe_id, remote_path, now_unix()],
            )?;
            Ok(conn.last_insert_rowid())
        })
    }

    pub fn get_transfer(&self, id: i64) -> Result<Transfer> {
        self.with_conn(|conn| {
            conn.query_row("SELECT id, item_id, recipe_id, remote_path, state, bytes_done, attempts, error, updated_at FROM transfers WHERE id = ?1", [id], row_transfer)
                .optional()?
                .ok_or_else(|| not_found("transfer", id))
        })
    }

    pub fn list_transfers_for_item(&self, item_id: i64) -> Result<Vec<Transfer>> {
        self.with_conn(|conn| {
            let mut stmt =
                conn.prepare("SELECT id, item_id, recipe_id, remote_path, state, bytes_done, attempts, error, updated_at FROM transfers WHERE item_id = ?1 ORDER BY id")?;
            Ok(stmt
                .query_map([item_id], row_transfer)?
                .collect::<rusqlite::Result<_>>()?)
        })
    }

    /// Move a transfer to a new state. `attempts` counts delivery tries, so
    /// it only bumps on terminal/error transitions (`done`, `failed`);
    /// per-chunk progress goes through [`Catalog::update_transfer_progress`].
    pub fn set_transfer_state(
        &self,
        id: i64,
        state: &str,
        bytes_done: i64,
        error: Option<&str>,
    ) -> Result<()> {
        Self::check_transfer_state(state)?;
        let bump_attempts = matches!(state, "done" | "failed");
        self.with_conn(|conn| {
            let n = conn.execute(
                "UPDATE transfers SET state = ?1, bytes_done = ?2, error = ?3,
                     attempts = attempts + ?4, updated_at = ?5 WHERE id = ?6",
                params![
                    state,
                    bytes_done,
                    error,
                    bump_attempts as i64,
                    now_unix(),
                    id
                ],
            )?;
            if n == 0 {
                return Err(not_found("transfer", id));
            }
            Ok(())
        })
    }

    /// Record upload progress without touching state or attempts.
    pub fn update_transfer_progress(&self, id: i64, bytes_done: i64) -> Result<()> {
        self.with_conn(|conn| {
            let n = conn.execute(
                "UPDATE transfers SET bytes_done = ?1, updated_at = ?2 WHERE id = ?3",
                params![bytes_done, now_unix(), id],
            )?;
            if n == 0 {
                return Err(not_found("transfer", id));
            }
            Ok(())
        })
    }

    // ---- IPTC templates ----

    pub fn insert_iptc_template(&self, name: &str, fields_json: &str, scope: &str) -> Result<i64> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO iptc_templates (name, fields_json, scope, updated_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![name, fields_json, scope, now_unix()],
            )?;
            Ok(conn.last_insert_rowid())
        })
    }

    pub fn get_iptc_template(&self, id: i64) -> Result<IptcTemplate> {
        self.with_conn(|conn| {
            conn.query_row(
                "SELECT id, name, fields_json, scope, updated_at FROM iptc_templates WHERE id = ?1",
                [id],
                row_template,
            )
            .optional()?
            .ok_or_else(|| not_found("iptc_template", id))
        })
    }

    pub fn list_iptc_templates(&self) -> Result<Vec<IptcTemplate>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, name, fields_json, scope, updated_at FROM iptc_templates ORDER BY id",
            )?;
            Ok(stmt
                .query_map([], row_template)?
                .collect::<rusqlite::Result<_>>()?)
        })
    }

    // ---- send recipes ----

    pub fn insert_send_recipe(
        &self,
        name: &str,
        config_json: &str,
        is_active: bool,
    ) -> Result<i64> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO send_recipes (name, config_json, is_active, updated_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![name, config_json, is_active as i64, now_unix()],
            )?;
            Ok(conn.last_insert_rowid())
        })
    }

    pub fn get_send_recipe(&self, id: i64) -> Result<SendRecipe> {
        self.with_conn(|conn| {
            conn.query_row(
                "SELECT id, name, config_json, is_active, updated_at FROM send_recipes WHERE id = ?1",
                [id],
                row_send_recipe,
            )
            .optional()?
            .ok_or_else(|| not_found("send_recipe", id))
        })
    }

    /// The newest recipe flagged active. Callers keep at most one active
    /// recipe; this is a convention, not a constraint — follow-up: a partial
    /// unique index on (is_active) WHERE is_active = 1.
    pub fn active_send_recipe(&self) -> Result<Option<SendRecipe>> {
        self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    "SELECT id, name, config_json, is_active, updated_at FROM send_recipes WHERE is_active = 1 ORDER BY id DESC LIMIT 1",
                    [],
                    row_send_recipe,
                )
                .optional()?)
        })
    }

    // ---- code replacements ----

    pub fn upsert_code(&self, group: &str, code: &str, value: &str) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO code_replacements (group_name, code, value) VALUES (?1, ?2, ?3)
                 ON CONFLICT(group_name, code) DO UPDATE SET value = excluded.value",
                params![group, code, value],
            )?;
            Ok(())
        })
    }

    pub fn lookup_code(&self, group: &str, code: &str) -> Result<Option<String>> {
        self.with_conn(|conn| {
            Ok(conn
                .query_row(
                    "SELECT value FROM code_replacements WHERE group_name = ?1 AND code = ?2",
                    params![group, code],
                    |row| row.get(0),
                )
                .optional()?)
        })
    }

    pub fn list_codes_in_group(&self, group: &str) -> Result<Vec<CodeReplacement>> {
        self.with_conn(|conn| {
            let mut stmt = conn
                .prepare("SELECT id, group_name, code, value FROM code_replacements WHERE group_name = ?1 ORDER BY code")?;
            Ok(stmt
                .query_map([group], row_code)?
                .collect::<rusqlite::Result<_>>()?)
        })
    }

    // ---- ingest batches ----

    /// States an ingest batch may hold: active, then one terminal state.
    const INGEST_BATCH_STATES: &[&str] = &["running", "done", "failed"];

    fn check_ingest_batch_state(state: &str) -> Result<()> {
        if Self::INGEST_BATCH_STATES.contains(&state) {
            Ok(())
        } else {
            Err(Error::InvalidValue(format!(
                "unknown ingest batch state: {state}"
            )))
        }
    }

    pub fn insert_ingest_batch(&self, dest_dir: &str, state: &str) -> Result<i64> {
        Self::check_ingest_batch_state(state)?;
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO ingest_batches (dest_dir, state, started_at)
                 VALUES (?1, ?2, ?3)",
                params![dest_dir, state, now_unix()],
            )?;
            Ok(conn.last_insert_rowid())
        })
    }

    pub fn get_ingest_batch(&self, id: i64) -> Result<IngestBatch> {
        self.with_conn(|conn| {
            conn.query_row(
                "SELECT id, volume_uuid, volume_label, session_name, dest_dir, backup_dir, template_id, state, started_at, finished_at FROM ingest_batches WHERE id = ?1",
                [id],
                row_batch,
            )
            .optional()?
            .ok_or_else(|| not_found("ingest_batch", id))
        })
    }

    pub fn finish_ingest_batch(&self, id: i64, state: &str) -> Result<()> {
        Self::check_ingest_batch_state(state)?;
        self.with_conn(|conn| {
            let n = conn.execute(
                "UPDATE ingest_batches SET state = ?1, finished_at = ?2 WHERE id = ?3",
                params![state, now_unix(), id],
            )?;
            if n == 0 {
                return Err(not_found("ingest_batch", id));
            }
            Ok(())
        })
    }

    // ---- settings ----

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO settings (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )?;
            Ok(())
        })
    }

    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        self.with_conn(|conn| {
            Ok(conn
                .query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| {
                    row.get(0)
                })
                .optional()?)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::tests::scratch_db;

    fn photo_fixture(root_id: i64, n: i64) -> NewPhoto {
        NewPhoto {
            root_id,
            path: format!("/photos/DSC{n:05}.NEF"),
            dir: "/photos".into(),
            base_name: format!("DSC{n:05}"),
            ext: "NEF".into(),
            kind: "raw".into(),
            bytes: 1024,
            mtime: 1_700_000_000,
        }
    }

    #[test]
    fn stores_cover_every_table() {
        let path = scratch_db("stores");
        let db = Catalog::open(&path).unwrap();

        let root = db.insert_root("/photos", "folder").unwrap();
        assert_eq!(db.get_root(root).unwrap().path, "/photos");
        assert_eq!(db.get_root_by_path("/photos").unwrap().unwrap().id, root);
        assert_eq!(db.list_roots().unwrap().len(), 1);

        let template = db
            .insert_iptc_template("press", r#"{"credit":"x"}"#, "ingest")
            .unwrap();
        assert_eq!(db.get_iptc_template(template).unwrap().scope, "ingest");
        assert_eq!(db.list_iptc_templates().unwrap().len(), 1);

        let batch = db.insert_ingest_batch("/photos/2026", "running").unwrap();
        db.finish_ingest_batch(batch, "done").unwrap();
        let batch_row = db.get_ingest_batch(batch).unwrap();
        assert_eq!(batch_row.state, "done");
        assert!(batch_row.finished_at.is_some());
        assert!(db.insert_ingest_batch("/photos/2026", "bogus").is_err());

        let photo = db.insert_photo(&photo_fixture(root, 1)).unwrap();
        assert_eq!(db.get_photo(photo).unwrap().base_name, "DSC00001");
        assert_eq!(db.list_photos_by_root(root).unwrap().len(), 1);
        db.set_photo_rating(photo, 5).unwrap();
        assert_eq!(db.get_photo(photo).unwrap().rating, 5);

        let item = db
            .insert_item("/photos", "DSC00001", Some(photo), None)
            .unwrap();
        assert_eq!(db.get_item(item).unwrap().raw_photo_id, Some(photo));
        assert_eq!(
            db.get_item_by_dir_base("/photos", "DSC00001")
                .unwrap()
                .unwrap()
                .id,
            item
        );

        db.upsert_edit(item, r#"{"version":1}"#, None).unwrap();
        db.upsert_edit(item, r#"{"version":2}"#, Some(r#"{"aspect":"3:2"}"#))
            .unwrap();
        let edit = db.get_edit(item).unwrap().unwrap();
        assert_eq!(edit.develop_json, r#"{"version":2}"#);
        db.mark_crs_written(item).unwrap();
        assert!(db.get_edit(item).unwrap().unwrap().crs_written_at.is_some());

        let sel = db
            .insert_selection(item, Some("fan"), r#"[{"rect":[0,0,1,1]}]"#)
            .unwrap();
        assert_eq!(db.get_selection(sel).unwrap().name.as_deref(), Some("fan"));
        assert_eq!(db.list_selections_for_item(item).unwrap().len(), 1);
        db.upsert_mask(sel, "proxy", "abc", &[1, 2, 3, 4]).unwrap();
        assert_eq!(
            db.get_mask(sel, "proxy").unwrap().unwrap().rle,
            vec![1, 2, 3, 4]
        );
        assert!(db.get_mask(sel, "full").unwrap().is_none());

        let recipe = db
            .insert_retouch_recipe("faces", r#"{"ops":[]}"#, true)
            .unwrap();
        assert!(db.get_retouch_recipe(recipe).unwrap().is_sticky);
        assert_eq!(db.list_retouch_recipes().unwrap().len(), 1);

        let v1 = db
            .insert_retouch_version(item, None, r#"{"ops":[]}"#, false)
            .unwrap();
        let v2 = db
            .insert_retouch_version(item, Some(v1), r#"{"ops":[1]}"#, true)
            .unwrap();
        assert_eq!(db.list_retouch_versions_for_item(item).unwrap().len(), 2);
        assert_eq!(db.top_retouch_version(item).unwrap().unwrap().id, v2);

        db.insert_preset("warm", Some("user"), "/presets/warm.xmp", "user")
            .unwrap();
        assert_eq!(db.list_presets().unwrap().len(), 1);

        db.insert_detection(item, r#"{"x":0}"#, r#"[]"#, "libfacedetection")
            .unwrap();
        assert_eq!(db.list_detections_for_item(item).unwrap().len(), 1);

        let send = db
            .insert_send_recipe("ftp", r#"{"host":"x"}"#, true)
            .unwrap();
        assert_eq!(db.active_send_recipe().unwrap().unwrap().id, send);

        let tx = db.insert_transfer(item, send, "/remote/a.jpg").unwrap();
        db.set_transfer_state(tx, "uploading", 512, None).unwrap();
        let tx_row = db.get_transfer(tx).unwrap();
        assert_eq!(
            (tx_row.state.as_str(), tx_row.bytes_done),
            ("uploading", 512)
        );
        // Progress updates move bytes only: no attempt is consumed.
        db.update_transfer_progress(tx, 1024).unwrap();
        let tx_row = db.get_transfer(tx).unwrap();
        assert_eq!(
            (tx_row.state.as_str(), tx_row.bytes_done, tx_row.attempts),
            ("uploading", 1024, 0)
        );
        // Only terminal/error transitions bump attempts.
        db.set_transfer_state(tx, "failed", 1024, Some("reset by peer"))
            .unwrap();
        let tx_row = db.get_transfer(tx).unwrap();
        assert_eq!(tx_row.attempts, 1);
        assert!(db.set_transfer_state(tx, "bogus", 0, None).is_err());
        assert_eq!(db.list_transfers_for_item(item).unwrap().len(), 1);

        db.upsert_code("sports", "=h1=", "SHL").unwrap();
        assert_eq!(
            db.lookup_code("sports", "=h1=").unwrap().as_deref(),
            Some("SHL")
        );
        assert_eq!(db.list_codes_in_group("sports").unwrap().len(), 1);

        db.set_setting("theme", "dark").unwrap();
        assert_eq!(db.get_setting("theme").unwrap().as_deref(), Some("dark"));
        assert_eq!(db.get_setting("missing").unwrap(), None);

        assert!(db.get_root(999_999).is_err());
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn set_item_side_completes_a_half_pair() {
        let path = scratch_db("item-side");
        let db = Catalog::open(&path).unwrap();
        let root = db.insert_root("/photos", "folder").unwrap();
        let raw = db.insert_photo(&photo_fixture(root, 1)).unwrap();
        let item = db
            .insert_item("/photos", "DSC00001", Some(raw), None)
            .unwrap();
        assert_eq!(db.get_item(item).unwrap().jpeg_photo_id, None);

        let jpeg = db
            .insert_photo(&NewPhoto {
                path: "/photos/DSC00001.JPG".into(),
                ext: "JPG".into(),
                kind: "jpeg".into(),
                ..photo_fixture(root, 1)
            })
            .unwrap();
        db.set_item_side(item, jpeg, false).unwrap();
        let completed = db.get_item(item).unwrap();
        assert_eq!(completed.raw_photo_id, Some(raw));
        assert_eq!(completed.jpeg_photo_id, Some(jpeg));

        assert!(db.set_item_side(999_999, jpeg, false).is_err());
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn unique_violations_are_detected() {
        let path = scratch_db("unique-violation");
        let db = Catalog::open(&path).unwrap();
        let root = db.insert_root("/photos", "folder").unwrap();
        db.insert_photo(&photo_fixture(root, 1)).unwrap();
        let dup = db.insert_photo(&photo_fixture(root, 1)).unwrap_err();
        assert!(dup.is_unique_violation());
        assert!(!Error::NotFound("x".into()).is_unique_violation());
        std::fs::remove_file(path).ok();
    }
}
