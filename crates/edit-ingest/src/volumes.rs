//! Mounted-volume detection: sysinfo polling plus Linux udev enrichment.
//!
//! The pipeline is intentionally split into pure pieces so unit tests can run
//! with injected fake disk data and no real hardware:
//!
//! ```text
//! DiskSource (sysinfo) ─┐
//!                        ├─► snapshot() ─► Vec<Volume> ─► diff() ─► Vec<VolumeEvent>
//! udev entries (Linux) ──┘
//! ```
//!
//! `Disk::name()` is treated as opaque (on Linux it is the device node from
//! `/proc/mounts`, not a volume label). Labels and filesystem UUIDs come from
//! udev `ID_FS_LABEL` / `ID_FS_UUID` properties joined on the device node.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// How often the volume list is re-polled when no hotplug event arrives.
pub const POLL_INTERVAL: Duration = Duration::from_secs(3);

/// One mounted volume (usually a camera card) as seen by ingest.
///
/// `identity` is the stable key: `uuid:<ID_FS_UUID>` when the filesystem
/// reports a UUID, otherwise `mount:<mount point>`. The human-readable
/// `label` is never part of the identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Volume {
    /// Stable identity: `uuid:<fs uuid>` or `mount:<mount point>`.
    pub identity: String,
    /// Opaque OS device name (e.g. `/dev/sdb1` on Linux). Never shown as a label.
    pub device: String,
    /// Where the volume is mounted.
    pub mount_point: PathBuf,
    /// Human-readable filesystem label (`ID_FS_LABEL`), if any.
    pub label: Option<String>,
    /// Filesystem UUID (`ID_FS_UUID`), if any. Drives [`Volume::identity`].
    pub fs_uuid: Option<String>,
    /// Filesystem type as reported by the OS (e.g. `exfat`, `vfat`).
    pub fs_type: String,
    /// Total capacity in bytes.
    pub capacity_bytes: u64,
    /// Free space in bytes.
    pub free_bytes: u64,
    /// True for USB-attached readers and similar removable media.
    pub removable: bool,
    /// True when mounted read-only.
    pub read_only: bool,
}

impl Volume {
    /// Build the stable identity for a volume: filesystem UUID wins, the
    /// mount point is the fallback. The label is never used.
    pub fn identity_for(fs_uuid: Option<&str>, mount_point: &std::path::Path) -> String {
        match fs_uuid.filter(|uuid| !uuid.is_empty()) {
            Some(uuid) => format!("uuid:{uuid}"),
            None => format!("mount:{}", mount_point.display()),
        }
    }

    /// A plain folder offered as an ingest source ("Choose folder…").
    pub fn folder(path: &Path) -> Self {
        Self {
            identity: format!("folder:{}", path.display()),
            device: String::new(),
            mount_point: path.to_path_buf(),
            label: None,
            fs_uuid: None,
            fs_type: "folder".to_string(),
            capacity_bytes: 0,
            free_bytes: 0,
            removable: false,
            read_only: false,
        }
    }

    /// Whether this looks like a camera card or external drive rather than
    /// a system mount. Removable media and anything under `/media`,
    /// `/run/media` or `/mnt` qualify; `/`, `/home`, `/boot`, `/tmp` and
    /// friends never do. (Anything else can still be chosen as a folder.)
    pub fn is_ingest_source(&self) -> bool {
        const EXTERNAL: &[&str] = &["/media", "/run/media", "/mnt"];
        let mount = self.mount_point.as_path();
        if mount == Path::new("/") {
            return false;
        }
        if EXTERNAL.iter().any(|prefix| mount.starts_with(prefix)) {
            return true;
        }
        if self.removable {
            return true;
        }
        false
    }

    /// Human-readable name: label, then mount directory name, then device.
    pub fn display_name(&self) -> String {
        if let Some(label) = self.label.as_deref().filter(|label| !label.is_empty()) {
            return label.to_string();
        }
        if let Some(name) = self
            .mount_point
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
        {
            return name.to_string();
        }
        self.device.clone()
    }

    /// Percentage of capacity in use, 0–100. Empty volumes report 0.
    pub fn used_percent(&self) -> f32 {
        if self.capacity_bytes == 0 {
            return 0.0;
        }
        let used = self
            .capacity_bytes
            .saturating_sub(self.free_bytes.min(self.capacity_bytes));
        used as f32 / self.capacity_bytes as f32 * 100.0
    }
}

