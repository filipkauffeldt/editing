-- 001_initial_schema: the full v1 data model (plan section 1.3).
--
-- `items` is the unit that edits, selections, retouch recipes, detections,
-- and sends attach to. A paired RAW+JPEG item stamps both files; an unpaired
-- file makes an item with one side null.

CREATE TABLE roots (
  id INTEGER PRIMARY KEY,
  path TEXT NOT NULL UNIQUE,
  kind TEXT NOT NULL,              -- folder | card
  added_at INTEGER NOT NULL
);

CREATE TABLE iptc_templates (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL,
  fields_json TEXT NOT NULL,
  scope TEXT NOT NULL,             -- global | ingest
  updated_at INTEGER NOT NULL
);

CREATE TABLE ingest_batches (
  id INTEGER PRIMARY KEY,
  volume_uuid TEXT,
  volume_label TEXT,
  session_name TEXT,
  dest_dir TEXT NOT NULL,
  backup_dir TEXT,
  template_id INTEGER REFERENCES iptc_templates(id),
  state TEXT NOT NULL,
  started_at INTEGER NOT NULL,
  finished_at INTEGER
);

CREATE TABLE photos (
  id INTEGER PRIMARY KEY,
  root_id INTEGER NOT NULL REFERENCES roots(id),
  path TEXT NOT NULL UNIQUE,
  dir TEXT NOT NULL,
  base_name TEXT NOT NULL,         -- DSC01234
  ext TEXT NOT NULL,
  kind TEXT NOT NULL,              -- raw | jpeg
  bytes INTEGER NOT NULL,
  mtime INTEGER NOT NULL,
  width INTEGER, height INTEGER, orientation INTEGER,
  taken_at INTEGER, camera TEXT, lens TEXT,
  iso INTEGER, aperture REAL, shutter TEXT, focal INTEGER,
  rating INTEGER NOT NULL DEFAULT 0,
  color_label TEXT,
  ingest_batch_id INTEGER REFERENCES ingest_batches(id),
  created_at INTEGER NOT NULL
);

CREATE TABLE items (
  id INTEGER PRIMARY KEY,
  dir TEXT NOT NULL,
  base_name TEXT NOT NULL,
  raw_photo_id INTEGER REFERENCES photos(id),
  jpeg_photo_id INTEGER REFERENCES photos(id),
  UNIQUE(dir, base_name)
);

CREATE TABLE edits (
  item_id INTEGER PRIMARY KEY REFERENCES items(id),
  develop_json TEXT NOT NULL DEFAULT '{}',   -- LightCraft DevelopSettings
  crop_json TEXT,
  updated_at INTEGER NOT NULL,
  crs_written_at INTEGER
);

CREATE TABLE selections (
  id INTEGER PRIMARY KEY,
  item_id INTEGER NOT NULL REFERENCES items(id),
  name TEXT,
  shapes_json TEXT NOT NULL,       -- normalized shapes
  updated_at INTEGER NOT NULL
);

CREATE TABLE masks (                 -- frozen rasterized masks
  selection_id INTEGER NOT NULL REFERENCES selections(id),
  size TEXT NOT NULL,              -- proxy | full
  hash TEXT NOT NULL,
  rle BLOB NOT NULL,
  PRIMARY KEY (selection_id, size)
);

CREATE TABLE retouch_recipes (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL,
  ops_json TEXT NOT NULL,
  is_sticky INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL
);

CREATE TABLE retouch_versions (
  id INTEGER PRIMARY KEY,
  item_id INTEGER NOT NULL REFERENCES items(id),
  parent_id INTEGER REFERENCES retouch_versions(id),
  recipe_json TEXT NOT NULL,
  is_top INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL
);

CREATE TABLE presets (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL,
  group_name TEXT,
  path TEXT NOT NULL UNIQUE,      -- crs:XMP file
  source TEXT NOT NULL,           -- user | imported | lightroom
  updated_at INTEGER NOT NULL
);

CREATE TABLE detections (
  id INTEGER PRIMARY KEY,
  item_id INTEGER NOT NULL REFERENCES items(id),
  region_json TEXT NOT NULL,
  boxes_json TEXT NOT NULL,
  detector TEXT NOT NULL,
  created_at INTEGER NOT NULL
);

CREATE TABLE jobs (
  id INTEGER PRIMARY KEY,
  kind TEXT NOT NULL,
  payload_json TEXT NOT NULL,
  state TEXT NOT NULL,             -- queued | running | done | failed | cancelled
  attempts INTEGER NOT NULL DEFAULT 0,
  error TEXT,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
CREATE INDEX idx_jobs_state ON jobs(state);

CREATE TABLE send_recipes (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL,
  config_json TEXT NOT NULL,
  is_active INTEGER NOT NULL DEFAULT 0,
  updated_at INTEGER NOT NULL
);

CREATE TABLE transfers (
  id INTEGER PRIMARY KEY,
  item_id INTEGER NOT NULL REFERENCES items(id),
  recipe_id INTEGER NOT NULL REFERENCES send_recipes(id),
  remote_path TEXT NOT NULL,
  state TEXT NOT NULL,             -- queued | uploading | done | failed
  bytes_done INTEGER NOT NULL DEFAULT 0,
  attempts INTEGER NOT NULL DEFAULT 0,
  error TEXT,
  updated_at INTEGER NOT NULL
);

CREATE TABLE code_replacements (
  id INTEGER PRIMARY KEY,
  group_name TEXT NOT NULL,
  code TEXT NOT NULL,
  value TEXT NOT NULL,
  UNIQUE(group_name, code)
);

CREATE TABLE settings (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
