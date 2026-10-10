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
///
/// [`Crop::quarter_turns`] comes first: the oriented image is turned by whole
/// clockwise quarter turns, and everything else (the straighten angle, the
/// frame, the aspect lock) lives in that turned image, whose width and height
/// swap on an odd count.
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
    /// The lock turned on its side: 3:2 becomes 2:3, Original follows the
    /// photo's other orientation. Ignored without a lock and for 1:1.
    #[serde(default)]
    pub aspect_flipped: bool,
    /// Whole clockwise quarter turns applied to the oriented image before
    /// anything else, 0 to 3. Rows saved before the rotate step load as 0.
    #[serde(default)]
    pub quarter_turns: u8,
}

impl Default for Crop {
    fn default() -> Self {
        Self {
            rect: Rect::full(),
            rotation_degrees: 0.0,
            aspect: None,
            aspect_flipped: false,
            quarter_turns: 0,
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

impl Aspect {
    /// Every preset in the order the crop panel lists them (after Free).
    pub const ALL: [Aspect; 6] = [
        Aspect::Original,
        Aspect::Square,
        Aspect::ThreeTwo,
        Aspect::TwoThree,
        Aspect::FourThree,
        Aspect::SixteenNine,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Aspect::Original => "Original",
            Aspect::Square => "1:1",
            Aspect::ThreeTwo => "3:2",
            Aspect::TwoThree => "2:3",
            Aspect::FourThree => "4:3",
            Aspect::SixteenNine => "16:9",
        }
    }

    /// Width over height of the locked crop, in pixels. `Original` follows
    /// the oriented source `(width, height)`.
    pub fn ratio(self, source: (f64, f64)) -> f64 {
        match self {
            Aspect::Original => source.0 / source.1,
            Aspect::Square => 1.0,
            Aspect::ThreeTwo => 3.0 / 2.0,
            Aspect::TwoThree => 2.0 / 3.0,
            Aspect::FourThree => 4.0 / 3.0,
            Aspect::SixteenNine => 16.0 / 9.0,
        }
    }
}

/// Smallest crop edge, as a share of the image edge.
pub const MIN_EDGE: f32 = 0.02;

/// Largest straighten angle in degrees, matching the develop engine.
pub const MAX_ANGLE: f32 = 45.0;

/// `rect` after the whole image turns a quarter clockwise (or back), in the
/// normalized coordinates of the turned image. The straighten angle is
/// unchanged by a quarter turn, since both turn about the image centre.
pub fn turn_rect(rect: Rect, clockwise: bool) -> Rect {
    if clockwise {
        Rect::new(1.0 - rect.y - rect.h, rect.x, rect.h, rect.w)
    } else {
        Rect::new(rect.y, 1.0 - rect.x - rect.w, rect.h, rect.w)
    }
}

/// The pixel size of an image of `size` after `quarter_turns`.
pub fn turned_size(size: (f64, f64), quarter_turns: u8) -> (f64, f64) {
    if quarter_turns % 2 == 1 {
        (size.1, size.0)
    } else {
        size
    }
}

/// Pixels of slack when testing whether a corner lies inside the image, so
/// f32 rects at a few thousand pixels do not flicker around an exact fit.
const FIT_EPS_PX: f64 = 0.05;

/// `p` turned clockwise on screen by `degrees` about `c` (y grows downward).
fn rotate_about(p: (f64, f64), c: (f64, f64), degrees: f64) -> (f64, f64) {
    let (s, k) = degrees.to_radians().sin_cos();
    let (dx, dy) = (p.0 - c.0, p.1 - c.1);
    (c.0 + dx * k - dy * s, c.1 + dx * s + dy * k)
}

/// The rect's corners in pixels of the straightened canvas, clockwise from
/// the top-left.
fn corners_px(rect: Rect, source: (f64, f64)) -> [(f64, f64); 4] {
    let (x0, y0) = (rect.x as f64 * source.0, rect.y as f64 * source.1);
    let (x1, y1) = (
        (rect.x + rect.w) as f64 * source.0,
        (rect.y + rect.h) as f64 * source.1,
    );
    [(x0, y0), (x1, y0), (x1, y1), (x0, y1)]
}

/// The frame's corners in source pixels: the canvas turned back by the
/// straighten angle about the image centre.
pub fn source_corners(rect: Rect, angle_degrees: f32, source: (f64, f64)) -> [(f64, f64); 4] {
    let centre = (source.0 / 2.0, source.1 / 2.0);
    corners_px(rect, source).map(|p| rotate_about(p, centre, -(angle_degrees as f64)))
}

/// Whether the whole frame lies inside the rotated image.
pub fn rect_fits(rect: Rect, angle_degrees: f32, source: (f64, f64)) -> bool {
    source_corners(rect, angle_degrees, source)
        .iter()
        .all(|&(x, y)| {
            x >= -FIT_EPS_PX
                && y >= -FIT_EPS_PX
                && x <= source.0 + FIT_EPS_PX
                && y <= source.1 + FIT_EPS_PX
        })
}

/// Largest `s <= 1` such that a frame of half extents `(hx, hy)` pixels,
/// centred on the source point `c` (inside the image), still fits when
/// scaled by `s` about that centre.
fn max_scale_about(c: (f64, f64), hx: f64, hy: f64, angle_degrees: f32, source: (f64, f64)) -> f64 {
    let mut scale: f64 = 1.0;
    for (sx, sy) in [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
        let (dx, dy) = rotate_about((sx * hx, sy * hy), (0.0, 0.0), -(angle_degrees as f64));
        for (centre, extent, d) in [(c.0, source.0, dx), (c.1, source.1, dy)] {
            if d > 1e-12 {
                scale = scale.min((extent - centre) / d);
            } else if d < -1e-12 {
                scale = scale.min(centre / -d);
            }
        }
    }
    scale.max(0.0)
}

fn lerp_rect(a: Rect, b: Rect, t: f32) -> Rect {
    let mix = |from: f32, to: f32| from + (to - from) * t;
    Rect::new(mix(a.x, b.x), mix(a.y, b.y), mix(a.w, b.w), mix(a.h, b.h))
}

fn from_centre(cx: f64, cy: f64, w: f64, h: f64, source: (f64, f64)) -> Rect {
    Rect::new(
        ((cx - w / 2.0) / source.0) as f32,
        ((cy - h / 2.0) / source.1) as f32,
        (w / source.0) as f32,
        (h / source.1) as f32,
    )
}

/// Pull `rect` inside the rotated image. A frame that already fits is
/// returned as is. Otherwise it keeps its size and slides toward the image
/// centre as little as it must; when even a centred frame is too big it
/// shrinks about the centre, keeping its aspect.
pub fn constrain_rect(rect: Rect, angle_degrees: f32, source: (f64, f64)) -> Rect {
    if rect_fits(rect, angle_degrees, source) {
        return rect;
    }
    let (w, h) = (rect.w as f64 * source.0, rect.h as f64 * source.1);
    let middle = (source.0 / 2.0, source.1 / 2.0);
    let centred = from_centre(middle.0, middle.1, w, h, source);
    if !rect_fits(centred, angle_degrees, source) {
        let s = max_scale_about(middle, w / 2.0, h / 2.0, angle_degrees, source);
        return from_centre(middle.0, middle.1, w * s, h * s, source);
    }
    let (mut lo, mut hi) = (0.0f32, 1.0f32);
    for _ in 0..40 {
        let mid = (lo + hi) / 2.0;
        if rect_fits(lerp_rect(rect, centred, mid), angle_degrees, source) {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    lerp_rect(rect, centred, hi)
}

/// The furthest point toward `wanted` that still fits, starting from a
/// frame that fits. Dragging a handle past the image edge stops at the edge
/// instead of snapping back. The fitting frames form a convex set, so the
/// segment from `valid` to `wanted` fits up to one parameter.
pub fn reach_rect(valid: Rect, wanted: Rect, angle_degrees: f32, source: (f64, f64)) -> Rect {
    if rect_fits(wanted, angle_degrees, source) {
        return wanted;
    }
    if !rect_fits(valid, angle_degrees, source) {
        return constrain_rect(wanted, angle_degrees, source);
    }
    let (mut lo, mut hi) = (0.0f32, 1.0f32);
    for _ in 0..40 {
        let mid = (lo + hi) / 2.0;
        if rect_fits(lerp_rect(valid, wanted, mid), angle_degrees, source) {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    lerp_rect(valid, wanted, lo)
}

/// The largest frame of pixel ratio `ratio` (width over height) that fits
/// inside `within` and shares its centre.
pub fn fit_ratio(within: Rect, ratio: f64, source: (f64, f64)) -> Rect {
    let (w, h) = (within.w as f64 * source.0, within.h as f64 * source.1);
    let (nw, nh) = if w / h > ratio {
        (h * ratio, h)
    } else {
        (w, w / ratio)
    };
    let (cx, cy) = (
        (within.x + within.w / 2.0) as f64 * source.0,
        (within.y + within.h / 2.0) as f64 * source.1,
    );
    from_centre(cx, cy, nw, nh, source)
}

/// Width over height of the frame in source pixels.
pub fn rect_ratio(rect: Rect, source: (f64, f64)) -> f64 {
    (rect.w as f64 * source.0) / (rect.h as f64 * source.1)
}

impl Crop {
    /// Width over height the frame is locked to, in pixels of the oriented
    /// `source`, or `None` for a free crop.
    pub fn aspect_ratio(&self, source: (f64, f64)) -> Option<f64> {
        let ratio = self.aspect?.ratio(source);
        Some(if self.aspect_flipped {
            1.0 / ratio
        } else {
            ratio
        })
    }

    /// Whether the crop changes nothing.
    pub fn is_identity(&self) -> bool {
        self.rect == Rect::full() && self.rotation_degrees == 0.0 && self.quarter_turns % 4 == 0
    }

    /// The frame as it lies in the image before the quarter turns, for
    /// formats that have no key for them. The straighten angle is the same
    /// either side of a quarter turn.
    pub fn unturned_rect(&self) -> Rect {
        (0..self.quarter_turns % 4).fold(self.rect, |rect, _| turn_rect(rect, false))
    }

    /// The crop with its frame pulled inside the rotated `source` image.
    pub fn constrained(&self, source: (f64, f64)) -> Crop {
        Crop {
            rect: constrain_rect(self.rect, self.rotation_degrees, source),
            ..self.clone()
        }
    }
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

    fn near(a: f64, b: f64, tol: f64) -> bool {
        (a - b).abs() <= tol
    }

    #[test]
    fn aspect_presets_resolve_to_pixel_ratios() {
        let source = (6000.0, 4000.0);
        assert!(near(Aspect::Original.ratio(source), 1.5, 1e-12));
        assert!(near(
            Aspect::Original.ratio((4000.0, 6000.0)),
            2.0 / 3.0,
            1e-12
        ));
        assert_eq!(Aspect::Square.ratio(source), 1.0);
        assert!(near(Aspect::ThreeTwo.ratio(source), 1.5, 1e-12));
        assert!(near(Aspect::TwoThree.ratio(source), 2.0 / 3.0, 1e-12));
        assert!(near(Aspect::FourThree.ratio(source), 4.0 / 3.0, 1e-12));
        assert!(near(Aspect::SixteenNine.ratio(source), 16.0 / 9.0, 1e-12));
        assert_eq!(Aspect::ALL.len(), 6);
        assert_eq!(Aspect::SixteenNine.label(), "16:9");
    }

    #[test]
    fn a_positive_angle_turns_the_image_clockwise() {
        // The canvas top-left corner samples the source up and to the left of
        // where an unrotated canvas would: the image content has turned
        // clockwise about its centre. Numbers worked by hand for a 200×100
        // image at +10°: the corner offset (-100, -50) from the centre turns
        // back by -10° to (-107.16, -31.88) from the centre.
        let corners = source_corners(Rect::full(), 10.0, (200.0, 100.0));
        assert!(near(corners[0].0, 100.0 - 107.1632, 1e-3), "{corners:?}");
        assert!(near(corners[0].1, 50.0 - 31.8756, 1e-3), "{corners:?}");
        // Opposite corner is the point reflection through the centre.
        assert!(near(corners[2].0, 100.0 + 107.1632, 1e-3));
        assert!(near(corners[2].1, 50.0 + 31.8756, 1e-3));
        // A negative angle mirrors it.
        let back = source_corners(Rect::full(), -10.0, (200.0, 100.0));
        assert!(near(back[0].1, 50.0 - 49.2404 - 17.3648, 1e-3), "{back:?}");
    }

    #[test]
    fn the_unrotated_frame_maps_to_its_own_pixels() {
        let rect = Rect::new(0.25, 0.1, 0.5, 0.8);
        let corners = source_corners(rect, 0.0, (400.0, 200.0));
        assert!(near(corners[0].0, 100.0, 1e-4) && near(corners[0].1, 20.0, 1e-4));
        assert!(near(corners[2].0, 300.0, 1e-4) && near(corners[2].1, 180.0, 1e-4));
        assert!(rect_fits(rect, 0.0, (400.0, 200.0)));
        assert!(!rect_fits(
            Rect::new(0.6, 0.0, 0.5, 1.0),
            0.0,
            (400.0, 200.0)
        ));
    }

    #[test]
    fn straightening_a_full_frame_shrinks_to_the_inscribed_rect() {
        // A square turned by 45° keeps a 1/(cos+sin) share of each edge.
        let source = (100.0, 100.0);
        assert!(!rect_fits(Rect::full(), 45.0, source));
        let fitted = constrain_rect(Rect::full(), 45.0, source);
        let share = 1.0 / (45f64.to_radians().cos() + 45f64.to_radians().sin());
        assert!(near(fitted.w as f64, share, 1e-4), "{fitted:?}");
        assert!(near(fitted.h as f64, share, 1e-4), "{fitted:?}");
        assert!(near((fitted.x + fitted.w / 2.0) as f64, 0.5, 1e-4));
        assert!(rect_fits(fitted, 45.0, source));
        // Idempotent: a frame that fits comes back untouched.
        assert_eq!(constrain_rect(fitted, 45.0, source), fitted);
    }

    #[test]
    fn a_small_frame_keeps_its_size_and_slides_inward() {
        let source = (300.0, 200.0);
        let corner = Rect::new(0.8, 0.7, 0.2, 0.3);
        assert!(!rect_fits(corner, 10.0, source));
        let moved = constrain_rect(corner, 10.0, source);
        assert!(rect_fits(moved, 10.0, source));
        assert!(near(moved.w as f64, 0.2, 1e-6) && near(moved.h as f64, 0.3, 1e-6));
        assert!(moved.x < corner.x && moved.y < corner.y, "{moved:?}");
    }

    #[test]
    fn constrain_always_lands_inside_for_any_angle() {
        let source = (600.0, 400.0);
        let rects = [
            Rect::full(),
            Rect::new(0.0, 0.0, 1.0, 0.5),
            Rect::new(0.6, 0.6, 0.4, 0.4),
            Rect::new(0.1, 0.2, 0.7, 0.7),
            Rect::new(0.0, 0.9, 0.1, 0.1),
        ];
        for tenth in -450..=450 {
            let angle = tenth as f32 / 10.0;
            for rect in rects {
                let fitted = constrain_rect(rect, angle, source);
                assert!(
                    rect_fits(fitted, angle, source),
                    "{rect:?} at {angle}° -> {fitted:?}"
                );
                assert!(fitted.w > 0.0 && fitted.h > 0.0);
                assert!(fitted.w <= rect.w + 1e-6 && fitted.h <= rect.h + 1e-6);
                if rect.w > 0.0 && rect.h > 0.0 {
                    let before = rect_ratio(rect, source);
                    assert!(near(rect_ratio(fitted, source), before, before * 1e-3));
                }
            }
        }
    }

    #[test]
    fn reaching_past_the_edge_stops_at_the_edge() {
        let source = (400.0, 200.0);
        let valid = Rect::new(0.25, 0.25, 0.5, 0.5);
        // Straight drag of the right edge far past the image at angle 0.
        let wanted = Rect::new(0.25, 0.25, 1.5, 0.5);
        let reached = reach_rect(valid, wanted, 0.0, source);
        assert!(rect_fits(reached, 0.0, source));
        assert!(
            near((reached.x + reached.w) as f64, 1.0, 1e-3),
            "{reached:?}"
        );
        // Under rotation the frame stops at the rotated image edge.
        let reached = reach_rect(valid, wanted, 8.0, source);
        assert!(rect_fits(reached, 8.0, source));
        assert!(reached.w > valid.w && reached.w < wanted.w, "{reached:?}");
        // Anything that fits is taken as is.
        let ok = Rect::new(0.3, 0.3, 0.2, 0.2);
        assert_eq!(reach_rect(valid, ok, 8.0, source), ok);
    }

    #[test]
    fn fit_ratio_centres_the_largest_matching_frame() {
        let source = (6000.0, 4000.0);
        let square = fit_ratio(Rect::full(), 1.0, source);
        assert!(near(rect_ratio(square, source), 1.0, 1e-5));
        assert!(near((square.h) as f64, 1.0, 1e-6));
        assert!(near(square.x as f64, (1.0 - square.w as f64) / 2.0, 1e-6));
        let wide = fit_ratio(Rect::full(), 16.0 / 9.0, source);
        assert!(near(rect_ratio(wide, source), 16.0 / 9.0, 1e-5));
        assert!(near(wide.w as f64, 1.0, 1e-6));
        assert!(near((wide.y + wide.h / 2.0) as f64, 0.5, 1e-6));
    }

    #[test]
    fn crop_json_round_trips_with_every_field() {
        let crop = Crop {
            rect: Rect::new(0.1, 0.2, 0.6, 0.5),
            rotation_degrees: -3.25,
            aspect: Some(Aspect::FourThree),
            aspect_flipped: true,
            quarter_turns: 3,
        };
        let json = serde_json::to_string(&crop).unwrap();
        assert_eq!(serde_json::from_str::<Crop>(&json).unwrap(), crop);
        assert!(!crop.is_identity());
        assert!(Crop::default().is_identity());
    }

    #[test]
    fn a_flipped_lock_turns_the_ratio_on_its_side() {
        let source = (6000.0, 4000.0);
        let mut crop = Crop::default();
        assert_eq!(crop.aspect_ratio(source), None);
        crop.aspect = Some(Aspect::ThreeTwo);
        assert!(near(crop.aspect_ratio(source).unwrap(), 1.5, 1e-12));
        crop.aspect_flipped = true;
        assert!(near(crop.aspect_ratio(source).unwrap(), 2.0 / 3.0, 1e-12));
        crop.aspect = Some(Aspect::Original);
        assert!(near(crop.aspect_ratio(source).unwrap(), 2.0 / 3.0, 1e-12));
        crop.aspect = Some(Aspect::Square);
        assert_eq!(crop.aspect_ratio(source), Some(1.0));
    }

    #[test]
    fn a_crop_saved_before_the_flip_flag_still_loads() {
        let json = r#"{"rect":{"x":0.1,"y":0.1,"w":0.5,"h":0.5},"rotation_degrees":2.0,"aspect":"ThreeTwo"}"#;
        let crop: Crop = serde_json::from_str(json).unwrap();
        assert!(!crop.aspect_flipped);
        assert_eq!(crop.aspect, Some(Aspect::ThreeTwo));
        assert_eq!(crop.quarter_turns, 0, "rows from before rotate load as 0");
    }

    #[test]
    fn a_crop_saved_with_quarter_turns_keeps_them() {
        let json = r#"{"rect":{"x":0,"y":0,"w":1,"h":1},"rotation_degrees":0.0,"aspect":null,"quarter_turns":1}"#;
        let crop: Crop = serde_json::from_str(json).unwrap();
        assert_eq!(crop.quarter_turns, 1);
        assert!(!crop.is_identity(), "a bare turn is still an edit");
    }

    #[test]
    fn a_quarter_turn_moves_the_frame_with_the_picture() {
        // A frame in the top-left corner of a landscape image sits in the
        // top-right of the portrait one after a clockwise turn.
        let rect = Rect::new(0.0, 0.0, 0.5, 0.25);
        let cw = turn_rect(rect, true);
        assert_eq!(cw, Rect::new(0.75, 0.0, 0.25, 0.5));
        assert_eq!(turn_rect(rect, false), Rect::new(0.0, 0.5, 0.25, 0.5));
        // Four turns come home, and the two directions undo each other.
        let start = Rect::new(0.1, 0.2, 0.3, 0.4);
        let round = (0..4).fold(start, |r, _| turn_rect(r, true));
        assert!(near(round.x as f64, 0.1, 1e-6) && near(round.h as f64, 0.4, 1e-6));
        let back = turn_rect(turn_rect(start, true), false);
        assert!(near(back.x as f64, 0.1, 1e-6) && near(back.w as f64, 0.3, 1e-6));
    }

    #[test]
    fn a_turned_frame_keeps_its_pixels_and_swaps_its_ratio() {
        let source = (6000.0, 4000.0);
        let rect = Rect::new(0.1, 0.2, 0.5, 0.4);
        for clockwise in [true, false] {
            let turned = turn_rect(rect, clockwise);
            let size = turned_size(source, 1);
            assert_eq!(size, (4000.0, 6000.0));
            assert!(near(rect_ratio(turned, size), 1.0 / rect_ratio(rect, source), 1e-5));
            assert!(rect_fits(turned, 0.0, size));
        }
        assert_eq!(turned_size(source, 2), source);
        assert_eq!(turned_size(source, 3), (4000.0, 6000.0));
    }

    #[test]
    fn the_unturned_frame_fits_the_straightened_original() {
        // A turned frame that fits stays a fitting frame once the turn is
        // taken back out, at the same straighten angle.
        let source = (600.0, 400.0);
        let turned_size = (400.0, 600.0);
        let rect = Rect::new(0.3, 0.3, 0.3, 0.2);
        let angle = 7.0;
        let turned = turn_rect(rect, true);
        assert!(rect_fits(turned, angle, turned_size));
        let crop = Crop {
            rect: turned,
            rotation_degrees: angle,
            quarter_turns: 1,
            ..Crop::default()
        };
        let before = crop.unturned_rect();
        assert!(near(before.x as f64, rect.x as f64, 1e-6));
        assert!(near(before.y as f64, rect.y as f64, 1e-6));
        assert!(near(before.w as f64, rect.w as f64, 1e-6));
        assert!(rect_fits(before, angle, source));
    }
}