/// One raw disk row from the OS, decoupled from sysinfo so tests can inject
/// fakes. `device` is the opaque OS name (`Disk::name`); labels/UUIDs arrive
/// separately via [`UdevEntry`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskInfo {
    pub device: String,
    pub mount_point: PathBuf,
    pub fs_type: String,
    pub capacity_bytes: u64,
    pub free_bytes: u64,
    pub removable: bool,
    pub read_only: bool,
}

/// One udev block-device row: label and UUID joined onto volumes by device node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UdevEntry {
    pub devnode: String,
    pub label: Option<String>,
    pub uuid: Option<String>,
}

/// Source of [`DiskInfo`] rows. The production source reads sysinfo; tests
/// inject fakes.
pub trait DiskSource {
    fn disks(&self) -> Vec<DiskInfo>;
}

/// Production [`DiskSource`] backed by `sysinfo::Disks`.
#[derive(Debug, Default, Clone, Copy)]
pub struct SysinfoSource;

impl DiskSource for SysinfoSource {
    fn disks(&self) -> Vec<DiskInfo> {
        sysinfo::Disks::new_with_refreshed_list()
            .list()
            .iter()
            .map(|disk| DiskInfo {
                device: disk.name().to_string_lossy().into_owned(),
                mount_point: disk.mount_point().to_path_buf(),
                fs_type: disk.file_system().to_string_lossy().into_owned(),
                capacity_bytes: disk.total_space(),
                free_bytes: disk.available_space(),
                removable: disk.is_removable(),
                read_only: disk.is_read_only(),
            })
            .collect()
    }
}

/// Change between two snapshots, keyed by [`Volume::identity`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VolumeEvent {
    Added(Volume),
    Removed(Volume),
    /// Same identity, but label/capacity/flags changed.
    Changed(Volume),
}

/// Join disk rows with udev label/UUID rows into volumes.
///
/// The udev join matches on the device node (`Disk::name` on Linux is the
/// `/dev/…` node, which is also the udev `DEVNAME`). Unmatched disks keep
/// `label`/`fs_uuid` as `None` and fall back to mount-point identity.
///
/// Collision rule: two rows with the same identity (e.g. a cloned card imaged
/// with the same filesystem UUID) collapse to one volume — first row wins.
/// Identity is stable, not unique, so C2 must still treat the mount point as
/// the copy source.
pub fn snapshot(disks: &[DiskInfo], udev_entries: &[UdevEntry]) -> Vec<Volume> {
    let mut seen: Vec<String> = Vec::new();
    disks
        .iter()
        .map(|disk| {
            let udev_match = udev_entries
                .iter()
                .find(|entry| entry.devnode == disk.device);
            let fs_uuid = udev_match.and_then(|entry| entry.uuid.clone());
            let label = udev_match.and_then(|entry| entry.label.clone());
            Volume {
                identity: Volume::identity_for(fs_uuid.as_deref(), &disk.mount_point),
                device: disk.device.clone(),
                mount_point: disk.mount_point.clone(),
                label,
                fs_uuid,
                fs_type: disk.fs_type.clone(),
                capacity_bytes: disk.capacity_bytes,
                free_bytes: disk.free_bytes,
                removable: disk.removable,
                read_only: disk.read_only,
            }
        })
        .filter(|volume| {
            if seen.contains(&volume.identity) {
                return false;
            }
            seen.push(volume.identity.clone());
            true
        })
        .collect()
}

/// Diff two snapshots by volume identity.
pub fn diff(old: &[Volume], new: &[Volume]) -> Vec<VolumeEvent> {
    let mut events = Vec::new();
    for volume in new {
        match old.iter().find(|known| known.identity == volume.identity) {
            None => events.push(VolumeEvent::Added(volume.clone())),
            Some(known) if known != volume => events.push(VolumeEvent::Changed(volume.clone())),
            Some(_) => {}
        }
    }
    for volume in old {
        if !new.iter().any(|known| known.identity == volume.identity) {
            events.push(VolumeEvent::Removed(volume.clone()));
        }
    }
    events
}

/// Poll once: read disks from `source` and join with `udev_entries`.
///
/// Udev entries are a parameter (rather than read inside) so tests never touch
/// hardware; production callers pass [`enumerate_udev()`].
pub fn poll_once(source: &impl DiskSource, udev_entries: &[UdevEntry]) -> Vec<Volume> {
    snapshot(&source.disks(), udev_entries)
}

