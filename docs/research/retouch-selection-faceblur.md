# Light retouching: selection tools + face blur

**Date:** 2026-10-07
**Status:** research + decision record; no code written. Upstream facts verified 2026-10-07 (see §14).
**Scope:** fold light retouching (region blur/mosaic), selection tools (lasso, quick select)
and face detection inside a drawn region into the `editing` plan, with library choices and an
implementation path.

Engine direction adopted after this review: **LightCraft replaces darktable-cli as the only
render engine**; `PLAN.md` carries the resulting decisions, milestones (M0–M7) and the
retouch track (R0–R4).

---

## 1. Context

The app is a minimal Photo Mechanic + Lightroom hybrid:

- GPUI + gpui-component UI, contact sheet + single view, keyboard-first loop
  (tag → quick-edit → retouch → send → advance).
- Render path (updated): previews and development via LightCraft crates; export rendered
  in-process to JPEG; IPTC via resident exiftool; FTP send; SQLite catalog/queue. Own
  quick-edit tone math and the darktable-cli subprocess are retired.
- `PLAN.md` non-goals now allow region-masked blur/mosaic, selection tools and face detection
  inside a region. Layers, compositing, healing/clone, HDR/pano, AI segmentation and face
  recognition/identity stay out.

New requirements, restated:

1. Draw a region (rect / ellipse / lasso / brush) and blur or mosaic everything inside it —
   e.g. a stand full of fans.
2. Inside a drawn region, **detect faces** and blur only those (with manual toggle of false
   positives).
3. General selection tools: **lasso** (freehand polygonal) and **quick select** (brush that
   grows to similar pixels).

---

## 2. Findings

### 2.1 LightCraft — `storytold/lightcraft`

- Open-source, clean-room Lightroom reimplementation, pure Rust, **MIT OR Apache-2.0**,
  workspace version 0.2.1, edition 2024, MSRV 1.90. ~1.8k stars at review time, very active.
- UI is **egui/eframe** (`crates/ui-egui`) — irrelevant; only non-UI crates are candidates.
- Workspace members (verified from the root `Cargo.toml`): `geom`, `sysmem`, `color`,
  `raster`, `tiff`, `raw`, `codecs`, `meta`, `develop`, `scenes`, `pipeline`, `gpu`, `catalog`,
  `preview`, `merge`, `engine`, `ui-egui`, `mcp`.
- Crates are **not published on crates.io** (verified: `lightcraft-develop` 404s) → git
  dependencies pinned to a revision.
- Relevant capability:
  - `lightcraft-develop`: `DevelopSettings` (pure serializable JSON, sliders by id,
    stable hash for cache keys), presets.
  - `lightcraft-pipeline`: scene-referred linear Rec.2020 → display sRGB render, CPU
    reference implementation, stage cache, same code path for proxy and full-res
    (preview == export). Masks exist (`pipeline/src/masks.rs`): Brush/Linear/Radial/
    ColorRange/LuminanceRange shapes, Add/Subtract/Intersect components, normalized
    oriented-image coordinates, evaluated to an alpha plane at output resolution.
    Dependencies verified: `geom`, `color`, `raster`, `develop`, `rayon`, `serde` — no wgpu.
  - `lightcraft-raw`: pure-Rust decoders (DNG variants, CR2, NEF/NRW incl. Huffman
    lossless/lossy, ARW, uncompressed RAF/ORF, packed RW2, PEF) + embedded preview for
    **all** containers incl. CR3. Full decode **not** supported for: CR3, compressed RAF/ORF,
    RW2 v4, NEF lossy-after-split, sRAW.
  - `lightcraft-engine`: every action is a command with a JSON id; UI/CLI/control/MCP are
    frontends. Includes `crs.rs` (LR `crs:` XMP → DevelopSettings) and XMP sidecar
    read/write.
  - `lightcraft-cli render <in> -o <out> [--set control=value] [--settings FILE.json] …` —
    a `darktable-cli` replacement, plus `run`, `mcp`, `snapshot`, `merge`.
  - `pipeline/spots.rs`: heal/clone/remove spots; `redeye.rs`, `dust.rs`.
  - **No pixelate/mosaic**; `LocalAdjustments` is tone/colour sliders only. No face detection
    (AI masks blocked upstream on a model-strategy decision).
  - Honest self-assessment (ROADMAP/README): raw coverage, colour fidelity and no
    camera-calibration DB for non-DNG raws; CR3 and compressed RAF/ORF open as embedded
    previews only.
