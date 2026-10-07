use serde::{Deserialize, Serialize};

use crate::geometry::Rect;
use crate::ids::ItemId;
use crate::retouch::RetouchRecipe;
use crate::{DevelopSettings, Error, Result};

/// Requested render resolution.
///
/// Sliders render [`RenderSize::Proxy`] while dragging, the full proxy on
/// release, and [`RenderSize::Full`] at export — one code path at every size.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RenderSize {
    /// Downscaled proxy; the long edge in pixels.
    Proxy(u16),
    /// Full source resolution.
    Full,
}

/// Target color output for a render.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutputColor {
    /// Display path (canvas).
    DisplaySrgb,
    /// Export path (file with embedded profile handling).
    ExportSrgb,
}

/// Free crop + aspect lock + rotate/straighten state for one item.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Crop {
    /// Normalized crop rect in source coordinates.
    pub rect: Rect,
    /// Straighten angle in degrees (positive is clockwise).
    pub rotation_degrees: f32,
    /// Locked aspect preset, if any.
    pub aspect: Option<Aspect>,
}

impl Default for Crop {
    fn default() -> Self {
        Self {
            rect: Rect::full(),
            rotation_degrees: 0.0,
            aspect: None,
        }
    }
}

/// Aspect-ratio presets offered by the crop UI (plan section 2.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Aspect {
    Original,
    Square,
    ThreeTwo,
    TwoThree,
    FourThree,
    SixteenNine,
}

/// One render request: everything the develop engine needs.
#[derive(Clone, Debug, PartialEq)]
pub struct RenderRequest {
    pub item: ItemId,
    pub size: RenderSize,
    pub develop: DevelopSettings,
    pub crop: Option<Crop>,
    pub retouch: Option<RetouchRecipe>,
    pub color: OutputColor,
}

/// A decoded/rendered image: BGRA, 8 bits per channel.
///
/// `stride` is bytes per row and must be at least `width * 4`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub pixels: Vec<u8>,
}

impl Frame {
    /// Allocate a zeroed frame. Errors when dimensions are zero or the
    /// allocation would overflow.
    pub fn zeroed(width: u32, height: u32) -> Result<Self> {
        if width == 0 || height == 0 {
            return Err(Error::InvalidInput(
                "frame dimensions must be nonzero".into(),
            ));
        }
        let stride = width
            .checked_mul(4)
            .ok_or_else(|| Error::InvalidInput("frame width overflows stride".into()))?;
        let len = (stride as usize)
            .checked_mul(height as usize)
            .ok_or_else(|| Error::InvalidInput("frame allocation overflows".into()))?;
        Ok(Self {
            width,
            height,
            stride,
            pixels: vec![0; len],
        })
    }

    /// Check buffer geometry without copying.
    pub fn validate(&self) -> Result<()> {
        if self.width == 0 || self.height == 0 {
            return Err(Error::InvalidInput(
                "frame dimensions must be nonzero".into(),
            ));
        }
        let min_stride = self
            .width
            .checked_mul(4)
            .ok_or_else(|| Error::InvalidInput("frame width overflows stride".into()))?;
        if self.stride < min_stride {
            return Err(Error::InvalidInput(
                "frame stride is smaller than width * 4".into(),
            ));
        }
        let min_len = (self.stride as usize)
            .checked_mul(self.height as usize)
            .ok_or_else(|| Error::InvalidInput("frame buffer size overflows".into()))?;
        if self.pixels.len() < min_len {
            return Err(Error::InvalidInput(
                "frame buffer is smaller than stride * height".into(),
            ));
        }
        Ok(())
    }

    /// One BGRA pixel. Returns `None` out of bounds.
    pub fn pixel(&self, x: u32, y: u32) -> Option<[u8; 4]> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let i = y as usize * self.stride as usize + x as usize * 4;
        let p = self.pixels.get(i..i + 4)?;
        Some([p[0], p[1], p[2], p[3]])
    }
}

/// An embedded camera preview (used when raw decode is unsupported).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreviewFrame {
    pub width: u32,
    pub height: u32,
    /// BGRA pixels at `width * 4 * height` bytes.
    pub pixels: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_zeroed_and_pixel() {
        let mut frame = Frame::zeroed(4, 2).unwrap();
        frame.validate().unwrap();
        assert_eq!(frame.pixels.len(), 4 * 4 * 2);
        frame.pixels[4] = 255;
        assert_eq!(frame.pixel(1, 0), Some([255, 0, 0, 0]));
        assert_eq!(frame.pixel(4, 0), None);
    }

    #[test]
    fn frame_rejects_bad_geometry() {
        assert!(Frame::zeroed(0, 8).is_err());
        let bad = Frame {
            width: 4,
            height: 1,
            stride: 8,
            pixels: vec![0; 8],
        };
        assert!(bad.validate().is_err());
    }

    #[test]
    fn crop_defaults_to_full_frame() {
        let crop = Crop::default();
        assert_eq!(crop.rect, Rect::full());
        assert_eq!(crop.rotation_degrees, 0.0);
        assert_eq!(crop.aspect, None);
    }
}
