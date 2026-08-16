# editing — Plan

A minimal, fast, keyboard-first photo workflow app: Photo Mechanic's ingest and
metadata muscle combined with Lightroom's quick-edit feel, built with Rust +
GPUI. One integrated loop: tag, quick-edit, send, advance — export and FTP
upload happen in the background while you move to the next photo.

Open source. Linux first, cross-platform (Windows, macOS) after.

## Non-goals

- No catalog migrations from Lightroom, no import of LR catalogs
- No layers, masking, healing/clone, HDR/pano merge
- No printing, maps, face recognition, slideshows, cloud sync
- No tethering (v1)
- Not a darktable replacement — darktable is an export engine, used headless

## Decisions (grilled & settled)

| Decision | Choice |
|---|---|
| License | Open source |
| UI framework | GPUI + gpui-component (pin git revisions, expect churn) |
| Export engine | Bundled darktable-cli, version-pinned (GPLv3 → subprocess only, never linked) |
| Raw handling | NEF + other raws via rawler embedded-preview extraction (LGPL, pure Rust) |
| LR interop | Best-effort `crs:` sidecars (ProcessVersion=11.0) so LR can read edits/crop; LR is never required in the flow |
| Metadata storage | XMP sidecar always for raws; embedded IPTC in exported JPEGs |
| Metadata writer | Resident `exiftool -stay_open` process (only full IIM+XMP writer) |
| Code replacements | Photo Mechanic-compatible replacement files (TSV, `=code=` syntax, hot codes) |
| IPTC fields | Full PM stationery-pad set (phased, see M3) |
| Presets | Lightroom `.xmp` (crs: namespace) only |
| Quick-edit math | Own scene-referred implementation (no open-source PV2012 exists to copy) |
| Crop | Free crop + aspect presets + rotate/straighten |
| Ingest | Manual trigger only; multi-card parallel; one verified backup destination |
| Ingest naming | User-defined folder/filename rules with variables (PM-style subset) |
| RAW+JPEG | Paired by base filename, treated as one item; caption/edit the JPEG, stamp both |
| Transfer | Plain FTP v1 (suppaftp, REST resume); FTPS/SFTP later |
| Send model | Multiple saved send recipes, one active; explicit send key then auto-advance |
| State | SQLite catalog (ratings, tags, upload status, ingest history, queue) |
| Secrets | OS keychain via `keyring` crate |

## Architecture

```
┌─────────────────────────── GPUI app shell ───────────────────────────┐
│  Contact sheet (uniform_list/grid)  │  Single view (canvas, crop)    │
│  Template editor, recipe editor     │  Sliders, code-replacement UI  │
│  Ingest dialog (per-card progress)  │  Send queue / status           │
└──────────────┬───────────────────────────────────────┬───────────────┘
               │                                       │
   ┌───────────▼──────────┐              ┌────────────▼─────────────┐
   │ Preview pipeline     │              │ Edit state               │
   │ rawler preview/img   │              │ own tone/WB math (rayon) │
   │ zune-jpeg decode     │◄─────────────│ crop, crs:XMP sidecar    │
   │ → BGRA RenderImage   │              │ + catalog row            │
   └───────────┬──────────┘              └────────────┬─────────────┘
               │                                        │
   ┌───────────▼──────────┐   ┌────────────▼──────┐ ┌──▼──────────────┐
   │ Ingest service       │   │ Metadata service  │ │ Export service  │
   │ per-device workers   │   │ exiftool -stay_   │ │ darktable-cli   │
   │ temp→BLAKE3→rename   │   │ open (IIM+XMP)    │ │ subprocess pool │
   │ + backup destination │   │ code replacements │ │ + embed IPTC    │
   └───────────┬──────────┘   └────────────┬──────┘ └──┬──────────────┘
               │                           │           │
   ┌───────────▼───────────────────────────▼───────────▼──────────────┐
   │ SQLite catalog + durable job queue (rusqlite, worker threads)    │
   └───────────────────────────────┬──────────────────────────────────┘
                                   │
                      ┌────────────▼────────────┐
                      │ Transfer queue          │
                      │ suppaftp (plain FTP)    │
                      │ resume, retry, status   │
                      └─────────────────────────┘
```

### Key mechanics

- **Previews without raw decode.** rawler's `extract_preview_pixels` pulls the
  camera-embedded JPEG (CR3/NEF/ARW/CR2/RAF/RW2/DNG) in milliseconds. Fallback:
  `exiftool -b -JpgFromRaw`. Full raw decode is never on the interactive path.
- **Quick edits are proxy-based.** Sliders run a rayon-parallel tone/WB pass on
  the preview (~1–2 ms per op at 24 MP; drag on a downscaled proxy). Exposure =
  `2^EV` gain; WB via daylight-model temp/tint→gains; highlights/shadows as
  tone-equalizer-style adaptive bands; whites/blacks as endpoint remaps.
- **One edit state, two artifacts.** Internal edit model writes: (1) best-effort
  `crs:` XMP sidecar via exiftool (LR-readable: Exposure2012, *2012 sliders,
  ToneCurvePV2012, Crop*, HasCrop, ProcessVersion=11.0); (2) generated darktable
  history XMP (or .dtstyle) for export. LR and dt sidecars are kept strictly
  separate (naming conventions collide).
- **Ingest is RPD-shaped.** One worker pipeline per device; files copied to temp
  random names, BLAKE3-verified, then atomically renamed into place; backup
  copies fan out from the already-downloaded temp file (never re-read the card);
  duplicate detection by (original filename, EXIF time, size). RAW+JPEG pairing
  by base filename. IPTC template (ingest-scoped "local" snapshot) stamped in
  the same pass.
