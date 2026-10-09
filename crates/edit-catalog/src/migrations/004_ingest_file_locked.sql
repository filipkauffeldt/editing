-- 004_ingest_file_locked: whether the camera had the file locked.
--
-- A photo protected in the camera (Nikon "Protect") is stored on the card as
-- a read-only file. The ingest scan records that per file, so "Only locked
-- photos" can copy just those and the history keeps the fact.

ALTER TABLE ingest_files ADD COLUMN locked INTEGER NOT NULL DEFAULT 0;
