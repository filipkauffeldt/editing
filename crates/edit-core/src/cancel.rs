use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::Result;
use crate::error::Error;

/// Cooperative cancellation token handed to long renders.
///
/// Cloned handles share one flag: any owner may call [`CancelToken::cancel`]
/// and the worker observes it through [`CancelToken::is_cancelled`] or
/// [`CancelToken::check`].
#[derive(Clone, Debug, Default)]
pub struct CancelToken {
    flag: Arc<AtomicBool>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// Signal cancellation to every clone.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    /// True once any owner called [`CancelToken::cancel`].
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// [`Error::Cancelled`] when cancelled, `Ok(())` otherwise.
    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_propagates_to_clones() {
        let token = CancelToken::new();
        let clone = token.clone();
        assert!(!clone.is_cancelled());
        token.cancel();
        assert!(clone.is_cancelled());
        assert_eq!(clone.check(), Err(Error::Cancelled));
    }
}
