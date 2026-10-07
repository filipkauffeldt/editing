# editing — Plan

A minimal, fast, keyboard-first photo workflow app: Photo Mechanic's ingest and
metadata muscle combined with Lightroom's quick-edit feel, built with Rust +
GPUI. One integrated loop: tag, quick-edit, retouch, send, advance — rendering,
face detection and FTP/SFTP upload happen in the background while you move to
the next photo.

Open source. Linux first, cross-platform (Windows, macOS) after.

## Non-goals

- No catalog migrations from Lightroom, no import of LR catalogs
- No layers, compositing, PSD; no healing/clone/HDR/pano in v1. Retouching is
  limited to region/face blur and mosaic
- No printing, maps, face recognition/identity, slideshows, cloud sync
- No AI segmentation models in v1 — faces via a dedicated detector only
- No darktable/RawTherapee fallback: LightCraft is the only render engine, and
  raws it cannot fully decode (CR3, compressed RAF/ORF, RW2 v4, …) stay
  preview-grade until upstream implements them — Canon users wait
- No tethering (v1)

## Decisions (grilled & settled)

| Decision | Choice |
|---|---|
| License | Open source |
| UI framework | GPUI (base) + gpui-component, with gpui-base as the behavior layer (pin git revisions, expect churn) |
| Render engine | LightCraft lower crates — `develop`, `pipeline`, `raw`, `codecs`, optional `preview` — linked in-process, git-pinned to a revision. No subprocess render engine |
| Edit model | LightCraft `DevelopSettings` for preview and export (single model; own tone math retired) |
| Raw handling | `lightcraft-raw` decode where supported; embedded camera preview otherwise; no fallback engine |
| Retouch | Own post-develop stage (mosaic default, Gaussian optional); pixel kernels from `photocraft-algo`, vendored if it churns |
| Selections | Own normalized shape model; wand/matting primitives from `photocraft-algo::{selection, matting}`; own GPUI canvas for lasso/brush |
| Face detection | Swappable `FaceDetector` trait; R0 bake-off libfacedetection-rust (BSD-3) vs tract+YuNet; SCRFD escalation only if crowd recall fails |
| Privacy default | Mosaic with strength presets; non-destructive recipes re-rendered at preview and export |
| LR interop | Best-effort `crs:` sidecars (ProcessVersion=11.0) so LR can read edits/crop; LR is never required in the flow |
| Metadata storage | XMP sidecar always for raws; embedded IPTC in exported JPEGs |
| Metadata writer | Resident `exiftool -stay_open` process (only full IIM+XMP writer) |
| Code replacements | Photo Mechanic-compatible replacement files (TSV, `=code=` syntax, hot codes) |
| IPTC fields | Full PM stationery-pad set (phased, see M3) |
| Presets | Lightroom `.xmp` (crs: namespace) only |
| Crop | Free crop + aspect presets + rotate/straighten |
| Ingest | Manual trigger only; multi-card parallel; one verified backup destination |
| Ingest naming | User-defined folder/filename rules with variables (PM-style subset) |
| RAW+JPEG | Paired by base filename, treated as one item; caption/edit the JPEG, stamp both |
| Transfer | FTP (suppaftp, REST resume) and SFTP (russh-sftp) in v1; FTPS later |
| Send model | Multiple saved send recipes, one active; explicit send key then auto-advance |
| State | SQLite catalog (ratings, tags, upload status, ingest history, queue, selections, retouch recipes, detections) |
| Secrets | OS keychain via `keyring` crate |

## Architecture

