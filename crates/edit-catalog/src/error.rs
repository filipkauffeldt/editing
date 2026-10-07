use thiserror::Error;

/// Errors raised by the catalog and job queue.
#[derive(Debug, Error)]
pub enum Error {
    /// SQLite failure.
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// A row referenced by id does not exist.
    #[error("not found: {0}")]
    NotFound(String),
    /// A value violates a domain rule (unknown job kind/state, bad JSON).
    #[error("invalid value: {0}")]
    InvalidValue(String),
    /// The catalog mutex was poisoned by a panicking thread.
    #[error("catalog lock poisoned")]
    LockPoisoned,
}

/// Convenience alias for catalog operations.
pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// True for SQLite UNIQUE constraint violations: the caller tried to
    /// insert a row whose unique key already exists. Folder scans use this
    /// to skip an already-cataloged file instead of aborting the whole scan.
    pub fn is_unique_violation(&self) -> bool {
        matches!(
            self,
            Self::Sqlite(rusqlite::Error::SqliteFailure(err, _))
                if err.code == rusqlite::ErrorCode::ConstraintViolation
        )
    }
}
