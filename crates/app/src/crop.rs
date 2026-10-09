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

/// How close to the photo's edges a frame counts as the whole photo.
const FULL_SLACK: f32 = 1e-3;

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
    /// The frame body: slides the frame.
    Move,
    /// Outside a frame that is smaller than the photo: turns the photo.
    Rotate,
    /// On an uncropped photo: draws a new frame from the press point.
    Draw,
}

/// Whether the frame is the whole photo, give or take noise.
pub fn is_full(rect: Rect) -> bool {
    rect.x <= FULL_SLACK
        && rect.y <= FULL_SLACK
        && rect.w >= 1.0 - FULL_SLACK
        && rect.h >= 1.0 - FULL_SLACK
}

/// What a press at `at` (normalized) picks up, for an image painted `view`
/// pixels across. Corners win over edges, edges over the body; the grab band
/// shrinks on a small frame so the body stays reachable. Inside the frame it
/// is the body, or a new frame when the frame is the whole photo; outside it
/// turns the photo, which a whole-photo frame has no room for.
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
            let inside = rect.contains(edit_core::Point::new(at.0, at.1));
            return match (inside, is_full(rect)) {
                (true, true) => Some(Grab::Draw),
                (true, false) => Some(Grab::Move),
                (false, false) => Some(Grab::Rotate),
                (false, true) => None,
            };
        }
    };
    Some(Grab::Handle(handle))
}

/// Hand-off to one background worker where only the newest request matters:
/// a request replaces an unstarted older one, so a fast pointer never builds
/// a queue of stale work.
#[derive(Debug)]
pub struct Latest<T> {
    wanted: Option<T>,
    worker: bool,
}

impl<T> Default for Latest<T> {
    fn default() -> Self {
        Self {
            wanted: None,
            worker: false,
        }
    }
}

impl<T> Latest<T> {
    /// Ask for `value`. `true` when the caller must start the worker.
    pub fn request(&mut self, value: T) -> bool {
        self.wanted = Some(value);
        !std::mem::replace(&mut self.worker, true)
    }

    /// The worker's next job. `None` stops the worker.
    pub fn take(&mut self) -> Option<T> {
        let next = self.wanted.take();
        if next.is_none() {
            self.worker = false;
        }
        next
    }

    /// Drop what has not started.
    pub fn clear(&mut self) {
        self.wanted = None;
    }
}

