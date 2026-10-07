-- 002_ingest_files: per-file ingest history.
--
-- One row per file the ingest copy pipeline saw, keyed for the
-- Lightroom-style duplicate check on (original name, EXIF capture time,
-- size). `dest_path`/`backup_path`/`blake3`/`photo_id` fill in as the copy,
-- verify, backup and catalog stages complete; `state` is the live per-file
-- state shown in the ingest dialog.

CREATE TABLE ingest_files (
  id INTEGER PRIMARY KEY,
  batch_id INTEGER NOT NULL REFERENCES ingest_batches(id),
  original_name TEXT NOT NULL,
  taken_at INTEGER,
  bytes INTEGER NOT NULL,
  source_path TEXT NOT NULL,
  dest_path TEXT,
  backup_path TEXT,
  blake3 TEXT,
  photo_id INTEGER REFERENCES photos(id),
  state TEXT NOT NULL,             -- queued | copying | verifying | backup | renaming | cataloged | duplicate | failed
  error TEXT,
  created_at INTEGER NOT NULL
);

CREATE INDEX idx_ingest_files_dedupe ON ingest_files(original_name, taken_at, bytes);
CREATE INDEX idx_ingest_files_batch ON ingest_files(batch_id);
