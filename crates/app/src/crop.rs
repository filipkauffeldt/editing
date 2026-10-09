//! Crop mode (plan sections 2.3 and 2.5): the draft crop, handle drags with
//! an aspect lock, and hit-testing.
//!
//! Pure, UI-free state covered by `cargo test`; the canvas, panel and mouse
//! wiring live in [`crate::views`]. Every rect here is normalized to the
//! straightened canvas (see [`edit_core::Crop`]), the same frame the crop
//! canvas paints, so a pointer maps to it with a divide and nothing else.

use edit_core::crop::{MAX_ANGLE, MIN_EDGE, constrain_rect, fit_ratio, reach_rect};
use edit_core::{Aspect, Crop, Rect};

/// Screen pixels around an edge or corner that grab it.
pub const GRAB_PX: f32 = 10.0;

/// How far from a full, level frame still counts as no crop at all, as a
/// share of the image edge. An aspect fit on a matching image lands on the
/// full frame only up to float noise.
const IDENTITY_SLACK: f32 = 1e-4;

/// The eight handles around the frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handle {
    TopLeft,
    Top,
    TopRight,
    Right,
    BottomRight,
    Bottom,
    BottomLeft,
    Left,
}

impl Handle {
    pub const ALL: [Handle; 8] = [
        Handle::TopLeft,
        Handle::Top,
        Handle::TopRight,
        Handle::Right,
        Handle::BottomRight,
        Handle::Bottom,
        Handle::BottomLeft,
        Handle::Left,
    ];

    /// Which sides of the frame this handle moves: left, right, top, bottom.
    fn sides(self) -> (bool, bool, bool, bool) {
        match self {
            Handle::TopLeft => (true, false, true, false),
            Handle::Top => (false, false, true, false),
            Handle::TopRight => (false, true, true, false),
            Handle::Right => (false, true, false, false),
            Handle::BottomRight => (false, true, false, true),
            Handle::Bottom => (false, false, false, true),
            Handle::BottomLeft => (true, false, false, true),
            Handle::Left => (true, false, false, false),
        }
    }

    /// Where the handle sits on a frame, normalized.
    pub fn position(self, rect: Rect) -> (f32, f32) {
        let (left, right, top, bottom) = self.sides();
        let x = if left {
            rect.x
        } else if right {
            rect.x + rect.w
        } else {
            rect.x + rect.w / 2.0
        };
        let y = if top {
            rect.y
        } else if bottom {
            rect.y + rect.h
        } else {
            rect.y + rect.h / 2.0
        };
        (x, y)
    }
}

/// What a press on the canvas picked up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Grab {
    Handle(Handle),
    Move,
}

/// The handle, the frame body, or nothing under `at` (normalized), for an
/// image painted `view` pixels across. Corners win over edges, edges over
/// the body; the grab band shrinks on a small frame so the body stays
/// reachable.
pub fn hit_test(rect: Rect, at: (f32, f32), view: (f32, f32)) -> Option<Grab> {
    if view.0 <= 0.0 || view.1 <= 0.0 {
        return None;
    }
    let tol_x = (GRAB_PX / view.0).min(rect.w / 3.0);
    let tol_y = (GRAB_PX / view.1).min(rect.h / 3.0);
    let (x1, y1) = (rect.x + rect.w, rect.y + rect.h);
    let near_left = (at.0 - rect.x).abs() <= tol_x;
    let near_right = (at.0 - x1).abs() <= tol_x;
    let near_top = (at.1 - rect.y).abs() <= tol_y;
    let near_bottom = (at.1 - y1).abs() <= tol_y;
    let across = at.0 >= rect.x - tol_x && at.0 <= x1 + tol_x;
    let down = at.1 >= rect.y - tol_y && at.1 <= y1 + tol_y;
    let handle = match (near_left, near_right, near_top, near_bottom) {
        (true, _, true, _) => Handle::TopLeft,
        (_, true, true, _) => Handle::TopRight,
        (_, true, _, true) => Handle::BottomRight,
        (true, _, _, true) => Handle::BottomLeft,
        (_, _, true, _) if across => Handle::Top,
        (_, _, _, true) if across => Handle::Bottom,
        (true, ..) if down => Handle::Left,
        (_, true, ..) if down => Handle::Right,
        _ => {
            return rect
                .contains(edit_core::Point::new(at.0, at.1))
                .then_some(Grab::Move);
        }
    };
    Some(Grab::Handle(handle))
}

