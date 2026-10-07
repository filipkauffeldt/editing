# Implementation plan and build target

Status: draft for review. This document turns `PLAN.md` into a build target and a
merge order. It has four parts:

1. The architecture skeleton: crates, traits, data model, job model.
2. The frontend design: screens, layout, state, keyboard map.
3. The PR sequence: 19 PR-sized steps with branches, bases, scopes, and demos.
4. The agent workflow: how to run the steps with multiple agents in T3 Code.

`PLAN.md` stays the source of truth for scope and decisions. When a decision
changes, change `PLAN.md` first, then this file.

## 1. Architecture

### 1.1 Crate layout

```
crates/
  app             GPUI binary: windows, views, keymaps, service wiring
  edit-core       Shared types and traits. No I/O, no upstream UI deps
  edit-catalog    SQLite: schema, migrations, typed stores, durable job queue
  edit-render     LightCraft bridge: decode, develop, proxy, export encode
  edit-retouch    Shape model, mask rasterization, mosaic/gaussian, face boxes
  edit-metadata   exiftool resident writer, XMP/EXIF read, templates, codes
  edit-ingest     Volume detection, copy pipeline, naming rules, backup
  edit-transfer   FTP queue with resume and retry
```

Rules:

- `edit-core` depends on nothing in this workspace.
- Every service depends on `edit-core` and `edit-catalog`. Services do not
  depend on each other. Cross-service flow goes through the job queue and
  `edit-core` types.
- `app` depends on all services. It contains no business logic. It wires
  services into GPUI entities.
- Upstream crates are pinned in the root manifest. Each service wraps its
  upstream crate in one module. A revision bump is one small PR, never a rider
  on a feature PR.

### 1.2 Threading and jobs

The GPUI main thread owns views and entities. Long work never runs there.

- Cancellable work runs on `BackgroundExecutor` tasks. The view keeps the
  `Task` handle so it drops cleanly.
- Ingest copy, FTP upload, and the exiftool resident process use dedicated
  `std::thread` workers.
- Every durable task is a row in the `jobs` table. N worker threads claim jobs
  with one SQL transaction. On launch, `running` rows return to `queued`. A
  crash mid-send resumes.
- Workers send `(job_id, progress)` events over a `flume` channel. One
  `cx.spawn` loop drains the channel and updates entities. Background threads
  never touch entities directly.
- Batch CPU work inside one job uses rayon. `BackgroundExecutor::timer` gives
  backoff for retries.

Job kinds: `catalog_scan`, `thumb`, `preview`, `develop_full`, `retouch_render`,
`export`, `face_detect`, `metadata_write`, `ingest_copy`, `ingest_verify`,
`backup_copy`, `upload`.

### 1.3 Data model

Draft schema. `edit-catalog` owns it. Migration files are numbered in order.
One PR adds one migration number.

```sql
CREATE TABLE roots (
  id INTEGER PRIMARY KEY,
  path TEXT NOT NULL UNIQUE,
  kind TEXT NOT NULL,              -- folder | card
  added_at INTEGER NOT NULL
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

CREATE TABLE items (                 -- RAW+JPEG pairs and lone files
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

CREATE TABLE iptc_templates (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL,
  fields_json TEXT NOT NULL,
  scope TEXT NOT NULL,             -- global | ingest
  updated_at INTEGER NOT NULL
);

CREATE TABLE send_recipes (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL,
  config_json TEXT NOT NULL,
  is_active INTEGER NOT NULL DEFAULT 0,
  updated_at INTEGER NOT NULL
);

CREATE TABLE code_replacements (
  id INTEGER PRIMARY KEY,
  group_name TEXT NOT NULL,
  code TEXT NOT NULL,
  value TEXT NOT NULL,
  UNIQUE(group_name, code)
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

CREATE TABLE settings (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
```

`items` is the unit that edits, selections, retouch recipes, detections, and
sends attach to. A paired RAW+JPEG item stamps both files. An unpaired file
makes an item with one side null.

### 1.4 Contracts in `edit-core`

The upstream types stay behind these traits. A revision bump touches one
bridge module, not the app.

```rust
pub trait PreviewProvider {
    fn embedded_preview(&self, path: &Path) -> Result<PreviewFrame>;
}

pub trait DevelopEngine {
    fn render(&self, req: RenderRequest, cancel: &CancelToken) -> Result<Frame>;
}

pub trait RetouchStage {
    fn apply(&self, frame: &mut Frame, recipe: &RetouchRecipe, mask: &MaskPlane)
        -> Result<()>;
}

pub trait FaceDetector {
    fn detect(&self, frame: &Frame, region: Rect) -> Result<Vec<FaceBox>>;
}
```

