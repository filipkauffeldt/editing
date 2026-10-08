//! Shared edit-model types and service traits.
//!
//! This is the single edit model for the app: sliders set
//! [`DevelopSettings`] (re-exported from the pinned LightCraft revision) and
//! everything else — crop, retouch recipes, face boxes — lives in the
//! normalized types below. Do not add a second tone model.
//!
//! No I/O and no UI dependencies live here. Services wrap their upstream
//! crates behind the traits in [`traits`].

pub mod cancel;
pub mod crop;
pub mod error;
pub mod faces;
pub mod geometry;
pub mod ids;
pub mod ingest;
pub mod render;
pub mod retouch;
pub mod traits;

pub use cancel::CancelToken;
pub use crop::{Aspect, Crop};
pub use error::{Error, Result};
pub use faces::FaceBox;
pub use geometry::{Point, Rect};
pub use ids::{ItemId, PhotoId};
pub use ingest::IngestFileState;
pub use render::{Frame, OutputColor, PreviewFrame, RenderRequest, RenderSize};
pub use retouch::{MaskPlane, RetouchOp, RetouchRecipe};

/// LightCraft develop settings: the only tone/edit model.
///
/// Re-exported from the pinned `lightcraft-develop` revision (see the R0
/// memo in `docs/research/retouch-r0-memo.md`). Serialized as JSON in the
/// catalog `edits` table.
pub use lightcraft_develop::DevelopSettings;
