# App plumbing research: volumes, job queues, GPUI async, packaging, secrets

Date: 2026-08-16
Scope: cross-platform (Linux/Windows/macOS) Rust + GPUI desktop photo app — the boring-but-critical plumbing.
Method: every claim below was checked against a primary source (docs.rs, crate APIs and source, Microsoft Learn, official repos). Facts I could not verify are called out explicitly in [Unverified / open questions](#unverified--open-questions).

---

## 1. Detecting mounted volumes / camera cards

### sysinfo — good baseline for mount points, NOT for volume labels on Linux

`sysinfo` 0.39.6 exposes a dedicated disks API: `Disks::new_with_refreshed_list()`, and `Disk::{name, mount_point, file_system, kind, total_space, available_space, is_removable, is_read_only, usage}`.
- API: https://docs.rs/sysinfo/latest/sysinfo/struct.Disk.html and https://docs.rs/sysinfo/latest/sysinfo/struct.Disks.html

What `Disk::name()` actually is, is platform-dependent. I verified the Linux implementation in source:
- The list is built by parsing `/proc/mounts`; `Disk::name()` returns `device_name`, which is the `fs_spec` column — i.e. the device node like `/dev/sdb1`, **not** a volume label like `EOS_DIGITAL`. Source: https://docs.rs/sysinfo/latest/src/sysinfo/unix/linux/disk.rs.html (see `DiskInner::name()` returning `&self.device_name`, and `get_all_list` pushing `fs_spec` as `device_name`).
- `is_removable()` on Linux is true iff the device resolves to an entry under `/dev/disk/by-id/` whose name starts with `usb-` (same file, `get_all_list`). This catches USB card readers (the normal camera-card case), but is a heuristic, not "is a camera".
- Pseudo/kernel filesystems (`proc`, `sysfs`, `cgroup`, `iso9660`, …) are filtered out by fs type; `tmpfs` and network mounts (CIFS/NFS/autofs) are excluded **by default** behind the `linux-tmpfs` / `linux-netdevs` features, and the docs warn that `statvfs` on hard-mounted NFS/CIFS can hang. See the warnings on https://docs.rs/sysinfo/latest/sysinfo/struct.Disks.html
- sysinfo provides **no volume serial numbers** on any platform (no such method exists on `Disk`).

Windows/macOS `name()` semantics are not documented per-platform in the docs (see [Unverified](#unverified--open-questions)); sysinfo's macOS implementation uses IOKit/CoreFoundation/OpenDirectory bindings, not DiskArbitration (visible in the platform dependency list on the docs.rs crate page).

### notify — file events, not a hotplug API (use after the card is mounted)

`notify` 8.2.0 gives inotify (Linux) / FSEvents (macOS) / ReadDirectoryChangesW (Windows) via `recommended_watcher`, plus a `PollWatcher` fallback. Its documented "Known Problems" matter for us:
- Pseudo-filesystems like `/proc` and `/sys` do not emit events → use `PollWatcher` with `compare_contents` (this also matters if you try to watch `/proc/mounts` for mount changes).
- Network filesystems (NFS, WSL watching Windows paths) may emit nothing.
- Linux inotify per-user watch limits can break large recursive watches (`fs.inotify.max_user_watches`).
- Source: https://docs.rs/notify/latest/notify/ (sections "Known Problems", examples)

Conclusion: use `notify` to watch a mounted card's `DCIM/` tree for new files; do **not** use it to detect card insertion/removal.

### Linux: udev — the right tool for hotplug + labels

The `udev` crate 0.9.3 (maintained under the Smithay org, libudev bindings) provides exactly what camera-card detection needs:
- Hotplug events: `udev::MonitorBuilder::new()?.match_subsystem_devtype("block", "partition")?.listen()` returns a `MonitorSocket` that implements `AsFd`/`AsRawFd` (integrates with any poll loop; the docs' own example filters on `block`). https://docs.rs/udev/latest/udev/struct.MonitorBuilder.html
- Enumeration of current devices via `Enumerator`, and `Device::property_value(...)` for udev properties. On a FAT32 card, udev populates `ID_FS_LABEL` (this is literally where `EOS_DIGITAL` shows up), `ID_FS_UUID`, `ID_FS_TYPE`, `DEVNAME`, etc. (crate items: https://docs.rs/udev/latest/udev/ — `Enumerator`, `Device`, `Properties`).

### Windows: three Win32 calls + one window message

All verified on Microsoft Learn:
- `GetLogicalDrives()` — bitmask of assigned drive letters (bit 0 = A:, bit 1 = B:, …). https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-getlogicaldrives
- `GetVolumeInformationW(root, ...)` — returns the **volume name** (label, e.g. `EOS_DIGITAL`) via `lpVolumeNameBuffer` and the **volume serial number** via `lpVolumeSerialNumber` (the OS-assigned, format-time serial). https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-getvolumeinformationw
- `GetDriveTypeW(root)` — `DRIVE_REMOVABLE` (2) covers "floppy drive, thumb drive, or flash card reader". https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-getdrivetypew
- Push notifications: `WM_DEVICECHANGE` window message (`DBT_DEVICEARRIVAL` 0x8000, `DBT_DEVICEREMOVECOMPLETE` 0x8004, `DBT_DEVICEREMOVEPENDING` 0x8003), delivered to a `WindowProc` — i.e. you need a Win32 window (a GPUI app has one). https://learn.microsoft.com/en-us/windows/win32/devio/wm-devicechange

### macOS: DiskArbitration bindings exist

`objc2-disk-arbitration` 0.3.2 (part of the objc2 framework bindings, updated 2026-08) exposes:
- `DASession` (+ `DASessionScheduleWithRunLoop` / `DASessionSetDispatchQueue`),
- push callbacks: `DARegisterDiskAppearedCallback`, `DARegisterDiskDisappearedCallback`, `DARegisterDiskDescriptionChangedCallback`,
- `DADiskCreateFromVolumePath` + `DADiskCopyDescription` (the description dictionary),
- description keys: `kDADiskDescriptionVolumeNameKey` (the `EOS_DIGITAL` label), `kDADiskDescriptionVolumeUUIDKey`, `kDADiskDescriptionMediaRemovableKey`, `kDADiskDescriptionMediaEjectableKey`, `kDADiskDescriptionVolumeMountableKey`, …
- Source: https://docs.rs/objc2-disk-arbitration/latest/objc2_disk_arbitration/

Caveat: the crate's docs mark many of these C functions ⚠ *Deprecated* (they now carry deprecation annotations from Apple's SDK, and an `kDADiskDescriptionFSKitPrefix` constant exists) — Apple is steering new code toward FSKit, but DiskArbitration remains the working mechanism for volume names/mount events today.

### Recommendation (volumes)

- Baseline on all 3 OSes: poll `sysinfo::Disks::refresh(true)` every ~2–5 s → gives mount point, removable flag, fs type, capacity. Treat `Disk::name()` as opaque (device node on Linux).
- Enrich per platform:
  - Linux: `udev` monitor on `block` subsystem for instant insert/remove; read `ID_FS_LABEL` / `ID_FS_UUID` from device properties for card naming and stable identity.
  - Windows: `GetLogicalDrives` + `GetDriveTypeW(DRIVE_REMOVABLE)` + `GetVolumeInformationW` (label + serial). Optionally handle `WM_DEVICECHANGE` later for push; polling is a fine v1.
  - macOS: `objc2-disk-arbitration` — `DARegisterDiskAppeared/Disappeared` + `DADiskCopyDescription` for `VolumeName`/`VolumeUUID`.
- Use `notify` (debounced, e.g. `notify-debouncer-full`) only to watch the mounted `DCIM` tree while ingesting.
- Identity across sessions: prefer filesystem UUID (udev `ID_FS_UUID`, Windows volume serial, macOS `VolumeUUID`) over label — labels collide ("EOS_DIGITAL" is every Canon card).

---

## 2. Background job queue / thread pool

### What's available

- **rayon** 1.12.0: work-stealing data parallelism — parallel iterators, `join`, `scope`, custom `ThreadPoolBuilder`. Ideal for *batch CPU work inside one job* (e.g. generating 500 thumbnails), not a durable job queue. https://docs.rs/rayon/latest/rayon/
- **Channels**: `std::sync::mpsc` for simple cases; `crossbeam-channel` and `flume` are the two multi-producer/multi-consumer options in common use — evidence they're battle-tested in this exact space: `notify` uses `crossbeam-channel`/`flume` as optional event senders (https://docs.rs/notify/latest/notify/ dependency list), and **GPUI itself depends on `flume`** on Linux (see gpui dependency list in §3).
- **Durable queue = SQLite**:
  - `rusqlite` 0.40.2 — "Ergonomic wrapper for SQLite", 92.9M downloads, last release 2026-08-08. Has `bundled` / `bundled-windows` features (compile SQLite in via `libsqlite3-sys`, so you don't depend on the distro's sqlite) and a `serde_json` feature for JSON payloads. https://crates.io/crates/rusqlite and https://docs.rs/crate/rusqlite/latest/features
  - `sqlx` 0.9.0 — "async, pure Rust SQL crate featuring compile-time checked queries… Supports PostgreSQL, MySQL, and SQLite", last release 2026-05-21. https://crates.io/crates/sqlx

### Recommendation (jobs)

- **Use `rusqlite` (with `bundled`), not `sqlx`**, for the embedded desktop database/queue: it's synchronous (call it from worker threads or `BackgroundExecutor` tasks — no tokio runtime needed in a GPUI app), lighter, and `bundled` removes system-sqlite variance across the three OSes. `sqlx`'s wins (async pool, compile-time checked queries against a live DB) pay off in servers, and would drag in a full async runtime GPUI doesn't use.
- Architecture (roll-your-own, ~200 lines, no external "jobqueue" crate):
  - One `jobs` table: `id INTEGER PK, kind TEXT, payload TEXT(json), state TEXT(queued|running|done|failed), attempts INT, error TEXT, created_at, updated_at`.
  - N dedicated worker `std::thread`s (or GPUI background tasks, see §3) claiming jobs atomically: `BEGIN; SELECT ... WHERE state='queued' ORDER BY id LIMIT 1; UPDATE ... SET state='running'; COMMIT` (SQLite serializes writers, so no races).
  - Progress: worker sends `(job_id, progress)` over a `flume`/`crossbeam-channel` channel; a GPUI `cx.spawn` task drains it and updates UI entities.
  - CPU-heavy inner loops (thumbnailing) use `rayon` inside the worker.
- This survives crashes (jobs restart as `queued` on launch if `state='running'`), which matters for long ingest/export/upload jobs.

---

## 3. GPUI async: executors, tasks, and how jobs should run

gpui is now published on crates.io (`gpui` 0.2.2, owned by `zed-industries`, repo `zed-industries/zed`, site gpui.rs). Key verified facts:

- The crate docs describe "An async executor that is integrated with the platform's event loop. See the `executor` module" — i.e. async and the UI runloop are one system. https://docs.rs/gpui/latest/gpui/
- **`Task<T>`** implements `Future`; "If you drop a task it will be cancelled immediately. Calling `Task::detach` allows the task to continue running, but with no way to return a value." Also `Task::detach_and_log_err(cx)`. https://docs.rs/gpui/latest/gpui/struct.Task.html
- **`BackgroundExecutor`**: "In production this is a thread pool with no ordering guarantees." Key methods (all verified): `spawn(future: impl Future + Send + 'static) -> Task<R>`, `spawn_labeled`, `timer(duration) -> Task<()>`, `num_cpus()`, `is_main_thread()`, `scoped(...)` (wait for a group of tasks), `block` / `block_with_timeout`. https://docs.rs/gpui/latest/gpui/struct.BackgroundExecutor.html
- **`App::spawn`** gives you an `AsyncApp` handle usable across `await` points ("You're provided with an instance when calling `App::spawn`"); there's also `AsyncWindowContext` for window-scoped spawns, and a `ForegroundExecutor` for main-thread tasks. https://docs.rs/gpui/latest/gpui/
- **Executor internals are smol-family**: gpui's dependency list includes `smol ^2.0`, `async-task ^4.7`, `waker-fn`, `parking`, `num_cpus`, `futures`, `postage`, `flume` — i.e. the same async-task-based executor pattern as smol, not tokio. https://docs.rs/gpui/latest/gpui/ (Dependencies)
- **Platform stacks** (from the same dependency list, per-target):
  - Linux: `blade-graphics` (Vulkan) for rendering, `wayland-client` + `x11rb` + `xkbcommon` + `calloop` for windowing/input, `cosmic-text` for text, `oo7` (Secret Service) and `ashpd` (portals).
  - macOS: Metal (`metal`, `objc2-metal`, `core-…`).
  - Windows: `windows ^0.61`, `windows-core`, `windows-numerics`, `windows-registry` target deps exist in 0.2.2 — and Zed ships a Windows build today (https://zed.dev/download shows macOS, Windows, Linux downloads). Note the crate README's "Getting Started" text still says "be on macOS or Linux" — it lags the actual Windows support.

### Recommendation (GPUI async)

- Run ingest/export/upload **on GPUI's executor**: `cx.background_spawn(async move { ... })` (equivalently `cx.background_executor().spawn(...)`) for the long work; keep the returned `Task` in the view that owns the job so dropping it cancels cleanly, or `.detach()` for fire-and-forget.
- Post progress to the UI with a `cx.spawn` loop that `await`s a channel (`flume`/`smol::channel`-style receiver) and calls `cx.update(...)` / notifies an entity — this is the pattern the executor design (background pool + foreground event loop) implies; never touch entities from background threads directly.
- Use `BackgroundExecutor::timer` for backoff/retry, `scoped` for parallel batches, and rayon inside a background task for CPU fan-out.
- Plain `std::thread`s are also fine (GPUI doesn't forbid them); wrap their results into the executor world via a channel as above. Prefer the executor for anything cancellable.

---

## 4. Packaging & distribution (Linux / Windows / macOS)

Tool state (all version/date facts from crates.io and the repos, checked 2026-08-16):

- **cargo-dist** (`dist`) — **0.32.0**, last release 2026-05-22, actively maintained (3.4k+ commits). It plans/builds "tarballs and installers", generates its own GitHub Actions release pipeline (`dist init` → `release.yml` implementing plan → build → host → publish → announce), uploads to GitHub Releases. Repo: https://github.com/axodotdev/cargo-dist ; book: https://axodotdev.github.io/cargo-dist/book/ ; version data: https://crates.io/crates/cargo-dist
- **cargo-packager** — **0.11.8** (2025-11-27), now at CrabNebula (`crabnebula-dev/cargo-packager`). Explicitly supports the **GUI installer formats we need**: macOS `.app` + `.dmg`; Linux `.deb`, **AppImage**, pacman; Windows **NSIS `.exe`** and **MSI via WiX**. Ships a companion updater (`cargo-packager-updater`). Config via `Packager.toml` / `Cargo.toml [package.metadata.packager]`. https://github.com/crabnebula-dev/cargo-packager
- **cargo-bundle** — revived: 0.7.0 (2025-06) → 0.8.0 (2025-07) → 0.9.0 (2025-09) → 0.10.0 (2026-04) → **0.11.0 (2026-05-30)**, published by mdsteele (edition 2024). The old "unmaintained since 2020" wisdom is outdated. https://crates.io/crates/cargo-bundle (repo burtonageo/cargo-bundle)
- **cargo-wix** — **0.3.9** (2025-03-13), builds MSI with the WiX Toolset. https://crates.io/crates/cargo-wix (repo https://github.com/volks73/cargo-wix)
- **AppImage** itself: official docs + packaging guide at https://docs.appimage.org/

GPUI/Zed-specific packaging notes (from Zed's own docs):

- **Linux system dependencies** (this is the GPUI runtime surface): Zed's `script/linux` installs e.g. `libvulkan1`/`vulkan-loader`, `libwayland-dev`, `libxkbcommon-x11-dev`, `libx11-xcb-dev`/`libxcb-devel`, `libasound2-dev` (ALSA), `libfontconfig-dev`, `libglib2.0-dev`, `libva-dev`, `libssl-dev`, `libzstd-dev`, `libsqlite3-dev`, `pipewire`, `xdg-desktop-portal`. Source: https://github.com/zed-industries/zed/blob/main/script/linux
- Zed's maintainer guide documents the packaging shape: two binaries (a `zed` CLI plus the editor at `../../libexec/zed-editor`), a `.desktop` template (`crates/zed/resources/zed.desktop.in`, FreeDesktop naming, must be executable), lib inspection for the shared-library list, `ZED_UPDATE_EXPLANATION` to redirect auto-update, and current Flatpak sandbox caveats ("Zed's current Flatpak integration exits the sandbox on startup"). Source: https://github.com/zed-industries/zed/blob/main/docs/src/development/linux.md
- Zed itself distributes via install script + documented Linux channels (https://zed.dev/download), not AppImage — deb/rpm repos are the reference path for a GPUI app on Linux.

### Recommendation (packaging)

- Use **cargo-dist** as the release automation backbone (tag → CI builds → GitHub Release with artifacts) — it's the most actively maintained piece and Zed-adjacent projects use it.
- Use **cargo-packager** to produce the actual GUI installers per OS: `.app`/`.dmg` (macOS), `.msi` (WiX) and/or NSIS `.exe` (Windows), `.deb` + AppImage (Linux). It is the one tool that covers exactly this matrix today; cargo-wix is the fallback if we want hand-written WiX; cargo-bundle is a viable lighter alternative now that it's maintained again.
- Linux specifics for a GPUI app: depend on `vulkan-loader`/`libvulkan1`, `libwayland-client`, `libxkbcommon`, `libxcb`, `fontconfig`, `glib`, `alsa-lib`, `libva` in deb/AppImage metadata (list modeled on Zed's `script/linux`); ship a `.desktop` file following Zed's FreeDesktop notes; consider `ZED_UPDATE_EXPLANATION`-style redirect if we ship without an auto-updater; be skeptical of Flatpak sandboxing for an app that mounts filesystems and reads camera cards.

---

## 5. App config & secrets (FTP credentials)

- **`keyring` is at 4.1.6 and actively maintained**, but the architecture changed in v4:
  - The umbrella crate with the **`v1` feature** "behaves essentially the same as the v1 version of Keyring behaved: … easy, platform-independent setting and reading of passwords/secrets on macOS, Windows, and \*nix platforms". https://docs.rs/keyring/latest/keyring/
  - The new split: **`keyring-core`** + per-platform *credential store* crates: `apple-native-keyring-store` (macOS Security framework), `windows-native-keyring-store` (Windows Credential Manager), `dbus-secret-service-keyring-store` (sync DBus Secret Service) and `zbus-secret-service-keyring-store` (async), `linux-keyutils-keyring-store`, plus `db-keystore` and an Android store (full list in the crate's dependency list, same URL). The docs explicitly advise applications that want control to depend on `keyring-core` + the specific stores rather than pulling everything in.
  - Maintenance: repo moved to https://github.com/open-source-cooperative/keyring-rs — originally by Walther Chen, "currently maintained by Dan Brotsky" (brotskydotcom), 700+ commits. https://github.com/open-source-cooperative/keyring-rs
- **Linux backend reality**: "the native secure store" on Linux means the freedesktop **Secret Service** (GNOME Keyring/KWallet via DBus) — that's what the `dbus-secret-service`/`zbus-secret-service` stores talk to; keyutils is a fallback for headless cases.
- Interesting precedent: **GPUI itself depends on `oo7`** (a Rust Secret Service client) on Linux (gpui dependency list, §3) — so Zed stores its secrets through the same mechanism on Linux.

### Recommendation (secrets)

- Store FTP credentials in the OS keychain via `keyring` **`v1` feature** for the simple path (one service/account pair per profile, e.g. `service = "editing-app"`, `account = "ftp:<profile-id>"`); if we want leaner deps later, switch to `keyring-core` + `apple-native-keyring-store` / `windows-native-keyring-store` / `zbus-secret-service-keyring-store` explicitly.
- Keep everything non-secret (profile hosts, usernames if acceptable, preferences) in the app database/settings file — never the password; read the secret lazily at upload time.
- Document for Linux users that a Secret Service implementation (GNOME Keyring/KWallet) is expected — same assumption Zed makes on Linux.

---

## Unverified / open questions

1. **sysinfo `Disk::name()` on Windows/macOS**: Linux behavior verified in source (device node). Windows/macOS per-platform semantics are not documented; I could not reach the platform source files. Verify empirically (likely volume name on Windows, since that's the Win32 idiom) before relying on it — the recommendations above only depend on `mount_point()`, `is_removable()`, `file_system()`, which are documented cross-platform.
2. **GPUI Windows maturity**: Windows target deps exist in gpui 0.2.2 and Zed ships Windows downloads, but the crate README still says "macOS or Linux" — treat Windows as supported-but-young and test early.
3. **DiskArbitration deprecation**: the `objc2-disk-arbitration` docs mark many functions deprecated (FSKit is Apple's stated successor). No replacement crate for FSKit volume monitoring was surveyed.
4. **Job-framework crates** (fang, apalis, etc.) were not investigated — likely overkill for an embedded desktop queue, but not verified here.
5. cargo-dist's installer *format* coverage (which of msi/pkg/homebrew it emits today) lives in its book and wasn't enumerated here; the repo README only claims "tarballs and installers" generically.

## Source index

- sysinfo: https://docs.rs/sysinfo/latest/sysinfo/struct.Disk.html · https://docs.rs/sysinfo/latest/sysinfo/struct.Disks.html · Linux source: https://docs.rs/sysinfo/latest/src/sysinfo/unix/linux/disk.rs.html
- notify: https://docs.rs/notify/latest/notify/
- udev: https://docs.rs/udev/latest/udev/ · https://docs.rs/udev/latest/udev/struct.MonitorBuilder.html
- Win32: https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-getlogicaldrives · https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-getvolumeinformationw · https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-getdrivetypew · https://learn.microsoft.com/en-us/windows/win32/devio/wm-devicechange
- macOS: https://docs.rs/objc2-disk-arbitration/latest/objc2_disk_arbitration/
- jobs: https://docs.rs/rayon/latest/rayon/ · https://crates.io/crates/rusqlite · https://docs.rs/crate/rusqlite/latest/features · https://crates.io/crates/sqlx
- gpui: https://docs.rs/gpui/latest/gpui/ · https://docs.rs/gpui/latest/gpui/struct.Task.html · https://docs.rs/gpui/latest/gpui/struct.BackgroundExecutor.html
- packaging: https://github.com/axodotdev/cargo-dist · https://axodotdev.github.io/cargo-dist/book/ · https://crates.io/crates/cargo-dist · https://github.com/crabnebula-dev/cargo-packager · https://crates.io/crates/cargo-bundle · https://crates.io/crates/cargo-wix · https://docs.appimage.org/ · https://github.com/zed-industries/zed/blob/main/docs/src/development/linux.md · https://github.com/zed-industries/zed/blob/main/script/linux · https://zed.dev/download
- secrets: https://docs.rs/keyring/latest/keyring/ · https://github.com/open-source-cooperative/keyring-rs