- XMP specifics: reads `crs:` (best-effort approximation), writes its own settings JSON;
  merges into existing sidecars without dropping foreign namespaces. LR `crs:` writing for
  interop stays our job (exiftool).

### 2.2 PhotoCraft — `storytold/photocraft`

- Photoshop reimplementation, pure Rust, **MIT OR Apache-2.0**, early alpha (~11.8k stars at
  review time), workspace 0.3.0, edition 2024, MSRV 1.95, egui UI.
- The app is out of scope. The useful piece is `crates/algo` (`photocraft-algo`):
  - Dependencies verified: `photocraft-geom`, `photocraft-color`, `photocraft-raster`,
    `serde`, `rayon` (native only). No UI, no wgpu.
  - Filters are depth-agnostic (normalized `f32`, any channel count, 8/16/32-bit surfaces),
    tile/region aware, selection aware, rayon-parallel.
  - `FilterParams::Mosaic { cell_size }` present; Gaussian/box/motion/radial/surface blur;
    pixelate family (Color Halftone, Crystallize, Facet, Fragment, Mezzotint, Pointillize).
  - `apply(surface, params, area, bounds, selection)` mixes output with the original by the
    selection mask — no separate blending code needed.
  - `selection.rs`: `magic_wand`, `wand_region` → `Region { bbox, mask }`, `combine_region`
    (union/subtract/intersect), `color_range`, `hue_range`, `tone_range`, mask modify
    (expand/contract/border/smooth/feather), anti-aliased lasso primitives.
  - `matting.rs`: guided-filter edge refinement, smart radius, feather, contrast/shift edge,
    colour decontamination — Photoshop's "Select and Mask" in miniature.
  - Also available if ever needed: `retouch.rs` (dodge/burn/local blur/sharpen dab kernels),
    `content_aware.rs`, `inpaint.rs`, `erase.rs`, `liquify.rs`, `pyramid.rs`, `segment.rs`.
- No face detection anywhere in PhotoCraft.

### 2.3 Face detection (neither ArtCraft app has it)

Options, in recommendation order for this project (all behind a trait):

| Option | Runtime | License | Notes |
|---|---|---|---|
| `libfacedetection_rs` (official Rust port in `ShiqiYu/libfacedetection`, `rust/`) | pure Rust, static model data generated into source, NMS + runtime AVX2 dispatch, MSRV 1.85 | BSD-3-Clause (LICENSE verified) | First-party port, small, fast, no ONNX runtime. No C ABI in 0.1.0. README shows `libfacedetection_rs = "0.1"` but the crate did **not** resolve on crates.io on 2026-10-07 — pin git/path until confirmed. Best default to spike first. |
| `tract` + YuNet ONNX (OpenCV Zoo `face_detection_yunet_2023mar.onnx`) | pure Rust runtime | tract MIT/Apache; model Apache-2.0 (verify bundle terms) | Clean licensing, good accuracy on medium faces, slower than the port above. |
| SCRFD 2.5g/10g via `ort` (ONNX Runtime bindings) | C++ runtime (MIT) | crate MIT, model Apache-2.0 | Best small-face recall. Escalation only if crowd recall proves insufficient. |
| `rusty-yunet`, `rust-faces`, `rusty_scrfd` | mixed | mixed | Wrappers, some stale/frozen; use only if the above fail. |

Accuracy note: the libfacedetection model reports WIDER Face AP **0.887 easy / 0.871 medium /
0.768 hard**. The earlier assumption of a ~68 % YuNet-class hard figure was too low; SCRFD-class
still has the edge on very small faces, but the gap is smaller than it first appeared. YuNet
emits five landmarks; verify the Rust port's `Face` struct exposes them (its README shows
score/box only).

The sports/fan-section use case is a **small-face recall problem**: ROI-first cropping + tiling
matters more than model choice. Minimal detectable face is ~10×10 px at the detector input.

---

## 3. Decision record (recommended, now adopted in `PLAN.md`)

