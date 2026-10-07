//! Shared ingest vocabulary.
//!
//! [`IngestFileState`] is the single source of truth for the per-file ingest
//! states stored in `ingest_files.state`. The catalog (storage), the ingest
//! worker, and the dialog all use this type instead of raw string literals,
//! so the state machine lives in one place.

use std::fmt;
use std::str::FromStr;

/// Lifecycle of one file in the ingest copy pipeline.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum IngestFileState {
    /// Recorded, not yet copied.
    #[default]
    Queued,
    Copying,
    Verifying,
    /// Copying to the backup destination.
    Backup,
    /// Landing (renaming) into the folder/filename rule.
    Renaming,
    /// Copied, verified, and inserted into the catalog.
    Cataloged,
    /// Already ingested; not copied again.
    Duplicate,
    /// The copy failed; `error` on the row says why.
    Failed,
}

impl IngestFileState {
    /// Every state, in lifecycle order.
    pub fn all() -> &'static [IngestFileState] {
        &[
            Self::Queued,
            Self::Copying,
            Self::Verifying,
            Self::Backup,
            Self::Renaming,
            Self::Cataloged,
            Self::Duplicate,
            Self::Failed,
        ]
    }

    /// The value stored in `ingest_files.state`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Copying => "copying",
            Self::Verifying => "verifying",
            Self::Backup => "backup",
            Self::Renaming => "renaming",
            Self::Cataloged => "cataloged",
            Self::Duplicate => "duplicate",
            Self::Failed => "failed",
        }
    }

    /// True for the two terminal states (`cataloged`, `duplicate`, `failed`).
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Cataloged | Self::Duplicate | Self::Failed)
    }

    /// True when the row has landed (or may have landed) bytes on disk.
    pub fn has_landed_bytes(self) -> bool {
        matches!(self, Self::Backup | Self::Renaming | Self::Cataloged)
    }
}

impl FromStr for IngestFileState {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "queued" => Ok(Self::Queued),
            "copying" => Ok(Self::Copying),
            "verifying" => Ok(Self::Verifying),
            "backup" => Ok(Self::Backup),
            "renaming" => Ok(Self::Renaming),
            "cataloged" => Ok(Self::Cataloged),
            "duplicate" => Ok(Self::Duplicate),
            "failed" => Ok(Self::Failed),
            other => Err(format!("unknown ingest file state: {other}")),
        }
    }
}

impl fmt::Display for IngestFileState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_round_trip() {
        for state in IngestFileState::all() {
            assert_eq!(state.as_str().parse::<IngestFileState>().unwrap(), *state);
        }
        assert!("bogus".parse::<IngestFileState>().is_err());
    }

    #[test]
    fn terminal_and_landed_classification() {
        assert!(IngestFileState::Cataloged.is_terminal());
        assert!(IngestFileState::Duplicate.is_terminal());
        assert!(IngestFileState::Failed.is_terminal());
        assert!(!IngestFileState::Renaming.is_terminal());
        assert!(IngestFileState::Renaming.has_landed_bytes());
        assert!(IngestFileState::Cataloged.has_landed_bytes());
        assert!(!IngestFileState::Queued.has_landed_bytes());
    }
}
