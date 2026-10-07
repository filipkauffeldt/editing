# Rust image pipeline research: raw decode, previews, quick-edit ops, color, JPEG codecs

> **Superseded on engine and raw handling (2026-10-07).** This doc's recommendations —
> rawler for previews, darktable-cli for development/export, own quick-edit tone math, and
> a subprocess fallback for unsupported raws — are retired. `PLAN.md` now uses
> `lightcraft-raw` for previews/decode and `lightcraft-pipeline` + `DevelopSettings` for
> development and export, with no fallback engine. See `PLAN.md` and
> `docs/research/retouch-selection-faceblur.md`. The format/embedded-preview analysis, GPUI
> BGRA rendering notes and codec comparisons below remain useful as technical reference.

**Date:** 2026-08-16
**Context:** Cross-platform Rust/GPUI desktop photo editor. Raw development + export delegated to `darktable-cli`; the app must quickly decode embedded JPEG previews from raws and apply real-time "quick edit" ops (exposure, WB, highlights/shadows, white/black point) on-screen.
**Method:** All claims verified against primary sources (crates.io registry data via lib.rs/docs.rs, source repositories, official docs). Source URLs inline; full list at the end.

---

## TL;DR

| Concern | Recommendation | Why |
|---|---|---|
| Raw parsing + embedded preview extraction | **`rawler` 0.7.2** | Pure Rust, active, CR3+NEF(Z-series) coverage, *public* preview-extraction API returning `image::DynamicImage` |
| JPEG decode | **`zune-jpeg`** (already the decoder inside `image` 0.25) | Pure Rust, near libjpeg-turbo speed, no C toolchain |
| JPEG encode (if needed) | `image`'s built-in encoder; **`turbojpeg`** if profiling demands | zune-jpeg is decode-only |
| Quick-edit pixel ops | Custom rayon-parallel pass (LUT + per-channel gain) | Trivial math; `imageproc` is scalar and assumes linear light |
| Resize for display proxy | **`fast_image_resize`** (+`rayon` feature) | SIMD; 10–20× faster than `image`'s resize |
| Color management | **`lcms2`** (ICC transforms) or **`moxcms`** (pure Rust) | lcms2 mature/SIMD-free but battle-tested; moxcms pure Rust + SIMD, already in `image`'s tree |
| Display | `gpui::RenderImage` (BGRA frames via `image::Frame`) | GPUI natively consumes `image` 0.25 frames |
| Fallback | `exiftool -b -JpgFromRaw` / `darktable-cli` subprocess | Covers formats/cameras rawler misses |

---

## 1. Raw decoding in Rust

### 1.1 `rawler` (in the dnglab repository) — recommended

