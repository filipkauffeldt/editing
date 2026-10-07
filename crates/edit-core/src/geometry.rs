use serde::{Deserialize, Serialize};

/// A 2D point. Landmark positions are normalized to the frame (0..1);
/// pixel-space points are documented at each use site.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    pub fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

/// A rectangle in normalized coordinates (0..1 relative to frame size).
///
/// All selection shapes, face boxes, and crop rects are normalized so they
/// stay valid across proxy and full-resolution renders.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    /// The full frame.
    pub fn full() -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            w: 1.0,
            h: 1.0,
        }
    }

    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    /// Area in normalized units (0..1 for a rect inside the frame).
    pub fn area(self) -> f32 {
        self.w.max(0.0) * self.h.max(0.0)
    }

    /// True when the rect lies fully inside the unit square.
    pub fn is_normalized(self) -> bool {
        self.x >= 0.0
            && self.y >= 0.0
            && self.w >= 0.0
            && self.h >= 0.0
            && self.x + self.w <= 1.0
            && self.y + self.h <= 1.0
    }

    /// Clamp the rect into the unit square.
    pub fn clamped(self) -> Self {
        let x = self.x.clamp(0.0, 1.0);
        let y = self.y.clamp(0.0, 1.0);
        Self {
            x,
            y,
            w: self.w.clamp(0.0, 1.0 - x),
            h: self.h.clamp(0.0, 1.0 - y),
        }
    }

    /// True when `p` (normalized) falls inside the rect.
    pub fn contains(self, p: Point) -> bool {
        p.x >= self.x && p.x < self.x + self.w && p.y >= self.y && p.y < self.y + self.h
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_helpers() {
        assert!(Rect::full().is_normalized());
        assert!(!Rect::new(-0.1, 0.0, 0.5, 0.5).is_normalized());
        let clamped = Rect::new(-0.1, 0.2, 2.0, 0.5).clamped();
        assert!(clamped.is_normalized());
        assert!(Rect::full().contains(Point::new(0.5, 0.5)));
        assert!(!Rect::full().contains(Point::new(1.0, 1.0)));
    }
}
