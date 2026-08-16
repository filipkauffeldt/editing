# Presets, metadata, code replacements, FTP & preview extraction — research findings

Date: 2026-08-16. Method: primary sources only — crates.io API, GitHub repos/API, official project docs (Adobe, Camera Bits, rclone, ExifTool). Every claim links to its source. This doc complements [gpui-and-components.md](./gpui-and-components.md).

---

## 1. Lightroom preset formats

### Current format: XMP with the `crs:` namespace

Since Lightroom Classic 7.3 (April 2018), Develop presets are stored as `.xmp` files rather than `.lrtemplate` ([Adobe LrC what's-new 2018](https://helpx.adobe.com/lightroom-classic/help/whats-new/2018.html); the legacy→XMP split is also described by community converters such as [yuzukumo/LRtemplate2XMPPresets](https://github.com/yuzukumo/LRtemplate2XMPPresets) — "Convert legacy Lightroom .lrtemplate presets into modern .xmp presets").

An XMP preset is a plain RDF/XML file whose properties live in Adobe's Camera Raw namespace (`crs:`, `http://ns.adobe.com/camera-raw-settings/1.0/`). ExifTool's XMP tag-name documentation is the most complete public reference for these tags ([exiftool.org/TagNames/XMP.html — XMP-crs](https://exiftool.sourceforge.net/TagNames/XMP.html#crs), which covers the "Photoshop Camera Raw namespace tags" and notes wryly that "Adobe pollutes the metadata space with these incredibly bulky image editing parameters"). Verified tag names relevant to a minimal editor's quick-edit model:

| Concept | `crs:` attribute(s) (verified present in exiftool's XMP-crs tables) |
|---|---|
| Exposure | `Exposure2012` |
| Whites / Blacks | `Whites2012`, `Blacks2012` |
| Highlights / Shadows | `Highlights2012`, `Shadows2012` |
| Contrast | `Contrast2012` |
| Clarity/Texture etc. | `Clarity2012` |
| White balance | `WhiteBalance` ("As Shot" or "Custom"), `IncrementalTemperature`, `IncrementalTint` |
| Tone curve (PV2012) | `ToneCurvePV2012` (point curve), `ToneCurveName2012` ("Linear", "Medium Contrast", …) |
| Process version | `ProcessVersion` (e.g. "11.0" — selects which tone-process the 2012 sliders belong to) |
| Local corrections | `CircGradBasedCorrExposure2012`, `GradientBasedCorrWhites2012`, `PaintBasedCorrections…` families |

All are plain XML attributes/elements parseable with any XML parser (see §2 on quick-xml). Presets also carry `crs:Name`, `crs:Group`, `crs:ShortName`, `crs:SupportsAmount` etc. for grouping/labels, and `crs:Look` for profiles — same XMP-crs reference above. Adobe's official XMP home: <https://developer.adobe.com/xmp/docs/> and the SDK at <https://github.com/adobe/XMP-Toolkit-SDK>.

### Legacy format: `.lrtemplate`

`.lrtemplate` is an old-style NeXT/Adobe plist text file containing a dictionary with `id`, `internalName`, `type` (`"Develop"`), and `value` → `settings` → the **same slider names** as the XMP attributes (`Blacks2012`, `Clarity2012`, `ColorNoiseReduction`, …). Verified from the parser output shown in [shadawck/lrjson's README](https://github.com/shadawck/lrjson) ("Transform Lightroom standard file (like lrtemplate or xmp) into manipulable json file"), which prints a parsed `.lrtemplate` containing `"Blacks2012": 40, "Clarity2012": 18, …`. Practical consequence: converting `.lrtemplate` → your internal model is (a) parse a brace/comma plist, (b) reuse the same key-mapping table as XMP. That is exactly what the community converters do.

### Open-source converters (all small; none is a Rust library)

Searches of GitHub (`lrtemplate`, `lightroom darktable`, `lightroom preset converter` via the GitHub API) found **no maintained lrtemplate→darktable-style project**; the ecosystem is small one-file scripts:

- [lavalava45/LRtemplate2XMPFormatter](https://github.com/lavalava45/LRtemplate2XMPFormatter) — Python, MIT. Batch-converts `.lrtemplate` → formatted `.xmp`, zips output.
- [shadawck/lrjson](https://github.com/shadawck/lrjson) — Python, PyPI `lrjson`. Parses both `.lrtemplate` and `.xmp` presets into JSON.
- [SillyDuck/xmp2lrtemplate](https://github.com/SillyDuck/xmp2lrtemplate) — Python, the reverse direction (XMP → lrtemplate).
- [yuzukumo/LRtemplate2XMPPresets](https://github.com/yuzukumo/LRtemplate2XMPPresets) — Python CLI + Cloudflare Workers frontend, GPL-3.0.
- [anthonyreimche/3rd-Party-Preset-Tools](https://github.com/anthonyreimche/3rd-Party-Preset-Tools) — TypeScript ".xmp and .lrtemplate preset import" (for the Safelight editor).
- [laurentzziu/acv2lrtemplate](https://github.com/laurentzziu/acv2lrtemplate) — Ruby, Photoshop `.acv` curves → `.lrtemplate` (useful as a curves-encoding reference).
- Adjacent: [RawLabo/QuickRawPicker](https://github.com/RawLabo/QuickRawPicker) (culling tool) shows the pattern of interoperating via the Lightroom/Bridge/darktable **XMP sidecar** convention rather than preset import.

No established project converts presets *into darktable styles* automatically; darktable's own styles are a different XMP dialect (its sidecar `darktable:` namespace). For our app, implementing `.lrtemplate` + `.xmp` preset import natively in Rust is a modest, well-understood task — the format knowledge above is sufficient.

---

## 2. Rust metadata crates (EXIF / IPTC / XMP) vs exiftool

### `xmp_toolkit` — Rust bindings to Adobe's XMP Toolkit SDK

- crates.io: <https://crates.io/crates/xmp_toolkit> — **v1.12.1, updated 2026-03-19**, license **MIT OR Apache-2.0**, 218k downloads, repo [github.com/adobe/xmp-toolkit-rs](https://github.com/adobe/xmp-toolkit-rs) (maintained by Adobe itself).
- README (fetched): "Parse XMP metadata found in many file formats. Inspect and modify the XMP data model. Embed and update XMP metadata in many file formats." It **incorporates the March 2025 C++ Adobe XMP Toolkit SDK**, requires Rust ≥ 1.88, and is tested on Windows (MSVC only), macOS (Intel + Apple silicon), Ubuntu x86_64/aarch64, and iOS.
- The C++ SDK itself is on GitHub at [adobe/XMP-Toolkit-SDK](https://github.com/adobe/XMP-Toolkit-SDK) — **BSD 3-Clause** license (per GitHub repo license field), C++, actively pushed (last push 2025-11). So the whole stack is permissively licensed; the cost is a C++ build dependency and a large binary-size/compile-time footprint.

### `kamadak-exif` — EXIF

- crates.io: <https://crates.io/crates/kamadak-exif> — **v0.6.1, updated 2024-11-06**, BSD-2-Clause, 12.4M downloads, pure Rust, repo [kamadak/exif-rs](https://github.com/kamadak/exif-rs). Description: "Exif parsing library written in pure Rust" — it is a **reader** (EXIF/TIFF/JPEG parsing); there is no writer. Fine for reading capture time, camera, lens, serial, ISO, etc. at ingest speed.

### `quick-xml` — for XMP parsing

- crates.io: <https://crates.io/crates/quick-xml> — **v0.41.0, updated 2026-06-29**, MIT, 370M downloads, repo [tafia/quick-xml](https://github.com/tafia/quick-xml). "High performance xml reader and writer." The pragmatic choice for reading/writing XMP packets (sidecars and presets) without pulling in the Adobe SDK.

### IPTC (IIM / IPTC Core)

There is **no mature, pure-Rust IPTC IIM codec** today. crates.io search for "iptc" (via crates.io API) surfaced:

- [`iptc`](https://crates.io/crates/iptc) (v0.3.0, 2025) — "Read IPTC tags from JPEG files in pure Rust, with partial support for Tiff" — read-only, JPEG-focused.
- [`gamut-iptc`](https://crates.io/crates/gamut-iptc) / [`gamut-metadata`](https://crates.io/crates/gamut-metadata) (v1.0.0, 2026) — "the legacy IIM/Photoshop-IRB codec, IPTC Core over XMP, and IIM-XMP reconciliation" — promising scope, but the [gamut repo](https://github.com/visualcommons/gamut) self-describes as "Early development… no guarantees on API stability yet" and marks `gamut-iptc`/`gamut-xmp`/`gamut-metadata` as **"scaffolding (impl in progress)"**. One to watch, not to build on.
- [`rexiv2`](https://crates.io/crates/rexiv2) (v0.10.0, last updated 2023-01) — safe wrapper over gexiv2/Exiv2 giving read/write EXIF+XMP+IPTC; stale, drags in GLib, and the underlying Exiv2 is GPL-family — a licensing trap for a closed-source app.
- [`xmpkit`](https://crates.io/crates/xmpkit) (v0.1.6, 2025-11) — "Pure Rust implementation of Adobe XMP Toolkit" — too early/low adoption.
- [`oximedia-metadata`](https://crates.io/crates/oximedia-metadata) (v0.2.1, 2026) — multi-format metadata incl. XMP/EXIF/IPTC; young.

### ExifTool as the write path

ExifTool remains the reference implementation: a "platform-independent Perl library plus command-line application" that **reads and writes EXIF, GPS, IPTC (IIM), XMP**, MakerNotes, etc. ([exiftool.org](https://exiftool.org/), v13.59 released May 2026, still actively maintained). Its docs explicitly cover our exact use cases: writing IPTC/XMP into files and sidecars, `-tagsFromFile` copying, and `-stay_open` persistent batch mode for performance (see [application docs](https://exiftool.sourceforge.net/exiftool_pod.html) — `stay_open FLAG  Keep reading -@ argfile even after EOF`). For IPTC the exiftool tag reference distinguishes legacy IIM (`IPTC:` group) from `XMP-iptcCore:`/`XMP-iptcExt:` ([IPTC tag names](https://exiftool.sourceforge.net/TagNames/IPTC.html), [XMP tag names](https://exiftool.sourceforge.net/TagNames/XMP.html)) — exactly the dual-write Photo Mechanic does.

### Practical recommendation for our app

Read path: native Rust (`kamadak-exif` + `quick-xml` for XMP sidecars) — fast, no subprocess per file during ingest browsing. Write path (IPTC templates applied at ingest / export): **shell out to `exiftool -stay_open`** as a long-lived child process — battle-tested against every maker's files, handles IIM↔XMP reconciliation, avoids GPL SDKs and a C++ toolchain; degrade gracefully if exiftool is missing. If bundling a Perl runtime is unacceptable long-term, the escape hatch is `xmp_toolkit` (permissive, Adobe-maintained) for XMP-only writing, but it won't write legacy IIM.

---

## 3. Photo Mechanic code replacements (primary: Camera Bits docs)

From ["Introduction to Code Replacements in Photo Mechanic"](https://docs.camerabits.com/support/solutions/articles/48000223660-introduction-to-code-replacements-in-photo-mechanic):

- **File format**: plain **UTF-8 tab-delimited text** (spreadsheet "Tab-Separated Values"). Column 1 = code, columns 2+ = replacement variants. "You must use the tab key to separate your columns. Spaces will not work." Codes are **case-sensitive**; recommended short and lowercase.
- **Delimiter**: an editable single character, **default `=`**. Typing `=code=` triggers instant replacement in any Metadata (IPTC) Info / Template text field. Example: `=wc8=` → "Alex Ovechkin".
- **Variant selection**: `=code#2=`, `=code#3=` … pick the 2nd/3rd tab-separated replacement.
- **Comments**: lines (or trailing columns) beginning with `//` are comments — "Comments must either start at the beginning of a line or be the last column on a line."
- **Variables inside codes/replacements**: PM variables `{monthname}`, `{day}`, `{year4}`, `{serial}` … can appear in the code column or in replacements, e.g. `={monthname}=` translates the evaluated month via a lookup table; a row `mydate⇥{day} {monthname} {year4}` acts as a macro. Works "in every text field of the Metadata (IPTC) Info and Metadata (IPTC) Template windows."
- **Multiple files**: Edit ▸ Settings ▸ Set Code Replacements; multiple files allowed, most-recently-added wins conflicts, files auto-reload on change.

Advanced extension — ["Hot Codes"](https://docs.camerabits.com/support/solutions/articles/48000358684-hot-codes) (header declarations that older PM versions treat as comments):

- `//==⇥{serial}` / `//==⇥{fbas:0,3}` — **code generators**: try several metadata sources in order ("try this OR that"); `{fbas:0,3}` = substring of filename base (start,len).
- `//##⇥full⇥last⇥title⇥editor` — **names** for each replacement column (custom variables).
- `//??⇥{cmnt}|Unknown⇥Other⇥all` — **per-column defaults**, with `|`-chained variable fallbacks when no code matches.
- `//$$⇥ns1` — **namespaces**; invoked as `[ns1.hotcode]`.
- Invocation: hot codes are written as `[full]`, `[title]` inside IPTC fields and ingest path templates (e.g. `Creator: [full]`, folder path `[editor]/{iseq}[last]`), versus classic `={serial}#3=` syntax.

Note: the classic syntax users actually type is delimiter-based (`=code=`), not `\d1\`; PM's system is variables `{…}`, code replacements `=code=`, selectors `#N`, hot codes `[name]`. A PM-compatible importer should implement the tab-delimited file format, `//` comments, the Hot Codes header lines, `#N` selectors, and variable nesting — all documented in the two articles above.

---

## 4. FTP / FTPS / SFTP in Rust, or shell out to rclone/curl/lftp?

### Native Rust crates (all verified on crates.io / GitHub)

- **[`suppaftp`](https://crates.io/crates/suppaftp)** — **v10.0.1 (2026-07-13)**, MIT/Apache-2.0, 3.5M downloads, repo [veeso/suppaftp](https://github.com/veeso/suppaftp). FTP **and FTPS** (explicit via `into_secure()`; implicit via `connect_secure_implicit()` behind the `deprecated` feature) with **native-tls or rustls** (aws-lc-rs / ring) backends; **sync and async** (tokio or smol); stream-based transfers; **both passive and active** modes; command coverage explicitly includes `REST` (byte-offset resume), `APPE`, `ABOR`, `EPSV/EPRT`; implements RFC 2428/2389; LIST output parser. This is the maintained successor of `rust-ftp`.
- **[`ftp`](https://crates.io/crates/ftp)** (rust-ftp) — v3.0.1, **last updated 2018-04-15** → abandoned; don't use.
- **SFTP option A: [`ssh2`](https://crates.io/crates/ssh2)** — v0.9.6 (2026-06-30), MIT/Apache-2.0, 7.4M downloads, bindings to C **libssh2** ([alexcrichton/ssh2-rs](https://github.com/alexcrichton/ssh2-rs)). Mature but C dependency.
- **SFTP option B: [`russh-sftp`](https://crates.io/crates/russh-sftp)** — **v2.4.0 (2026-08-03)**, Apache-2.0, 2.5M downloads, SFTP client **and** server for pure-Rust [russh](https://github.com/Eugeny/russh); actively released (2.3.0 May 2026, 2.4.0 Aug 2026). Pure-Rust SSH stack, no C.

### rclone as the robust upload engine

rclone has first-class ("Tier 1") backends for both protocols and runs `copy` as a **one-shot command**:

- **FTP backend** ([rclone.org/ftp/](https://rclone.org/ftp/)): supports plain FTP plus **implicit FTPS (`--ftp-tls`, port 990) and explicit FTPS (`--ftp-explicit-tls`)**, TLS options, SOCKS/HTTP-CONNECT proxies. Limitations: **passive mode only**, **no checksums** (compares size/modtime), no `rclone about`.
- **SFTP backend** ([rclone.org/sftp/](https://rclone.org/sftp/)): password / key-file / ssh-agent / cert auth, `known_hosts` validation and TOFU **host-key pinning** (`--sftp-pin-host-key`), checksums via remote shell commands (md5sum etc., auto-detected), chunk-size/concurrency tuning for high-latency links, and `--sftp-ssh` to delegate to the system `ssh` binary.
- **One-shot queue semantics** ([rclone copy](https://rclone.org/commands/rclone_copy/)): "Copy the source to the destination. Does not transfer files that are identical on source and destination, testing by size and modification time or MD5SUM." Built-ins that matter for a background upload queue: `--max-transfer`, `--max-duration`, `--cutoff-mode`, `--retries`, `--order-by 'size,ascending'`, `--transfers N` (parallelism), `--partial-suffix .partial` (atomic temp-file uploads), `--log-file`, `--files-from` (feed an explicit upload list), `--dry-run`. Re-running the same `rclone copy` after a crash is the resume strategy: completed files are skipped, partials restart. Note rclone does **not** do in-file byte resume over FTP; the FTP protocol's `REST` command does (RFC 959 §4.1.3), which is what suppaftp's `REST` support and `curl -C -` ([curl man page](https://curl.se/docs/manpage.html)) expose — for large files over flaky FTP that's an argument for a curl/lftp path or native suppaftp with manual `REST`.

### Practical recommendation

Ship both: **suppaftp (FTPS) + russh-sftp (SFTP) natively** for integrated progress UI, credential storage, and `REST`-based byte resume on FTP; and **offer rclone as an optional external engine** (`rclone copy --files-from …` one-shot invocations) for users who want its retry/checksum/host-pinning robustness — it's a single static binary, MIT-licensed, trivially bundled. lftp/curl are fine fallbacks but add harder-to-parse output; keep them out of the app if rclone is available.

---

## 5. Ingest speed: embedded previews in camera raw files

Raw files carry ready-made JPEGs (thumbnail and/or full-size preview) that can be extracted **without decoding the raw data** — a metadata-offset read plus byte copy. ExifTool documents this as a headline feature: "Extracts thumbnail images, preview images, and large JPEG images from RAW files" ([exiftool.org features](https://exiftool.org/#features)), and the application docs give the canonical recipes (from [exiftool_pod.html](https://exiftool.sourceforge.net/exiftool_pod.html)):

```
exiftool -b -ThumbnailImage image.jpg > thumbnail.jpg
exiftool -b -JpgFromRaw -w _JFR.JPG -ext NEF -r .
exiftool -b -jpgfromraw -w %d%f_%ue.jpg -execute -b -previewimage -w %d%f_%ue.jpg ...
```

- `-b` = "Output metadata in binary format" (dumps the embedded image bytes); combined with `-w`/`-r`/`-ext` it becomes a batch preview extractor.
- Tag names differ by maker: `JpgFromRaw` (Nikon NEF, Canon CR2 composite), `PreviewImage` (largest embedded JPEG on many bodies), `ThumbnailImage` (small). Sizes vary by camera/body settings — some embed full-resolution JPEGs, others ~1600px previews — verify per supported body. The relevant composite/derived tags and their sources are documented in exifTool's Composite tag notes (e.g. `PreviewImageStart`/`ThumbnailOffset`-style pointers, [exiftool.org homepage](https://exiftool.org/) and [TagNames/Composite](https://exiftool.sourceforge.net/TagNames/Composite.html)).
- This is exactly the Photo-Mechanic-class ingest trick: build contact sheets and cull at ingest from embedded JPEGs; only decode raws when the user opens an image.

For a Rust implementation without exiftool: read the embedded JPEG via `exiftool -b` during development, and longer-term extract natively (raw formats are TIFF-like containers; `rawloader`-style offset parsing or a small per-maker module). XMP/IPTC writing at ingest does not touch the preview (sidecars or padding-safe in-place edits via exiftool).

---

## Recommendations

1. **Presets**: implement import for both `.xmp` (quick-xml, `crs:` keys) and `.lrtemplate` (mini-plist parser; same `*2012` key names). Map only the quick-edit subset first: `Exposure2012`, `Whites2012`, `Blacks2012`, `Highlights2012`, `Shadows2012`, `Contrast2012`, `Clarity2012`, WB (`WhiteBalance`/`IncrementalTemperature`/`IncrementalTint`), `ToneCurvePV2012`. Skip `ProcessVersion` < PV2012 presets initially.
2. **Metadata**: read natively (`kamadak-exif` + `quick-xml`); write via a persistent `exiftool -stay_open` child process (IIM + XMP dual write, PM-compatible), with `xmp_toolkit` (Adobe, MIT/Apache-2.0, bundles BSD-3-Clause C++ SDK) as the fallback if we must eliminate the Perl dependency. Avoid Exiv2/rexiv2 (GPL + GLib + stale).
3. **IPTC model**: follow the IPTC Core (XMP) fields as the source of truth, mirror to legacy IIM on write (exiftool handles both; `gamut-iptc` is the pure-Rust one to re-evaluate in a year).
4. **Code replacements**: implement the PM spec — UTF-8 TSV, `//` comments, configurable delimiter (default `=`), `#N` variant selectors, variable interpolation `{var}` (incl. substring `{fbas:0,3}`), Hot Codes headers `//==`, `//##`, `//??`, `//$$` with `[name]` invocation and `|`-chained defaults. Both Camera Bits articles above are complete enough to serve as the spec.
5. **Upload**: native queue on **suppaftp** (FTPS, both TLS backends, REST resume, async) + **russh-sftp** (pure-Rust SFTP); optional **rclone** external engine using one-shot `rclone copy --files-from` for bulletproof retry/skip/resume semantics. Bundle rclone's static binary; parse its `--log-file`/`--combined` output for queue state.
6. **Ingest**: extract embedded JPEGs (`-b -JpgFromRaw`/`-b -PreviewImage`) for instant contact sheets; cache them as proxy files keyed by content hash. Budget a per-camera verification pass since embedded preview sizes vary by vendor.

**Source index**: [exiftool.org](https://exiftool.org/) · [exiftool pod](https://exiftool.sourceforge.net/exiftool_pod.html) · [XMP-crs tags](https://exiftool.sourceforge.net/TagNames/XMP.html#crs) · [adobe/XMP-Toolkit-SDK](https://github.com/adobe/XMP-Toolkit-SDK) · [adobe/xmp-toolkit-rs](https://github.com/adobe/xmp-toolkit-rs) · [xmp_toolkit](https://crates.io/crates/xmp_toolkit) · [kamadak-exif](https://crates.io/crates/kamadak-exif) · [quick-xml](https://crates.io/crates/quick-xml) · [iptc](https://crates.io/crates/iptc) · [gamut](https://github.com/visualcommons/gamut) · [rexiv2](https://crates.io/crates/rexiv2) · [suppaftp](https://github.com/veeso/suppaftp) · [ftp (abandoned)](https://crates.io/crates/ftp) · [ssh2](https://crates.io/crates/ssh2) · [russh-sftp](https://crates.io/crates/russh-sftp) · [rclone ftp](https://rclone.org/ftp/) · [rclone sftp](https://rclone.org/sftp/) · [rclone copy](https://rclone.org/commands/rclone_copy/) · [PM code replacements](https://docs.camerabits.com/support/solutions/articles/48000223660-introduction-to-code-replacements-in-photo-mechanic) · [PM hot codes](https://docs.camerabits.com/support/solutions/articles/48000358684-hot-codes) · [lrjson](https://github.com/shadawck/lrjson) · [LRtemplate2XMPFormatter](https://github.com/lavalava45/LRtemplate2XMPFormatter) · [LRtemplate2XMPPresets](https://github.com/yuzukumo/LRtemplate2XMPPresets) · [Adobe LrC 2018 what's new](https://helpx.adobe.com/lightroom-classic/help/whats-new/2018.html)
