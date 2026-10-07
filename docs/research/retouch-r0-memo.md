# R0 retouch spike — measured results and decisions

**Date:** 2026-10-07
**Status:** spike complete, numbers measured on this machine
**Branch:** `spike/retouch-r0` (PR A3)
**Scope:** R0/T1–T4 — in-process LightCraft render, face-detector bake-off,
photocraft-algo Mosaic + Gaussian.

Everything in `spikes/retouch-r0/` is a standalone crate (own `Cargo.toml`,
not a member of the app workspace). Scratch binaries: `render`, `parity`,
`facebake`, `retouch`.

---

## 1. Pinned revisions (committed in `spikes/retouch-r0/Cargo.toml`)

| Crate | Rev | Date |
|---|---|---|
| `lightcraft-{develop,pipeline,raw,codecs,color,raster,geom}` | `f2f449611d248a907f755d6e18f3701db5755f03` | 2026-10-07 |
| `photocraft-{algo,raster,color,geom}` | `90df9cfc159e5916be92c6b25f30c13913dfdd70` | 2026-10-07 |
| `libfacedetection_rs` (rust/ port, git) | `acf7b254121927e7dced30e233a2e03119f28ea2` | 2026-06-28 |
| `tract-onnx` (crates.io) | `0.21.10` | — |

Notes:

- All three git crates are **not on crates.io** (confirmed again today), so they
  are git-pinned. `libfacedetection_rs` is the official port under
  `ShiqiYu/libfacedetection/rust/`; package name `libfacedetection_rs`.
- `lightcraft-codecs` is pulled with `default-features = false` (drops the
  AVIF/JXL encoders; PNG + JPEG decode are always on) to keep the spike build
  lean.
- `Cargo.lock` is committed for reproducibility.

## 2. T1 — render one NEF in-process with `lightcraft-pipeline`

Sample (from raw.pixls.us, CC0):

- `Nikon D7500 — 14bit Lossless NEF`
  `https://raw.pixls.us/getfile.php/1534/nice/Nikon%20-%20D7500%20-%2014bit%2014bit%20compressed%20%28Lossless%29%20%283:2%29.NEF`
- 26.8 MB, 5600×3728 (20.9 MP), SHA-256
  `430b4f1be4e53a011861b63294ad19fdc0353ec3ea62104f77d4c193e3dd3fc8`

`./target/release/render <nef> out.png` (release build, 8-core box):

```
full size:  load 913 ms + render 768 ms + encode 216 ms = 1904 ms  (5600×3728, 39.4 MB PNG)
proxy 1600: load 341 ms + render 64  ms + encode  15 ms =  420 ms  (1600×1065)
```

- The NEF takes the "file-local camera look" path: LightCraft fits a colour
  matrix + tone curve to the embedded JPEG (NEF has no colour matrix), so
  `SourceInfo { raw: true, relative_wb: true, camera_tone: Some(..) }`.
- Interactive proxy render is ~64 ms once the raw is decoded and developed.
- The in-process bridge (`src/lightcraft_bridge.rs`) is adapted from
  `lightcraft-engine::files` + `camera_preview` at the pinned rev, with
  attribution. It is the decode→develop-source path the app's render service
  will need.

## 3. T2 — parity + cold-start vs in-process

`lightcraft-cli` was built from the same rev
(`cargo build --release -p lightcraft-cli`). Forcing CPU
(`LIGHTCRAFT_GPU=0`):

```
$ /usr/bin/time lightcraft-cli render <nef> -o cli.png
    Elapsed (wall): 2.31 s   (user 7.42 s over ~3.5 cores, peak RSS 1.76 GB)
$ ./render <nef> -o mine.png   →  1.90 s total (913 ms decode+develop)
$ ./parity cli.png mine.png
    mean |diff|/channel: 0.000000   max |diff|: 0   →  PASS: pixel-identical
```

- **Output parity: pixel-identical.** The only PNG byte difference is metadata —
  the CLI embeds EXIF/XMP text chunks, the spike uses `EncodeMeta::default()`.
- **Cold-start cost is real:** the CLI spends ~2.3 s wall per run on process
  startup + library import + decode + render, versus ~64 ms for an in-process
  proxy render after the raw is loaded, and 768 ms for a full-size render. The
  subprocess path also drags in the whole app: the CLI binary is **191.7 MB**
  (engine + ui-egui + wgpu) vs the spike `render` binary at **7.1 MB**.
- Confirms the plan's direction: link the lower crates, do not shell out to a
  render engine.

## 4. T3 — face-detector bake-off

