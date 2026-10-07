use thiserror::Error;

/// Errors raised through the shared service traits.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum Error {
    /// Caller cancelled the operation via [`crate::CancelToken`].
    #[error("operation cancelled")]
    Cancelled,
    /// A request field is out of range or inconsistent.
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// An upstream engine (render, detector, preview) failed.
    #[error("{stage} failed: {message}")]
    Engine {
        stage: &'static str,
        message: String,
    },
}

/// Convenience alias used by every trait in [`crate::traits`].
pub type Result<T> = std::result::Result<T, Error>;
