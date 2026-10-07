# Lightroom edit math & round-trip: research findings

> **Superseded on engine choice (2026-10-07).** The darktable-cli export path and the
> own-tone-math preview plan are retired; `PLAN.md` now uses LightCraft `DevelopSettings` +
> `lightcraft-pipeline` for preview and export. See `PLAN.md` and
> `docs/research/retouch-selection-faceblur.md`. The PV2012 slider semantics, `crs:` field
> mapping and exiftool round-trip findings below are still current and are the basis for LR
> preset import and the sidecar writer.

**Date:** 2026-08-16
**Scope:** Slider semantics of Lightroom's PV2012 Basic panel; open-source reimplementations/converter mappings; darktable module equivalents and what is programmatically settable; XMP (`crs:`) round-trip to Lightroom Classic via exiftool; ProcessVersion gotchas.
**Audience:** the Rust/GPUI cross-platform photo editor (live preview must feel like LR's Basic panel; crops must round-trip; export via `darktable-cli`).

> Method note: every claim below cites its source. "Primary" = source code, official docs/specs, or raw files written by the tool in question. The exiftool forum (normally a good source for `crs:` write behavior) was offline during research ("ExifTool forum currently unavailable").

---

## 1. Lightroom PV2012 slider semantics

### 1.1 What Adobe officially documents

Adobe's user guide defines the Basic panel controls (per-process-version) as:

| Slider | Official definition |
| --- | --- |
| Exposure (All PV) | "Sets the overall image brightness… values are in increments equivalent to aperture values (f‑stops) on your camera. An adjustment of +1.00 is similar to…" one stop. |
| Highlights (PV2012) | "Adjusts bright image areas. Drag to the left to darken highlights and recover 'blown out' highlight details." |
| Shadows (PV2012) | "Adjusts dark image areas." |
| Whites (PV2012) | "Adjusts white clipping. Drag to the left to reduce clipping in highlights. Drag to the right to increase highlight clipping." |
| Blacks (PV2012) | "Adjusts black clipping. Drag to the left to increase black clipping (map more shadows to pure black)." |
| Temp/Tint | White-balance fine-tune sliders in the Basic panel. |

Source: *Work with image tone and color* (Lightroom Classic user guide), https://helpx.adobe.com/lightroom-classic/desktop/process-and-develop-photos/image-tone-color.html

The *Develop module options* page adds that PV2012 "offers new tone controls and new tone-mapping algorithms for high-contrast images", introduced with Lightroom 4, and lists the process lineage: PV2003 (LR1–2) → PV2010 (LR3) → PV2012 (LR4+) → "Process Version 5" (improved negative dehaze, low-light shadows) → "Process Version 6" (current; reduces banding in Color/B&W Mixer).
Source: https://helpx.adobe.com/lightroom-classic/desktop/process-and-develop-photos/develop-module-options.html

**Key takeaway:** In PV2012, *Whites* and *Blacks* are clipping-point controls (they move the white/black points), while *Highlights* and *Shadows* are adaptive tone-compression controls. Adobe publishes **no pixel-level math**; the exact tone-mapping/luminance-mask internals are proprietary and no complete public reimplementation exists (see §1.3).

### 1.2 Slider ranges and XMP representation (observational, from real LR-written files)

Real sidecars written by Lightroom (used as fixtures in open-source projects):

- `crs:Exposure2012="-0.10"` (real, steps of 0.05; UI range ±5 EV) — file: https://github.com/evanoberholster/imagemeta/blob/master/meta/xmp/test/canon_eos_r6_cr3_lightroom_custom_adjustments.xmp
- `crs:Highlights2012="-74"`, `crs:Shadows2012="+31"`, `crs:Whites2012="+12"`, `crs:Blacks2012="-36"` (all within −100..+100) — same file.
- `crs:WhiteBalance="Custom"`, `crs:Temperature="4350"` (Kelvin), `crs:Tint="+3"` — same file. Tint is a small signed integer; third-party importers treat its full scale as ±150 (see §1.3).
- `crs:ToneCurvePV2012` is an RDF `Seq` of `"<x>, <y>"` points on a 0–255 scale, e.g. `0,3` / `38,26` / `186,178` / `255,252`; per-channel curves in `ToneCurvePV2012Red/Green/Blue`; preset name in `crs:ToneCurveName2012` ∈ {`Linear`, `Medium Contrast`, `Strong Contrast`, `Custom`} — same file; the enum is confirmed by darktable's parser (src/develop/lightroom.c:612-622, https://github.com/darktable-org/darktable/blob/master/src/develop/lightroom.c).
- Older LR files (`ProcessVersion 6.7`, LR CC 2015-era): `crs:Blacks2012>-10`, `crs:Exposure2012>+0.12`, `crs:Highlights2012>-83`, `crs:Shadows2012>+64`, `crs:Whites2012>+1` — https://github.com/bpatrik/pigallery2/blob/master/demo/images/IMG_5910.jpg.xmp

Exiftool's tag table (writable group `XMP-crs`) confirms types: `Exposure2012` real, `Highlights2012/Shadows2012/Whites2012/Blacks2012/Contrast2012` integer, `ToneCurvePV2012*` string list (`Seq`), `ProcessVersion` string.
Sources: https://exiftool.sourceforge.net/TagNames/XMP.html#crs and the exiftool source of that table, https://github.com/exiftool/exiftool/blob/master/lib/Image/ExifTool/XMP.pm (crs block around lines 1430–1650).

The PPR10K benchmark (learning-based retouching on 7,581 RAWs) uses exactly these XMP develop settings (`Temperature, Tint, Exposure2012, Highlights2012, Shadows2012, Whites2012, Blacks2012, …`) as ground-truth edits, and drives the *real* Lightroom Classic CLI to render them rather than reimplementing the engine — a strong signal that the slider→pixel math is not publicly reproducible.
Sources: https://github.com/csjliang/PPR10K (paper: arXiv:2008.12310); slider list at https://github.com/csjliang/PPR10K/blob/master/utils/data_augment_get_target_xmp.py

### 1.3 Open-source implementations that approximate/reimplement the sliders

**No project reimplements PV2012 faithfully.** What exists are importers with hand-tuned approximations:

1. **darktable's built-in Lightroom XMP importer** (`src/develop/lightroom.c`, shipped feature; docs: https://docs.darktable.org/usermanual/development/en/overview/sidecar-files/sidecar-import/):
   - `Exposure2012` → `exposure` module's `exposure` param, **value passed through directly as EV** (lightroom.c:529-537).
   - `Blacks2012` → `exposure` module's `black` param via a **piecewise-linear lookup table**: `{-100 → 0.020, -50 → 0.005, 0 → 0, +50 → -0.005, +100 → -0.010}` (lightroom.c:279-285).
   - Crop: `CropTop/Right/Left/Bottom` (normalized 0–1) + `CropAngle` (**negated** on import) + `HasCrop` → deprecated crop&rotate module (lightroom.c:494-503, 516-519).
   - Parametric curve, ToneCurveName2012, HSL→color zones, vignette, grain, spot removal are imported. **Highlights2012, Shadows2012, Whites2012, Temperature, Tint are NOT imported at all** (no handlers; matches the documented module list).
2. **RapidRAW** (Rust/Tauri raw editor) imports LR `.xmp` presets (`src-tauri/src/preset_converter.rs`, https://github.com/CyberTimon/RapidRAW/blob/main/src-tauri/src/preset_converter.rs):
   - `Exposure2012/Highlights2012/Whites2012/Blacks2012/Contrast2012/…` copied 1:1 into its own slider model;
   - `Shadows2012` scaled **×1.5** (clamped to 100);
   - `Temperature` converted to a mired delta vs. `AsShotTemperature` (default 5500 K), normalized by **±150 mired** → ±100 internal slider;
   - `Tint` scaled **/150** → ±100;
   - `ToneCurvePV2012` points imported 0–255, with a special dampening heuristic for shadow lifts below x=64.
3. **ImageToolbox** (Android, Kotlin) parses `ToneCurve(PV2012)[Red/Green/Blue]` from both `.xmp` and `.lrtemplate` presets and maps them onto a generic cubic-spline curve object: https://github.com/T8RIN/ImageToolbox/blob/master/lib/curves/src/main/java/com/t8rin/curves/CurvesPresetCodec.kt
4. **Magic Lantern's deflicker module** *writes* a minimal LR-compatible XMP (`ProcessVersion="6.7"`, `Exposure2012`, `ToneCurve` 0,0/255,255) so Lightroom picks up its exposure fixes: https://github.com/reticulatedpines/magiclantern_simplified/blob/dev/modules/deflick/deflick.c (proof that LR accepts third-party-written `crs:` XMP).
5. The `.lrtemplate` ecosystem (legacy LR preset JSON): parsers/converters exist — `LRtemplate2XMPPresets` (https://github.com/yuzukumo/LRtemplate2XMPPresets), `LRtemplate2XMPFormatter` (https://github.com/lavalava45/LRtemplate2XMPFormatter), `lrjson` (https://github.com/shadawck/lrjson) — but they target LR's own XMP, not darktable.

**Conclusion for §1:** treat LR sliders as the *UI contract* (EV exposure, ±100 highlights/shadows/whites/blacks, K/tint WB, 0–255 tone curve), and treat the underlying math as ours to choose, benchmarked against LR "feel" (whites/blacks = endpoint/clipping controls; highlights/shadows = smooth, mask-like tonal relighting).

---

## 2. LR→darktable preset converters

The specific names in the task brief were checked against GitHub repository search (unauthenticated API):

- `lr2dt` → **0 repositories** (https://api.github.com/search/repositories?q=lr2dt)
- `lightroom2darktable` → **0 repositories** (https://api.github.com/search/repositories?q=lightroom2darktable)
- `lrtool` → no Lightroom-related hit in the top 20 (all unrelated projects; https://api.github.com/search/repositories?q=lrtool)
- Searches for "preset converter darktable", "lrtemplate darktable", "darktable style lightroom xmp" → no active converter repos (one adjacent result: `pfrederiksen/negclone`, film-profile fingerprinting, not a slider converter).

**These tools do not exist (anymore) on GitHub under those names.** The LR→darktable conversion landscape is in fact:

| Tool | Status | Mapping |
| --- | --- | --- |
| darktable built-in LR importer (`src/develop/lightroom.c`) | Shipped, current | See §1.3 item 1: exposure EV passthrough; Blacks lookup; crop; tone curve name/parametric; HSL; vignette; grain; spot removal. No highlights/shadows/whites/WB. |
| RapidRAW preset importer (Rust) | Active (2025) | LR XMP preset → RapidRAW's own engine (not darktable). §1.3 item 2. |
| ImageToolbox curves importer (Kotlin) | Active | Curves only. §1.3 item 3. |
| presetconverter.com (commercial web service) | Site is a JS shell; could not verify content during research — treat as unverified | claims LR→darktable style conversion; no citable mapping table. |
| Community guidance (pixls.us) | Active | "Lightroom 'Basic' module equivalents" thread: Exposure→exposure; Highlights/Shadows/Whites/Blacks→tone equalizer; WB→color calibration (https://discuss.pixls.us/t/42551). "Translate Lightroom instructions to Darktable": consensus is there is no faithful 1:1 translation (https://discuss.pixls.us/t/34943). |

darktable itself ships a Capture One migration SQL (`tools/migrate_capture_one_pro.sql`) but **no** LR preset converter, and `tools/dtstyle_to_xmp.py` converts darktable `.dtstyle`→XMP (the reverse direction). Source: repo tree https://github.com/darktable-org/darktable/tree/master/tools

---

## 3. darktable module equivalents & programmatic control

### 3.1 Module semantics (docs + source structs)

- **exposure** (`src/iop/exposure.c`): scene-referred, linear. Params (introspection v7, exposure.c:65-74): `mode`, `black` (−1..+1, "black level correction"), `exposure` (−18..+18 EV, default 0; GUI slider narrower but right-click accepts ±18), deflicker percentile/target, compensate-EXIF-bias flags. Doc: https://docs.darktable.org/usermanual/5.0/en/module-reference/processing-modules/exposure/
- **tone equalizer** (`src/iop/toneequal.c`): 9 luminance bands centered at −8…0 EV (toneequal.c:140-157), each ±2 EV gain, applied through a guided-filter luminance mask with detail preservation (params incl. `blending`, `feathering`, `details`, toneequal.c:170-190). The module was created in 2019 explicitly because users wanted "the basic blacks/shadows/highlights/whites cursors of Lightroom" (origin thread by its author: https://discuss.pixls.us/t/10678). Param names are literally `blacks…speculars` with GUI labels "blacks/deep shadows/shadows/light shadows/mid-tones/dark highlights/highlights/whites/speculars". Doc: https://docs.darktable.org/usermanual/5.0/en/module-reference/processing-modules/tone-equalizer/
- **white balance** (`src/iop/temperature.c`): raw-RGB channel gains. Params (introspection v4, temperature.c:67-74) are `red/green/blue/various` coefficients + `preset` — the K/tint sliders are GUI-level and are converted to/from gains via daylight/blackbody spectral models (`_spd_daylight`, `_temperature_tint_to_XYZ`, temperature.c:320, 395). Doc (temp/tint, presets, "as shot to reference"): https://docs.darktable.org/usermanual/5.0/en/module-reference/processing-modules/white-balance/
- **color calibration** (`src/iop/channelmixerrgb.c`): CAT-based; offers temperature(K)/tint or CIE Luv hue/chroma for the illuminant; recommended modern WB tool (white-balance doc above links it as "more modern and flexible"). Doc: https://docs.darktable.org/usermanual/5.0/en/module-reference/processing-modules/color-calibration/
- **color balance rgb** (`src/iop/colorbalance.c`): lift/gamma/gain + saturation/contrast around an 18% fulcrum (colorbalance.c:97-105). Doc: https://docs.darktable.org/usermanual/5.0/en/module-reference/processing-modules/color-balance/
- **Shipped "demo" styles:** darktable does not ship any LR-demo styles/presets; there is no `add_preset` in toneequal.c and the styles doc references user-created styles only. (Styles docs: https://docs.darktable.org/usermanual/development/en/module-reference/utility-modules/lighttable/styles/)

### 3.2 What's programmatically settable via sidecar XMP (dt namespace)

- Sidecar structure: `Xmp.darktable.history[N]` `rdf:Seq` items with `darktable:operation`, `darktable:params`, `darktable:modversion`, `darktable:enabled`, `darktable:blendop_params`, etc., plus `darktable:xmp_version`, `darktable:history_end`. Namespace `http://darktable.sf.net/`. Officially *written* by darktable; reading external/generic XMP is supported ("load sidecar file", drag&drop). Docs: https://docs.darktable.org/usermanual/development/en/overview/sidecar-files/sidecar/
  - Caveat from same doc: once a file is in darktable's library, **the DB takes precedence over later external XMP edits** unless you re-import or enable the update check at startup.
- `darktable:params` encoding (src/common/exif.cc:3246-3299, written at exif.cc:4918): the module's C params struct serialized as either **`"gz" + two-digit factor + base64(zlib-compressed struct)`** (when ≥100 bytes and compression helps) or **lowercase hex** of the raw struct bytes. `modversion` selects the struct layout (e.g. exposure v7, temperature v4, toneequalizer v2). This is *technically writable* by external code but **fragile across darktable versions** — you must pack the exact struct for the target module version.
- **The robust programmatic path is `darktable-cli`** (https://docs.darktable.org/usermanual/development/en/special-topics/program-invocation/darktable-cli/):
  ```
  darktable-cli <input> [<xmp file>] <output> [--width W] [--height H] \
      [--style <name>] [--style-overwrite] [--out-ext jpeg|tiff|...] \
      [--hq 1] [--core --conf plugins/imageio/format/jpeg/quality=95 ...]
  ```
  - the optional **`<xmp file>` argument takes a sidecar containing a history stack** and applies it during export — i.e. you can generate a dt-XMP (or use `--style` with a `.dtstyle` installed into the config dir) and export headlessly;
  - `--style` requires `--core --configdir …` and loading `data.db` (`--apply-custom-presets 1`, default);
  - `.dtstyle` format is the same plugin list in plain XML (`<style><plugin><operation>exposure</operation><op_params>…`), per the shipped converter `tools/dtstyle_to_xmp.py` (https://github.com/darktable-org/darktable/blob/master/tools/dtstyle_to_xmp.py).
- **LR XMP and darktable:** darktable reads `<basename>.xmp` (LR naming) at import — but only tags/color labels/ratings/GPS plus the limited develop migration list in §1.3; it never writes that file (docs: https://docs.darktable.org/usermanual/development/en/overview/sidecar-files/sidecar-import/).

---

## 4. XMP round-trip to Lightroom (exiftool-written sidecars)

### 4.1 What Lightroom reads/writes

- Lightroom stores develop settings in XMP (sidecar for RAWs, embedded for DNG/JPEG/TIFF/PSD) and syncs on **Metadata → Read Metadata From File / Save Metadata To File** (and optionally auto-write; LrC 13.0 changed auto-write frequency). Official doc: https://helpx.adobe.com/lightroom-classic/desktop/organize-photos-in-lightroom-classic/metadata-basics-actions.html
- Third-party XMP in `crs:` is accepted — proven by Magic Lantern writing exposure XMP that LR honors (§1.3 item 4) and by the entire preset ecosystem (`.xmp` presets are just `crs:` descriptions with `crs:Name`).

### 4.2 The `crs:` tag set to write (all writable in exiftool group `XMP-crs`)

From exiftool's tag table (https://exiftool.sourceforge.net/TagNames/XMP.html#crs) and its source (https://github.com/exiftool/exiftool/blob/master/lib/Image/ExifTool/XMP.pm, lines ~1430–1650):

| Purpose | Tags (writable) | Notes |
| --- | --- | --- |
| Marker | `HasSettings` (boolean), `AlreadyApplied` (boolean), `RawFileName` (string) | Real LR files set `HasSettings="True"`; `AlreadyApplied="False"` on raws / `"True"` on rendered JPEGs. Verified in https://github.com/evanoberholster/imagemeta/blob/master/meta/xmp/test/canon_eos_r6_cr3_lightroom_custom_adjustments.xmp and pigallery2 sample. |
| Process | `ProcessVersion` (string) | See §5. |
| WB | `WhiteBalance` (string: As Shot/Custom/Daylight/…), `Temperature` (integer, Kelvin), `Tint` (integer, ~±150) | **exiftool naming gotcha:** exiftool exposes `crs:Temperature` as tag name **`ColorTemperature`** ("tag ID is 'Temperature'", XMP.pm:1460) — write `-XMP-crs:ColorTemperature=5600` or configure an alias. Presets may instead use `IncrementalTemperature`/`IncrementalTint` (relative to As Shot; XMP.pm:1503-1504). |
| Basic PV2012 | `Exposure2012` (real, EV ±5), `Contrast2012`, `Highlights2012`, `Shadows2012`, `Whites2012`, `Blacks2012` (integers −100..+100) | All confirmed writable (XMP.pm:1625-1631). |
| Curves | `ToneCurvePV2012[Red/Green/Blue]` (Seq of "x, y", 0–255), `ToneCurveName2012` (Linear/Medium Contrast/Strong Contrast/Custom), `ParametricShadows/Darks/Lights/Highlights` + splits | XMP.pm:1638-1641; format verified in real files (§1.2). |
| Crop | `HasCrop` (boolean), `CropLeft/CropTop/CropRight/CropBottom` (real, **normalized 0–1 on the original image**), `CropAngle` (real, degrees), `CropWidth/CropHeight` (real, in `CropUnits`), `CropUnits` (0=pixels/1=inches/2=cm) | XMP.pm:1430-1448. Semantics cross-verified by two independent readers: darktable (lightroom.c:494-503 — imports as normalized box, **angle negated**) and nomacs (DkMetaData.cpp:1466-1524 reads them as percentages). Real example: `CropLeft=0, CropTop=0.231776, CropRight=0.768224, CropBottom=1, CropAngle=0, HasCrop=True` (https://github.com/bpatrik/pigallery2/blob/master/demo/images/IMG_5910.jpg.xmp). |
| Catalog | `AlreadyInCatalog` | **Not** in exiftool's crs table and not observed in the examined LR files — don't rely on it; writing undeclared tags needs an exiftool config file. |

Example write command shape (sidecar for `IMG_0001.CR3`):

```
exiftool -config crs.cfg IMG_0001.CR3.xmp \
  -XMP-crs:ProcessVersion="11.0" -XMP-crs:HasSettings=True \
  -XMP-crs:WhiteBalance="Custom" -XMP-crs:ColorTemperature=5600 -XMP-crs:Tint=+5 \
  -XMP-crs:Exposure2012="+0.35" -XMP-crs:Highlights2012=-40 -XMP-crs:Shadows2012=+25 \
  -XMP-crs:Whites2012=+10 -XMP-crs:Blacks2012=-8 \
  -XMP-crs:HasCrop=True -XMP-crs:CropLeft=0.05 -XMP-crs:CropTop=0.1 \
  -XMP-crs:CropRight=0.95 -XMP-crs:CropBottom=0.9 -XMP-crs:CropAngle=0 \
  -XMP-crs:RawFileName="IMG_0001.CR3"
```

(In practice all listed tags except `AlreadyInCatalog` are predefined and writable; `-config` is only needed for custom/unknown tags.)

### 4.3 Round-trip gotchas

1. **Sidecar filename**: Lightroom's sidecar naming is `<basename>.xmp`; darktable's is `<basename>.<ext>.xmp` (docs §3.2 / §4 sources). If both tools touch the same folder, darktable will *read* the LR name but write its own.
2. **Crop semantics**: `CropLeft/Top/Right/Bottom` are normalized to the **original, unrotated** image; the crop rectangle is rotated by `CropAngle` about its center. darktable's importer negates the angle (sign-convention difference — check against LR behavior when we implement). Always write `HasCrop="True"` plus the four edges (+ angle); omit or `False` for uncropped.
3. **Whites/Blacks/Highlights/Shadows are PV2012-only** — the unsuffixed `Exposure/Highlight*/Shadows/Brightness` tags belong to PV2003/PV2010 recipes and are ignored (or misread) when `ProcessVersion` says 2012+. Always write the `2012` suffixed set with `ProcessVersion` ≥ 6.7-family.
4. **Temperature for JPEGs/TIFFs** behaves differently than for raws (LR applies WB differently on rendered files); safest round-trip is raw + sidecar.
5. LR only picks settings up from a file/sidecar it considers current — on import, or via *Read Metadata from File*; it will not watch files. (Workflow doc above.)
6. The tone curve must contain at least the implicit endpoints; LR writes explicit `0,0`/`255,255` anchors — include them.

---

## 5. ProcessVersion values and gotchas

UI name ↔ XMP string mapping (Adobe documents only the UI names at https://helpx.adobe.com/lightroom-classic/desktop/process-and-develop-photos/develop-module-options.html; the numeric strings below are observational from real files):

| `crs:ProcessVersion` | UI process | Evidence |
| --- | --- | --- |
| *(absent)* / early strings | 2003 (LR1–2) | not observed in corpus |
| `5.7` | 2010-era recipe, edited later in LR6 | amberbit fixture (`Version="6.1"` + `ProcessVersion="5.7"`): https://github.com/amberbit/xmp/blob/master/spec/fixtures/xmp.xml |
| `6.7` | **PV2012, original release (ACR 6.7 / LR4, 2012)** | pigallery LR file (`Version 9.1`, `ProcessVersion 6.7`); also what Magic Lantern writes: https://github.com/reticulatedpines/magiclantern_simplified/blob/dev/modules/deflick/deflick.c |
| `10.0` | PV2012 revision (ACR 10 / LR 7, 2017) | LibrePhotos fixture (`Version="10.4"`, `ProcessVersion="10.0"`): https://github.com/LibrePhotos/librephotos/blob/dev/apps/backend/api/tests/fixtures/niaz.xmp |
| `11.0` | PV2012 revision (ACR 11 / LR 8, 2018+) — **the de-facto "PV2012" value modern presets use** | photoprism sample (https://github.com/photoprism/photoprism/blob/develop/assets/samples/canon_eos_6d.xmp), ImageToolbox preset writer, countless presets |
| `15.4` | **"Process Version 6" (current, LR Classic 14/17-era)** | real LR 17.0-written file (imagemeta, §1.2) and generated LR 17.2 presets (https://github.com/FUTC-Coding/preset-generator/blob/main/main.py) |

Gotchas:

- **"PV2012b" does not exist as a public concept.** PV2012 silently received revisions over the years (`6.7` → `10.0` → `11.0` …) without Adobe renaming it in the UI; the UI only gained new *named* versions with "Version 5" and "Version 6" (2024+). Do not invent `11.1`/`2012b` values.
- `crs:Version` ≠ `crs:ProcessVersion`: `Version` is the writing app version (e.g. `17.0`, `10.4`); `ProcessVersion` is the required engine revision. Copying a modern `Version` with an old `ProcessVersion` is normal (see amberbit file).
- **Recommended write values:** `ProcessVersion="11.0"` for maximum compatibility with any LR from ~2019 that still treats it as current-family PV2012, or `15.4` to match current LR Classic behavior. Both are read fine; `11.0` is what third-party preset generators ship today.
- Exiftool does not validate `ProcessVersion` values (plain writable string, XMP.pm:1602) — typos will silently make LR fall back to defaults for the 2012 sliders.

---

## Recommendation

### (a) Live preview in-app (GPUI)

Implement the LR **UI contract** with our own scene-referred math, not a fake-PV2012 clone:

- `Exposure` (EV ±5): pure linear gain `2^EV` on scene-referred RGB — matches both Adobe's f-stop definition and darktable's exposure module (§1.1, §3.1).
- `Temp (K) / Tint`: convert K → chromaticity via a daylight/blackbody model and apply as channel gains, exactly like darktable's temperature module does (temperature.c:320,395). Store absolute Kelvin + integer tint in our state so XMP write-back is lossless.
- `Highlights/Shadows` (−100..+100): a masked, band-limited exposure adjustment — i.e. a tone-equalizer-style operator (guided/edge-aware luminance mask, gain limited to ~±1–2 EV, soft band centers rather than hard thresholds). This is both the community-consensus LR analogue and the operator darktable built *for* LR refugees (§3.1, pixls 42551/10678).
- `Whites/Blacks` (−100..+100): white/black **clipping-point** controls (soft endpoint shifts before display transform), per Adobe's own definitions (§1.1); a small basecurve/filmic-style endpoint remap is closer to LR than routing them through the mask.
- Tone curve (0–255 PV2012 format) as the final display-referred stage if we expose it.

### (b) Final export via darktable

- Author a **darktable history XMP programmatically** (small, fixed module set: `temperature` (as-shot-to-target gains), `exposure` (EV + black), `toneequal` (9 bands ±2 EV), optional `colorbalance`/`tonecurve`) and run `darktable-cli <raw> <our.xmp> <out.jpg>` (§3.2). Pin the darktable version we target so `modversion` + packed params stay valid; the params blob is `gz+base64`/hex of the C structs (exif.cc:3246).
- Alternatively generate a `.dtstyle` and use `--style`, which avoids per-image XMP plumbing but needs the style installed in a config dir.
- Map: EV→`exposure.exposure`; Blacks slider→`exposure.black` (start from darktable's own `lr2dt_blacks` table, lightroom.c:279); Highlights/Shadows/Whites→`toneequal` bands (`highlights`, `shadows`/`midtones`, `whites`) with our slider→EV curve; WB→`temperature` coeffs (or `channelmixerraw` CAT temp). Expect "same feel", not bit-identical output — every source agrees exact LR equivalence is unattainable (§1, §2, pixls 34943).
- **Keep LR XMP and darktable XMP as two separate artifacts** written from one internal edit state: `crs:` sidecar (exiftool, per §4.2, `ProcessVersion=11.0`, `HasCrop`+`Crop{L,T,R,B}`+`CropAngle`) for Lightroom round-trip; dt history XMP/style for export. Never let darktable's own sidecar naming/format fight LR's (§4.3.1).

---

## Source index

1. exiftool XMP-crs tag table — https://exiftool.sourceforge.net/TagNames/XMP.html#crs
2. exiftool XMP.pm (tag definitions/writability) — https://github.com/exiftool/exiftool/blob/master/lib/Image/ExifTool/XMP.pm
3. Adobe: Develop module options (process versions) — https://helpx.adobe.com/lightroom-classic/desktop/process-and-develop-photos/develop-module-options.html
4. Adobe: Work with image tone and color (Basic panel semantics) — https://helpx.adobe.com/lightroom-classic/desktop/process-and-develop-photos/image-tone-color.html
5. Adobe: Metadata basics and actions (XMP read/save) — https://helpx.adobe.com/lightroom-classic/desktop/organize-photos-in-lightroom-classic/metadata-basics-actions.html
6. darktable LR importer — https://github.com/darktable-org/darktable/blob/master/src/develop/lightroom.c
7. darktable docs: importing foreign/LR sidecars — https://docs.darktable.org/usermanual/development/en/overview/sidecar-files/sidecar-import/
8. darktable docs: sidecar files — https://docs.darktable.org/usermanual/development/en/overview/sidecar-files/sidecar/
9. darktable docs: darktable-cli — https://docs.darktable.org/usermanual/development/en/special-topics/program-invocation/darktable-cli/
10. darktable docs: exposure / tone equalizer / white balance / color calibration / color balance — https://docs.darktable.org/usermanual/5.0/en/module-reference/processing-modules/{exposure,tone-equalizer,white-balance,color-calibration,color-balance}/
11. darktable sources: exposure.c, toneequal.c, temperature.c, colorbalance.c, common/exif.cc, tools/dtstyle_to_xmp.py — https://github.com/darktable-org/darktable/tree/master
12. pixls.us: tone equalizer origin thread — https://discuss.pixls.us/t/10678
13. pixls.us: Lightroom "Basic" module equivalents — https://discuss.pixls.us/t/42551
14. pixls.us: Translate Lightroom instructions to Darktable — https://discuss.pixls.us/t/34943
15. RapidRAW preset converter (Rust) — https://github.com/CyberTimon/RapidRAW/blob/main/src-tauri/src/preset_converter.rs
16. ImageToolbox curves preset codec (Kotlin) — https://github.com/T8RIN/ImageToolbox/blob/master/lib/curves/src/main/java/com/t8rin/curves/CurvesPresetCodec.kt
17. Real LR-written XMP fixtures — imagemeta `canon_eos_r6_cr3_lightroom_custom_adjustments.xmp`, pigallery2 `IMG_5910.jpg.xmp`, photoprism `canon_eos_6d.xmp`, LibrePhotos `niaz.xmp`, amberbit `spec/fixtures/xmp.xml` (URLs in §1.2/§5)
18. Magic Lantern deflicker (writes LR XMP) — https://github.com/reticulatedpines/magiclantern_simplified/blob/dev/modules/deflick/deflick.c
19. PPR10K (LR XMP as retouch ground truth; drives LR CLI) — https://github.com/csjliang/PPR10K
20. GitHub repo searches (negative results for lr2dt / lightroom2darktable) — https://api.github.com/search/repositories?q=lr2dt , …?q=lightroom2darktable