Key shared types:

```rust
pub enum RenderSize { Proxy(u16), Full }
pub enum OutputColor { DisplaySrgb, ExportSrgb }

pub struct RenderRequest {
    pub item: ItemId,
    pub size: RenderSize,
    pub develop: DevelopSettings,   // LightCraft type, serialized in the catalog
    pub crop: Option<Crop>,
    pub retouch: Option<RetouchRecipe>,
    pub color: OutputColor,
}

pub struct Frame { width: u32, height: u32, stride: u32, pixels: Vec<u8> } // BGRA
pub struct Rect { x: f32, y: f32, w: f32, h: f32 }                        // normalized
pub struct FaceBox { region: Rect, confidence: f32, landmarks: Option<[Point; 5]> }
```

`edit-core` re-exports `DevelopSettings` from the pinned LightCraft revision.
That is the single edit model. Do not add a second tone model.

### 1.5 Render path

```
source file
  │
  ├─ raw:  lightcraft-raw decode, or embedded preview when decode is unsupported
  └─ jpeg: image / zune-jpeg decode
  │
  ▼
lightcraft-pipeline develop at the requested size
  │
  ▼
retouch stage (post-develop): rasterized mask, mosaic or gaussian, feathered
  │
  ▼
crop
  │
  ├─ display: BGRA Frame into paint_image
  └─ export: JPEG encode with quality, resize, sRGB, IPTC embed
```

One code path serves every size. Sliders render a downscaled proxy while
dragging, the full proxy on release, and full resolution at export. The plan
calls this the fix for preview and export drift.

### 1.6 Ingest path

```
volume appears
  │
  ▼
enumerate DCIM, pair RAW+JPEG by base name
  │
  ▼
copy to temp random name, BLAKE3 verify
  │
  ▼
atomic rename into the folder/filename rule
  │
  ├─ backup copy fans out from the temp file
  ├─ catalog insert, duplicate check on (original name, EXIF time, size)
  ├─ IPTC ingest snapshot job
  └─ thumbnail jobs
```

The contact sheet refreshes per file through job progress events. Ingest never
re-reads the card for the backup.

## 2. Frontend design

### 2.1 Window shell

One window. Four regions.

```
┌ TitleBar: editing          [Open Folder] [Ingest] [Send] ──────────────┐
├ Sidebar ──────┬ Contact sheet / Single view ─────────────┬ Panel ──────┤
│ Sessions      │  ┌──────┐ ┌──────┐ ┌──────┐ ┌──────┐    │ Develop    │
│  2026-10-07   │  │ ▣ ★  │ │ ▣    │ │ ▣ ✓  │ │ ▣ !  │    │ Crop       │
│  2026-10-06   │  └──────┘ └──────┘ └──────┘ └──────┘    │ Retouch    │
│ Filters       │  ┌──────┐ ┌──────┐ ┌──────┐ ┌──────┐    │ Metadata   │
│  ★ >= 3       │  │ ▣    │ │ ▣    │ │ ▣    │ │ ▣    │    │            │
│  Unuploaded   │  └──────┘ └──────┘ └──────┘ └──────┘    │            │
├───────────────┴─────────────────────────────────────────┴────────────┤
│ StatusBar: 1 selected · 12 queued · 3 uploaded · FTP idle · 42 items  │
└────────────────────────────────────────────────────────────────────────┘
```

Component mapping, from `PLAN.md`:

| Surface | Component |
|---|---|
| Window shell | `Root`, `TitleBar`, `Sidebar`, `StatusBar` |
| Contact sheet | custom grid over `uniform_list` with gpui-base virtualization |
| Single view, crop, retouch | `canvas()` + `paint_image` / `paint_path` overlays |
| Template editor, recipes | `Form`, `Input`, `Textarea`, `Select`, `Combobox` |
| Ingest dialog | `Dialog`, per-card `Progress`, file `Table` |
| Menus and keys | `Menu`, gpui actions, JSON keymaps, `Kbd` cheat sheet |

### 2.2 Contact sheet

- Virtualized rows of cells. Cell shows thumbnail, rating stars, upload badge,
  ingest badge, filename on hover.
- Selection with arrows, Shift+arrows for ranges, Ctrl+A for all, mouse click
  and drag.