### Dataset (recorded, not committed)

- **WIDER Face validation set** (CUHK), 3226 images, via the HuggingFace mirror
  `CUHK-CSE/wider_face` (`data/WIDER_val.zip` + `data/wider_face_split.zip`).
  WIDER Face is **research-only (non-commercial)** — used here only to measure,
  never redistributed.
- Selected **20 crowd images**: the 15 densest (Demonstration, Parade, Sports
  Fan, Festival, …) plus 5 from the 100–250-face band, for a total of
  **7873 face boxes**. No image has a face >150 px, so the "large" bucket is
  empty by construction (crowd shots are a small-face problem — the point of
  the spike). The WIDER `invalid` flag (blur/pose/expression) was ignored; all
  boxes are counted.

### Metrics

Recall by face-size bucket (long edge in image px), IoU ≥ 0.5 matching, greedy
NMS. CPU time over all 20 images. Thresholds: `libfd` at its port defaults
(conf 0.2, NMS 0.45) and at 0.5; `yunet` at crowd settings (0.6) and OpenCV
default (0.9).

| Detector | conf | tiny <24px | small 24–50 | medium 50–150 | overall | false positives | CPU (20 imgs) |
|---|---|---|---|---|---|---|---|
| libfacedetection_rs | 0.2 | 14.80 % | 60.95 % | 44.00 % | 18.81 % | 2894 | ~1.1 s |
| libfacedetection_rs | 0.5 | 8.72 % | 55.59 % | 36.00 % | 12.78 % | 427 | ~0.86 s |
| tract + YuNet | 0.6 | 13.71 % | 62.48 % | 54.00 % | 18.01 % | 361 | ~2.8 s |
| tract + YuNet | 0.9 | 0.01 % | 0.92 % | 8.00 % | 0.14 % | 0 | ~2.9 s |

Bucket sizes: tiny 7170, small 653, medium 50, large 0.

Readings:

- **Both models are weak on this set, and it is the set, not the model:** 91 %
  of the faces are under 24 px (many ~6×6 px), below the ~10–20 px detector
  minimum. Absolute recall is low for both; the comparison is still valid.
- **CPU: libfd ≈ 43–55 ms/image, YuNet ≈ 142 ms/image** (≈3.3× slower, tract on
  CPU, 640×640 input).
- **Recall per false positive favours YuNet:** at ~equal FP (~400), YuNet keeps
  18.0 % recall while libfd drops to 12.8 %. libfd's 0.2 default is very
  permissive (2894 FP). YuNet also yields **5 landmarks** per face; the
  libfacedetection port exposes score + box only.

### Binary size (release `facebake`, `strip`ped)

| Build | size |
|---|---|
| baseline (no detector) | 433 KB |
| `--features libfd` | 1.93 MB (adds ~1.5 MB, model compiled into source) |
| `--features tract` | 18.2 MB (adds ~17.8 MB; + 227 KB ONNX at runtime) |

The YuNet ONNX (`face_detection_yunet_2023mar.onnx`) is 232,589 bytes,
SHA-256 `8f2383e4dd3cfbb4553ea8718107fc0423210dc964f9f4280604804ed2552fa4`,
Apache-2.0.

### Recommendation

**Ship `libfacedetection_rs` as the R3 default**, behind the `FaceDetector`
trait, and keep `tract` + YuNet as the documented swap-in.

Reasons:

1. **Size & distribution:** ~1.5 MB compiled in, no model file to bundle, so
   "bundle in-process, no download-on-first-use" holds trivially. tract adds
   ~18 MB of runtime plus a separate 227 KB ONNX artifact.
2. **Speed:** ~3.3× faster per image (background job, but still a queue-
   draining CPU cost on bursts of 40).
3. **License:** BSD-3-Clause (verified in `rust/`), no redistribution terms to
   worry about. YuNet is also clean (Apache-2.0), so this is not decisive.
4. **Recall:** on the crowd case the two are within noise on small faces; the
   model choice is not the lever — the plan's ROI-first cropping + tiling is.
   libfd's permissive default needs a raised threshold or the toggleable-box UI
   will flood.

When to switch to YuNet (already fully implemented in `src/yunet.rs`): if we
want the **5 landmarks** for rotated face ellipses (libfd has none → falls back
to expanded boxes, which the plan already allows), or if false positives at a
usable threshold prove unacceptable. SCRFD/ort stays the escalation path if
crowd recall is still too low.

## 5. T4 — photocraft-algo Mosaic + Gaussian on a render