/// A grab in progress: the frame and pointer it started from.
struct Drag {
    grab: Grab,
    start: Rect,
    from: (f32, f32),
}

/// The draft behind crop mode. It starts from the item's committed crop and
/// only becomes the committed crop when the user applies it.
pub struct CropSession {
    draft: Crop,
    /// The oriented source size in pixels, known once the straightened
    /// canvas has painted. Aspect math and the fit checks need it.
    source: Option<(f64, f64)>,
    drag: Option<Drag>,
}

impl CropSession {
    pub fn enter(committed: Option<&Crop>) -> Self {
        Self {
            draft: committed.cloned().unwrap_or_default(),
            source: None,
            drag: None,
        }
    }

    pub fn draft(&self) -> &Crop {
        &self.draft
    }

    /// Whether the straightened canvas has painted, so edits can run.
    pub fn has_source(&self) -> bool {
        self.source.is_some()
    }

    pub fn is_dragging(&self) -> bool {
        self.drag.is_some()
    }

    /// The straightened canvas painted at `width × height`: the source's
    /// own aspect. Pulls the draft inside the rotated image.
    pub fn set_source(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        let source = (width as f64, height as f64);
        self.source = Some(source);
        self.draft.rect = constrain_rect(self.draft.rect, self.draft.rotation_degrees, source);
    }

    /// Turn the image under the frame. The frame shrinks or slides as far as
    /// it must to stay inside the rotated image.
    pub fn set_angle(&mut self, degrees: f32) {
        self.draft.rotation_degrees = degrees.clamp(-MAX_ANGLE, MAX_ANGLE);
        if let Some(source) = self.source {
            self.draft.rect = constrain_rect(self.draft.rect, self.draft.rotation_degrees, source);
        }
    }

    /// Lock an aspect preset, or free the frame with `None`. A lock fits the
    /// largest frame of that ratio inside the current one, about its centre.
    pub fn set_aspect(&mut self, aspect: Option<Aspect>) {
        self.draft.aspect = aspect;
        if let (Some(aspect), Some(source)) = (aspect, self.source) {
            let fitted = fit_ratio(self.draft.rect, aspect.ratio(source), source);
            self.draft.rect = constrain_rect(fitted, self.draft.rotation_degrees, source);
        }
    }

    /// Back to the full, level, free frame.
    pub fn reset(&mut self) {
        self.draft = Crop::default();
        self.drag = None;
    }

    /// Press at `at` (normalized) on an image painted `view` pixels across.
    /// `true` when something was grabbed.
    pub fn begin_drag(&mut self, at: (f32, f32), view: (f32, f32)) -> bool {
        if self.source.is_none() {
            return false;
        }
        let Some(grab) = hit_test(self.draft.rect, at, view) else {
            return false;
        };
        self.drag = Some(Drag {
            grab,
            start: self.draft.rect,
            from: at,
        });
        true
    }

    /// The pointer moved to `at` (normalized) with a grab held. The frame
    /// follows as far as the rotated image allows.
    pub fn drag_to(&mut self, at: (f32, f32)) {
        let (Some(drag), Some(source)) = (self.drag.as_ref(), self.source) else {
            return;
        };
        let delta = (at.0 - drag.from.0, at.1 - drag.from.1);
        let ratio = self.draft.aspect.map(|a| a.ratio(source));
        let wanted = wanted_rect(drag.start, drag.grab, delta, ratio, source);
        let angle = self.draft.rotation_degrees;
        let current = self.draft.rect;
        self.draft.rect = if ratio.is_some() {
            reach_rect(current, wanted, angle, source)
        } else {
            // Free drags settle one axis at a time, so a frame pushed past a
            // corner slides along the edge instead of sticking to it.
            let across = Rect::new(wanted.x, current.y, wanted.w, current.h);
            let reached = reach_rect(current, across, angle, source);
            reach_rect(
                reached,
                Rect::new(reached.x, wanted.y, reached.w, wanted.h),
                angle,
                source,
            )
        };
    }