```
┌─────────────────────────── GPUI app shell ───────────────────────────┐
│  Contact sheet (uniform_list/grid)  │  Single view (canvas: crop,     │
│  Template editor, recipe editor     │  lasso/brush, face boxes)       │
│  Ingest dialog (per-card progress)  │  Send queue / status            │
└──────────────┬───────────────────────────────────────┬───────────────┘
               │                                       │
   ┌───────────▼──────────┐              ┌────────────▼─────────────┐
   │ Render service       │              │ Edit state               │
   │ lightcraft-pipeline  │◄─────────────│ DevelopSettings + crop   │
   │ proxy + full res     │              │ selections, recipes,     │
   │ + retouch stage      │              │ detections, crs:XMP      │
   └───────────┬──────────┘              └────────────┬─────────────┘
               │                                       │
   ┌───────────▼──────────┐   ┌───────────────────────▼──────────────┐
   │ Selection service    │   │ Face detect service                  │
   │ shapes → mask planes │   │ ROI crop → tiles → detector → NMS →  │
   │ wand + matting       │   │ dedupe/expand → landmarks            │
   └───────────┬──────────┘   └───────────────────────┬──────────────┘
               │                                      │
   ┌───────────▼──────────┐   ┌────────────▼──────┐   │
   │ Ingest service       │   │ Metadata service  │   │
   │ per-device workers   │   │ exiftool -stay_   │   │
   │ temp→BLAKE3→rename   │   │ open (IIM+XMP)    │   │
   │ + backup destination │   │ code replacements │   │
   └───────────┬──────────┘   └────────────┬──────┘   │
               │                           │          │
   ┌───────────▼───────────────────────────▼──────────▼───────────────┐
   │ SQLite catalog + durable job queue (rusqlite, worker threads)    │
   │ edits, selections, retouch recipes, detections, export jobs      │
   └───────────────────────────────┬──────────────────────────────────┘
                                   │
                      ┌────────────▼────────────┐
                      │ Transfer queue          │
                      │ suppaftp + russh-sftp   │
                      │ resume, retry, status   │
                      └─────────────────────────┘
```

### UI component mapping

Styled widgets come from `gpui-component`; `gpui-base` provides the unstyled
behavior layer (focus, overlay positioning, virtualization, a11y) so bespoke
surfaces can use the same behaviors without fighting a styled component.

| App surface | Component |
|---|---|
| Window shell | `Root`, `TitleBar`, `Sidebar`, `StatusBar` |
| Contact sheet | Custom grid: `uniform_list` (gpui) of chunked rows over gpui-base virtualization; `DataTable` if a table view is ever needed |
| Single view / crop / retouch | `canvas()` / custom `Element` + `paint_image`/`paint_path` (crop rect, thirds, handles; lasso/brush/face-box overlays); `Slider`s for quick edits |
| IPTC template editor | `Form` (multi-column, validated) + `Input`/`Textarea`/`Select`/`Combobox` |
| Ingest dialog | `Dialog` + per-card `Progress` + `Table` for file lists |
| Send recipes | `Form` + named-recipe `List` |
| Upload status | `Badge`/`Tag` on thumbnails, `Notification` for failures |
| Menus / shortcuts | `Menu` (context menus), gpui actions + JSON keymaps (Alt+arrows, Alt+S), `Kbd` for cheat-sheet |

### Key mechanics

- **Previews without raw decode.** `lightcraft-raw` pulls the camera-embedded
  JPEG (CR3/NEF/ARW/CR2/RAF/RW2/DNG) in milliseconds. Escape hatch:
  `exiftool -b -JpgFromRaw`. Full raw decode is never on the interactive path;
  the render service decodes at full resolution in the background.
- **One edit model.** Sliders set `DevelopSettings` values rendered through
  `lightcraft-pipeline`: a downscaled proxy while dragging, the full proxy on
  release, full resolution at export — the same code path at every size, so
  preview and export skip the drift problem entirely. Own tone math is retired.
- **One edit state, two artifacts.** Internal state is `DevelopSettings` + crop
  + retouch recipes in the catalog; a best-effort `crs:` XMP sidecar is written
  via exiftool for LR readability (Exposure2012, *2012 sliders,
  ToneCurvePV2012, Crop*, HasCrop, ProcessVersion=11.0).
- **Retouch is a post-develop stage.** Selections are normalized shapes
  (rect/ellipse/polygon/brush); the rasterized mask is cached in the catalog.
  Mosaic (default) or Gaussian is blended through the mask and its feather at
  render resolution, identically in preview and export. Recipes are
  non-destructive and re-render on change.
- **Face detection is ROI-first.** Detection runs on the largest embedded
  preview: crop the drawn region, tile it when faces are small, run the
  detector, NMS, expand boxes, derive landmark ellipses. Every face is
  toggleable; manual boxes are always available.
- **Ingest is RPD-shaped.** One worker pipeline per device; files copied to temp
  random names, BLAKE3-verified, then atomically renamed into place; backup
  copies fan out from the already-downloaded temp file (never re-read the card);
  duplicate detection by (original filename, EXIF time, size). RAW+JPEG pairing
  by base filename. IPTC template (ingest-scoped "local" snapshot) stamped in
  the same pass.
- **Everything queued is durable.** Export jobs, metadata writes, uploads, face
  detection, and retouch re-renders are rows in SQLite worked by background
  threads; the app can crash mid-send and resume. Upload status is per-photo
  (queued/uploading/done/failed) and visible as colored badges in the contact
  sheet.

