//! Volume detection and the verified ingest copy pipeline.
//!
//! [`volumes`] polls mounted cards (sysinfo plus Linux udev enrichment).
//! [`naming`] renders Photo-Mechanic-style folder/filename rules, [`copy`]
//! copies to a temp name, verifies with BLAKE3, fans out one backup, and
//! atomically renames into place, and [`ingest`] runs the durable copy job
//! (duplicate checks, catalog inserts, thumbnail jobs).
//!
//! Linux-first per PLAN.md. Windows (`GetVolumeInformationW`) and macOS
//! (DiskArbitration) volume code comes in a later cross-platform milestone.
//!
//! Identity rule: a volume is keyed by its **filesystem UUID**, never by its
//! label — labels collide (`EOS_DIGITAL` is every Canon card). Volumes without
//! a UUID fall back to their mount point.

pub mod copy;
pub mod ingest;
pub mod naming;
#[cfg(test)]
pub(crate) mod testutil;
pub mod volumes;

pub use copy::{
    CopyError, CopyOutcome, CopyRequest, CopyStage, copy_verified, copy_verified_with, hash_file,
    sweep_temp_files, unique_path,
};
pub use ingest::{
    CardFile, FileMetadata, FollowUpJob, IngestCopyPayload, IngestError, IngestPlan,
    IngestPlanRequest, IngestSummary, IngestTemplate, LandedPhoto, batch_volume_identity,
    dest_root_path, enumerate_card, plan_card, plan_ingest, run_ingest_copy, run_ingest_copy_with,
};
pub use naming::{
    DEFAULT_DEST_RULE, DEFAULT_FILENAME_RULE, NamingContext, NamingError, expand_tilde, render_dir,
    render_filename, static_prefix, validate, validate_filename_rule, with_original_extension,
};
pub use volumes::{
    DiskInfo, DiskSource, POLL_INTERVAL, SysinfoSource, UdevEntry, Volume, VolumeEvent, diff,
    enumerate_udev, poll_once, snapshot, spawn_udev_ping, watch_loop,
};