- Filter bar: rating, color label, upload state, file kind.
- Sort: capture time, filename, ingest order.
- Live updates while ingest runs. A new file appears with an amber badge that
  turns green after thumbnails and metadata land.

### 2.3 Single view

- Fit, 100%, and zoom states. Scroll to zoom, drag to pan, `0` resets to fit.
- Left and Right walk the item list. The view preloads the proxy for neighbors.
- The right panel shows Develop, Crop, Retouch, and Metadata tabs.
- All slider changes write to the edit buffer, then debounce to the catalog.

### 2.4 Crop mode

- The canvas draws the full proxy, dimmed outside the crop rect.
- Overlays: crop border, thirds grid, eight handles, aspect lock list.
- Straighten slider rotates the image under a fixed crop frame.
- Aspect presets: free, original, 1:1, 3:2, 2:3, 4:3, 16:9.

### 2.5 Retouch mode

- Tool palette: rectangle, ellipse, lasso, quick-select brush.
- Selection list on the right with visible and delete controls.
- Operation settings: mosaic cell size, gaussian radius, feather.
- Privacy presets: low, medium, high, tuned in R0 tests.
- Face boxes render as overlay outlines. Each box has a toggle. A manual add
  button draws a new box. Detect-in-region runs on the current selection or on
  a drawn rectangle.
- Applying a recipe to a multi-selection runs batch jobs with per-photo status.

### 2.6 Metadata

- Template editor: multi-column form, one row per IPTC field.
- Phase 1 fields: caption, keywords, headline, byline and title, credit,
  source, copyright, instructions, edit status, rights.
- Phase 2 fields: location, contact, categories, custom fields.
- Variables inside fields, resolved at apply time: date, sequence, session,
  camera, IPTC fields.
- Code replacement preview under each field shows the resolved text before
  apply.

### 2.7 Ingest dialog

- Card list with label, capacity, free space, and per-card progress bar.
- Destination folder and filename rules with variable pickers.
- Backup destination, one, optional.
- IPTC template selector, defaulting to the ingest snapshot.
- A live file table during ingest with per-file state: copying, verifying,
  renaming, backup, cataloged, failed.

### 2.8 Send panel and queue

- Recipe list, one active recipe marked.
- Recipe form: file format, quality, resize long edge, output color, filename
  rule, FTP profile, remote directory.
- `Alt+S` sends the current item with the active recipe, then advances.
- Status bar shows the upload queue. Per-photo badges: amber queued, blue
  uploading, green done, red failed. A failed item offers retry with resume.

### 2.9 State model

- `AppState` root entity: roots, current root, item list, selection set,
  current item, job registry, transfer registry.
- One view entity per surface: `ContactSheet`, `SingleView`, `IngestDialog`,
  `TemplateEditor`, `RetouchPanel`, `SendPanel`, `StatusBar`.
- One `EditSession` for the current item: `DevelopSettings`, `Crop`, retouch
  recipes, dirty flag. Commits to the catalog on debounce and on navigation.
- Job progress events update views through the flume drain loop.

### 2.10 Keyboard map, proposal

| Keys | Action |
|---|---|
| `Alt+Left` / `Alt+Right` | previous / next item |
| `Alt+S` | send current item, then advance |
| `G` | toggle contact sheet and single view |
| `I` | open ingest dialog |
| `T` | open template editor |
| `C` | crop mode |
| `R` | retouch mode |
| `0` to `5` | rating |
| `P` / `X` | pick / reject |
| `[` / `]` | rotate 90 degrees |
| `Z` | fit to window |
| `Ctrl+Z` / `Ctrl+Shift+Z` | undo / redo |
| `Ctrl+A` | select all |

The map lives in a JSON keymap file so it is editable without a rebuild.

## 3. PR sequence

### 3.1 Rules

- One PR is one vertical slice. It builds and passes CI on its own base.
- A dependent PR branches from its parent branch while the parent is open.
  After the parent merges, later dependent PRs branch from main.
- Open the PR against the base branch, not against main, while the base is open.
  Merge bottom-up. GitHub retargets child PRs to main after the parent merges.
- One migration number per PR, allocated at branch time. Schema PRs never run
  in parallel with the same number.
- `edit-core` changes go in their own small PR when they touch a merged trait.
- Pins stay in A1. A bump is its own PR.

### 3.2 Dependency graph