    pub fn end_drag(&mut self) {
        self.drag = None;
    }

    /// The crop to commit: `None` when the draft changes nothing.
    pub fn result(&self) -> Option<Crop> {
        let rect = self.draft.rect;
        let full = rect.x.abs() < IDENTITY_SLACK
            && rect.y.abs() < IDENTITY_SLACK
            && (rect.w - 1.0).abs() < IDENTITY_SLACK
            && (rect.h - 1.0).abs() < IDENTITY_SLACK;
        (!full || self.draft.rotation_degrees != 0.0).then(|| self.draft.clone())
    }
}

/// Where a grab would put the frame with no image edge in the way. A lock
/// keeps the pixel ratio: a corner follows the larger of the two moves from
/// the opposite corner, an edge resizes about the frame's middle.
fn wanted_rect(
    start: Rect,
    grab: Grab,
    delta: (f32, f32),
    ratio: Option<f64>,
    source: (f64, f64),
) -> Rect {
    let handle = match grab {
        Grab::Move => {
            return Rect::new(start.x + delta.0, start.y + delta.1, start.w, start.h);
        }
        Grab::Handle(handle) => handle,
    };
    let (left, right, top, bottom) = handle.sides();
    let (x0, y0) = (start.x, start.y);
    let (x1, y1) = (start.x + start.w, start.y + start.h);
    let nx0 = if left {
        (x0 + delta.0).min(x1 - MIN_EDGE)
    } else {
        x0
    };
    let nx1 = if right {
        (x1 + delta.0).max(x0 + MIN_EDGE)
    } else {
        x1
    };
    let ny0 = if top {
        (y0 + delta.1).min(y1 - MIN_EDGE)
    } else {
        y0
    };
    let ny1 = if bottom {
        (y1 + delta.1).max(y0 + MIN_EDGE)
    } else {
        y1
    };
    let (w, h) = (nx1 - nx0, ny1 - ny0);
    let Some(ratio) = ratio else {
        return Rect::new(nx0, ny0, w, h);
    };
    // Work in source pixels so the ratio is the picture's, not the canvas's.
    let (wpx, hpx) = (w as f64 * source.0, h as f64 * source.1);
    let horizontal = left || right;
    let vertical = top || bottom;
    let (lw, lh) = match (horizontal, vertical) {
        (true, true) => {
            let wide = wpx.max(hpx * ratio);
            (wide, wide / ratio)
        }
        (true, false) => (wpx, wpx / ratio),
        _ => (hpx * ratio, hpx),
    };
    // A narrow ratio on a small edge can undershoot the minimum.
    let grow = (MIN_EDGE as f64 * source.0 / lw)
        .max(MIN_EDGE as f64 * source.1 / lh)
        .max(1.0);
    let (lw, lh) = ((lw * grow / source.0) as f32, (lh * grow / source.1) as f32);
    let lx = if left {
        x1 - lw
    } else if right || !horizontal && !vertical {
        x0
    } else {
        start.x + (start.w - lw) / 2.0
    };
    let ly = if top {
        y1 - lh
    } else if bottom {
        y0
    } else {
        start.y + (start.h - lh) / 2.0
    };
    Rect::new(lx, ly, lw, lh)
}

#[cfg(test)]
mod tests {
    use super::*;
    use edit_core::crop::{rect_fits, rect_ratio};

    const SOURCE: (f64, f64) = (6000.0, 4000.0);
    const VIEW: (f32, f32) = (900.0, 600.0);