| Concern | Recommendation | Alternatives / notes |
|---|---|---|
| Develop/preview/export engine | **Link LightCraft lower crates** (`develop`, `pipeline`, `raw`, `codecs`, optional `preview`) behind our own traits | CLI subprocess keeps churn out of-process but blocks an in-process retouch stage; engine facade pulls catalog + wgpu. Adopted: in-process crates. |
| Raw fallback | **None.** `lightcraft-raw` decode where supported; embedded camera preview otherwise | darktable-cli fallback dropped by decision; CR3/compressed RAF/ORF stay preview-grade until upstream lands — Canon users wait |
| Retouch pixel ops | **`photocraft-algo`** (or vendor specific kernels) — Mosaic + Gaussian blur | `lightcraft-raster::blur` for Gaussian if avoiding the dep; mosaic is trivial to vendor (~40 lines) |
| Selection primitives | **`photocraft-algo::selection` + `matting`** for wand/region/refine; own GPUI canvas for lasso/brush interaction | Rolling our own re-implements feather/guided-filter quality |
| Selection/region model | **Own model** (normalized coords), stored in catalog; borrow LightCraft's `MaskShape` conventions for familiarity | Adopting LightCraft's mask model wholesale only makes sense if we adopt their engine |
| Face detection | **Trait `FaceDetector`**, spike `libfacedetection_rs` vs tract+YuNet on real crowd photos, pick one; SCRFD/ort escalation path | No upstream reuse exists; model choice stays swappable |
| Face blur op | Mosaic (default, more irreversible) or strong Gaussian, applied at render resolution, tied to face box size | Never destructive; recipe re-renders at export |
| Where retouch runs | **Post-develop, display-encoded, at render resolution**, shared by preview proxy and export | Scene-linear would change the visual result users expect from "blur that face" |
| Upstreaming | Optional later: contribute blur/pixelate as a LightCraft local adjustment | Only worth it if we adopt the LightCraft engine/CLI as the render path (we adopted the crates, not the CLI) |

---

## 4. Proposed architecture

Adopted by `PLAN.md`. Services around an in-process render service:

```
GPUI app shell (contact sheet, single view with canvas overlays, send queue)
        │
        ├── Selection service   shapes → mask planes (wand/matting)
        ├── Face detect service ROI crop → tiles → detector → NMS → landmarks
        ├── Render service      lightcraft-pipeline → retouch stage → JPEG encode
        ├── Edit state          DevelopSettings + crop + selections + recipes + detections
        ├── Metadata service    exiftool -stay_open (IIM + XMP, crs:)
        └── Ingest service      per-device workers, verified copies
                    │
        SQLite catalog + durable job queue (edits, detections, export jobs)
                    │
        Transfer queue (suppaftp, plain FTP)
```

Seams (traits, so every dependency stays swappable):

- `PreviewProvider`: raw → display image (`lightcraft-raw` embedded preview; exiftool escape hatch).
- `DevelopEngine`: `DevelopSettings` → rendered buffer (`lightcraft-pipeline`).
- `FaceDetector`: RGBA/f32 image + ROI → `Vec<Face { bbox, landmarks, score }>`.
- `RetouchStage`: rendered image + `RetouchRecipe` → rendered image (own, engine-agnostic).

---

## 5. Feature design

### 5.1 Region & selection model (own crate, e.g. `edit-retouch`)

Coordinate convention: normalized to the **uncropped, oriented** image, long-edge units for
sizes (same as LightCraft masks — familiarity, easy porting).

```rust
enum Shape {
    Rect { x: f64, y: f64, w: f64, h: f64 },
    Ellipse { cx: f64, cy: f64, rx: f64, ry: f64, angle_deg: f64 },
    Polygon { points: Vec<(f64, f64)> },          // lasso
    BrushStrokes { strokes: Vec<BrushStroke> },   // quick-select / mask brush
}
struct Selection {
    components: Vec<(MaskOp, Shape)>, // Add / Subtract / Intersect
    feather: f64,                     // long-edge units
    inverted: bool,
}
```

- Store the editable shapes as source of truth; cache the rasterized alpha plane (RLE or raw
  `f32`) keyed by `(selection_hash, reference_size)`.
- Selections are reusable: assign to a photo, or "sticky" across a burst/selection
  (the fan-section workflow).

### 5.2 Quick select

Build from `photocraft-algo` primitives:

1. User brushes over pixels. Each dab (spaced by `radius/4`) becomes a seed.
2. `wand_region(px, area, seed, tolerance, contiguous = true, anti_alias = true)` →
   `Region { bbox, mask }`.