```
P0 ← main
A1 ← main
A2 ← A1
A3 ← A1
B1 ← A2      B2 ← A2      B3 ← A2      E2 ← A2
C1 ← B1      C2 ← B3      C3 ← B2      C4 ← C3
D1 ← C1      D2 ← C1      D3 ← C2 + C3
D4 ← D1      E1 ← D1      E3 ← E1
F1 ← E1 + E2 F2 ← E3      F3 ← E2
G1 ← F1 + F2 G2 ← F3
```

E2 depends on A2, F1 depends on E1 and E2, D3 depends on C2 and C3, G1 depends
on F1 and F2.

### 3.3 Stage A, contracts

| PR | Branch | Base | Scope | Demo after merge |
|---|---|---|---|---|
| P0 | `docs/implementation-plan` | main | This document. | The plan is in the repo. |
| A1 | `feature/m0-workspace-shell` | main | Workspace, pins, app shell window, CI fmt/clippy/test, MSRV 1.95, decision log. | `cargo run` opens an empty window. CI is green. |
| A2 | `feature/core-catalog-queue` | A1 | `edit-core` types and traits, `edit-catalog` schema, migrations, job queue, workers, flume events, crash recovery. | A scratch job run survives a kill and resumes. |
| A3 | `spike/retouch-r0` | A1 | R0/T1 to T4 in scratch binaries. Render one NEF in-process, detector bake-off, mosaic and gaussian on a render. | A memo with measured numbers and pinned revisions. |

A1 and A2 are the critical path. A3 runs next to them because it touches no app
crate.

### 3.4 Stage B, foundations

| PR | Branch | Base | Scope | Demo after merge |
|---|---|---|---|---|
| B1 | `feature/m1-browse` | A2 | Folder open, catalog scan, embedded previews, contact sheet, RAW+JPEG pairing, single view, keyboard nav. | Open a folder, browse a grid, walk items with keys. |
| B2 | `feature/metadata-io` | A2 | exiftool `-stay_open` writer, EXIF/XMP read, minimal `crs:` sidecar write. | A sidecar appears for one photo. LR reads it. |
| B3 | `feature/ingest-volumes` | A2 | Volume detection with sysinfo and udev, ingest dialog shell. | Plug a card, the dialog lists it. |

### 3.5 Stage C