    fn session(rect: Rect) -> CropSession {
        let mut session = CropSession::enter(Some(&Crop {
            rect,
            ..Crop::default()
        }));
        session.set_source(6000, 4000);
        session
    }

    fn grab(session: &mut CropSession, at: (f32, f32)) {
        assert!(session.begin_drag(at, VIEW), "nothing to grab at {at:?}");
    }

    fn near(a: f64, b: f64, tol: f64) -> bool {
        (a - b).abs() <= tol
    }

    #[test]
    fn hit_testing_prefers_corners_then_edges_then_the_body() {
        let rect = Rect::new(0.2, 0.2, 0.6, 0.6);
        let hit = |x: f32, y: f32| hit_test(rect, (x, y), VIEW);
        assert_eq!(hit(0.2, 0.2), Some(Grab::Handle(Handle::TopLeft)));
        assert_eq!(hit(0.8, 0.2), Some(Grab::Handle(Handle::TopRight)));
        assert_eq!(hit(0.8, 0.8), Some(Grab::Handle(Handle::BottomRight)));
        assert_eq!(hit(0.2, 0.8), Some(Grab::Handle(Handle::BottomLeft)));
        assert_eq!(hit(0.5, 0.2), Some(Grab::Handle(Handle::Top)));
        assert_eq!(hit(0.5, 0.8), Some(Grab::Handle(Handle::Bottom)));
        assert_eq!(hit(0.2, 0.5), Some(Grab::Handle(Handle::Left)));
        assert_eq!(hit(0.8, 0.5), Some(Grab::Handle(Handle::Right)));
        assert_eq!(hit(0.5, 0.5), Some(Grab::Move));
        assert_eq!(hit(0.05, 0.05), None);
        assert_eq!(hit(0.95, 0.5), None);
        // Ten screen pixels either side of an edge still grab it.
        assert_eq!(
            hit(0.2 - 8.0 / 900.0, 0.5),
            Some(Grab::Handle(Handle::Left))
        );
        assert_eq!(hit(0.2 - 40.0 / 900.0, 0.5), None);
        assert_eq!(hit_test(rect, (0.5, 0.5), (0.0, 0.0)), None);
    }

    #[test]
    fn a_tiny_frame_keeps_a_body_to_grab() {
        let rect = Rect::new(0.4, 0.4, 0.03, 0.03);
        assert_eq!(hit_test(rect, (0.415, 0.415), VIEW), Some(Grab::Move));
    }

    #[test]
    fn handles_sit_on_the_frame() {
        let rect = Rect::new(0.2, 0.1, 0.6, 0.4);
        assert_eq!(Handle::TopLeft.position(rect), (0.2, 0.1));
        assert_eq!(Handle::Right.position(rect), (0.8, 0.3));
        assert_eq!(Handle::Bottom.position(rect), (0.5, 0.5));
        assert_eq!(Handle::ALL.len(), 8);
    }

    #[test]
    fn a_free_edge_drag_resizes_and_stops_at_the_image() {
        let mut s = session(Rect::new(0.2, 0.2, 0.5, 0.5));
        grab(&mut s, (0.7, 0.45));
        s.drag_to((0.85, 0.45));
        assert!(near(s.draft().rect.w as f64, 0.65, 1e-5), "{:?}", s.draft());
        assert!(near(s.draft().rect.h as f64, 0.5, 1e-5));
        s.drag_to((1.4, 0.45));
        let r = s.draft().rect;
        assert!(near((r.x + r.w) as f64, 1.0, 1e-3), "{r:?}");
        s.end_drag();
        assert!(!s.is_dragging());
    }

    #[test]
    fn a_free_corner_drag_cannot_flip_the_frame() {
        let mut s = session(Rect::new(0.2, 0.2, 0.5, 0.5));
        grab(&mut s, (0.2, 0.2));
        s.drag_to((0.95, 0.95));
        let r = s.draft().rect;
        assert!(r.w >= MIN_EDGE - 1e-6 && r.h >= MIN_EDGE - 1e-6, "{r:?}");
        assert!(near((r.x + r.w) as f64, 0.7, 1e-5), "anchor moved: {r:?}");
    }