3. Union with the running selection via `combine_region`, touching only the new region's bbox.
4. On brush release or "Refine": `matting::guided_filter_*` refine, `morph`
   (expand/contract), feather; optional decontaminate.
5. Modifier keys: Alt = subtract dab, Shift = add.

Determinism (corrected from the first draft): a frozen mask **hash** detects drift but cannot
reproduce pixels. Recommendation: **freeze the rasterized mask** (RLE is cheap even at 45 MP)
as the export truth, keep brush dabs + tolerance for editing, and use the hash only as a cache
key. Re-running the wand at export resolution is the alternative, but only with a pinned
algorithm version plus golden tests.

### 5.3 Lasso

- Freehand pointer path → simplify (RDP, tolerance ~1.5 px screen) → polygon in normalized
  coordinates.
- Rasterize with anti-aliased coverage; optional `feather`; combine with existing selection
  (add/subtract/intersect).
- Zero library dependency; falls out of the canvas already planned for crop.

### 5.4 Retouch ops

- **Mosaic**: cell-average over `cell_size` (aligned to region bbox origin so tiling is
  stable; `photocraft-algo` anchors cell layout so preview/export agree). Vendor the ~40-line
  kernel if pulling the crate feels heavy.
- **Gaussian blur**: `photocraft-algo` blur (or `lightcraft-raster::blur::gaussian`).
- Blend through the selection alpha with feather; apply ops in a fixed order (mosaic/blur
  after global develop, before encode). `photocraft-algo::apply` already does the blend.
- Privacy strength guidance (UI presets):
  - Mosaic `cell_size ≈ max(8 px, face_box_long_edge / 10)` at export resolution.
  - Gaussian `sigma ≈ face_box_long_edge / 6`, minimum 3σ kernel.
  - Expand face masks 10–25 % and use a landmark-derived ellipse so hairline/chin are
    covered; feather lightly.
  - Mosaic is the default for faces (Gaussian is theoretically invertible under strong
    priors).
- Later (not v1): dodge/burn brush, spot heal — `photocraft-algo::retouch` and LightCraft
  `spots.rs` are the two candidate sources.

### 5.5 Face detection in a region

Pipeline (all in a background job):

1. Resolve the source image for detection: the **largest embedded preview** rather than a
   small proxy (2–4 MP is fine for face boxes).
2. Crop the drawn region (normalized → pixel rect) with ~5 % margin.
3. Detector input handling:
   - Downscale the crop to the detector input (e.g. 640×640); if estimated face size is below
     the detector minimum (~10 px at input), **tile the crop** into overlapping tiles, or
     upscale.
   - ROI-first + tiling is the difference between "finds faces" and "finds nothing" on crowd
     shots.
4. NMS, then filter: min score, min size, optional max size, dedupe across tiles.
5. Expand boxes (×1.2–1.25), derive rotated ellipses from the 5 landmarks when available.
6. Store `Detection { bbox, landmarks, score, detector_id, detector_version, source_hash }` in
   the catalog; show boxes in the UI, all enabled by default, click to toggle, Alt-click to
   add a manual face box.
7. Re-detection is cached: same `(photo, detector_version, region, source_hash)` → reuse.

### 5.6 Batch workflow ("blur the fan section in these 40 photos")

- A `RetouchRecipe` is an ordered list of operations: `{ selection or region,
  face_filter (none | in_region), effect (mosaic | blur), strength }`.
- Recipes can be saved and applied to a multi-selection of photos; regions are normalized so
  they stay meaningful across the burst.
- Detection jobs enqueue per photo; results streams into the UI (progress per photo, like
  ingest). "Apply all detected faces" is one keypress; failures are collectible and
  retryable from the queue.
- Because everything lives in the catalog as data, changing strength re-renders on demand
  (previews) and at export (final).

---

## 6. Integration notes

### 6.1 Dependencies (starting point)

```toml
[dependencies]
lightcraft-develop  = { git = "https://github.com/storytold/lightcraft", rev = "<pinned>" }
lightcraft-pipeline = { git = "https://github.com/storytold/lightcraft", rev = "<pinned>" }
lightcraft-raw      = { git = "https://github.com/storytold/lightcraft", rev = "<pinned>" }
lightcraft-codecs   = { git = "https://github.com/storytold/lightcraft", rev = "<pinned>" }
photocraft-algo     = { git = "https://github.com/storytold/photocraft", rev = "<pinned>" }
# exactly one of:
libfacedetection_rs = { git = "https://github.com/ShiqiYu/libfacedetection", rev = "<pinned>" } # path rust/; not on crates.io on 2026-10-07
# or tract + bundled YuNet ONNX, or ort + SCRFD
```

