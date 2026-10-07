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
    /// A progress subscriber went away; the job result is still durable.
    #[error("progress channel disconnected")]
    ProgressDisconnected,
}

/// Convenience alias for catalog operations.
pub type Result<T> = std::result::Result<T, Error>;