/// Current udev block-device rows on Linux; empty elsewhere (Windows/macOS
/// volume code comes in a later milestone).
///
/// This enumerates the whole `block` subsystem while the hotplug monitor only
/// watches the `partition` devtype. The mismatch is deliberate: enumeration is
/// a superset, so a partition event always finds its entry on rescan, and the
/// [`POLL_INTERVAL`] tick covers anything the monitor misses (at most one
/// tick late).
#[cfg(target_os = "linux")]
pub fn enumerate_udev() -> Vec<UdevEntry> {
    let mut enumerator = match udev::Enumerator::new() {
        Ok(enumerator) => enumerator,
        Err(_) => return Vec::new(),
    };
    if enumerator.match_subsystem("block").is_err() {
        return Vec::new();
    }
    let devices = match enumerator.scan_devices() {
        Ok(devices) => devices,
        Err(_) => return Vec::new(),
    };
    devices
        .filter_map(|device| {
            let devnode = device.devnode()?.to_string_lossy().into_owned();
            let text = |key: &str| {
                device
                    .property_value(key)
                    .map(|value| value.to_string_lossy().into_owned())
                    .filter(|value| !value.is_empty())
            };
            Some(UdevEntry {
                devnode,
                label: text("ID_FS_LABEL"),
                uuid: text("ID_FS_UUID"),
            })
        })
        .collect()
}

/// Current udev block-device rows on Linux; empty elsewhere (Windows/macOS
/// volume code comes in a later milestone).
#[cfg(not(target_os = "linux"))]
pub fn enumerate_udev() -> Vec<UdevEntry> {
    Vec::new()
}

/// Spawn the Linux udev hotplug watcher: each block-partition add/remove/change
/// event sends a `()` ping so the poll loop rescans immediately instead of
/// waiting for the next [`POLL_INTERVAL`] tick. Returns `None` off Linux.
#[cfg(target_os = "linux")]
pub fn spawn_udev_ping(ping: flume::Sender<()>) -> Option<std::thread::JoinHandle<()>> {
    // The monitor socket is `!Send`, so it is created and polled on the same
    // thread here.
    Some(std::thread::spawn(move || {
        let socket = (|| {
            udev::MonitorBuilder::new()
                .ok()?
                .match_subsystem_devtype("block", "partition")
                .ok()?
                .listen()
                .ok()
        })();
        let socket = match socket {
            Some(socket) => socket,
            None => return,
        };
        for _event in socket.iter() {
            if ping.send(()).is_err() {
                break;
            }
        }
    }))
}

/// Spawn the Linux udev hotplug watcher (see the Linux variant). Off Linux
/// there is no watcher yet (cross-platform bring-up is a later milestone), so
/// this returns `None`.
#[cfg(not(target_os = "linux"))]
pub fn spawn_udev_ping(_ping: flume::Sender<()>) -> Option<std::thread::JoinHandle<()>> {
    None
}

/// Wait for the next poll tick, waking early on a udev hotplug ping.
///
/// A disconnected ping channel (no udev thread off Linux, or a dead watcher)
/// must sleep the full interval: `recv_timeout` on a disconnected channel
/// returns immediately, and spinning on it would pin a CPU polling forever.
fn wait_for_next_tick(ping: &flume::Receiver<()>, interval: Duration) {
    match ping.recv_timeout(interval) {
        Ok(()) | Err(flume::RecvTimeoutError::Timeout) => {}
        Err(flume::RecvTimeoutError::Disconnected) => std::thread::sleep(interval),
    }
}