    #[test]
    fn a_locked_corner_drag_keeps_the_pixel_ratio_from_every_corner() {
        for (corner, start, targets) in [
            (
                Handle::TopLeft,
                (0.25, 0.25),
                [(0.1, 0.2), (0.4, 0.3), (0.05, 0.05)],
            ),
            (
                Handle::TopRight,
                (0.75, 0.25),
                [(0.9, 0.2), (0.6, 0.3), (0.95, 0.05)],
            ),
            (
                Handle::BottomRight,
                (0.75, 0.75),
                [(0.9, 0.8), (0.6, 0.6), (0.99, 0.99)],
            ),
            (
                Handle::BottomLeft,
                (0.25, 0.75),
                [(0.1, 0.8), (0.4, 0.6), (0.02, 0.99)],
            ),
        ] {
            let mut s = session(Rect::new(0.25, 0.25, 0.5, 0.5));
            s.set_aspect(Some(Aspect::ThreeTwo));
            assert!(near(rect_ratio(s.draft().rect, SOURCE), 1.5, 1e-4));
            for target in targets {
                grab(&mut s, start);
                s.drag_to(target);
                s.end_drag();
                let r = s.draft().rect;
                assert!(
                    near(rect_ratio(r, SOURCE), 1.5, 1e-3),
                    "{corner:?} to {target:?}: {r:?}"
                );
                assert!(rect_fits(r, 0.0, SOURCE), "{corner:?} to {target:?}: {r:?}");
            }
        }
    }

    #[test]
    fn a_locked_edge_drag_resizes_about_the_middle() {
        let mut s = session(Rect::new(0.2, 0.2, 0.6, 0.4));
        s.set_aspect(Some(Aspect::Square));
        let before = s.draft().rect;
        let centre_y = before.y + before.h / 2.0;
        grab(&mut s, (before.x + before.w, centre_y));
        s.drag_to((before.x + before.w - 0.1, centre_y));
        let r = s.draft().rect;
        assert!(near(rect_ratio(r, SOURCE), 1.0, 1e-3), "{r:?}");
        assert!(r.w < before.w);
        assert!(near((r.y + r.h / 2.0) as f64, centre_y as f64, 1e-4));
        assert!(
            near(r.x as f64, before.x as f64, 1e-5),
            "left edge is the anchor"
        );
    }

    #[test]
    fn moving_slides_the_frame_without_resizing_it() {
        let mut s = session(Rect::new(0.2, 0.2, 0.4, 0.4));
        grab(&mut s, (0.4, 0.4));
        s.drag_to((0.5, 0.45));
        let r = s.draft().rect;
        assert!(
            near(r.x as f64, 0.3, 1e-5) && near(r.y as f64, 0.25, 1e-5),
            "{r:?}"
        );
        assert!(near(r.w as f64, 0.4, 1e-6) && near(r.h as f64, 0.4, 1e-6));
        // Pushed past the corner, it stops at the image.
        s.drag_to((5.0, 5.0));
        let r = s.draft().rect;
        assert!(near((r.x + r.w) as f64, 1.0, 1e-3) && near((r.y + r.h) as f64, 1.0, 1e-3));
        assert!(near(r.w as f64, 0.4, 1e-5));
    }

