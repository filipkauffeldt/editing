use serde::{Deserialize, Serialize};

use crate::geometry::{Point, Rect};

/// One detected face. Boxes come from the detector; manual boxes are always
/// available and every box is toggleable in the UI.
///
/// The R0 pick (`libfacedetection_rs`) exposes score + box only, so
/// `landmarks` is `None` there and the mask falls back to expanded boxes (as
/// the plan allows). A landmark detector (YuNet) fills the five points for
/// rotated face ellipses.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FaceBox {
    /// Normalized box in frame coordinates.
    pub region: Rect,
    /// Detector confidence in 0..1.
    pub confidence: f32,
    /// Optional five facial landmarks (normalized, eye/eye/nose/mouth
    /// corners per the detector's convention).
    pub landmarks: Option<[Point; 5]>,
}

impl FaceBox {
    pub fn new(region: Rect, confidence: f32) -> Self {
        Self {
            region,
            confidence,
            landmarks: None,
        }
    }

    pub fn with_landmarks(mut self, landmarks: [Point; 5]) -> Self {
        self.landmarks = Some(landmarks);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn face_box_landmarks_optional() {
        let no_marks = FaceBox::new(Rect::full(), 0.9);
        assert_eq!(no_marks.landmarks, None);
        let marks = [Point::new(0.1, 0.2); 5];
        assert_eq!(no_marks.with_landmarks(marks).landmarks, Some(marks));
    }
}
