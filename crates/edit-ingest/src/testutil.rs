//! Shared test helpers for the `edit-ingest` test modules.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// A unique scratch directory under the system temp dir, created fresh.
pub(crate) fn scratch_dir(name: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir =
        std::env::temp_dir().join(format!("edit-ingest-{}-{stamp}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}