`./target/release/retouch <render.png> <out-dir>` (20.9 MP, 5600×3728):

```
mosaic:   auto 1100 ms | tile128 == auto: true | tile257 == auto: true | rerun identical
gaussian: auto  500 ms | tile128 == auto: ~1 LSB | tile257 == auto: ~1 LSB | rerun identical
masked mosaic (central 50% region): 1217 ms, 5.2 M px changed inside, 0 outside
mosaic full-downscaled vs proxy-applied: mean|diff| 3.77/255 (1.5 %), 0.6 % channel samples differ
mean preserved (full): mosaic Δ0.016, gaussian Δ0.008; (proxy): mosaic Δ0.012
```

- **Mosaic is exactly tile-invariant.** Cells are anchored at the reference
  bounds' origin (`(x - b.x0).div_euclid(cell)`), so tile size and thread order
  do not change a single pixel.
- **Gaussian is tile-invariant to ~1 LSB**, not bit-exact: the radius-8 path is
  three box passes, and float reassociation across tile boundaries shifts ~2500
  of 20.8 M pixels by one level (mean |diff| < 0.00005). Deterministic per tile
  size; visually identical. Use a fixed tile size in the retouch stage (the
  default `apply` is fine).
- **Selection blending works:** `apply(.., selection)` reads channel 0 of the
  mask surface as coverage (gotcha recorded in `src/photocraft_bridge.rs`);
  pixels outside the mask are byte-identical to the source.
- **Preview/full consistency:** a normalized recipe (cell = long-edge fraction)
  applied at proxy and at full size agrees within 1.5 % mean |diff| after
  resampling, and both preserve the region's mean colour to < 0.02 levels.
  Performance at 20.9 MP: mosaic 1.1 s + gaussian 0.5 s — within the plan's
  `< 300 ms @ 45 MP` retouch budget when scoped to a region (region-only apply
  is the app's path, not whole-image).

## 6. License / redistribution decision

- All upstream code is MIT/Apache-2.0 (LightCraft, PhotoCraft) or BSD-3-Clause
  (libfacedetection_rs) — no copyleft, no vendoring required.
- Detector pick **libfacedetection_rs** embeds its model in generated source
  (BSD-3, no separate artifact), so there is nothing extra to redistribute.
- The YuNet ONNX is Apache-2.0 and may be bundled if that path is chosen; it is
  **not committed** (git-ignored) and its download is recorded in §4.
- The WIDER Face images and annotations are **research-only and not committed**;
  they are git-ignored under `spikes/`.

## 7. Download steps (reproduce from scratch)

```sh
# sample NEF
curl -sL "https://raw.pixls.us/getfile.php/1534/nice/Nikon%20-%20D7500%20-%2014bit%2014bit%20compressed%20%28Lossless%29%20%283:2%29.NEF" -o nef-d7500.nef

# YuNet ONNX (for the tract path)
curl -sL https://github.com/opencv/opencv_zoo/raw/main/models/face_detection_yunet/face_detection_yunet_2023mar.onnx -o face_detection_yunet_2023mar.onnx

# WIDER Face val + annotations (research-only)
curl -sL "https://huggingface.co/datasets/CUHK-CSE/wider_face/resolve/main/data/WIDER_val.zip" -o WIDER_val.zip
curl -sL "https://huggingface.co/datasets/CUHK-CSE/wider_face/resolve/main/data/wider_face_split.zip" -o wider_face_split.zip
```

Run the spikes:

```sh
cargo build --release
./target/release/render nef-d7500.nef out.png                 # T1
./target/release/parity cli.png out.png                       # T2
./target/release/facebake wider_face_val_bbx_gt.txt --images WIDER_val/images --yunet face_detection_yunet_2023mar.onnx   # T3
./target/release/retouch out.png retouch_out                  # T4
```

## 8. Notes / blockers

- No task was blocked. The tract path needed three fixes before YuNet ran:
  the 2023mar ONNX is **640×640 BGR** (not 320 RGB), its 12 outputs are split
  per stride (cls/obj/bbox/kps × strides 8/16/32), and boxes are anchor deltas
  (`cx=(c+dx)·s`, `w=exp(dw)·s`, `score=√(cls·obj)`) per
  `opencv/objdetect/src/face_detect.cpp`. All encoded in `src/yunet.rs`.
- A temporary btrfs ENOSPC on `/home` (full metadata) interrupted the first
  build; resolved by the environment, no code impact.
- The bake-off has no large-face data (crowd photos contain none); large faces
  are the easy case both detectors handle, so this does not change the pick.
