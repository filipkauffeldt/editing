# Prior-art survey: photo workflow apps (ingest, variables, IPTC templates, FTP send)

Research for a minimal Photo Mechanic + Lightroom-hybrid workflow app (Rust/GPUI). Primary
sources only: official docs, and source code where the app is open. Every claim cites its
source. Companion survey: `presets-metadata-ftp.md` (library-level options for IPTC/FTP).

---

## 1. Rapid Photo Downloader (RPD) — Linux, open source (GPL), Python/Qt

Official site & docs: <https://damonlynch.net/rapid/> and
<https://damonlynch.net/rapid/documentation/> (site was unreachable from this machine;
claims below verified directly against the source at commit `a119bc4`:
<https://github.com/damonlynch/rapid-photo-downloader/tree/a119bc4>).

### Multi-device simultaneous ingest

- Feature statement: "Downloads from and backs up to multiple devices simultaneously."
  (README, <https://github.com/damonlynch/rapid-photo-downloader/blob/a119bc4/README.md>)
- In-app tip text: "You can simultaneously download from multiple cameras, smartphones,
  memory cards, and hard drives — as many devices as your computer can handle at one time."
  (<https://github.com/damonlynch/rapid-photo-downloader/blob/a119bc4/raphodo/ui/didyouknow.py#L91-L98>)
- Architecture: the program is "multiple OS-level processes that communicate with each
  other using the messaging library 0MQ" — one pipeline per device, not a thread pool
  (README, same URL; design notes at <https://damonlynch.net/rapid/design.html>).
- Device identity: every source device gets a `scan_id`; files, download records, and
  even provisional destination folders are keyed by it
  (<https://github.com/damonlynch/rapid-photo-downloader/blob/a119bc4/raphodo/rpdsql.py#L72-L96>,
  <https://github.com/damonlynch/rapid-photo-downloader/blob/a119bc4/raphodo/folderpreviewmanager.py#L219-L241>).
- Per-device progress: `DownloadTracker` keeps per-`scan_id` stats (files downloaded /
  failed, bytes copied, bytes backed up) and exposes both `get_percent_complete(scan_id)`
  and `get_overall_percent_complete()` — i.e., UI shows per-device *and* aggregate
  progress
  (<https://github.com/damonlynch/rapid-photo-downloader/blob/a119bc4/raphodo/downloadtracker.py>).
- Raw copying streams chunk-by-chunk and reports byte-granular progress back to the UI
  process
  (<https://github.com/damonlynch/rapid-photo-downloader/blob/a119bc4/raphodo/copyfiles.py#L137-L167>,
  L239-L259).
- Copy-then-rename design: every file is first copied into a temp dir under a **random
  5-digit name** (because duplicates across source folders can collide), then verified,
  then renamed/moved into the final generated folder structure
  (<https://github.com/damonlynch/rapid-photo-downloader/blob/a119bc4/raphodo/copyfiles.py#L529-L538>
  plus `renameandmovefile.py`). Filename-conflict handling happens after the bytes are
  safely on disk.

### Rename templates / variables

- Names (for files and for generated subfolders) are composed from component
  categories, each with a two-level choice (category → variant): **Date-Time** (many
  formats), **Filename**, **Metadata**, **Sequences** ("Downloads today", "Stored
  number", "Session numbers", "Sequence letter"), **Text**, **Job code**, and
  image/video number — see the imported constants and dispatch in
  <https://github.com/damonlynch/rapid-photo-downloader/blob/a119bc4/raphodo/generatename.py>
  and the user-facing strings ("Downloads today", "Stored number", "Sequence letter") in
  <https://github.com/damonlynch/rapid-photo-downloader/blob/a119bc4/po/rapid-photo-downloader.pot#L680-L704>.
- Sequence semantics: "Downloads today" resets daily; "Stored number" persists across
  sessions; RAW+JPEG pairs can share one sequence number (documented in CHANGES.md,
  <https://github.com/damonlynch/rapid-photo-downloader/blob/a119bc4/CHANGES.md> L1118).
- A **job code** (free-text per shoot, e.g. client name) is a first-class rename and
  subfolder variable (po strings, same file L502-L521).

### Backup to second (and further) destinations

- Backups happen *while* downloading, to any number of destinations, and "If you are
  backing up to more than one device, Rapid Photo Downloader will backup to each device
  simultaneously instead of one after the other."
  (<https://github.com/damonlynch/rapid-photo-downloader/blob/a119bc4/CHANGES.md> L911;
  README feature list "Back up photos and videos as they are downloaded.")
- Key detail: the backup worker copies **from the freshly downloaded temp file**, not
  from the card (`source = rpd_file.download_full_file_name`,
  <https://github.com/damonlynch/rapid-photo-downloader/blob/a119bc4/raphodo/backupfile.py#L195-L199>)
  and each backup device gets its own worker (`worker_id=device_id` dispatch in
  <https://github.com/damonlynch/rapid-photo-downloader/blob/a119bc4/raphodo/rapid.py#L3855-L3868>).
- Already-backed-up files are detected and skipped (or overwritten per preference), with
  distinct problem records for "backup already exists"
  (<https://github.com/damonlynch/rapid-photo-downloader/blob/a119bc4/raphodo/backupfile.py#L170-L187>).

### Checksum verification

- With verification enabled, the MD5 of the source bytes is computed **in memory while
  copying** (no second read of the card):
  <https://github.com/damonlynch/rapid-photo-downloader/blob/a119bc4/raphodo/copyfiles.py#L153-L165>
  (filesystem path) and L264-L292 (libgphoto camera path via `return_file_bytes`).
- The backup is verified by re-reading the backup file and comparing its MD5 to the
  download's MD5
  (<https://github.com/damonlynch/rapid-photo-downloader/blob/a119bc4/raphodo/backupfile.py#L200-L204>).
- Caveat worth knowing: on current `main` the preference is force-disabled at startup
  (`self.prefs.verify_file = False`,
  <https://github.com/damonlynch/rapid-photo-downloader/blob/a119bc4/raphodo/rapid.py#L580>;
  default in preferences L382) — the mechanism exists but is switched off, presumably a
  performance decision (Python MD5 of full bytes on the copy path). A Rust port can make
  this effectively free with BLAKE3/xxHash.

---

## 2. Photo Mechanic (Camera Bits)

### Ingest workflow (Ingest dialog, Cmd/Ctrl+G)

All from "Ingesting Photos":
<https://docs.camerabits.com/support/solutions/articles/48000207409-ingesting-photos>

- Ingest is a **background task**: "you won't have to wait for the ingest process to
  finish before you can work on your images"; a contact sheet opens to the destination
  *as soon as some files are copied* (default), or after ingest, or never.
- **Sources**: Disks (cards, drives — with Rescan/Unmount/Hide), Folders, or From
  Selection. Multiple disks/folders can be selected at once (Cmd/Ctrl-click) for a
  single ingest session.
- **Destinations**: a Primary destination plus an optional **Secondary** destination
  (second copy on ingest). By default PM creates a `YYYYMMDD` dated folder, but folder
  names are fully templated.
- **Destination folder templates**: "into folder with name" accepts plain text plus
  variables, and `/` (macOS) or `\` (Windows) inside the name **creates nested
  subfolders** — the docs' own example is
  `/Pictures/{year4}/{datesortdash} - Event Name Here/` (variables resolve from EXIF).
  Options: directly into destination, dated folder only, named folder, dated+named.
  "Use Folder Sequence" adds `{ingestsequence}`, which increments **per ingest source**
  rather than per photo.
- **Source directory structure**: Ignore / Keep separate source folder names only /
  Preserve all source directories.
- **Incremental Ingest**: "only new photos since the last Ingest on this computer will
  be copied" — PM remembers per-computer which images were already ingested (no
  database needed for this).
- **Auto Ingest**: begin an ingest automatically on card mount, reusing the most recent
  Ingest window settings.
- **Filter Files**: copy everything (default), locked-only first ("Copy Locked and
  Unlocked Photos" copies locked files first), or by file type.
- **Metadata on ingest**: "Apply Metadata Template to Photos" with a **Local template**
  (exists only in the Ingest window) or the **Global** template used everywhere else.
  The docs' tip: put per-shoot facts (copyright, location) in the local ingest template,
  then progressively apply broader templates later.
- **Rename on ingest**: "Rename Ingested Photos As" with variables + a Sequence number
  whose starting value PM **remembers between ingests** until reset.
- **After ingest**: Unmount source disk(s); Erase source disk(s) after ingest (docs
  themselves recommend against erasing).
- Snapshots button (lightning bolt) saves/restores the whole dialog state.

### Live Ingest

From "[Legacy] Live Ingest":
<https://docs.camerabits.com/support/solutions/articles/48001252609--legacy-live-ingest>

- "Live Ingest monitors a folder and ingests new photos that appear in it according to
  your specifications. This is used for shooting wirelessly or while tethered." It runs
  until cancelled / app close.
- **File-stability delay**: waits a configurable delay (default 30 s) before ingesting a
  new file "to make sure the photo has finished loading" (important for RAWs); option
  "Wait for RAW+JPEG pairs when {seqn} is used".
- Speed tip from the same doc: shoot RAW+JPEG but transfer **only the JPEG** to the
  laptop, cull/rate the JPEGs live, then ingest the RAWs from the card later and merge
  metadata with Tools → Update IPTC/XMP → "Read IPTC/XMP from JPEG" (with Combined
  RAW+JPEG view).

### Send Photos (FTP/SFTP) semantics

From "Upload to Photo Sharing & Storage Services":
<https://docs.camerabits.com/support/solutions/articles/48000361261>

- Entry points: File → **FTP Photos as...** or right-click → Upload; the File Uploader
  window manages **Connections** (saved accounts / FTP credentials per service).
- Supported: FTP, FTP v6 (IPv4+IPv6), SFTP, plus ~15 cloud/photo services.
- **Metadata on upload**: each uploader has its own Local metadata template plus the
  Global one; "Applying the template on upload only affects the transferred files, not
  the local files." The Local template is remembered **per connection**.
- **Background behavior**: after clicking Send, an "Upload Tasks" window opens *in the
  background* showing the queue and errors; each Contact Sheet thumbnail gets a status
  indicator — **green** = last attempt succeeded, **yellow** = in progress (doubles as a
  per-file progress bar), **red** = failed.
- What gets transmitted is governed by the same engine as "Save Photos As"
  (<https://docs.camerabits.com/support/solutions/articles/48001077663>): original or
  resized/cropped, JPEG quality or **file-size limit** ("Limit File Size... will attempt
  to make the best quality image possible within the filesize limit"), convert to sRGB,
  sharpen, apply IPTC template, rename, watermark; output never overwrites existing
  files (renaming-resolution preference).

### Variables

From "Introduction to Variables"
(<https://docs.camerabits.com/support/solutions/articles/48000207639-introduction-to-variables-in-photo-mechanic>)
and the full "List of Photo Mechanic Variables"
(<https://docs.camerabits.com/en/support/solutions/articles/48000358438>):

- Syntax: `{name}` placeholders usable "in most text fields within Photo Mechanic" —
  filenames, folder names, and **inside other metadata fields** ("including a metadata
  field's variable into the textbox of another metadata field").
- Size: ~300 variables grouped as: Camera/Image-specific (`{filename}`, `{frame}`,
  `{iso}`, `{serialnum}`, ...), APEX values, Standard IPTC fields (`{caption}`,
  `{keywords}`, `{byline}`, `{credit}`, `{transref}`, `{urgency}`, ...), Time & Date
  (`{date}`, `{datesort}` = YYYYMMDD, `{datesortdash}` = YYYY-MM-DD, `{year4}`,
  `{month0}`, `{day0}`, `{hour24}`, `{timesort}`, plus IPTC-date variants `{iptcdate}`,
  `{iptcyear4}`, `{iptcday0}`, `{iptchour24}`, `{iptctime}` and "now"/"today" variants),
  ICC, GPS, Special (`{sequence}`/`{seqn}`/`{auto}`, `{ingestsequence}`, `{index}`,
  `{total}`, `{rand3}`/`{rand4}`, `{nl}`, `{tab}`), User variables (`{jobname}`,
  address/contact fields), Client variables, and — telling — a whole **Getty variables**
  section (`{callforimage}`, `{routingdest}`, `{imagerank}`, ...) for wire-agency
  integration.
- `{seqn}` is a program-wide counter incremented per photo, with configurable start and
  count-by ("The Sequence Variable",
  <https://docs.camerabits.com/support/solutions/articles/48001205803-the-sequence-variable>).

(Code replacements: already documented in our own notes — skipped here per task.)

---

## 3. digiKam & Photini (IPTC templates / batch metadata)

### digiKam

- **Metadata Templates Manager** (Settings → Configure digiKam → Templates): templates
  group fields into **Rights** (Authors, Author Positions, Credit, Copyright, Right
  Usage Terms, Source, Instructions), **Location**, **Contact**, **Subjects** (IPTC
  Subject Codes). They are applied from the Captions sidebar and from the camera Import
  window's "On the Fly Operations (JPEG only)". Templates live in a plain file at
  `~/.local/share/digikam/` for backup/versioning.
  <https://docs.digikam.org/en/setup_application/templates_settings.html>
- **Import-time options**: file renaming from metadata fields with modifiers (same
  engine as the Advanced Rename tool), auto-creation of **date-based sub-albums** with
  custom date formats where a path separator creates nesting (`yyyy/MM/dd` → nested
  folders), plus JPEG-only lossless transforms/DNG conversion and a scripting hook
  (`%file`, `%orgfilename`...). Renaming/album creation deliberately uses metadata read
  **after** download because camera drivers can't always expose it before.
  <https://docs.digikam.org/en/import_tools/advanced_import.html>
- **Batch Queue Manager → Metadata Tools**: Apply Metadata Template, Assign Captions
  (title/captions/author), Assign Labels (pick/rating/color), Remove Metadata
  (fine-grained per standard), Time Adjust, Translate — i.e., metadata application is a
  queueable batch stage like any other tool.
  <https://docs.digikam.org/en/batch_queue/metadata_tools.html>
- **Metadata Synchronizer**: reconciles metadata between the digiKam *database* and the
  *files*, in either direction, respecting the Metadata settings page.
  <https://docs.digikam.org/en/maintenance_tools/maintenance_metadata.html>

### Photini

- Purpose-built minimal metadata editor: "doesn't try to be an all-singing,
  all-dancing image management powerhouse". Reads Exif/IPTC/XMP, **writes all three**
  "to maximise compatibility", optionally to XMP sidecars.
  <https://photini.readthedocs.io/en/latest/other/introduction.html>
- **Multi-image editing UX** (the part worth stealing): select N images and type once;
  fields showing `<multiple values>` offer a right-click menu to **copy one existing
  value to all selected**; whole-tab copy/paste via a private clipboard; explicit save
  (Ctrl+S) with per-thumbnail "unsaved changes" markers.
  <https://photini.readthedocs.io/en/latest/manual/descriptive_metadata.html>
- Legacy **IPTC-IIM byte limits** are surfaced inline (over-length text underlined
  blue) instead of silently truncating — same doc.

---

## 4. Lightroom Classic (Adobe) — import dialog essentials & tethered

### Import dialog

From "Specify import options":
<https://helpx.adobe.com/lightroom-classic/desktop/import-photos/photo-video-import-options.html>

- Right-hand panels: **File Handling**, **File Renaming**, **Apply During Import**.
- **File Handling**:
  - Build Previews: **Minimal** (use smallest embedded previews, render standard later),
    **Embedded & Sidecar** (largest camera-embedded preview — fast but not color
    managed), **Standard**, **1:1**; optional **Smart Previews**.
  - **Make A Second Copy To** — one-time backup of originals during a Copy/Move import
    (simpler than RPD's N-way live backup).
  - **Don't Import Suspected Duplicates**: duplicate = same *original filename* + same
    *EXIF capture date/time* + same *file size*.
- **File Renaming**: template menu (Custom Name - Sequence, Date - Filename, Shoot
  Name - Original File Number, Filename - Sequence, Edit...) + Custom Text + Start
  Number. (Template editor: "The Filename Template Editor and Text Template Editor",
  <https://helpx.adobe.com/lightroom-classic/desktop/import-photos/filename-template-editor-text-template.html>.)
- **Apply During Import**: a **Develop Settings preset**, a **Metadata preset**
  (None / saved preset / New...), and **Keywords** (comma-separated). For proprietary
  RAWs the metadata is written to XMP sidecars, not the files.
- **Import presets**: "Save Current Settings As New Preset" — the whole dialog is
  savable/reusable; plus a "Show Fewer Options" minimal window mode.

### Tethered / live import

From "Import photos from a tethered camera":
<https://helpx.adobe.com/lightroom-classic/desktop/import-photos/import-photos-tethered-camera.html>

- File → Tethered Capture → Start Tethered Capture: a **Session** name (folder), optional
  "Segment Photos By Shots" (subfolders per shot), Destination, **Information** (metadata
  template + keywords applied to captures), Auto Advance toggle; the floating tether bar
  can apply a Develop preset on capture and control shutter/aperture/ISO/WB. Supports
  Canon/Nikon/Sony/Fujifilm bodies; Live View with focus control for some.
- The LrC equivalent of Live Ingest, but locked to its own tether session rather than
  watching arbitrary folders.

---

## 5. Wire / sports-photo pipelines (Reuters/AP-style PM+FTP)

Agencies don't publish internal pipeline docs; the public record is first-person
accounts from wire shooters plus Camera Bits' own docs (§2). Two detailed public
accounts:

- **Scott Kelby, shooting NFL for a sports wire service**
  (<https://scottkelby.com/my-sports-photography-workflow-so-far/>):
  - "If there are 40 photographers in the photo work room, you see 40 copies of Photo
    Mechanic open on their laptops."
  - Halftime routine (target <15 min): download **two cards at once using two card
    readers**; PM Ingest applies a pre-written **IPTC Stationary Pad** (the old name for
    the Metadata Template — game, venue, copyright, updated score) and renames; cull by
    Tag (T) then color-label the final 6–8; caption each keeper with **Code
    Replacements** (jersey number → full "Team position Name (number)" string) so a full
    wire caption takes ~20 s; crop/tweak in ACR; export JPEGs; "you can FTP right from
    the program straight to the server of your local wire service."
  - Comments on the same page add the classic wire trick: lock/protect keepers **in
    camera** during breaks, then have Ingest copy only locked files first (matches PM's
    documented "copies the locked files first").
- **JC Ruiz, NFL/MLB photographer** (<https://www.jcruizphotography.com/sports-photography-workflow/>):
  uses PM **Live Ingest** (starts culling/deleting while the card is still ingesting),
  pre-loads keywords + a generic caption + metadata **before** ingest "so every photo
  starts off with the exact same information", then tag/star, Code Replacements for
  captions, and uploads several times per game ("after warmups, at halftime and after
  the post game handshakes").
- Supporting evidence that this is *the* wire pattern lives in PM itself: the Variables
  list has a dedicated **Getty Variables** section (call-for-image, routing
  destinations/exclusions — the vocabulary of agency picture desks), and IPTC fields
  like Transmission Reference (`{transref}`) and Urgency exist for wire metadata
  (<https://docs.camerabits.com/en/support/solutions/articles/48000358438>).

No comparably minimal/fast dedicated tool surfaced beyond PM itself; RPD is the open
alternative for the ingest half (§1), and generic "sports FTP" guides simply restate
the PM→Save As→FTP pipeline (e.g. <https://ftpsuite.com/blog/sports-photography-ftp-workflow/>).

---

## What to steal / what to skip (minimal v1)

v1 target: multi-card parallel ingest + IPTC templates on ingest + variables in
filenames/folders/descriptions + background FTP send.

### Steal

1. **Per-device ingest pipelines (RPD).** One independent download task per mounted
   card/device, with per-device progress (files, bytes, %) plus an aggregate bar
   (`downloadtracker.py` model). Rust: one task per device over a channel; trivially
   parallel in GPUI.
2. **Copy-to-temp → verify → resolve → rename (RPD).** Copy bytes under a temp name,
   run checksum + name-conflict resolution, then move into the generated structure.
   Names are never half-applied and re-ingests can't corrupt existing photos.
3. **Backup from the downloaded copy, to N destinations in parallel (RPD).** Never
   re-read the card for backups; mirror the temp file to each secondary destination
   concurrently and mark per-file backup status.
4. **Cheap checksum on by default.** RPD hashes source bytes in memory during the copy
   and compares the backup copy against it. Do it with BLAKE3/xxHash so it stays on by
   default (RPD had to disable MD5 verification on main for performance —
   `rapid.py#L580`).
5. **PM Ingest dialog semantics:** multi-select sources; primary + secondary
   destination; folder templates with variables where `/` nests subfolders; incremental
   ingest remembered per machine; auto-ingest on card mount; unmount after; open the
   browser *during* ingest so culling starts before the copy finishes; snapshot/saveable
   ingest presets (also LrC's "Save Current Settings As New Preset").
6. **PM variables as the v1 variable set:** `{filename}`/`{filenamebase}`, date parts
   (`{year4}`, `{month0}`, `{day0}`, `{datesortdash}`, capture-time `{hour24}`...), a
   few IPTC-value variables (`{iptcdate}`, `{caption}`, `{keywords}`), `{seqn}` with a
   remembered start value, `{ingestsequence}` per-source counter, and allow variables
   **inside metadata template fields** (PM's quiet superpower — one caption template
   adapts per photo).
7. **Local vs global IPTC template (PM).** A global default template plus an
   ingest-scoped local template (the wire "stationary pad": pre-game metadata + updated
   score) applied at ingest. Fields per digiKam's schema (rights/location/contact/
   subjects) are a good template editor starting point.
8. **PM uploader semantics:** saved FTP/SFTP connections; send runs in background with
   a queue/status window; per-thumbnail green/yellow/red indicators (yellow doubling as
   progress); optional "transmit a processed copy" (resize/quality-limit/sRGB/IPTC
   template applied only to transmitted files, originals untouched).
9. **LrC duplicate detection** (same original filename + EXIF capture time + size) as
   the incremental/"already ingested" fallback when no per-machine DB is desired.
10. **Photini batch-editing UX** for the captioning step: select-many + type once,
    `<multiple values>` with "apply this value to all selected", explicit save with
    per-image dirty markers.
11. **PM Live Ingest's file-stability delay** (watch folder, ~30 s settle, RAW+JPEG
    pairing) when live/tethered ingest lands — it's the correct minimal design.

### Skip (for v1)

1. **Catalogs/DAM databases** — LrC catalogs, PM Plus catalogs, digiKam's database +
   Metadata Synchronizer. File-based + incremental-memory (PM) covers v1.
2. **RAW rendering on the ingest path** — no develop presets at import (LrC), no RAW
   decode, no preview building beyond embedded JPEGs (PM's speed lesson).
3. **Cloud/social uploaders** — PM's ~15 services, digiKam exporters, Photini's
   Flickr/Ipernity/Pixelfed. FTP/SFTP only.
4. **digiKam's import-time transforms** — lossless rotate/convert, DNG conversion,
   import scripting hooks. v1 copies bit-exact.
5. **digiKam batch extras** — metadata translation, deep-learning quality scan/labels.
6. **Erase-card-after-ingest (PM)** — PM's own docs recommend against it.
7. **Code replacement engine** — needed for wire captions, but already separately
   documented by us; schedule after v1.
8. **Smart Previews / Assisted Culling (LrC), geotagging, face recognition** — out of
   scope.

---

## Source index

- RPD source (commit a119bc4): <https://github.com/damonlynch/rapid-photo-downloader/tree/a119bc4>
- RPD docs (official): <https://damonlynch.net/rapid/documentation/>, design: <https://damonlynch.net/rapid/design.html>
- PM Ingest: <https://docs.camerabits.com/support/solutions/articles/48000207409-ingesting-photos>
- PM Live Ingest: <https://docs.camerabits.com/support/solutions/articles/48001252609--legacy-live-ingest>
- PM Uploader/FTP: <https://docs.camerabits.com/support/solutions/articles/48000361261>
- PM Save As (transmit processing): <https://docs.camerabits.com/support/solutions/articles/48001077663>
- PM Variables intro: <https://docs.camerabits.com/support/solutions/articles/48000207639-introduction-to-variables-in-photo-mechanic>
- PM Variables list: <https://docs.camerabits.com/en/support/solutions/articles/48000358438>
- PM {seqn}: <https://docs.camerabits.com/support/solutions/articles/48001205803-the-sequence-variable>
- digiKam templates: <https://docs.digikam.org/en/setup_application/templates_settings.html>
- digiKam import: <https://docs.digikam.org/en/import_tools/advanced_import.html>
- digiKam batch metadata: <https://docs.digikam.org/en/batch_queue/metadata_tools.html>
- digiKam metadata sync: <https://docs.digikam.org/en/maintenance_tools/maintenance_metadata.html>
- Photini intro: <https://photini.readthedocs.io/en/latest/other/introduction.html>
- Photini descriptive metadata: <https://photini.readthedocs.io/en/latest/manual/descriptive_metadata.html>
- LrC import options: <https://helpx.adobe.com/lightroom-classic/desktop/import-photos/photo-video-import-options.html>
- LrC filename template editor: <https://helpx.adobe.com/lightroom-classic/desktop/import-photos/filename-template-editor-text-template.html>
- LrC tethered capture: <https://helpx.adobe.com/lightroom-classic/desktop/import-photos/import-photos-tethered-camera.html>
- Kelby wire workflow: <https://scottkelby.com/my-sports-photography-workflow-so-far/>
- Ruiz sports workflow: <https://www.jcruizphotography.com/sports-photography-workflow/>
