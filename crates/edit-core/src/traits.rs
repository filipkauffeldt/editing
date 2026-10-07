use std::path::Path;

use crate::cancel::CancelToken;
use crate::error::Result;
use crate::faces::FaceBox;
use crate::geometry::Rect;
use crate::render::{Frame, PreviewFrame, RenderRequest};
use crate::retouch::{MaskPlane, RetouchRecipe};

/// Fast embedded-preview access.
///
/// Backed by `lightcraft-raw` where supported, falling back to the camera
/// preview otherwise. Never on the full-decode path.
pub trait PreviewProvider: Send + Sync {
    fn embedded_preview(&self, path: &Path) -> Result<PreviewFrame>;
}

/// Full develop render through `lightcraft-pipeline`.
///
/// One code path serves every [`crate::RenderSize`]: downscaled proxy while
/// dragging, full proxy on release, full resolution at export.
pub trait DevelopEngine: Send + Sync {
    fn render(&self, req: RenderRequest, cancel: &CancelToken) -> Result<Frame>;
}

/// Post-develop retouch stage (mosaic default, Gaussian optional).
///
/// Blends the recipe through `mask` and its feather at render resolution,
/// identically in preview and export.
pub trait RetouchStage: Send + Sync {
    fn apply(&self, frame: &mut Frame, recipe: &RetouchRecipe, mask: &MaskPlane) -> Result<()>;
}

/// Swappable face detector.
///
/// The R0 default is `libfacedetection_rs` behind this trait; a YuNet/SCRFD
/// implementation can replace it without touching callers. Detection runs on
/// the largest embedded preview, ROI-first with tiling handled by the caller.
pub trait FaceDetector: Send + Sync {
    fn detect(&self, frame: &Frame, region: Rect) -> Result<Vec<FaceBox>>;
}