## The core loop (acceptance scenario)

1. Ingest dialog: two cards mounted → both ingest in parallel with per-card
   progress; photos land in `{yyyy}/{mm}-{dd}/{session}` folders + backup disk;
   ingest IPTC snapshot applied.
2. Contact sheet opens while ingest still runs (photos appear as they land).
3. Alt+→ to first photo. Type caption using code replacements (`\h1\` →
   "SHL", `{f:2}` … PM syntax), add keywords; set WB, +0.7 EV, crop 3:2, rotate
   1.2°; maybe apply an LR .xmp preset. Sliders drive DevelopSettings through
   the proxy pipeline.
4. Alt+S (send): current recipe export (render + retouch, resize/quality/sRGB/
   embedded IPTC/renamed file) enqueued → render service runs → FTP or SFTP
   upload starts in background → cursor auto-advances to next photo.
5. Green badge appears on the sent photo; a failed upload turns red and is
   retried with resume.

Target: step 3+4 under a few seconds per photo, keyboard only.

Retouch acceptance: draw a lasso over a fan section → mosaic; or detect faces
inside the region, toggle false positives, blur the rest; save the recipe and
apply it to a 40-photo burst — all background work, per-photo status visible in
the queue.

## Milestones

Each milestone is a vertical slice that runs end-to-end on Linux.

- **M0 — Scaffold.** Cargo workspace, GPUI shell window with gpui-component,
  repo CI (fmt/clippy/test), decision log in-repo. R0 spikes can start here.
- **M1 — Browse.** Open folder → SQLite catalog built → contact sheet grid with
  RAW+JPEG pairing; previews via lightcraft-raw; full-screen single view;
  keyboard nav.
- **M2 — Quick edit.** Exposure/WB/highlights/shadows/whites/blacks as
  `DevelopSettings` on a live proxy via lightcraft-pipeline; LR .xmp preset
  import + apply; free crop + aspect presets + rotate; edit state persisted
  (catalog + crs sidecar best-effort).
- **M3 — Metadata.** IPTC template editor (phase 1: caption, keywords, headline,
  byline/title, credit, source, copyright, instructions, edit status, rights;
  phase 2: full PM set incl. location, contact, categories, custom fields);
  variables inside fields; PM-compatible code replacements; batch apply;
  exiftool resident writer (XMP sidecar; IIM optional toggle).
- **M4 — Ingest.** Manual ingest dialog, multi-card parallel, per-card progress,
  folder/filename rules with variables, verified backup destination, duplicate
  detection, IPTC snapshot on ingest, contact sheet live-updates mid-ingest.
- **M5 — Send.** Render service (lightcraft-pipeline + retouch stage + JPEG
  encode, resize/quality/sRGB, IPTC embed, filename recipe); send recipes
  (multiple saved, one active); FTP and SFTP transfer queue with
  resume/retry/status badges; Alt+S send + auto-advance. Core loop demo done.
- **M6 — Retouch & face blur.** Selection tools on the canvas, mosaic/blur ops
  with privacy presets, detect-in-region with manual toggle, batch recipes and
  sticky regions (R0–R4 below).
- **M7 — Cross-platform + packaging.** Windows/macOS bring-up, cargo-packager
  installers, keychain secrets, crash-safe queue verified; no external engine
  to bundle.

### Retouch track (R0–R4)

R0 runs immediately in scratch binaries; R1 needs M2's canvas; R2 needs M5's
render path.

- **R0 — Spike (1–2 days).** Build LightCraft + photocraft-algo from pinned
  revs; render one NEF in-process; detector bake-off (libfacedetection-rust vs
  tract+YuNet: recall by face-size bucket, CPU time, binary size); convert a
  render into photocraft-algo surfaces and run Mosaic + Gaussian; verify
  tiling/preview-export consistency. Exit: a memo with measured numbers.
- **R1 — Selection tools.** `edit-retouch` crate (shapes → mask planes, feather,
  normalized coords), canvas interactions (rect/ellipse, lasso with RDP
  simplify, quick-select brush via wand_region + combine_region), catalog
  storage + mask cache, unit tests.
- **R2 — Retouch ops.** Post-develop stage in the render path (preview and
  export); Mosaic and Gaussian ops; strength UI + privacy presets; golden tests
  proving preview/export equality.
- **R3 — Face blur.** `FaceDetector` trait + chosen implementation behind a
  background queue; detect-in-region UI, boxes overlay with toggle and manual
  add; detection caching in the catalog.
- **R4 — Batch & recipes.** Save/apply retouch recipes across a selection,
  sticky regions across a burst, per-photo job status + retry, crash-resume
  test, performance pass.

## Stack summary

| Concern | Choice |
|---|---|
| UI | gpui + gpui-base + gpui-component (git-pinned to a matching zed revision) |
| Render engine | lightcraft-develop + lightcraft-pipeline + lightcraft-raw + lightcraft-codecs (git-pinned, in-process CPU reference) |
| Retouch & selections | photocraft-algo (Mosaic, Gaussian, wand/matting) + own `edit-retouch` shape model |
| Face detection | libfacedetection-rust (BSD-3) or tract + YuNet — picked in R0; SCRFD/ort escalation path |
| Decode/encode | lightcraft-codecs; image + zune-jpeg for app-side paths |
| Pixel ops | rayon, fast_image_resize |
| Color | lightcraft-pipeline; moxcms only if app-side ICC is needed |
| Metadata I/O | exiftool `-stay_open`; read: kamadak-exif + quick-xml |
| Export | in-process render service (no subprocess) |
| Transfer | suppaftp (FTP, REST resume) + russh-sftp (SFTP) |
| Catalog/queue | rusqlite (bundled) + worker threads + flume |
| Volumes | sysinfo polling + udev / GetVolumeInformationW / diskarbitration |
| Secrets | keyring |
| Packaging | cargo-packager / cargo-dist (no bundled external engine) |

## Risks & mitigations

- **GPUI pre-1.0 churn, thin docs** — pin revisions; the app is precisely the
  use case Zed's `image_viewer` and gpui-component's tables already cover.
- **LightCraft pre-1.0, crates unpublished, API churn** — pin git revisions;
  contain behind our traits (`PreviewProvider`, `DevelopEngine`, `RetouchStage`,
  `FaceDetector`); all upstream code is MIT/Apache, so vendoring a crate we
  depend on is always an option.
- **LightCraft raw coverage and color** — CR3, compressed RAF/ORF, RW2 v4 and
  lossy-after-split NEF decode as embedded previews only; non-DNG raws develop
  with a neutral color matrix (muted color). Accepted for v1: no fallback
  engine; document per-camera expectations; Canon users wait for upstream.
- **A second tone implementation would reintroduce drift** — `DevelopSettings`
  is the only edit model; do not add another one.
- **Face detector misses small crowd faces** — ROI-first cropping + tiling;
  benchmark in R0; manual add/toggle always available; SCRFD/ort escalation.
- **Gaussian blur is theoretically invertible** — mosaic is the default for
  faces; strength presets tuned from the privacy test.
- **Two ArtCraft dependency trees** — only photocraft-algo plus one bridge
  module; vendor the kernels we use if it churns.
- **MSRV** — LightCraft needs edition 2024 / Rust 1.90, photocraft-algo 1.95,
  libfacedetection_rs 1.85; workspace MSRV is 1.95.
- **FTP credentials on the wire** — FTPS is not in v1. Prefer SFTP for
  sensitive work. Keychain storage protects credentials at rest only.
- **SFTP host key trust** — pin the host key on first use, warn on change, and
  never accept a changed key silently.

## Open questions

- Exact variable set for folder/filename rules (PM subset: dates, sequence,
  IPTC fields, session name — finalize in M4 design).
- IIM (legacy IPTC) dual-write: default on for agency compatibility, or off?
- Does the send recipe need per-recipe filename templates (PM-style) from day
  one, or is one global filename rule enough?
- Rotate: full 90° steps + fine straighten, or fine straighten only?
- Catalog scope: single "current working folder" session or a persistent
  all-photos library? (Leaning: sessions + ingest history, not a full library.)
- Face detector pick: R0 bake-off decides; bundle the model in-process (no
  download-on-first-use) — libfacedetection_rs embeds its model data.
- Quick-select determinism: freeze the rasterized mask at draw time, or store
  brush dabs + tolerance and re-run the wand at export with a pinned algorithm
  version? (Leaning: freeze the mask for export; keep dabs for editing.)
- Face mask shape: landmark-derived ellipses vs expanded boxes (landmarks if
  the picked detector exposes them).
- `crs:` write timing: keep best-effort writes on every edit, or only on an
  explicit save-metadata action? (Leaning: keep best-effort.)
