use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// A non-destructive retouch recipe: the ordered ops re-rendered at preview
/// and export. Stored as JSON in the catalog (`retouch_versions`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RetouchRecipe {
    pub ops: Vec<RetouchOp>,
}

impl RetouchRecipe {
    pub fn new(ops: Vec<RetouchOp>) -> Self {
        Self { ops }
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }
}

/// One privacy op. v1 covers mosaic (the default for faces) and Gaussian.
///
/// Strength presets (low/medium/high) map to `cell_px` / `radius_px` plus the
/// mask feather; the exact mapping is tuned in the retouch track.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum RetouchOp {
    Mosaic {
        /// Mosaic cell size in pixels at render resolution.
        cell_px: u32,
    },
    Gaussian {
        /// Blur radius in pixels at render resolution.
        radius_px: f32,
    },
}

/// A rasterized selection mask at render resolution.
///
/// Channel semantics: one coverage byte per pixel (0 = keep, 255 = fully
/// retouched); the stage blends through the coverage and its feather.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaskPlane {
    pub width: u32,
    pub height: u32,
    pub coverage: Vec<u8>,
}

impl MaskPlane {
    /// Build a mask plane, checking the buffer covers `width * height`.
    pub fn new(width: u32, height: u32, coverage: Vec<u8>) -> Result<Self> {
        if width == 0 || height == 0 {
            return Err(Error::InvalidInput(
                "mask dimensions must be nonzero".into(),
            ));
        }
        let len = width as usize * height as usize;
        if coverage.len() != len {
            return Err(Error::InvalidInput(format!(
                "mask buffer is {} bytes, need {len}",
                coverage.len()
            )));
        }
        Ok(Self {
            width,
            height,
            coverage,
        })
    }

    /// Coverage at `(x, y)` as 0.0..1. Returns `None` out of bounds.
    pub fn at(&self, x: u32, y: u32) -> Option<f32> {
        if x >= self.width || y >= self.height {
            return None;
        }
        Some(self.coverage[y as usize * self.width as usize + x as usize] as f32 / 255.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recipe_json_round_trip() {
        let recipe = RetouchRecipe::new(vec![
            RetouchOp::Mosaic { cell_px: 16 },
            RetouchOp::Gaussian { radius_px: 8.0 },
        ]);
        let json = serde_json::to_string(&recipe).unwrap();
        assert_eq!(
            serde_json::from_str::<RetouchRecipe>(&json).unwrap(),
            recipe
        );
    }

    #[test]
    fn mask_plane_checks_size() {
        assert!(MaskPlane::new(0, 4, vec![]).is_err());
        assert!(MaskPlane::new(2, 2, vec![0; 3]).is_err());
        let mask = MaskPlane::new(2, 2, vec![0, 128, 255, 64]).unwrap();
        assert_eq!(mask.at(1, 0), Some(128.0 / 255.0));
        assert_eq!(mask.at(2, 0), None);
    }
}
