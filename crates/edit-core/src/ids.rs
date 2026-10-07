use serde::{Deserialize, Serialize};
use std::fmt;

/// Opaque handle for a RAW+JPEG pair (or lone file) in the `items` table.
///
/// Edits, selections, retouch versions, detections, and sends all attach to
/// the item, never to a single file.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ItemId(pub i64);

/// Opaque handle for one row in the `photos` table (one file on disk).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PhotoId(pub i64);

macro_rules! impl_id {
    ($t:ty, $name:expr) => {
        impl $t {
            /// Wrap a raw catalog row id.
            pub fn new(id: i64) -> Self {
                Self(id)
            }

            /// The underlying catalog row id.
            pub fn get(self) -> i64 {
                self.0
            }
        }

        impl From<i64> for $t {
            fn from(id: i64) -> Self {
                Self(id)
            }
        }

        impl fmt::Debug for $t {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", $name, self.0)
            }
        }

        impl fmt::Display for $t {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}:{}", $name, self.0)
            }
        }
    };
}

impl_id!(ItemId, "ItemId");
impl_id!(PhotoId, "PhotoId");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip() {
        let item = ItemId::new(7);
        assert_eq!(item.get(), 7);
        assert_eq!(ItemId::from(7), item);
        assert_eq!(format!("{item}"), "ItemId:7");
        assert_ne!(PhotoId::new(7).get(), ItemId::new(8).get());
    }
}