- **What it is:** "Library to extract images and metadata from camera raw formats" — not literally a port of darktable's rawspeed; it is an evolution of `rawloader` maintained by Daniel Vogelbacher (cytrinox) inside the [dnglab](https://github.com/dnglab/dnglab) repo ("rawloader, which is basically the fundament of this library" — [rawler on lib.rs](https://lib.rs/crates/rawler)). The dnglab README credits the darktable team and rawspeed among its sources.
- **Version/maintenance:** 0.7.2 released 2026-02-26 (releases: 0.6.0 Jan 2024, 0.7.0 Jun 2025, 0.7.1 Oct 2025, 0.7.2 Feb 2026); Rust 2024 edition; MSRV 1.89; ~12,100 downloads/month; ~29K SLoC ([lib.rs](https://lib.rs/crates/rawler), `rawler/Cargo.toml` in repo). Repo: 1,570 commits, 656 stars, active issue tracker ([dnglab](https://github.com/dnglab/dnglab)).
- **License:** **LGPL-2.1** ([lib.rs](https://lib.rs/crates/rawler), [LICENSE](https://github.com/dnglab/dnglab/blob/main/LICENSE)). Copyleft — fine for an open-source app; for a proprietary app static linking a Rust LGPL lib requires satisfying LGPL relinking obligations or isolating it in a separate process. Decide early.
- **Format coverage:** CR3, CR2, CRW, NEF, NRW, ARW, SRF, SR2, RAF, ORF, RW2, PEF, DNG, ERF, 3FR, IIQ, MOS, MEF, MRW, KDC, DCS, DCR, SRW, ARI — all marked supported in the [README format table](https://github.com/dnglab/dnglab#supported-raw-file-formats). CR3 details tracked in [CR3_STATE.md](https://github.com/dnglab/dnglab/blob/main/CR3_STATE.md).
- **Camera coverage:** 876 camera models marked supported in [SUPPORTED_CAMERAS.md](https://github.com/dnglab/dnglab/blob/main/SUPPORTED_CAMERAS.md). Nikon Z line verified present: Z 5/6/7/8/9, Z 6_2/7_2, Z5_2, Z6_3, Z 30, Z 50, Z50_2, Z f, Z fc (12/14-bit modes listed per model).
- **API:** `decode_file()` → `RawImage` with mosaiced sensor data + metadata; uses `rayon` and `memmap2` internally; decodes embedded JPEGs via `image` 0.25 (`Cargo.toml` dependency: `image = { version = "0.25", features = ["jpeg"] }`) ([docs.rs](https://docs.rs/rawler/latest/rawler/)).
- **Caveats:**
  - API explicitly **not SemVer-stable yet** ("Rawler crate is now published to crates.io, but please notice that the API is not yet stable" — [dnglab README](https://github.com/dnglab/dnglab)). Pin versions.
  - Authors state panics on malformed files are acceptable for debuggability and recommend **not** using rawler on untrusted input in hostile environments ([README, "A note about processing of untrusted files"](https://github.com/dnglab/dnglab#a-note-about-processing-of-untrusted-files)). For a desktop editor reading the user's own files this is tolerable; keep decode on a crash-isolated thread/process.
  - Windows binaries "untested" for the dnglab CLI (README); the library itself is pure Rust, which is the portability argument.

### 1.2 `rawloader` — effectively dormant, no CR3

- Last release 0.37.2 on **2026-08-15**, but the previous one was 0.37.1 on **2022-04-15** — a 4-year gap; 0.37.0 was 2021 ([crates.io API](https://crates.io/api/v1/crates/rawloader/versions), [lib.rs](https://lib.rs/crates/rawloader)). Format list includes CR2/CRW/NEF/NRW/ARW/RAF/ORF/RW2/PEF/DNG etc. but **no CR3** ([README](https://github.com/pedrocr/rawloader)). LGPL-2.1, ~4,500 dl/month, 6K SLoC. Its author is now a rawler contributor. **Verdict: don't build on it** — modern Canon bodies are unsupported.

### 1.3 `libraw` bindings (LibRaw is C++, LGPL-2.1 / CDDL-1.0 / LibRaw license — [license note](https://lib.rs/crates/libraw))

| Crate | Latest | Status | Notes |
|---|---|---|---|
| [`libraw`](https://lib.rs/crates/libraw) (dcuddeback) | 0.1.1 (2015-11) | Dead | Rust 2015; links system `libraw_r` via pkg-config |
| [`libraw-rs`](https://lib.rs/crates/libraw-rs) (paolobarbolini) | 0.0.4 (2021-02) | Stale | Self-described "work in progress"; wraps LibRaw 0.20.1 (current LibRaw is 0.21/0.22-era) |
| [`rsraw`](https://lib.rs/crates/rsraw) (hexilee) | 0.1.1 (2026-03) | **Young but active** | Vendored LibRaw C++ compiled via `cc`/`bindgen` (needs C++ toolchain + CMake); MIT crate; thumbnail extraction (`extract_thumbs()` → JPEG/bitmap/…, size-sorted), unpack + demosaic/8/16-bit process; claims Linux/Windows/macOS/iOS/Android |
| [`rawlib`](https://lib.rs/crates/rawlib) (mtaech) | 0.7.1 (2026-07) | Active | Ships **prebuilt LibRaw 0.22.2 static libs for Windows MSVC + Linux x86_64 only** (macOS still on roadmap); embedded-thumbnail extraction + full decode with speed presets; own perf numbers below |
| [`raw_preview_rs`](https://lib.rs/crates/raw_preview_rs) | 0.1.2 (2025-08) | Active but niche | LibRaw 0.21.4 + libjpeg-turbo + TinyEXIF statically built; build.rs downloads deps at compile time; **GPL-3.0** — viral, likely disqualifying unless the app is GPL |

**Verdict:** If we must decode raws ourselves, **rawler is the realistic Rust-native choice** (coverage + activity + pure Rust + preview API). A LibRaw wrapper (`rsraw`/`rawlib`) is the fallback for maximum camera coverage, at the cost of a C++ build/packaging burden and LGPL LibRaw underneath.

---

## 2. Embedded JPEG preview extraction (no exiftool)

### 2.1 rawler exposes this publicly — key finding

The `rawler::analyze` module (public since it's a `pub mod` in [`lib.rs`](https://github.com/dnglab/dnglab/blob/main/rawler/src/lib.rs)) provides exactly what we need, verified in source ([analyze.rs](https://github.com/dnglab/dnglab/blob/main/rawler/src/analyze.rs)):

```rust
pub fn extract_thumbnail_pixels(path, params) -> Result<DynamicImage> // thumb, else preview
pub fn extract_preview_pixels(path, params)  -> Result<DynamicImage> // preview, else full render
pub fn extract_full_pixels(path, params)     -> Result<DynamicImage> // embedded full-size JPEG, else raw→sRGB render
```

- The decoder trait's `preview_image()` / `thumbnail_image()` hooks ([decoders/mod.rs:345-356](https://github.com/dnglab/dnglab/blob/main/rawler/src/decoders/mod.rs)) are implemented for **arw, cr2, cr3, dng, nef, pef, raf, rw2, tfr** decoders (verified by grep of `rawler/src/decoders/`).
- Output type is `image::DynamicImage` — the embedded JPEG is already decoded for us (via rawler's `image` dependency), which drops straight into our pipeline.
- The dnglab CLI's `analyze --preview-pixel / --thumbnail-pixel / --structure` options demonstrate the maturity of this path ([README](https://github.com/dnglab/dnglab#analyze-subcommand)).

### 2.2 Via LibRaw wrappers

- `rsraw`: `raw_image.extract_thumbs() -> Vec<ThumbnailImage{width, height, format, data}>` returns the **embedded JPEG bytes** without a full raw decode ([README, "Thumbnail Extraction"](https://lib.rs/crates/rsraw)).
- `rawlib`: `extract_thumbnail()` returns the largest embedded (usually JPEG) image; the README notes **Nikon bodies embed full-size JPEG previews**, and reports **~0.1 s per 25 MB NEF** single-file, **50 files/s** batch (16-core + NVMe) ([lib.rs](https://lib.rs/crates/rawlib)).

### 2.3 Rolling our own?

- TIFF-container raws (CR2/NEF/ARW/DNG/RW2) keep previews in IFD0/IFD2 strips — parseable with generic TIFF/EXIF crates (e.g. [`kamadak-exif`](https://lib.rs/crates/kamadak-exif)), but that's redundant work.
- **CR3 is ISO Base Media File Format (MP4 boxes), not TIFF** — a hand-rolled extractor needs a BMFF box parser; rawler already implements it (`rawler/src/formats/cr3`, `decoders/cr3.rs`).
- **Verdict: use rawler; don't write format parsers ourselves.**

### 2.4 exiftool fallback

ExifTool extracts embedded previews without raw decoding (`-b -JpgFromRaw` / `-PreviewImage`, `-w` to write files) — documented in the [ExifTool man page](https://exiftool.org/ExifTool_Pod.html). Useful as an escape hatch for cameras rawler can't parse; slow (spawns Perl process), so batch/background only.

---

## 3. Quick-edit processing crates & realistic performance

### 3.1 The crates

- **`image` 0.25.10** (2026-03; MIT/Apache; 15.7M dl/month) — decode/encode + `imageops` (brighten, contrast, resize, blur…). `rayon` is a default feature (used by some codec deps). JPEG decoding now delegated to **zune-jpeg 0.5.5**; ICC handling via **moxcms**; both visible in its dependency list ([lib.rs](https://lib.rs/crates/image)). `imageops` are scalar per-pixel loops — fine for convenience, not for hot paths.
- **`fast_image_resize` 6.0** — SIMD (SSE4.1/AVX2/Neon/Wasm) resizer for U8..F32×4 formats; optional `rayon` feature; provides sRGB↔linear `PixelComponentMapper` and explicitly warns that resizing in non-linear sRGB is incorrect ([README](https://github.com/Cykooz/fast_image_resize)). Published single-threaded x86_64 benchmarks (RGB8, 4928×3279 → 852×567) ([benchmarks-x86_64](https://github.com/Cykooz/fast_image_resize/blob/main/benchmarks-x86_64.md)):
  - Bilinear: **3.53 ms** (AVX2) vs 78.41 ms (`image`)
  - Bicubic: **7.23 ms** vs 127.38 ms
  - Lanczos3: **13.26 ms** vs 173.98 ms
- **`imageproc`** — active (840 commits, MIT). **Caveats that matter to us:** functions "implicitly assume that pixels colors are stored in a linear color space like RGB as opposed to a non-linear color space such as sRGB", and parallel (rayon) variants "may not always be faster" ([README](https://github.com/image-rs/imageproc)). It targets CV/graphics-editor ops (filters, morphology, contours), *not* photographic tone curves. Useful later for effects; not the core of a tone pipeline.
- **`rayon` 1.12.0** (2026-04; MIT/Apache; 38.5M dl/month; MSRV 1.80) — work-stealing data parallelism; `par_chunks_mut()` over scanlines is the natural shape for a tone-op pass ([lib.rs](https://lib.rs/crates/rayon)).

### 3.2 Is a 24 MP 8-bit tone/WB pass interactive? (estimate + cited anchors)

A 6000×4000 RGB8 buffer is 72 MB. Exposure/WB is per-channel gain; highlights/shadows and black/white point collapse to a per-channel 256-entry **LUT** (plus optional per-pixel blend). A full-resolution pass touches ~144 MB (read+write):

- **Arithmetic estimate:** at a conservative 10–25 GB/s effective single-core memory bandwidth, a single-threaded LUT pass costs ~**6–15 ms**; with rayon on 8 cores, ~**1–2 ms** — i.e. 60+ fps full-res updates are plausible on CPU, and trivially so on a ~2–4 MP screen-resolution proxy. *(This bullet is arithmetic, not a crate benchmark.)*
- **Anchor 1:** fast_image_resize processes a 16.2 MP RGB8 image (full read + filtered downsample) in **3.53 ms single-threaded AVX2** — same memory-traffic class as our LUT pass ([benchmarks](https://github.com/Cykooz/fast_image_resize/blob/main/benchmarks-x86_64.md)).
- **Anchor 2 (why we edit the preview, not the raw):** a LibRaw *half-size bilinear 8-bit* decode of a 44 MP RW2 costs ~**1.2 s/file** (~2.9 s at quality settings) ([rawlib perf table](https://lib.rs/crates/rawlib)) — raw demosaic is 2–3 orders of magnitude too slow for slider drags; an embedded-preview JPEG decode is tens of ms (§5).
- **Design implication:** apply edits to a display-size proxy during drags (LUT at 2 MP ≈ ~1 ms even single-core), re-render the 24 MP preview on slider release; final quality always comes from darktable-cli anyway.
- **Color-space note:** if we want *correct* exposure/WB math, do it in linear light (decode sRGB→linear once, keep f32/u16 working copy, convert back on display). Both fast_image_resize ([README](https://github.com/Cykooz/fast_image_resize)) and imageproc ([README](https://github.com/image-rs/imageproc)) make the same linear-light argument. A v1 "quick edit" can operate directly on sRGB like most quick-viewers, at some highlight-behavior cost.

---

## 4. Color management

- **`lcms2` 6.1.1** (2025-07; Kornel; ~333k dl/month) — safe Rust wrapper for LittleCMS; crate MIT (LCMS2 C library itself is MIT, per [littlecms.com](http://www.littlecms.com)); "up to date with LCMS 2.17"; README shows the exact pattern we need: `Transform::new(&camera_profile, PixelFormat::RGB_8, &Profile::new_srgb(), PixelFormat::RGB_8, Intent::Perceptual)` then `transform_in_place()` ([lib.rs](https://lib.rs/crates/lcms2)). Threading: use `*_context()` constructors to get `Send` transforms; `Flags::NO_CACHE` for `Sync` (LCMS has a thread-unsafe transform cache) ([README, "Threads"](https://lib.rs/crates/lcms2)).
- **`moxcms` 0.9.0** (2026-07; BSD-3-Clause/Apache-2.0) — **pure-Rust** ICC CMS with AVX/AVX-512/NEON/SSE paths; CMYK⬌RGBX, RGBX⬌RGBX, LAB, GRAY, up to 16 inks; 12.2M dl/month because `image` depends on it ([lib.rs](https://lib.rs/crates/moxcms)). Avoids shipping a C library.
- **What our pipeline actually needs:**
  - Camera-embedded JPEG previews are already camera-processed (near-sRGB); usually displayable directly. If a file carries an embedded ICC (e.g. Adobe RGB), decode + transform via lcms2/moxcms — the lcms2 README demonstrates extracting `ICC_PROFILE` APP2 markers from JPEGs and applying exactly this transform ([lib.rs](https://lib.rs/crates/lcms2)).
  - If we ever render from raw data ourselves, rawler returns the metadata needed to build the transform without ICC files: WB channel multipliers and a camera→XYZ color matrix (the rawloader metadata contract rawler inherits — "The multipliers to apply to the color channels for the white balance; A conversion matrix between the camera color space and XYZ" — [rawloader README](https://github.com/pedrocr/rawloader)). A 3×3 matrix × sRGB transfer function is then a few lines of shader-like Rust, no CMS needed.
  - Display-side (wide-gamut monitor profiles): lcms2 transform to the OS-provided monitor ICC.

---

## 5. JPEG decode/encode speed

| Crate | Role | Language/deps | Threads | Status |
|---|---|---|---|---|
| [`jpeg-decoder`](https://github.com/image-rs/jpeg-decoder) 0.3 | decode (+lossless) | Pure Rust | optional rayon | **Maintenance mode** — "image-rs seeks to move over to the zune-jpeg crate" ([README](https://github.com/image-rs/jpeg-decoder)) |
| [`zune-jpeg`](https://github.com/etemesi254/zune-image/tree/dev/crates/zune-jpeg) 0.5.15 (2026-03; 0.5.16-rc1 2026-08) | decode | Pure Rust, SSE/AVX intrinsics | single-threaded | Active; 12.4M dl/month; **default JPEG decoder of `image` 0.25** ([lib.rs](https://lib.rs/crates/zune-jpeg), [image deps](https://lib.rs/crates/image)) |
| [`turbojpeg`](https://lib.rs/crates/turbojpeg) 1.5.1 (2026-07) | decode + encode + lossless transforms | C (libjpeg-turbo, needs CMake + C compiler + NASM) | single-threaded API | Active; 247k dl/month; Unlicense/MIT |

- **Speed:** zune-jpeg's own benchmarks (7680×4320 baseline image, AMD Ryzen 5 4500u, weekly criterion runs) claim parity "within ±10 ms of libjpeg-turbo" ([README](https://github.com/etemesi254/zune-image/blob/dev/crates/zune-jpeg/README.md), [Benches.md](https://github.com/etemesi254/zune-image/blob/dev/crates/zune-jpeg/Benches.md), [live criterion report](https://etemesi254.github.io/assets/criterion/report/index.html)). libjpeg-turbo (hand-written SIMD asm) remains the reference; the turbojpeg crate exposes it if we want it ([lib.rs](https://lib.rs/crates/turbojpeg)).
- **Feature that matters to us:** zune-jpeg has first-class **incremental decoding** — stable-row checkpoints, recoverable EOF retries, and progressive-scan preview APIs designed for "decode while displaying" ([README, "Incremental input"](https://github.com/etemesi254/zune-image/blob/dev/crates/zune-jpeg/README.md)). Nice for paint-while-loading galleries.
- **Encode:** zune-jpeg is **decode-only**; for JPEG output use `image`'s encoder or turbojpeg (fastest, plus lossless crop/rotate for sidecar workflows).
- **Verdict:** zune-jpeg — it's already under `image`, so the preview decode costs us **zero extra dependencies**. Add turbojpeg only if export-side JPEG encoding becomes a measured bottleneck.

---

## 6. Display via GPUI

- `gpui` is published on crates.io (0.2.2, Apache-2.0, owned by zed-industries; repo [zed](https://github.com/zed-industries/zed), site [gpui.rs](https://gpui.rs)) and already depends on **`image` ^0.25.1** ([docs.rs dependency list](https://docs.rs/gpui/latest/gpui/struct.RenderImage.html)).
- `gpui::RenderImage` is "a cached and processed image, **in BGRA format**" and is constructed from `SmallVec<[image::animation::Frame; 1]>` ([docs.rs](https://docs.rs/gpui/latest/gpui/struct.RenderImage.html)). So our final quick-edit output should be an 8-bit **BGRA** buffer wrapped in an `image::Frame` (swizzle R/B at the end of the pixel pass, or decode straight to BGRA — zune-jpeg supports BGR/BGRA output ([README, Features](https://github.com/etemesi254/zune-image/blob/dev/crates/zune-jpeg/README.md))).

---

## 7. Recommended minimal pipeline

### Primary path (in-process, pure Rust)

```
raw file (CR3/NEF/ARW/…)
  │
  ▼ 1. rawler::analyze::extract_preview_pixels(path)   [LGPL-2.1 — see license note]
  │     → image::DynamicImage (embedded full-res JPEG, decoded via zune-jpeg)
  │     (internal fallback: thumbnail → preview → rawler raw→sRGB render)
  ▼ 2. Normalize once: → RGB8 (or f32 linear working copy for correct math)
  │     + optional ICC transform if preview carries one (lcms2 or moxcms)
  ▼ 3. Quick-edit pass (custom, rayon par_chunks_mut):
  │     WB = per-channel gain · exposure = gain ·
  │     black/white point + highlights/shadows = 256-entry LUTs/curves
  │     run on ~2 MP display proxy during drags; full 24 MP on release (~1–15 ms)
  ▼ 4. fast_image_resize (rayon feature) for proxy/fit scaling (SIMD, 3–13 ms class)
  ▼ 5. Output BGRA8 → image::Frame → gpui::RenderImage → img element
  ▼ 6. JPEG decode of user's plain JPEGs: image 0.25 (zune-jpeg) directly;
       consider zune-jpeg incremental mode for gallery paint-while-load
```

Crates: `rawler`, `image` (0.25), `fast_image_resize`, `rayon`, `lcms2` *or* `moxcms`, `gpui`. Everything else (imageproc, turbojpeg) is opt-in later.

### Fallback path (subprocess, no rawler)

- **Preview extraction:** `exiftool -b -JpgFromRaw -w .jpg IMG.CR3` (or `-PreviewImage`) for any camera rawler can't parse yet ([ExifTool docs](https://exiftool.org/ExifTool_Pod.html)).
- **Raw development / export / high-quality rendering:** `darktable-cli input.raw output.jpg --core ...` — CLI interface to darktable's full pipeline, documented in the darktable man page ([darktable-cli.pod](https://github.com/darktable-org/darktable/blob/master/doc/man/darktable-cli.pod)). Keep the app's edit model (exposure/WB/…) translatable to an XMP sidecar so darktable-cli rendering matches on-screen intent; that mapping needs its own validation pass later.
- **Architecture note:** keep both paths behind one `PreviewProvider` trait so LGPL rawler can also be dropped into a helper process if licensing ever demands it.

---

## Sources

- rawler: <https://lib.rs/crates/rawler> · <https://docs.rs/rawler/latest/rawler/> · <https://github.com/dnglab/dnglab> · <https://github.com/dnglab/dnglab/blob/main/SUPPORTED_CAMERAS.md> · <https://github.com/dnglab/dnglab/blob/main/CR3_STATE.md> · <https://github.com/dnglab/dnglab/blob/main/rawler/src/analyze.rs> · <https://github.com/dnglab/dnglab/blob/main/rawler/src/decoders/mod.rs> · <https://github.com/dnglab/dnglab/blob/main/rawler/src/lib.rs>
- rawloader: <https://github.com/pedrocr/rawloader> · <https://lib.rs/crates/rawloader> · <https://crates.io/api/v1/crates/rawloader/versions>
- libraw bindings: <https://lib.rs/crates/libraw> · <https://lib.rs/crates/libraw-rs> · <https://lib.rs/crates/rsraw> · <https://lib.rs/crates/rawlib> · <https://lib.rs/crates/raw_preview_rs>
- processing: <https://lib.rs/crates/image> · <https://github.com/Cykooz/fast_image_resize> · <https://github.com/Cykooz/fast_image_resize/blob/main/benchmarks-x86_64.md> · <https://github.com/image-rs/imageproc> · <https://lib.rs/crates/rayon>
- color: <https://lib.rs/crates/lcms2> · <http://www.littlecms.com> · <https://lib.rs/crates/moxcms>
- JPEG: <https://github.com/image-rs/jpeg-decoder> · <https://lib.rs/crates/zune-jpeg> · <https://github.com/etemesi254/zune-image/tree/dev/crates/zune-jpeg> (README, Benches.md) · <https://etemesi254.github.io/assets/criterion/report/index.html> · <https://lib.rs/crates/turbojpeg>
- GPUI: <https://docs.rs/gpui/latest/gpui/struct.RenderImage.html> · <https://gpui.rs>
- fallback tooling: <https://exiftool.org/ExifTool_Pod.html> · <https://github.com/darktable-org/darktable/blob/master/doc/man/darktable-cli.pod>