| PR | Branch | Base | Scope | Demo after merge |
|---|---|---|---|---|
| C1 | `feature/m2-render-sliders` | B1 | Render service on `lightcraft-pipeline`, proxy while dragging, full proxy on release, six sliders. | Drag exposure, see the proxy update live. |
| C2 | `feature/ingest-copy` | B3 | Temp copy, BLAKE3 verify, atomic rename, naming rules, one backup destination, duplicate detection. | Ingest one card with verified copies. |
| C3 | `feature/m3-templates` | B2 | IPTC template editor phase 1, variables, batch apply. | Write a template, apply to a selection. |
| C4 | `feature/m3-code-replacements` | C3 | PM-compatible replacement files, `=code=` syntax, hot codes. | `\h1\` expands in a caption. |

### 3.6 Stage D

| PR | Branch | Base | Scope | Demo after merge |
|---|---|---|---|---|
| D1 | `feature/m2-crop-rotate` | C1 | Free crop, aspect presets, rotate and straighten on the canvas. | Crop and straighten an item, state persists. |
| D2 | `feature/m2-presets-persistence` | C1 | LR `.xmp` import and apply, edit persistence through B2. | Apply a preset, reopen the app, edits remain. |
| D3 | `feature/m4-ingest-complete` | main after C2, C3 | Ingest IPTC snapshot, live contact sheet during ingest. | Ingest a card, photos appear as they land. |
| D4 | `feature/r1-selection-tools` | D1 | `edit-retouch` crate, rect/ellipse, lasso with RDP, quick-select brush, mask cache. | Draw a lasso, see the mask overlay. |

### 3.7 Stage E

| PR | Branch | Base | Scope | Demo after merge |
|---|---|---|---|---|
| E1 | `feature/m5-render-export` | D1 | Export path: full-res render, retouch hook, JPEG encode, resize, quality, sRGB, IPTC embed, filename recipe. | Export one edited item to JPEG. |
| E2 | `feature/m5-ftp-queue` | main after A2 | suppaftp worker, keyring secrets, resume and retry, status badges. | Upload to a test FTP server, kill the app, resume. |
| E3 | `feature/r2-retouch-ops` | E1 | Retouch stage in the render path, mosaic and gaussian, strength presets, golden tests for preview and export parity. | Mosaic a region, export, pixels match the preview. |

### 3.8 Stage F

| PR | Branch | Base | Scope | Demo after merge |
|---|---|---|---|---|
| F1 | `feature/m5-send-recipes` | main after E1, E2 | Multiple saved recipes, one active, `Alt+S`, auto-advance. | The core loop runs keyboard only. |
| F2 | `feature/r3-face-blur` | E3 | `FaceDetector` behind the queue, detect-in-region UI, box toggle and manual add, detection cache. | Detect faces in a fan region, blur them. |
| F3 | `feature/m7-secrets-config` | E2 | Keychain integration, app config, crash-safe queue verification. | Credentials live in the OS keychain. |

### 3.9 Stage G

| PR | Branch | Base | Scope | Demo after merge |
|---|---|---|---|---|
| G1 | `feature/r4-batch-recipes` | main after F1, F2 | Retouch recipes across a selection, sticky regions, per-photo status, crash-resume test, performance pass. | Blur the same region across a 40-photo burst. |
| G2 | `feature/m7-cross-platform` | F3 | Windows and macOS bring-up, cargo-packager and cargo-dist installers, Linux dependency metadata. | Installers build in CI for three platforms. |

## 4. Agent workflow in T3 Code

### 4.1 Thread per PR

Create one top-level thread per PR with `t3_thread_launch` and an explicit
worktree:

```json
{
  "title": "B1 browse",
  "workspaceStrategy": {
    "type": "worktree",
    "baseRef": "feature/core-catalog-queue",
    "branch": "feature/m1-browse",
    "startFromOrigin": false
  },
  "message": "PR brief: <scope, base branch, demo, plan sections, open questions>"
}
```

`startFromOrigin: false` is required while base branches exist only on the
local machine. The launch message must include the branch, base, scope, the
demo that proves the PR, the `PLAN.md` and `docs/research/` sections that
apply, and the instruction to open a PR against the base branch and link it.

### 4.2 Review round

Every PR gets one review round. Call `delegate_task` with role `review` and a
different model than the author. Give the reviewer the PR id, branch, base,
scope, and the diff range. Use the `code-review` skill. The author fixes the
findings, then the PR merges. For a second round, call `delegate_task` again
with the original brief, prior findings, author responses, and open objections.

### 4.3 Concurrency and build cost

- Three live worktrees at most. The graph above fits that.
- GPUI and LightCraft builds are heavy. Enable sccache and keep per-worktree
  target directories. Run at most three Rust builds at once.
- Reserve the critical path, A1 then A2, and do not start stage B before A2
  merges. Everything else follows the graph.

### 4.4 Model roles

Pick from `orchestrator_capabilities` at launch time. The list changes.

- Contracts and pixel work, A1, A2, C1, E1, E3, F2: strongest reasoning model.
- UI and feature work, B1 to F1: strong general model.
- Mechanical work, C4, F3, G2: fast model.
- Review: a different model family than the author, for example author on
  DeepSeek, review on Kimi or GPT.

### 4.5 Merge order

Merge bottom-up per stage. After each stage, run the milestone demo on main
with an integrator thread. Fix regressions before the next stage starts.

## 5. Milestone acceptance

- M1 after B1: open a folder, grid appears with previews, arrows walk items,
  pairing shows one cell per pair.
- M2 after D2: sliders, crop, rotate, and presets apply to the proxy, and the
  state survives a restart.
- M3 after D3: a template applied at ingest lands in the file and the contact
  sheet, and codes expand in captions.
- M4 after D3: two cards ingest in parallel with per-card progress, one backup
  destination, no card re-read, duplicates reported.
- M5 after F1: `Alt+→`, caption, `Alt+S`, badge turns green, cursor advances,
  all under a few seconds per photo.
- Retouch after G1: lasso a region, mosaic it, detect faces in another region,
  toggle boxes, apply the recipe to a burst, all with per-photo status.
- M7 after G2: installers for Linux, Windows, and macOS, secrets in the OS
  keychain, queue survives a kill.

## 6. Decisions to settle per PR

- A3: detector pick and mask shape. R0 memo decides.
- B1: catalog scope. Sessions with ingest history, not a full library, unless
  the demo says otherwise.
- B2: `crs:` write timing. Best-effort on edit by default.
- C2: variable set for folder and filename rules.
- D1: rotate. Fine straighten only, or 90 degree steps plus straighten.
- D4: quick-select determinism. Freeze the rasterized mask for export, keep
  dabs for editing, hash as cache key.
- E1: per-recipe filename templates. Start with one global rule plus the
  recipe-level override.
- E2: IIM dual-write default for agency compatibility.