/// Blocking watch loop for a background thread: poll every [`POLL_INTERVAL`],
/// wake early on udev pings, and send the full snapshot whenever it changes.
/// Never touches GPUI state; the app drains the channel on its executor.
pub fn watch_loop(source: impl DiskSource, changed: flume::Sender<Vec<Volume>>) {
    let (ping_tx, ping_rx) = flume::bounded::<()>(16);
    let _udev_thread = spawn_udev_ping(ping_tx);

    let mut last: Vec<Volume> = Vec::new();
    loop {
        let current = poll_once(&source, &enumerate_udev());
        if current != last {
            last = current.clone();
            if changed.send(current).is_err() {
                break;
            }
        }
        wait_for_next_tick(&ping_rx, POLL_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disk(device: &str, mount: &str) -> DiskInfo {
        DiskInfo {
            device: device.to_string(),
            mount_point: PathBuf::from(mount),
            fs_type: "exfat".to_string(),
            capacity_bytes: 64 * 1024 * 1024 * 1024,
            free_bytes: 16 * 1024 * 1024 * 1024,
            removable: true,
            read_only: false,
        }
    }

    fn udev_entry(devnode: &str, label: Option<&str>, uuid: Option<&str>) -> UdevEntry {
        UdevEntry {
            devnode: devnode.to_string(),
            label: label.map(str::to_string),
            uuid: uuid.map(str::to_string),
        }
    }

    #[test]
    fn identity_prefers_uuid_over_label() {
        let volumes = snapshot(
            &[disk("/dev/sdb1", "/media/card")],
            &[udev_entry(
                "/dev/sdb1",
                Some("EOS_DIGITAL"),
                Some("A1B2-C3D4"),
            )],
        );
        assert_eq!(volumes.len(), 1);
        assert_eq!(volumes[0].identity, "uuid:A1B2-C3D4");
        assert_eq!(volumes[0].label.as_deref(), Some("EOS_DIGITAL"));
    }

    #[test]
    fn same_label_different_uuid_is_two_volumes() {
        let volumes = snapshot(
            &[
                disk("/dev/sdb1", "/media/card-a"),
                disk("/dev/sdc1", "/media/card-b"),
            ],
            &[
                udev_entry("/dev/sdb1", Some("EOS_DIGITAL"), Some("AAAA-1111")),
                udev_entry("/dev/sdc1", Some("EOS_DIGITAL"), Some("BBBB-2222")),
            ],
        );
        assert_eq!(volumes.len(), 2);
        assert_ne!(volumes[0].identity, volumes[1].identity);
        assert_eq!(volumes[0].display_name(), "EOS_DIGITAL");
        assert_eq!(volumes[1].display_name(), "EOS_DIGITAL");
    }

    #[test]
    fn missing_uuid_falls_back_to_mount_point() {
        let volumes = snapshot(&[disk("/dev/sdb1", "/media/card")], &[]);
        assert_eq!(volumes.len(), 1);
        assert_eq!(volumes[0].identity, "mount:/media/card");
        assert_eq!(volumes[0].label, None);
    }

    #[test]
    fn empty_uuid_falls_back_to_mount_point() {
        let volumes = snapshot(
            &[disk("/dev/sdb1", "/media/card")],
            &[udev_entry("/dev/sdb1", Some("CARD"), Some(""))],
        );
        assert_eq!(volumes[0].identity, "mount:/media/card");
    }

    #[test]
    fn udev_join_ignores_unrelated_devices() {
        let volumes = snapshot(
            &[disk("/dev/sdb1", "/media/card")],
            &[udev_entry("/dev/sda1", Some("SYSTEM"), Some("SYS-UUID"))],
        );
        assert_eq!(volumes[0].label, None);
        assert_eq!(volumes[0].fs_uuid, None);
        assert_eq!(volumes[0].identity, "mount:/media/card");
    }

    #[test]
    fn display_name_falls_back_to_mount_dir_then_device() {
        let mut volumes = snapshot(&[disk("/dev/sdb1", "/media/my-card")], &[]);
        assert_eq!(volumes[0].display_name(), "my-card");

        volumes = snapshot(&[disk("/dev/sdb1", "/")], &[]);
        assert_eq!(volumes[0].display_name(), "/dev/sdb1");
    }

    #[test]
    fn used_percent_tracks_capacity() {
        let volumes = snapshot(&[disk("/dev/sdb1", "/media/card")], &[]);
        assert!((volumes[0].used_percent() - 75.0).abs() < f32::EPSILON);

        let empty = DiskInfo {
            capacity_bytes: 0,
            free_bytes: 0,
            ..disk("/dev/sdb1", "/media/card")
        };
        let volumes = snapshot(&[empty], &[]);
        assert_eq!(volumes[0].used_percent(), 0.0);
    }

    #[test]
    fn diff_reports_add_remove_and_change() {
        let before: Vec<Volume> = Vec::new();
        let after = snapshot(
            &[disk("/dev/sdb1", "/media/card")],
            &[udev_entry("/dev/sdb1", Some("CARD"), Some("UUID-1"))],
        );
        let events = diff(&before, &after);
        assert_eq!(events, vec![VolumeEvent::Added(after[0].clone())]);

        // Same snapshot: silent.
        assert!(diff(&after, &after).is_empty());

        // Relabeled card (same UUID) is a change, not an add/remove.
        let relabeled = snapshot(
            &[disk("/dev/sdb1", "/media/card")],
            &[udev_entry("/dev/sdb1", Some("RENAMED"), Some("UUID-1"))],
        );
        assert_eq!(events_len(&relabeled, &after), 1);
        assert!(matches!(
            diff(&after, &relabeled)[0],
            VolumeEvent::Changed(_)
        ));

        // Card pulled: removal carries the old volume.
        let events = diff(&after, &before);
        assert_eq!(events, vec![VolumeEvent::Removed(after[0].clone())]);
    }

    #[test]
    fn relabeled_card_keeps_stable_identity() {
        let first = snapshot(
            &[disk("/dev/sdb1", "/media/card")],
            &[udev_entry("/dev/sdb1", Some("CARD"), Some("UUID-1"))],
        );
        let second = snapshot(
            &[disk("/dev/sdb1", "/media/card")],
            &[udev_entry("/dev/sdb1", Some("RENAMED"), Some("UUID-1"))],
        );
        assert_eq!(first[0].identity, second[0].identity);
    }

    fn events_len(next: &[Volume], prev: &[Volume]) -> usize {
        diff(prev, next).len()
    }

    struct FakeSource(Vec<DiskInfo>);

    impl DiskSource for FakeSource {
        fn disks(&self) -> Vec<DiskInfo> {
            self.0.clone()
        }
    }

    #[test]
    fn poll_once_uses_injected_source_without_touching_hardware() {
        let source = FakeSource(vec![disk("/dev/sdb1", "/media/card")]);
        // Empty udev rows: no ID_FS_* enrichment, no hardware access.
        let volumes = poll_once(&source, &[]);
        assert_eq!(volumes.len(), 1);
        assert_eq!(volumes[0].device, "/dev/sdb1");
        assert_eq!(volumes[0].identity, "mount:/media/card");
    }

    #[test]
    fn duplicate_uuid_collapses_to_first_row() {
        let volumes = snapshot(
            &[
                disk("/dev/sdb1", "/media/card-a"),
                disk("/dev/sdc1", "/media/card-b"),
            ],
            &[
                udev_entry("/dev/sdb1", Some("CLONE"), Some("SAME-UUID")),
                udev_entry("/dev/sdc1", Some("CLONE"), Some("SAME-UUID")),
            ],
        );
        assert_eq!(volumes.len(), 1);
        assert_eq!(volumes[0].identity, "uuid:SAME-UUID");
        assert_eq!(volumes[0].device, "/dev/sdb1");
    }

    #[test]
    fn wait_returns_early_on_ping_and_sleeps_when_disconnected() {
        let (ping_tx, ping_rx) = flume::bounded::<()>(1);
        ping_tx.send(()).expect("test ping sends");
        let start = std::time::Instant::now();
        wait_for_next_tick(&ping_rx, Duration::from_secs(60));
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "ping should wake the wait immediately"
        );

        drop(ping_tx);
        let interval = Duration::from_millis(100);
        let start = std::time::Instant::now();
        wait_for_next_tick(&ping_rx, interval);
        assert!(
            start.elapsed() >= interval,
            "disconnected channel must sleep the full interval, not spin"
        );
    }

    fn volume_at(mount: &str, removable: bool) -> Volume {
        let mut volume = snapshot(&[disk("/dev/sdb1", mount)], &[]).remove(0);
        volume.removable = removable;
        volume
    }

    #[test]
    fn only_cards_and_external_drives_are_ingest_sources() {
        for mount in ["/media/card", "/run/media/filip/EOS_DIGITAL", "/mnt/usb"] {
            assert!(volume_at(mount, false).is_ingest_source(), "{mount}");
        }
        assert!(volume_at("/somewhere/reader", true).is_ingest_source());
        for mount in [
            "/",
            "/home",
            "/boot",
            "/boot/efi",
            "/tmp/.mount_t3_abc",
            "/var/lib/x",
        ] {
            assert!(!volume_at(mount, false).is_ingest_source(), "{mount}");
        }
        // Removable never rescues the root filesystem.
        assert!(!volume_at("/", true).is_ingest_source());
    }

    #[test]
    fn a_chosen_folder_is_a_source_of_its_own() {
        let folder = Volume::folder(Path::new("/home/filip/Pictures/card-dump"));
        assert_eq!(folder.identity, "folder:/home/filip/Pictures/card-dump");
        assert_eq!(folder.display_name(), "card-dump");
        assert_eq!(folder.used_percent(), 0.0);
    }
}