Rules:

- Pin every git dependency to a commit; record pin + date in the decision log.
- Workspace MSRV is **1.95** (LightCraft 1.90, photocraft-algo 1.95, libfacedetection_rs
  1.85).
- Do **not** pull `ui-egui`, and avoid `lightcraft-gpu`/wgpu unless profiling demands it
  (the pipeline CPU path has no wgpu dependency; verified).
- Avoid pulling the rest of PhotoCraft; `photocraft-algo` is self-contained. If it churns,
  vendor the 2–3 kernels we use with attribution (MIT/Apache allows it).
- Both families use `image 0.25` (LightCraft) and GPUI consumes `image 0.25` frames — keep
  that major aligned.

### 6.2 Type bridges

- `lightcraft-pipeline` output is display-encoded sRGB (`Rgb32f`); PhotoCraft's `algo` works
  on its own `Image`/`Surface` + `Rect`/`Ctx`. Convert at one boundary:
  `RenderedImage { data: Vec<f32>, w, h, ch }` ↔ both. Keep conversions in one module and
  unit-test round-trips losslessly (f32 → f32).
- Selection: `photocraft-algo` selection functions take `&[[f32; 4]]` / `&[[u8; 4]]` + `Rect`
  — trivial conversions.
- Two crate families are both named `geom`/`raster`/`color` (LightCraft and PhotoCraft): keep
  the bridge module the only conversion point.

### 6.3 XMP / interop

- Keep the exiftool `-stay_open` pipeline for IPTC + best-effort `crs:` sidecars; LightCraft
  reads `crs:` but does not write it, and preserves foreign namespaces.
- Face detections are our catalog data; optionally write MWG face-region metadata later
  (out of scope).
- Do not mix sidecar naming conventions: LightCraft uses `<stem>.xmp` by default and accepts
  `<file>.xmp`; our plan is the same for raws — verify no collision.

### 6.4 Coding constraints coming from upstream

- LightCraft: edition 2024, MSRV 1.90, workspace lints (`unsafe_code = "deny"`, `clippy.toml`),
  clippy `todo = "warn"`; internal functions are often `pub(crate)` — use the documented
  public surfaces (`DevelopSettings`, `pipeline::render*`, `MaskShape`, `FilterParams`,
  selection functions).
- PhotoCraft `algo`: `#![forbid(unsafe_code)]`, `deny(clippy::unwrap_used, expect_used,
  panic, …)`, edition 2024. Match to avoid friction.

---

## 7. Performance budgets

| Job | Budget | Notes |
|---|---|---|
| Quick-select dab (wand on proxy) | < 5 ms | `wand_region` with bbox = touches few pixels |
| Selection refine (guided filter) | < 50 ms on 2 MP proxy | boundary-scaled work |
| Retouch stage on export (45 MP) | < 300 ms | mosaic + blur on region; rayon-parallel |
| Face detect, single photo ROI | 0.2–2 s | model-dependent; background, not interactive |
| Preview render after retouch edit | reuse pipeline stage cache; retouch stage re-runs only when recipe changes | |

Interactive rule: all selection interaction on a display proxy; re-run at full resolution only
on apply/export, exactly like the existing quick-edit design.

---

## 8. Testing

1. **Selection math** (unit): wand region bboxes, combine ops, feather/expand, lasso polygon
   rasterization; edge cases at image borders.
2. **Filter goldens**: mosaic and blur on synthetic + real fixtures at proxy and full size —
   assert preview-path and export-path produce identical bytes for the same normalized recipe
   (the anti-drift guarantee).
3. **Face detection fixtures**: a small corpus (crowd shots at several densities/sizes, some
   no-face negatives). Metrics: recall per face-size bucket, false positives per image. Keep
   fixtures out of the repo (or in a separate private fixture repo).
4. **Privacy check**: after mosaic with the UI defaults, **automatically** re-run a detector
   (preferably the stronger SCRFD escalation model) over the redacted export and assert zero
   faces above threshold; keep 3–4 before/after goldens for human review of false negatives
   at mosaic boundaries. Downscale + sharpen before the detector to catch weak redaction.
