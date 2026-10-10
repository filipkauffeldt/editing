-- 005_preset_files: file stamp of each indexed preset.
--
-- The preset folder scan compares a file's modified time and size with these
-- columns, so an unchanged preset is not read again.

ALTER TABLE presets ADD COLUMN mtime INTEGER NOT NULL DEFAULT 0;
ALTER TABLE presets ADD COLUMN size INTEGER NOT NULL DEFAULT 0;