/// A grab in progress, with what it started from.
enum Drag {
    /// A handle or the body: the frame and pointer it started from.
    Frame {
        grab: Grab,
        start: Rect,
        from: (f32, f32),
    },
    /// Turning the photo from the angle it had at the press.
    Rotate { start_angle: f32, from: (f32, f32) },
    /// Drawing a new frame out from the press point.
    Draw { anchor: (f32, f32) },
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
    /// Start from the committed crop. A photo with none starts locked to its
    /// own aspect, so a frame drawn on it keeps the photo's shape; Free is
    /// one click away.
    pub fn enter(committed: Option<&Crop>) -> Self {
        Self {
            draft: committed.cloned().unwrap_or_else(Self::fresh),
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
        self.draft.aspect_flipped = false;
        if let (Some(aspect), Some(source)) = (aspect, self.source) {
            let fitted = fit_ratio(self.draft.rect, aspect.ratio(source), source);
            self.draft.rect = constrain_rect(fitted, self.draft.rotation_degrees, source);
        }
    }

    /// Turn the frame on its side: a landscape frame becomes portrait about
    /// its centre, and a lock flips with it (3:2 to 2:3). The frame shrinks
    /// only if the turned frame would leave the photo.
    pub fn flip_orientation(&mut self) {
        let Some(source) = self.source else {
            return;
        };
        if matches!(self.draft.aspect, Some(a) if a != Aspect::Square) {
            self.draft.aspect_flipped = !self.draft.aspect_flipped;
        }
        let rect = self.draft.rect;
        let (cx, cy) = (rect.x + rect.w / 2.0, rect.y + rect.h / 2.0);
        // Swap the frame's pixel width and height about its centre.
        let w = rect.h as f64 * source.1 / source.0;
        let h = rect.w as f64 * source.0 / source.1;
        let (w, h) = (w as f32, h as f32);
        let turned = Rect::new(cx - w / 2.0, cy - h / 2.0, w, h);
        self.draft.rect = constrain_rect(turned, self.draft.rotation_degrees, source);
    }

    /// The whole photo, level, locked to its own aspect.
    fn fresh() -> Crop {
        Crop {
            aspect: Some(Aspect::Original),
            ..Crop::default()
        }
    }

    /// Back to the whole photo, level, locked to its own aspect.
    pub fn reset(&mut self) {
        self.draft = Self::fresh();
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
        self.drag = Some(match grab {
            Grab::Rotate => Drag::Rotate {
                start_angle: self.draft.rotation_degrees,
                from: at,
            },
            Grab::Draw => Drag::Draw {
                anchor: (at.0.clamp(0.0, 1.0), at.1.clamp(0.0, 1.0)),
            },
            Grab::Handle(_) | Grab::Move => Drag::Frame {
                grab,
                start: self.draft.rect,
                from: at,
            },
        });
        true
    }

    /// The pointer moved to `at` (normalized) with a grab held. The frame
    /// follows as far as the rotated image allows, a rotate grab turns the
    /// photo about the frame's centre, and a draw grab stretches a new frame.
    pub fn drag_to(&mut self, at: (f32, f32)) {
        let (Some(drag), Some(source)) = (self.drag.as_ref(), self.source) else {
            return;
        };
        let ratio = self.draft.aspect_ratio(source);
        match *drag {
            Drag::Rotate { start_angle, from } => {
                let rect = self.draft.rect;
                let centre = (rect.x + rect.w / 2.0, rect.y + rect.h / 2.0);
                self.set_angle(start_angle + angle_delta(centre, from, at, source));
            }
            Drag::Draw { anchor } => {
                let drawn = draw_rect(anchor, at, ratio, source);
                // A press with no real stretch leaves the photo uncropped.
                if drawn.w >= MIN_EDGE && drawn.h >= MIN_EDGE {
                    self.draft.rect = drawn;
                }
            }
            Drag::Frame { grab, start, from } => {
                let delta = (at.0 - from.0, at.1 - from.1);
                let wanted = wanted_rect(start, grab, delta, ratio, source);
                let angle = self.draft.rotation_degrees;
                let current = self.draft.rect;
                self.draft.rect = if ratio.is_some() {
                    reach_rect(current, wanted, angle, source)
                } else {
                    // Free drags settle one axis at a time, so a frame pushed
                    // past a corner slides along the edge instead of sticking.
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
        }
    }

    pub fn end_drag(&mut self) {
        self.drag = None;
    }

    /// What a press at `at` would pick up, for hover feedback.
    pub fn hit(&self, at: (f32, f32), view: (f32, f32)) -> Option<Grab> {
        self.source?;
        hit_test(self.draft.rect, at, view)
    }

    /// Whether the grab held is turning the photo.
    pub fn is_rotating(&self) -> bool {
        matches!(self.drag, Some(Drag::Rotate { .. }))
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

/// Degrees the pointer has turned about `centre`, clockwise positive, from
/// `from` to `to` (all normalized; `source` is the photo's pixel size, so a
/// non-square photo does not skew the angle). Wrapped to -180..=180. A press
/// on the centre itself turns nothing.
pub fn angle_delta(
    centre: (f32, f32),
    from: (f32, f32),
    to: (f32, f32),
    source: (f64, f64),
) -> f32 {
    let vector = |p: (f32, f32)| {
        (
            (p.0 - centre.0) as f64 * source.0,
            (p.1 - centre.1) as f64 * source.1,
        )
    };
    let (a, b) = (vector(from), vector(to));
    if a.0.hypot(a.1) < 1.0 || b.0.hypot(b.1) < 1.0 {
        return 0.0;
    }
    // y grows downward, so atan2 grows clockwise on screen.
    let mut degrees = (b.1.atan2(b.0) - a.1.atan2(a.0)).to_degrees();
    if degrees > 180.0 {
        degrees -= 360.0;
    } else if degrees < -180.0 {
        degrees += 360.0;
    }
    degrees as f32
}

/// The frame drawn from `anchor` to the pointer at `at` (both normalized).
/// Free, it is the box between them; locked to `ratio` (width over height in
/// pixels) it follows the larger of the two moves. Either way it stays inside
/// the photo, shrinking toward the anchor when the pointer runs past an edge.
pub fn draw_rect(
    anchor: (f32, f32),
    at: (f32, f32),
    ratio: Option<f64>,
    source: (f64, f64),
) -> Rect {
    let at = (at.0.clamp(0.0, 1.0), at.1.clamp(0.0, 1.0));
    let (dx, dy) = (at.0 - anchor.0, at.1 - anchor.1);
    let (sx, sy) = (
        if dx < 0.0 { -1.0 } else { 1.0 },
        if dy < 0.0 { -1.0 } else { 1.0 },
    );
    let (mut w, mut h) = (dx.abs() as f64 * source.0, dy.abs() as f64 * source.1);
    if let Some(ratio) = ratio {
        w = w.max(h * ratio);
        h = w / ratio;
        // Room from the anchor to the photo's edge in the drawing direction.
        let room_w = (if sx > 0.0 { 1.0 - anchor.0 } else { anchor.0 }) as f64 * source.0;
        let room_h = (if sy > 0.0 { 1.0 - anchor.1 } else { anchor.1 }) as f64 * source.1;
        let fit = (room_w / w.max(1e-9)).min(room_h / h.max(1e-9)).min(1.0);
        w *= fit;
        h *= fit;
    }
    let (w, h) = ((w / source.0) as f32, (h / source.1) as f32);
    let x = if sx > 0.0 { anchor.0 } else { anchor.0 - w };
    let y = if sy > 0.0 { anchor.1 } else { anchor.1 - h };
    Rect::new(x, y, w, h)
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
        // Turning and drawing have their own drags; the frame stays put.
        Grab::Rotate | Grab::Draw => return start,
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
        assert_eq!(hit(0.05, 0.05), Some(Grab::Rotate));
        assert_eq!(hit(0.95, 0.5), Some(Grab::Rotate));
        assert_eq!(hit(1.3, -0.2), Some(Grab::Rotate), "past the photo too");
        // Ten screen pixels either side of an edge still grab it.
        assert_eq!(
            hit(0.2 - 8.0 / 900.0, 0.5),
            Some(Grab::Handle(Handle::Left))
        );
        assert_eq!(hit(0.2 - 40.0 / 900.0, 0.5), Some(Grab::Rotate));
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
        assert_eq!(*s.draft(), CropSession::fresh());

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

    #[test]
    fn a_whole_photo_frame_draws_inside_and_has_no_outside() {
        let full = Rect::full();
        let hit = |x: f32, y: f32| hit_test(full, (x, y), VIEW);
        assert_eq!(hit(0.5, 0.5), Some(Grab::Draw));
        assert_eq!(hit(0.3, 0.7), Some(Grab::Draw));
        // The edges still resize, and a press past the photo does nothing.
        assert_eq!(hit(1.0, 0.5), Some(Grab::Handle(Handle::Right)));
        assert_eq!(hit(0.0, 0.0), Some(Grab::Handle(Handle::TopLeft)));
        assert_eq!(hit(1.4, 0.5), None);
        assert!(is_full(full));
        assert!(!is_full(Rect::new(0.0, 0.0, 0.98, 1.0)));
    }

    #[test]
    fn the_pointer_angle_is_clockwise_positive_in_pixels() {
        let c = (0.5, 0.5);
        let square = (1000.0, 1000.0);
        // East to south of the centre is a quarter turn clockwise on screen.
        assert!(near(
            angle_delta(c, (0.9, 0.5), (0.5, 0.9), square) as f64,
            90.0,
            1e-3
        ));
        assert!(near(
            angle_delta(c, (0.5, 0.9), (0.9, 0.5), square) as f64,
            -90.0,
            1e-3
        ));
        // Across the wrap point the shortest way round wins.
        assert!(near(
            angle_delta(c, (0.1, 0.49), (0.1, 0.51), square) as f64,
            -2.865,
            0.02
        ));
        assert!(near(
            angle_delta(c, (0.1, 0.51), (0.1, 0.49), square) as f64,
            2.865,
            0.02
        ));
        // A wide photo does not skew it: the same pixel offsets give 45 degrees.
        let wide = (3000.0, 1000.0);
        let to = (0.5 + 100.0 / 3000.0, 0.5 + 100.0 / 1000.0);
        let from = (0.5 + 100.0 / 3000.0, 0.5);
        assert!(near(angle_delta(c, from, to, wide) as f64, 45.0, 0.01));
        // Pressing on the centre turns nothing.
        assert_eq!(angle_delta(c, c, (0.8, 0.5), square), 0.0);
    }

    #[test]
    fn dragging_outside_the_frame_turns_the_photo_and_clamps() {
        let mut s = session(Rect::new(0.3, 0.3, 0.4, 0.4));
        // East of the frame: outside it, so this grabs the photo.
        grab(&mut s, (0.95, 0.5));
        assert!(s.is_rotating());
        // Moving the pointer down turns the photo clockwise.
        s.drag_to((0.95, 0.62));
        let angle = s.draft().rotation_degrees;
        assert!(angle > 3.0 && angle < 20.0, "{angle}");
        assert!(rect_fits(s.draft().rect, angle, SOURCE));
        // Far past the limit it clamps.
        s.drag_to((0.5, 2.5));
        assert_eq!(s.draft().rotation_degrees.abs(), MAX_ANGLE);
        s.end_drag();
        assert!(!s.is_rotating());
        // The next turn starts from where the photo is.
        grab(&mut s, (0.95, 0.5));
        s.drag_to((0.95, 0.5));
        assert_eq!(s.draft().rotation_degrees.abs(), MAX_ANGLE);
    }

    #[test]
    fn drawing_on_an_uncropped_photo_stretches_a_new_frame() {
        let mut s = session(Rect::full());
        grab(&mut s, (0.3, 0.4));
        s.drag_to((0.6, 0.7));
        let r = s.draft().rect;
        assert!(
            near(r.x as f64, 0.3, 1e-5) && near(r.y as f64, 0.4, 1e-5),
            "{r:?}"
        );
        assert!(
            near(r.w as f64, 0.3, 1e-5) && near(r.h as f64, 0.3, 1e-5),
            "{r:?}"
        );
        // Dragging back past the anchor draws toward the other corner.
        s.drag_to((0.1, 0.1));
        let r = s.draft().rect;
        assert!(
            near(r.x as f64, 0.1, 1e-5) && near((r.x + r.w) as f64, 0.3, 1e-5),
            "{r:?}"
        );
        assert!(
            near(r.y as f64, 0.1, 1e-5) && near((r.y + r.h) as f64, 0.4, 1e-5),
            "{r:?}"
        );
        s.end_drag();
        // The new frame can be resized by its handles.
        grab(&mut s, (0.3, 0.4));
        s.drag_to((0.25, 0.35));
        assert!(
            near(s.draft().rect.w as f64, 0.15, 1e-5),
            "{:?}",
            s.draft().rect
        );
    }

    #[test]
    fn drawing_follows_the_selected_aspect_and_stays_in_the_photo() {
        let mut s = session(Rect::full());
        s.set_aspect(Some(Aspect::ThreeTwo));
        grab(&mut s, (0.2, 0.2));
        s.drag_to((0.5, 0.35));
        let r = s.draft().rect;
        assert!(near(rect_ratio(r, SOURCE), 1.5, 1e-3), "{r:?}");
        assert!(near(r.x as f64, 0.2, 1e-5) && near(r.y as f64, 0.2, 1e-5));
        // A tall pull is limited by the ratio, not the pointer.
        s.drag_to((0.3, 0.9));
        assert!(near(rect_ratio(s.draft().rect, SOURCE), 1.5, 1e-3));
        // From near a corner, running off the photo shrinks the frame toward
        // the anchor and keeps the ratio.
        s.end_drag();
        s.reset();
        s.set_aspect(Some(Aspect::Square));
        grab(&mut s, (0.85, 0.9));
        s.drag_to((1.5, 1.5));
        let r = s.draft().rect;
        assert!(r.x + r.w <= 1.0001 && r.y + r.h <= 1.0001, "{r:?}");
        assert!(near(rect_ratio(r, SOURCE), 1.0, 1e-3), "{r:?}");
        // Original and flipped locks.
        s.end_drag();
        s.reset();
        s.set_aspect(Some(Aspect::Original));
        grab(&mut s, (0.1, 0.1));
        s.drag_to((0.5, 0.5));
        assert!(near(rect_ratio(s.draft().rect, SOURCE), 1.5, 1e-3));
    }

    #[test]
    fn a_press_without_a_stretch_leaves_the_photo_uncropped() {
        let mut s = session(Rect::full());
        grab(&mut s, (0.5, 0.5));
        s.drag_to((0.505, 0.505));
        s.end_drag();
        assert_eq!(s.result(), None);
    }

    #[test]
    fn flipping_turns_the_frame_and_its_lock_on_their_side() {
        let mut s = session(Rect::new(0.2, 0.2, 0.6, 0.4));
        s.set_aspect(Some(Aspect::ThreeTwo));
        let before = s.draft().rect;
        s.flip_orientation();
        let r = s.draft().rect;
        assert!(s.draft().aspect_flipped);
        assert!(near(rect_ratio(r, SOURCE), 2.0 / 3.0, 1e-3), "{r:?}");
        // About the same centre, and inside the photo.
        assert!(near(
            (r.x + r.w / 2.0) as f64,
            (before.x + before.w / 2.0) as f64,
            1e-4
        ));
        assert!(rect_fits(r, 0.0, SOURCE));
        // Locked drags now keep the flipped ratio.
        grab(&mut s, (r.x + r.w, r.y + r.h));
        s.drag_to((r.x + r.w - 0.05, r.y + r.h - 0.05));
        assert!(near(rect_ratio(s.draft().rect, SOURCE), 2.0 / 3.0, 2e-3));
        s.end_drag();
        s.flip_orientation();
        assert!(!s.draft().aspect_flipped);
        assert!(near(rect_ratio(s.draft().rect, SOURCE), 1.5, 2e-3));
        // Picking a preset clears the flip; a square lock has no flip.
        s.flip_orientation();
        s.set_aspect(Some(Aspect::FourThree));
        assert!(!s.draft().aspect_flipped);
        s.set_aspect(Some(Aspect::Square));
        s.flip_orientation();
        assert!(!s.draft().aspect_flipped);
    }

    #[test]
    fn flipping_a_free_frame_swaps_its_pixel_width_and_height() {
        let mut s = session(Rect::new(0.1, 0.2, 0.6, 0.3));
        s.flip_orientation();
        let r = s.draft().rect;
        assert!(
            !s.draft().aspect_flipped,
            "free frames have no lock to flip"
        );
        assert!(near((r.w as f64) * SOURCE.0, 0.3 * SOURCE.1, 1.0), "{r:?}");
        assert!(
            near((r.h as f64) * SOURCE.1, 0.6 * SOURCE.0, 1.0) || r.h <= 1.0,
            "{r:?}"
        );
        assert!(rect_fits(r, 0.0, SOURCE));
    }

    #[test]
    fn only_the_newest_request_reaches_the_worker() {
        let mut latest = Latest::default();
        assert!(latest.request(1), "the first request starts the worker");
        assert!(!latest.request(2));
        assert!(!latest.request(3));
        assert_eq!(latest.take(), Some(3), "the stale ones are gone");
        // A request while the worker runs is picked up by its next take.
        assert!(!latest.request(4));
        assert_eq!(latest.take(), Some(4));
        assert_eq!(latest.take(), None, "an empty take stops the worker");
        assert!(latest.request(5), "so the next request starts a new one");
        latest.clear();
        assert_eq!(latest.take(), None);
    }

    #[test]
    fn hover_asks_the_same_question_as_a_press() {
        let s = session(Rect::new(0.2, 0.2, 0.6, 0.6));
        assert_eq!(s.hit((0.5, 0.5), VIEW), Some(Grab::Move));
        assert_eq!(s.hit((0.05, 0.5), VIEW), Some(Grab::Rotate));
        let before_paint = CropSession::enter(None);
        assert_eq!(before_paint.hit((0.5, 0.5), VIEW), None);
    }

    #[test]
    fn a_photo_without_a_crop_starts_locked_to_its_own_aspect() {
        let mut s = CropSession::enter(None);
        assert_eq!(s.draft().aspect, Some(Aspect::Original));
        s.set_source(6000, 4000);
        assert_eq!(s.result(), None, "the whole photo is still no crop");
        // Drawing keeps the photo's shape, and Free drops the lock.
        grab(&mut s, (0.2, 0.2));
        s.drag_to((0.5, 0.9));
        assert!(near(rect_ratio(s.draft().rect, SOURCE), 1.5, 1e-3));
        s.end_drag();
        s.reset();
        s.set_aspect(None);
        grab(&mut s, (0.2, 0.2));
        s.drag_to((0.5, 0.9));
        assert!(near(s.draft().rect.h as f64, 0.7, 1e-5));
        // A saved free crop reopens free.
        let saved = Crop {
            rect: Rect::new(0.1, 0.1, 0.5, 0.5),
            ..Crop::default()
        };
        assert_eq!(CropSession::enter(Some(&saved)).draft().aspect, None);
    }
}