- **Everything queued is durable.** Export jobs, metadata writes, and uploads
  are rows in SQLite worked by background threads; the app can crash mid-send
  and resume. Upload status is per-photo (queued/uploading/done/failed) and
  visible as colored badges in the contact sheet.

## The core loop (acceptance scenario)

1. Ingest dialog: two cards mounted → both ingest in parallel with per-card
   progress; photos land in `{yyyy}/{mm}-{dd}/{session}` folders + backup disk;
   ingest IPTC snapshot applied.
2. Contact sheet opens while ingest still runs (photos appear as they land).
3. Alt+→ to first photo. Type caption using code replacements (`\h1\` →
   "SHL", `{f:2}` … PM syntax), add keywords; set WB, +0.7 EV, crop 3:2, rotate
   1.2°; maybe apply an LR .xmp preset.
4. Alt+S (send): current recipe export (resize/quality/sRGB/embedded IPTC/
   renamed file) enqueued → darktable-cli renders → FTP upload starts in
   background → cursor auto-advances to next photo.
5. Green badge appears on the sent photo; a failed upload turns red and is
   retried with resume.

Target: step 3+4 under a few seconds per photo, keyboard only.

## Milestones

Each milestone is a vertical slice that runs end-to-end on Linux.

- **M0 — Scaffold.** Cargo workspace, GPUI shell window with gpui-component,
  repo CI (fmt/clippy/test), decision log in-repo.
- **M1 — Browse.** Open folder → SQLite catalog built → contact sheet grid with
  RAW+JPEG pairing; previews via rawler; full-screen single view; keyboard nav.
- **M2 — Quick edit.** Exposure/WB/highlights/shadows/whites/blacks on live
  preview; LR .xmp preset import + apply; free crop + aspect presets + rotate;
  edit state persisted (catalog + crs sidecar best-effort).
- **M3 — Metadata.** IPTC template editor (phase 1: caption, keywords, headline,
  byline/title, credit, source, copyright, instructions, edit status, rights;
  phase 2: full PM set incl. location, contact, categories, custom fields);
  variables inside fields; PM-compatible code replacements; batch apply;
  exiftool resident writer (XMP sidecar; IIM optional toggle).
- **M4 — Ingest.** Manual ingest dialog, multi-card parallel, per-card progress,
  folder/filename rules with variables, verified backup destination, duplicate
  detection, IPTC snapshot on ingest, contact sheet live-updates mid-ingest.
- **M5 — Send.** Export service (bundled darktable-cli worker pool, resize/
  quality/sRGB, IPTC embed, filename recipe); send recipes (multiple saved, one
  active); FTP transfer queue with resume/retry/status badges; Alt+S send +
  auto-advance. Core loop demo done.
- **M6 — Cross-platform + packaging.** Windows/macOS bring-up, cargo-packager
  installers, darktable-cli bundling per platform, keychain secrets, crash-safe
  queue verified.

## Stack summary

| Concern | Choice |
|---|---|
| UI | gpui + gpui-component (git-pinned) |
| Raw previews | rawler (LGPL) |
| Decode/encode | image + zune-jpeg (turbojpeg if encode speed matters) |
| Pixel ops | rayon, fast_image_resize |
| Color | lcms2 (or moxcms) |
| Metadata I/O | exiftool `-stay_open`; read: kamadak-exif + quick-xml |
| Export | darktable-cli subprocess pool (`--library :memory:`, `--apply-custom-presets 0`) |
| Transfer | suppaftp (plain FTP, REST resume) |
| Catalog/queue | rusqlite (bundled) + worker threads + flume |
| Volumes | sysinfo polling + udev / GetVolumeInformationW / diskarbitration |
| Secrets | keyring |
| Packaging | cargo-packager / cargo-dist |

## Risks & mitigations

- **GPUI pre-1.0 churn, thin docs** — pin revisions; the app is precisely the
  use case Zed's `image_viewer` and gpui-component's tables already cover.
- **darktable XMP param blobs are version-fragile** — bundle and pin one
  darktable version; generate params ourselves for a small fixed module set
  (exposure, temperature, tone equalizer); golden-file tests per release.
- **No canonical PV2012 math** — our sliders are approximations tuned to feel
  right on previews; export goes through darktable so WYSIWYG drift is possible
  → mitigate with A/B preview-vs-export check in QA and a "match preview" dev
  mode.
- **rawler LGPL + unstable API** — open source app, so obligation is easy;
  pin version, wrap behind a `PreviewProvider` trait with exiftool fallback.
- **Plain FTP credentials on the wire** — keychain-stored, but document the
  risk; FTPS/SFTP on the roadmap.
- **Bundling darktable** (~200 MB install) — acceptable for v1; make the bundle
  optional ("use system darktable ≥ X") as an escape hatch.

## Open questions

- Exact variable set for folder/filename rules (PM subset: dates, sequence,
  IPTC fields, session name — finalize in M4 design).
- IIM (legacy IPTC) dual-write: default on for agency compatibility, or off?
- Does the send recipe need per-recipe filename templates (PM-style) from day
  one, or is one global filename rule enough?
- Rotate: full 90° steps + fine straighten, or fine straighten only?
- Catalog scope: single "current working folder" session or a persistent
  all-photos library? (Leaning: sessions + ingest history, not a full library.)
