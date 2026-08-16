# Headless CLI photo engines: darktable-cli, RawTherapee/ART, exiftool, fallbacks, ingest

Research for the `editing` app (Rust/GPUI photo editor + IPTC manager). Question: which existing
open-source engines can we drive headlessly via CLI for raw development and metadata, and what are
the licensing/architecture consequences of each choice?

All claims below cite a primary source (official user manual, project homepage, or source file).

---

## 1. darktable-cli

### Headless operation

`darktable-cli` is a separate binary that "does not open any display – it works in pure console mode
without launching a GUI. This mode is particularly useful for servers running background jobs."
([darktable-cli manual](https://docs.darktable.org/usermanual/development/en/special-topics/program-invocation/darktable-cli/))

Invocation shape:

```
darktable-cli [<input file or folder>] [<xmp file>] <output file or folder>
              [--width <max width>] [--height <max height>] [--hq 0|1]
              [--upscale 0|1] [--style <name>] [--style-overwrite]
              [--apply-custom-presets 0|1] [--out-ext <ext>] [--import <file or dir>]
              [--library <path>] [--icc-type <t>] [--icc-file <f>] [--icc-intent <i>]
              [--core <darktable options>]
```

The source confirms it initializes the core with GUI off and no `data.db`
([src/cli/main.c](https://github.com/darktable-org/darktable/blob/master/src/cli/main.c): *"init dt
without gui and without data.db"* — `dt_init(m_argc, m_arg, FALSE, custom_presets, NULL)`). There is
no Lua initialization in the CLI binary (no lua references in `src/cli/main.c`).

### Raw file + existing XMP sidecar

- The second positional argument is an optional XMP sidecar "containing the history stack data to be
  applied during export. If this option is not provided darktable will search for an XMP file that
  belongs to the given input file(s)." (same manual page)
- `--library <path>`: "Read the image processing history stack from the specified library database
  instead of XMP sidecar files." The source is explicit about precedence:
  [src/cli/main.c](https://github.com/darktable-org/darktable/blob/master/src/cli/main.c) —
  *"When --library was provided, use the DB history; otherwise keep XMP override mode."*
- Contrast with the GUI app: once an image is imported into the GUI's `library.db`, "the database
  entries take precedence over the XMP file. Subsequent changes to the XMP file by any other
  software are not visible to darktable" unless configured otherwise
  ([sidecar files manual](https://docs.darktable.org/usermanual/development/en/overview/sidecar-files/sidecar/)).
  For our app this is good news: `darktable-cli` in default (no `--library`) mode is XMP-driven, so
  we own the sidecars and darktable won't fight us.
- Existing output files are never silently overwritten — the CLI prints *"output file already
  exists, it will get renamed"* ([src/cli/main.c](https://github.com/darktable-org/darktable/blob/master/src/cli/main.c)).
- XMP write-back: darktable stores "metadata, tags, and image processing steps" in `.XMP` sidecars
  next to the raws, auto-generated on import; duplicates get `<basename>_<N>.<ext>.xmp`
  ([sidecar files manual](https://docs.darktable.org/usermanual/development/en/overview/sidecar-files/sidecar/)).
  darktable can also import XMP produced by other apps
  ([sidecar import page](https://docs.darktable.org/usermanual/development/en/overview/sidecar-files/sidecar-import/)).

### Styles (.dtstyle)

- Styles are "stored as XML files with the extension `.dtstyle`", exportable/importable for sharing;
  a style captures a history-stack subset and each module's on/off state
  ([styles manual](https://docs.darktable.org/usermanual/development/en/module-reference/utility-modules/lighttable/styles/)).
- On the CLI: `--style <name>` applies a style at export; `--style-overwrite` replaces instead of
  appends. Using `--style` requires the darktable config dir (`--core --configdir ~/.config/darktable`)
  because styles live in `data.db` ([darktable-cli manual](https://docs.darktable.org/usermanual/development/en/special-topics/program-invocation/darktable-cli/)).
- Note the coupling: styles are referenced **by name from a config dir**, not by path — for a
  bundle-and-forget engine we'd ship a dedicated `--configdir` with our styles pre-imported.

### Export formats: JPEG/TIFF quality, resize

- Resize: `--width`/`--height` max bounds, `--hq` high-quality resampling (default true), `--upscale`
  (default false). Format from `--out-ext` (`jpeg`, `tiff`, `openexr`, `jxl`, `webp`, `png`, …) or
  derived from the output filename.
- Quality/bit-depth are not dedicated flags; they are config keys passed per-run:
  `--core --conf plugins/imageio/format/jpeg/quality=85`, `.../tiff/bpp=16`,
  `.../tiff/compress=1` etc. The manual documents the full option set (jpeg quality 5–100; tiff bpp
  8/16/32 + deflate; jxl bpp/quality/effort; webp lossless…)
  ([darktable-cli manual, export options](https://docs.darktable.org/usermanual/development/en/special-topics/program-invocation/darktable-cli/)).

### GPU vs CPU

- All `--core` options of the main binary are available to `darktable-cli`; notably
  `--disable-opencl` (force CPU) and `-t <num openmp threads>` (cap CPU threads)
  ([darktable binary manual](https://docs.darktable.org/usermanual/development/en/special-topics/program-invocation/darktable/)).
  Default behavior (OpenCL on if available) and tuning are covered in the
  [OpenCL special topic](https://docs.darktable.org/usermanual/development/en/special-topics/opencl/).

### Parallel instances

- `--apply-custom-presets 0` "allows you to run multiple instances of `darktable-cli` at the cost of
  being unable to use the `--style` option. Defaults to true."
  ([darktable-cli manual](https://docs.darktable.org/usermanual/development/en/special-topics/program-invocation/darktable-cli/))
- The library DB is locked per-process: darktable "will lock the library to the current user…
  writ[es] the current process identifier (PID) into a lock file `<library file>.lock`… If darktable
  finds an existing lock file for the library, it will terminate immediately." A separate `--library`
  path (or `:memory:`) per worker avoids contention
  ([darktable binary manual](https://docs.darktable.org/usermanual/development/en/special-topics/program-invocation/darktable/)).
- Practical recipe for N parallel exports: one `darktable-cli` per worker, each with
  `--library :memory: --apply-custom-presets 0` (XMP-driven), or a shared read-only config dir if
  styles are needed with `--library <unique-tmp>` per worker.

### Lua / control API

- `darktable-cli` does not initialize Lua (source above). Lua scripting belongs to the GUI binary
  (`--luacmd`, `-d lua`).
- "Library mode": a Lua script can `require("darktable")` and load `libdarktable.so` headless —
  "darktable is configured headless, so the functions relating to the user interface are not
  available" — but the manual marks it *"very experimental"* and recommends `--library :memory:`
  ([using darktable from a lua script](https://docs.darktable.org/usermanual/development/en/lua/darktable-from-lua/);
  API reference: [docs.darktable.org/lua/stable](https://docs.darktable.org/lua/stable/)). Also
  DBus control exists ([calling lua from dbus](https://docs.darktable.org/usermanual/development/en/lua/calling-from-dbus/)).
  Verdict: not a supported integration seam for a third-party app; stick to `darktable-cli`.

---

## 2. RawTherapee CLI and ART

### rawtherapee-cli

Dedicated CLI binary shipped since RT 5.1
([RawPedia: Command-Line Options](https://rawpedia.rawtherapee.com/Command-Line_Options)):

```
rawtherapee-cli [-o <output>|-O <output>] [-q] [-a] [-s|-S] [-p <files>] [-d]
                [-j[1-100] [-js<1-3>] | -b<8|16>] [-t[z] | -n] [-Y] [-f] -c <input>
```

- `-c` (must be last) takes files or folders; `-a` processes all supported types in folders.
- Sidecars: `-s` uses `photo.raw.pp3` if present (neutral values otherwise); `-S` = skip if missing.
- Profile stacking ("auto-apply" equivalent): `-p file.pp3` may be repeated, "each will be built on
  top of the previous one". Final profile = neutral → `-d` (default raw/non-raw profile from
  Preferences) → each `-p` in order → `-s`/`-S` sidecar last. The RawPedia examples show exactly the
  Photo-Mechanic-style pipeline our app wants: `-p iptc.pp3 -s -p exif.pp3 -p web.pp3` (apply IPTC,
  strip tags, resize/sharpen) in one pass
  ([RawPedia examples](https://rawpedia.rawtherapee.com/Command-Line_Options#Examples)).
- Export: `-j[1-100]` JPEG quality (default 92), `-js<1-3>` chroma subsampling, `-b<8|16>` bit
  depth, `-t[z]` TIFF (uncompressed/zip), `-n` PNG, `-Y` overwrite, `-f` fast-export pipeline,
  `-O` also copies the pp3 next to the output.
- No library, no lock files: each invocation is stateless → trivially parallelizable (one process
  per file/queue slot; `-q` skips loading caches for faster startup).
- XMP: RT's edit sidecars are `.pp3` only; there is no XMP round-trip in RT (metadata handling is
  EXIF-oriented; the IPTC/XMP example above is done by injecting values through a pp3 profile).
- License: GPLv3 ([LICENSE](https://github.com/Beep6581/RawTherapee/blob/dev/LICENSE)).

### ART (Another Raw Therapee)

ART is a RawTherapee derivative with simpler UX; processing profiles use the `.arp` extension "to
avoid conflicts with RawTherapee", and it has "better metadata handling (thanks to the exiv2 and
exiftool libraries), with (optional) support for reading and writing XMP sidecar files"; ratings and
color labels can be loaded/stored from/to XMP
([ART homepage](https://artraweditor.github.io/)). License: GPLv3 (same page).

Its CLI (`art-cli`) mirrors rawtherapee-cli and adds: `-b<8|16|16f|32>` (float TIFF), `-Ttype`
custom output formats, `-V` verbose, `--progress` (zenity-compatible progress), plus `--make-icc`
and `--check-lut` utilities; profile build order is identical (neutral → `-d` → `-p`… → `-s`/`-S`)
([rtgui/printhelp.h, ART source](https://github.com/artraweditor/ART/blob/master/rtgui/printhelp.h)).

Takeaway: ART gives the RT pipeline + real XMP metadata interop; RawTherapee is the more
conservative/faster-moving upstream with the bigger community. Both are safe to shell out to.

---

## 3. exiftool

### Coverage: IPTC (IIM), XMP, sidecars vs embedding

- exiftool is "a platform-independent Perl library plus a command-line application" that reads AND
  writes "EXIF, GPS, IPTC, XMP, JFIF, MakerNotes, GeoTIFF, ICC Profile, Photoshop IRB…"
  ([exiftool.org](https://exiftool.org/)). Version 13.59 (2026-05-27) at time of writing.
- Write priorities when no group is specified: **1) EXIF, 2) IPTC, 3) XMP**; you can force the group
  with `-IPTC:Caption=`-style prefixes ([writing docs](https://exiftool.sourceforge.net/writing.html),
  [FAQ](https://exiftool.sourceforge.net/faq.html)).
- IPTC legacy (IIM) vs modern: "The more recent IPTC Photo Metadata Standard actually uses the XMP
  format" — exiftool implements both, plus MWG recommendations to keep EXIF/IPTC/XMP synchronized
  (`-use mwg`) ([FAQ #3, #10](https://exiftool.sourceforge.net/faq.html)). For a 2026 app targeting
  wire agencies, write IIM (`-IPTC:…`) *and* XMP (`-XMP-dc:…`), or use MWG tags.
- Embedding vs sidecars: writes are in-place (JPEG/TIFF/RAW support R/W per format table on
  [exiftool.org](https://exiftool.org/)); the original is preserved as `file.ext_original` until
  deleted. Sidecar creation is first-class: "Metadata… may be stored in a separate metadata file.
  These are the only files that exiftool can create from scratch" — e.g.
  `exiftool -ext CR2 -o %d%f.xmp -r DIR` builds sidecars for a tree, and
  `exiftool -tagsfromfile %d%f.xmp -all:all -r DIR` restores them into images
  ([metafiles page](https://exiftool.sourceforge.net/metafiles.html)).
- Safety notes: "ExifTool will not rewrite a file if it detects a significant problem", editing is
  "lossless as far as image [pixels are] concerned", but full metadata deletion from RAW is not
  recommended ([writer limitations](https://exiftool.org/#limitations)).

### Performance

- Significant per-invocation startup cost (Perl); batch many files per command or use
  `-execute`/`-stay_open` argfile protocol — one user documented "a 60x speed increase" batching
  ([performance section](https://exiftool.org/#performance)). For a long-running Rust app, the
  `-stay_open` persistent process is the way to avoid interpreter startup per photo.

### Licensing

- "This is free software; you can redistribute it and/or modify it under the same terms as
  [Perl itself](http://dev.perl.org/licenses/)" ([license](https://exiftool.org/#license)) — i.e. the
  dual GPL-or-Artistic Perl license. Executing it as a subprocess from a proprietary/Rust app is
  unproblematic; copying its Perl into our codebase would not be.

### Rust alternatives (crates.io data, 2026-08)

| Crate | What | Status |
|---|---|---|
| [`kamadak-exif`](https://github.com/kamadak/exif-rs) | pure-Rust **EXIF** parser (JPEG/TIFF/HEIF/PNG/WebP); has a `writer.rs` (EXIF only) | most-used (12.4M downloads), no IPTC/XMP |
| [`xmp-toolkit`](https://github.com/adobe/xmp-toolkit-rs) | official Adobe Rust bindings to the C++ XMP Toolkit | actively maintained (2026), XMP only |
| [`rexiv2`](https://github.com/felixc/rexiv2) | bindings to gexiv2/Exiv2: read+write **Exif, XMP, IPTC** | last release 2023; C++ FFI |
| `exif` (libexif wrapper) | bindings to libexif | abandoned (2016, v0.0.1) |

**No maintained pure-Rust crate matches exiftool's coverage** (maker notes, hundreds of RAW
variants, IIM encoding rules). Realistic options for `editing`: drive `exiftool -stay_open` as a
child process (best coverage, what digiKam-style tools do), and/or use `kamadak-exif` +
`xmp-toolkit` for pure-Rust paths. (Crate facts: crates.io API.)

---

## 4. JPEG-only quick edits: ImageMagick vs libvips vs ffmpeg

- **libvips** — "a demand-driven, horizontally threaded image processing library… runs quickly and
  uses little memory", LGPL-2.1-or-later, ~300 operations, C/C++/**command-line** (`vips`) plus
  bindings in many languages; reads/writes JPEG, TIFF, PNG, WebP, HEIC, AVIF, JXL…; it is the engine
  behind sharp/imgproxy/Mastodon
  ([libvips.org](https://www.libvips.org/)). Designed for servers, no display, no raw support — the
  best headless story for crop/resize/recompress on already-rendered rasters.
- **ImageMagick** — "a collection of command-line tools… if you need to process an image dynamically
  from a web script or apply the same operations to multiple images… using a command-line utility
  like ImageMagick may be more efficient" (`magick`, `mogrify`, `identify`, …)
  ([command-line tools](https://imagemagick.org/command-line-tools/)). Mature and universal, but
  heavier memory use than libvips and a larger historic attack surface (it ships a
  [security policy](https://imagemagick.org/security-policy/) mechanism). Permissive
  Apache-2.0-style license ([LICENSE](https://github.com/ImageMagick/ImageMagick/blob/main/LICENSE)):
  fine to link or bundle in any app.
- **ffmpeg** — "a collection of libraries and tools to process multimedia content such as audio,
  video, subtitles and related metadata"
  ([FFmpeg README](https://github.com/FFmpeg/FFmpeg/blob/master/README.md)). It is video-first; it
  can re-encode a JPEG through its filter graph, but there is no still-image metadata/IPTC workflow
  worth building on. Skip for this project.

Verdict: **libvips** for the fast-path JPEG operations (thumbnail/crop/recompress), ImageMagick only
if we need a format libvips lacks, ffmpeg not at all.

---

## 5. Ingest: cards → verified, renamed, tagged copies

Photo-Mechanic-style ingest has four parts; here is what the open-source toolbox provides:

1. **Copy off the card** — `rsync -a --info=progress2 /media/card/DCIM/ /vault/raw/` does
   structure-preserving copy with resume; re-running an interrupted ingest is idempotent with
   `--ignore-existing` ("skip updating files that exist on receiver"); `-i`/`--itemize-changes`
   prints a per-file change summary (verifiable log); `--remove-source-files` exists but should be
   avoided until verification passes (all from the
   [rsync(1) man page](https://man.archlinux.org/man/rsync.1.en)).
2. **Verify** — rsync's `-c/--checksum` compares full-file checksums during transfer (useful for
   second copies); exiftool additionally "generates MD5/SHA256/SHA512 hashes of image data only for
   many file types (for comparison and validation of image data)" — i.e. content hashes that ignore
   metadata, ideal for verifying copies of raws ([features](https://exiftool.org/)).
3. **Unique names + date folders** — exiftool renames/moves by writing pseudo-tags, combined with
   the `-d` date-format option:
   `exiftool "-FileName<CreateDate" -d "%Y%m%d_%H%M%S.%%e" DIR` and
   `exiftool "-Directory<DateTimeOriginal" -d "%Y/%m/%d" DIR`
   ([renaming section](https://exiftool.org/#filename), more at
   [filename examples](https://exiftool.sourceforge.net/filename.html)). Name collisions are handled
   by the `%c` format code: "%c may be used to add a copy number to the output file name to avoid
   collisions with existing file names" ([filename docs](https://exiftool.sourceforge.net/filename.html)).
4. **Apply metadata on ingest** — the same exiftool pass can stamp IPTC/XMP (`-IPTC:Keywords+=…`,
   `-XMP-dc:Rights=©…`) while organizing, or the RawTherapee-style `-p iptc.pp3` profile trick
   during export ([RT example](https://rawpedia.rawtherapee.com/Command-Line_Options#Examples)).

For our app: rsync (or a Rust equivalent doing the same) for the byte copy + checksum verify,
exiftool batch for rename/organize/tag, darktable-cli/RT/art-cli only for pixels. Everything in one
ingest can be driven from Rust; the exiftool part should run as one batch invocation (or `-stay_open`)
per ingest job for performance.

---

## Comparison table

| | **darktable-cli** | **rawtherapee-cli** | **ART (art-cli)** | **own pipeline (Rust + libvips/libraw + exiftool)** |
|---|---|---|---|---|
| Headless use | Yes — pure console binary; GUI-free init in source | Yes — dedicated CLI binary | Yes — dedicated CLI binary | Native (we are the process) |
| Raw development | Excellent (scene-referred, OpenCL/OpenMP) | Excellent (lab pipeline, float processing) | RT pipeline, simplified UX | Must assemble (libraw/rawspeed demosaic, color mgmt) — big effort |
| XMP round-trip | Yes — reads sidecar (default) or `--library` DB; writes XMP; GUI app DB can override sidecars | No — `.pp3` sidecars only; IPTC/XMP only via profile injection | Partial/optional — `.arp` edits + optional XMP sidecars (exiv2/exiftool) | Via exiftool (full) or `xmp-toolkit`; we own the model |
| Preset support | Styles `.dtstyle` by name from `--configdir`; `--style`/`--style-overwrite`; needs `data.db` unless `--apply-custom-presets 0` | `-p a.pp3 -p b.pp3` stacking + `-d` defaults + `-s` sidecars; order = CLI order | Same model with `.arp`; adds `-Ttype`, float TIFF, `--progress` | Our own preset format; full control |
| JPEG/TIFF export control | `--out-ext` + `--core --conf plugins/imageio/format/…` (quality, bpp, compression) | `-j[1-100] -js<1-3> -b -t[z] -n -Y -f` | RT flags + `16f/32` TIFF, custom types | libvips: quality/bpp/strip; exact control |
| Parallel ingest/export | Yes with `--apply-custom-presets 0` and per-worker `--library` (`:memory:`); lock file otherwise kills 2nd instance | Yes — stateless per invocation; run N processes (`-q` for fast start) | Yes — same as RT | Yes — tokio/rayon, bounded by exiftool `-stay_open` and libvips concurrency |
| Engine control API | CLI only (Lua is GUI-binary; Lua "library mode" is experimental) | CLI only | CLI only | Ours |
| Licensing | GPLv3 — subprocess only; never link | GPLv3 — subprocess only | GPLv3 — subprocess only (its exiv2 dep is GPLv2+, [COPYING](https://github.com/Exiv2/exiv2/blob/main/COPYING)) | libvips LGPL-2.1+ (linkable), exiftool Perl license (exec), ImageMagick permissive — closed core remains possible |

## Recommendation

Use **darktable-cli as the default raw engine** (XMP-driven, best matches a Lightroom-ish model,
export options fully controllable via `--conf`), keep **ART/art-cli as the secondary** for its XMP
metadata interop and simpler profiles, and do **all metadata through exiftool** (`-stay_open`
resident process) with **libvips for JPEG fast-path edits**. Treat all raw engines as replaceable
subprocesses behind a trait — their GPL means process isolation is mandatory anyway.
