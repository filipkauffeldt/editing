use serde::{Deserialize, Serialize};

use crate::geometry::Rect;

/// Free crop + aspect lock + straighten state for one item.
///
/// The basis is LightCraft's crop model, so the app, the render engine and
/// the sidecar all read the same numbers. The oriented image (`w × h`) is
/// rotated by [`Crop::rotation_degrees`] about its own centre on a canvas of
/// the same size, and [`Crop::rect`] is the crop frame in that rotated
/// canvas, normalized to `w × h`. The frame stays fixed on screen while the
/// image turns under it, and it must lie inside the rotated image.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Crop {
    /// Normalized crop frame in the straightened canvas. At angle 0 this is
    /// the source's own normalized coordinates.
    pub rect: Rect,
    /// Straighten angle in degrees. Positive turns the image clockwise on
    /// screen, so a horizon that rises to the right is levelled by a
    /// positive angle.
    pub rotation_degrees: f32,
    /// Locked aspect preset, if any. `None` is a free crop.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crop_defaults_to_full_frame() {
        let crop = Crop::default();
        assert_eq!(crop.rect, Rect::full());
        assert_eq!(crop.rotation_degrees, 0.0);
        assert_eq!(crop.aspect, None);
    }
}