    #[test]
    fn dragging_under_a_straighten_angle_stays_inside_the_rotated_image() {
        let mut s = session(Rect::new(0.3, 0.3, 0.3, 0.3));
        s.set_angle(9.0);
        assert!(rect_fits(s.draft().rect, 9.0, SOURCE));
        for target in [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0), (0.5, 0.5)] {
            let centre = {
                let r = s.draft().rect;
                (r.x + r.w / 2.0, r.y + r.h / 2.0)
            };
            grab(&mut s, centre);
            s.drag_to(target);
            s.end_drag();
            assert!(rect_fits(s.draft().rect, 9.0, SOURCE), "{target:?}");
        }
        s.set_aspect(Some(Aspect::SixteenNine));
        let r = s.draft().rect;
        grab(&mut s, (r.x + r.w, r.y + r.h));
        s.drag_to((2.0, 2.0));
        assert!(rect_fits(s.draft().rect, 9.0, SOURCE));
        assert!(near(rect_ratio(s.draft().rect, SOURCE), 16.0 / 9.0, 2e-3));
    }

    #[test]
    fn straightening_shrinks_the_frame_to_the_rotated_image() {
        let mut s = session(Rect::full());
        s.set_angle(4.0);
        let r = s.draft().rect;
        assert!(r.w < 1.0 && r.h < 1.0);
        assert!(rect_fits(r, 4.0, SOURCE));
        assert!(near(rect_ratio(r, SOURCE), 1.5, 1e-3));
        // The angle clamps to the engine's range.
        s.set_angle(80.0);
        assert_eq!(s.draft().rotation_degrees, MAX_ANGLE);
        s.set_angle(-80.0);
        assert_eq!(s.draft().rotation_degrees, -MAX_ANGLE);
        assert!(rect_fits(s.draft().rect, -MAX_ANGLE, SOURCE));
    }

    #[test]
    fn an_angle_set_before_the_canvas_paints_is_applied_when_it_does() {
        let mut s = CropSession::enter(None);
        s.set_angle(6.0);
        assert!(!s.begin_drag((0.5, 0.5), VIEW), "no source yet");
        s.set_source(6000, 4000);
        assert!(rect_fits(s.draft().rect, 6.0, SOURCE));
        assert!(s.draft().rect.w < 1.0);
    }

    #[test]
    fn an_aspect_lock_fits_the_current_frame_about_its_centre() {
        let mut s = session(Rect::new(0.1, 0.2, 0.6, 0.6));
        let centre = (0.4f64, 0.5f64);
        s.set_aspect(Some(Aspect::SixteenNine));
        let r = s.draft().rect;
        assert!(near(rect_ratio(r, SOURCE), 16.0 / 9.0, 1e-4), "{r:?}");
        assert!(near((r.x + r.w / 2.0) as f64, centre.0, 1e-5));
        assert!(near((r.y + r.h / 2.0) as f64, centre.1, 1e-5));
        assert!(near(r.w as f64, 0.6, 1e-5), "width is the limit: {r:?}");
        // Original follows the source ratio.
        s.set_aspect(Some(Aspect::Original));
        assert!(near(rect_ratio(s.draft().rect, SOURCE), 1.5, 1e-4));
        // Free keeps the frame and drops the lock.
        let kept = s.draft().rect;
        s.set_aspect(None);
        assert_eq!(s.draft().rect, kept);
        assert_eq!(s.draft().aspect, None);
    }

    #[test]
    fn the_result_is_none_until_something_changes() {
        let mut s = session(Rect::full());
        assert_eq!(s.result(), None);
        // A lock that matches the image lands on the full frame up to noise.
        s.set_aspect(Some(Aspect::Original));
        assert_eq!(s.result(), None);
        s.set_angle(1.5);
        assert!(s.result().is_some());
        s.reset();
        assert_eq!(s.result(), None);
        assert_eq!(*s.draft(), Crop::default());

        let s = session(Rect::new(0.1, 0.1, 0.5, 0.5));
        let crop = s.result().unwrap();
        assert_eq!(crop.rect, Rect::new(0.1, 0.1, 0.5, 0.5));
    }

    #[test]
    fn entering_with_a_committed_crop_starts_from_it() {
        let committed = Crop {
            rect: Rect::new(0.2, 0.3, 0.5, 0.4),
            rotation_degrees: 2.0,
            aspect: Some(Aspect::FourThree),
            aspect_flipped: false,
        };
        let mut s = CropSession::enter(Some(&committed));
        assert_eq!(*s.draft(), committed);
        s.set_source(6000, 4000);
        assert_eq!(s.draft().rect, committed.rect, "a valid crop is left alone");
    }
}
