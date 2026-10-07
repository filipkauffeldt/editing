# Decisions

The decision log for the editing app. `PLAN.md` is the source of truth; this
file is the ledger of what was decided and when. A decision moves to
*superseded* here only after `PLAN.md` is updated.

Statuses:

- **Settled** — recorded in `PLAN.md` and not contested since.
- **Superseded** — replaced by a later decision; the replacing decision is
  listed in the same row.

## Settled in planning (2026-08)

| # | Decision | Choice |
|---|---|---|
| 1 | License | Open source (Apache-2.0) |
| 2 | UI framework | ~~GPUI (base) + gpui-component, with gpui-base as the behavior layer (pin git revisions, expect churn)~~ Superseded by the A1 pin rows below: pinned as crates.io snapshots (`gpui-pre`, `gpui-pre-platform`, `gpui-kit`) |
| 3 | Render engine | LightCraft lower crates — `develop`, `pipeline`, `raw`, `codecs`, optional `preview` — linked in-process, git-pinned to a revision. No subprocess render engine |
| 4 | Edit model | LightCraft `DevelopSettings` for preview and export (single model; own tone math retired) |
| 5 | Raw handling | `lightcraft-raw` decode where supported; embedded camera preview otherwise; no fallback engine |
| 6 | Retouch | Own post-develop stage (mosaic default, Gaussian optional); pixel kernels from `photocraft-algo`, vendored if it churns |
| 7 | Selections | Own normalized shape model; wand/matting primitives from `photocraft-algo::{selection, matting}`; own GPUI canvas for lasso/brush |
| 8 | Face detection | Swappable `FaceDetector` trait; R0 bake-off libfacedetection-rust (BSD-3) vs tract+YuNet; SCRFD escalation only if crowd recall fails |
| 9 | Privacy default | Mosaic with strength presets; non-destructive recipes re-rendered at preview and export |
| 10 | LR interop | Best-effort `crs:` sidecars (ProcessVersion=11.0) so LR can read edits/crop; LR is never required in the flow |
| 11 | Metadata storage | XMP sidecar always for raws; embedded IPTC in exported JPEGs |
| 12 | Metadata writer | Resident `exiftool -stay_open` process (only full IIM+XMP writer) |
| 13 | Code replacements | Photo Mechanic-compatible replacement files (TSV, `=code=` syntax, hot codes) |
| 14 | IPTC fields | Full PM stationery-pad set (phased, see M3) |
| 15 | Presets | Lightroom `.xmp` (crs: namespace) only; user presets saved in the same format |
| 16 | Settings transfer | Copy/Paste with field checkboxes, Apply Previous, Sync to selection, Create preset |
| 17 | Crop | Free crop + aspect presets + rotate/straighten |
| 18 | Ingest | Manual trigger only; multi-card parallel; one verified backup destination |
| 19 | Ingest naming | User-defined folder/filename rules with variables (PM-style subset) |
| 20 | RAW+JPEG | Paired by base filename, treated as one item; caption/edit the JPEG, stamp both |
| 21 | Transfer | FTP (suppaftp, REST resume) and SFTP (russh-sftp) in v1; FTPS later |
| 22 | Send model | Multiple saved send recipes, one active; explicit send key then auto-advance |
| 23 | State | SQLite catalog (ratings, tags, upload status, ingest history, queue, selections, retouch recipes, detections) |
| 24 | Secrets | OS keychain via `keyring` crate |

## Settled during implementation

| Date | PR | Decision | Choice |
|---|---|---|---|
| 2026-10 | A1 | GPUI pin | `gpui-pre` / `gpui-pre-platform` `=0.3.8` from crates.io, a snapshot of zed@`279fe07` (2026-10-04). Consumed from crates.io, not the zed git repo; this is the distribution path gpui-kit releases carry |
| 2026-10 | A1 | gpui-component pin | `gpui-kit` `=0.7.1` from crates.io (the longbridge/gpui-component repo was renamed to longbridge/gpui-kit; `gpui-kit` wires gpui + gpui-base + gpui-component + default assets) |
| 2026-10 | A1 | Workspace MSRV | Workspace `rust-version` floor is `1.95`; the toolchain actually used to build is stable 1.98.1, which gpui-pre 0.3.8 (zed@279fe07) requires. CI builds with 1.98.1. An MSRV check job may come later |
