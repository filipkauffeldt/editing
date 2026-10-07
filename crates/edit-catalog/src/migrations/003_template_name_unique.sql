-- 003_template_name_unique: one row per template name.
--
-- Templates are picked and referenced by name in the UI (the Select names
-- a row); two rows sharing a name would make the lookup ambiguous.
-- Collapse existing duplicates (oldest wins) behind a unique index.

DELETE FROM iptc_templates WHERE id NOT IN (SELECT MIN(id) FROM iptc_templates GROUP BY name);

CREATE UNIQUE INDEX iptc_templates_name_unique ON iptc_templates (name);