5. **Batch integration**: recipe over N photos → queue → deterministic output names/statuses,
   crash-resume.

---

## 9. Risks & mitigations

| Risk | Mitigation |
|---|---|
| LightCraft API churn, unpublished crates | Pin git revs; contain behind traits; all upstream code is MIT/Apache, so vendoring a needed crate is possible |
| Raw coverage/fidelity gaps (CR3, compressed RAF/ORF, RW2 v4, NEF lossy-after-split; no calibration DB) | Accepted for v1, no fallback engine; document per-camera expectations; LightCraft renders embedded previews for unsupported formats (preview-grade, flag quality); Canon users wait |
| Face detector fails on small crowd faces | ROI-first + tiling; benchmark before committing to a model; escalation path SCRFD/ort; manual add/toggle always available |
| Gaussian blur is invertible | Default to mosaic for faces; strength presets tuned from the privacy test |
| Memory/latency on 45 MP full-res retouch | Proxy interaction, full-res only on apply/export; rayon; reuse stage cache |
| Two ArtCraft dependency trees | Only `photocraft-algo` (4 small crates) + bridge module; vendor kernels if it becomes a problem |
| License surprise | All named code is MIT/Apache except the face model: libfacedetection is BSD-3-Clause (verified); verify YuNet ONNX redistribution terms if that path is chosen |

---

## 10. `PLAN.md` deltas

Applied 2026-10-07:

1. **Non-goals**: "no masking" removed; narrowed to no layers/compositing/PSD/healing; added
   no AI segmentation and no darktable/RawTherapee fallback (Canon waits for upstream).
2. **Decisions table**: render engine (LightCraft crates), edit model (DevelopSettings only),
   raw handling, retouch, selections, face detection, privacy default added; darktable,
   rawler and own tone math removed.
3. **Architecture**: render service + selection service + face detect service; catalog holds
   selections, recipes, detections.
4. **Risks**: upstream churn, raw coverage (no fallback), second-tone-math drift, small-face
   recall, blur reversibility.
5. **Core loop acceptance**: retouch scenario added (lasso → mosaic; face detect in region →
   blur; recipe across a 40-photo burst).

---

## 11. Milestones & task breakdown

Tracked in `PLAN.md` as M6 (feature) and R0–R4 (tasks). R0 runs immediately in scratch
binaries; R1 needs M2's canvas; R2 needs M5's render path.

- **R0 — Spike (1–2 days)**
  - T1: Build LightCraft crates from a pinned rev on Linux; render one NEF via
    `lightcraft-pipeline` in a scratch binary.
  - T2: Same file via `lightcraft-cli render`; compare cold-start vs in-process (CLI is now
    only a sanity reference, not the plan).
  - T3: Face-detector bake-off on 10–20 real crowd photos (libfacedetection_rs vs
    tract+YuNet; optionally SCRFD/ort): recall by face size, CPU time, binary size.
  - T4: Convert a rendered image into `photocraft-algo` surfaces and run Mosaic + Gaussian;
    verify tiling/preview-consistency.
  - Exit criteria: one-page memo updating §3 with measured numbers.
- **R1 — Selection tools (GPUI)**
  - T5: `edit-retouch` crate: `Shape`/`Selection` model, normalized coords, rasterize +
    feather.
  - T6: Canvas interactions: rect/ellipse, lasso (RDP simplify), quick-select brush using
    `wand_region` + `combine_region`; overlay rendering with `paint_path`.
  - T7: Selection storage in catalog + cache; unit tests.
- **R2 — Retouch ops**
  - T8: Retouch stage in render path (preview + export); Mosaic and Gaussian ops; strength
    UI; privacy presets.
  - T9: Golden tests proving preview/export equality.
  - T10: Single-photo acceptance: draw lasso around a fan section → mosaic → send.
- **R3 — Face blur**
  - T11: `FaceDetector` trait + chosen implementation behind a background queue.
  - T12: Detect-in-region UI: run on region, boxes overlay, toggle/manual add, apply.
  - T13: Detection caching in catalog; re-render only on change.
- **R4 — Batch & recipes**
  - T14: Retouch recipes (save/apply to selection), sticky regions across a burst.
  - T15: Batch detection jobs with per-photo status + retry; crash-resume test.
  - T16: Acceptance scenario; performance pass.

---

## 12. Open questions

