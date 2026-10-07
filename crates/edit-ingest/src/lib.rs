//! Volume detection for ingest.
//!
//! Linux-first per PLAN.md. The baseline polls [`sysinfo`] disks every
//! [`POLL_INTERVAL`]; on Linux a udev monitor on the `block` subsystem wakes
//! the poll loop early and enriches volumes with `ID_FS_LABEL` / `ID_FS_UUID`.
//! Windows (`GetVolumeInformationW`) and macOS (DiskArbitration) volume code
//! lands in M7.
//!
//! Identity rule: a volume is keyed by its **filesystem UUID**, never by its
//! label — labels collide (`EOS_DIGITAL` is every Canon card). Volumes without
//! a UUID fall back to their mount point.

pub mod volumes;

pub use volumes::{
    DiskInfo, DiskSource, POLL_INTERVAL, SysinfoSource, UdevEntry, Volume, VolumeEvent, diff,
    enumerate_udev, poll_once, snapshot, spawn_udev_ping, watch_loop,
};