1. Primary engine: settled — link LightCraft crates in-process (CLI only as a sanity check).
2. Unsupported-raw policy: settled — embedded-preview render, no fallback engine.
3. Quick-select determinism: freeze the rasterized mask for export, keep dabs for editing,
   hash as cache key (see §5.2).
4. Detector choice: pending R0 bake-off (T3). Bundle the model in-process; note
   `libfacedetection_rs` was not resolvable on crates.io on 2026-10-07.
5. Face mask shape: landmarks if the chosen detector exposes them (YuNet model does; verify
   the Rust port), else expanded boxes.
6. Does the app write `crs:` develop sidecars from the `DevelopSettings` model (LR
   round-trip), or keep slider state app-native and only write on explicit "save metadata"?
   Existing plan says best-effort crs always; revisit after T1.

---

## 13. References

- `filipkauffeldt/editing`: `README.md`, `PLAN.md`, `docs/research/{cli-engines,rust-image-pipeline,lr-edit-math-and-roundtrip,workflow-apps-survey}.md`.
- LightCraft: <https://github.com/storytold/lightcraft> — root `Cargo.toml`,
  `crates/develop/src/settings.rs` (`LocalAdjustments`, `MaskShape`),
  `crates/pipeline/src/{masks,spots,local}.rs`, `crates/raw/src/lib.rs`,
  `crates/engine/src/crs.rs`, `docs/xmp-interop.md`, `apps/lightcraft-cli/src/main.rs`,
  `ROADMAP.md`.
- PhotoCraft: <https://github.com/storytold/photocraft> — `crates/algo/src/{lib.rs,
  blur.rs, pixelate.rs, selection.rs, matting.rs, retouch.rs}`, `crates/algo/Cargo.toml`.
- Face detection: <https://github.com/ShiqiYu/libfacedetection> (`rust/` port, LICENSE),
  <https://github.com/sonos/tract>, OpenCV Zoo YuNet (`models/face_detection_yunet/`),
  SCRFD via <https://pykeio.github.io/ort/>.
- Prior art: RapidRAW (LR preset import, `src-tauri/src/preset_converter.rs`), darktable LR
  importer (`src/develop/lightroom.c`).

---

## 14. Verification log (2026-10-07)

Checked directly against upstream:

- GitHub pages for `storytold/lightcraft` (1.8k stars, MIT/Apache, 0.2.1 workspace in root
  `Cargo.toml`, edition 2024, MSRV 1.90), `storytold/photocraft` (11.8k stars, MIT/Apache,
  0.3.0, edition 2024, MSRV 1.95), `ShiqiYu/libfacedetection` (12.8k stars; `rust/` port;
  LICENSE = 3-clause BSD).
- Raw `Cargo.toml` for `lightcraft-pipeline`: deps `geom`, `color`, `raster`, `develop`,
  `rayon`, `serde` — no wgpu. Raw `Cargo.toml` for `photocraft-algo`: `geom`, `color`,
  `raster`, `serde`, optional rayon.
- `photocraft-algo` source: `FilterParams::Mosaic { cell_size }`, selection-aware `apply`.
- `libfacedetection` Rust port README: `libfacedetection_rs`, static generated model data,
  scalar + runtime AVX2, MSRV 1.85, no C ABI in 0.1.0, WIDER Face AP 0.887/0.871/0.768.
- crates.io API: `lightcraft-develop`, `photocraft-algo`, `libfacedetection_rs` all 404 as of
  2026-10-07 (control lookup for `serde` succeeds) → git/path pins required.

Corrections applied from the first draft: star counts updated; libfacedetection license
resolved to BSD-3-Clause; YuNet-class hard recall corrected upward (0.768); crates.io
availability checked; quick-select determinism changed from "frozen mask hash" to "freeze the
mask, hash as cache key"; MSRV corrected to 1.95 workspace-wide.

---

## 15. Handoff checklist (updated)

- [x] Save this document as `docs/research/retouch-selection-faceblur.md` (this file).
- [x] Update `PLAN.md` per §10 (non-goals, decisions, architecture, risks, acceptance).
- [ ] Create the `edit-retouch` crate skeleton per §5.1 (after M0 workspace exists; no
      dependencies yet).
- [ ] Run R0/T1–T4 and write the memo with measured numbers.
- [ ] Pick the detector and record the license/redistribution decision.
- [ ] Pin LightCraft + PhotoCraft git revisions in `Cargo.toml` and in the decision log.
