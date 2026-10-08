//! Develop session for M2 (plan sections 1.5, 2.3): the edit buffer, the
//! six LR-style tone sliders, drag tracking, and the pure scheduling rules
//! behind proxy renders and edit persistence.
//!
//! Pure, UI-free state covered by `cargo test`; the GPUI wiring lives in
//! [`crate::views`]. Sliders address `DevelopSettings` through LightCraft's
//! own control table — the only edit model.

use std::collections::{HashMap, HashSet};

use edit_core::{CancelToken, DevelopSettings, RenderSize};
use lightcraft_develop::ControlSpec;

/// The six LR-style tone sliders in develop-panel order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SliderId {
    Exposure,
    Contrast,
    Highlights,
    Shadows,
    Whites,
    Blacks,
}

impl SliderId {
    pub const ALL: [SliderId; 6] = [
        SliderId::Exposure,
        SliderId::Contrast,
        SliderId::Highlights,
        SliderId::Shadows,
        SliderId::Whites,
        SliderId::Blacks,
    ];

    /// LightCraft control id (range truth lives in the pinned control table).
    pub const fn control_id(self) -> &'static str {
        match self {
            SliderId::Exposure => "light.exposure",
            SliderId::Contrast => "light.contrast",
            SliderId::Highlights => "light.highlights",
            SliderId::Shadows => "light.shadows",
            SliderId::Whites => "light.whites",
            SliderId::Blacks => "light.blacks",
        }
    }

    pub fn spec(self) -> Option<&'static ControlSpec> {
        lightcraft_develop::CONTROLS
            .iter()
            .find(|spec| spec.id == self.control_id())
    }

    pub fn get(self, settings: &DevelopSettings) -> f64 {
        lightcraft_develop::controls::get(settings, self.control_id()).unwrap_or(0.0)
    }

    pub fn set(self, settings: &mut DevelopSettings, value: f64) {
        lightcraft_develop::controls::set(settings, self.control_id(), value);
    }

    pub fn label(self) -> &'static str {
        self.spec().map(|spec| spec.label).unwrap_or("")
    }
}

/// Pure develop state: the edit buffer plus the drag the view reports.
pub struct DevelopSession {
    buffer: DevelopSettings,
    /// Catalog item the buffer belongs to. `None` until a load lands, and
    /// again after a failed load: the buffer then holds another item's
    /// values, so edits and saves must not touch the new item.
    owner: Option<i64>,
    /// Slider a drag is live on, if any.
    pub dragging: Option<crate::develop::SliderId>,
}

impl DevelopSession {
    pub fn new(buffer: DevelopSettings) -> Self {
        Self {
            buffer,
            owner: None,
            dragging: None,
        }
    }

    /// The edit buffer (never mutated in place by callers).
    pub fn buffer(&self) -> &DevelopSettings {
        &self.buffer
    }

    /// The item the buffer holds the edit of.
    pub fn owner(&self) -> Option<i64> {
        self.owner
    }

    pub fn slider_value(&self, id: SliderId) -> f64 {
        id.get(&self.buffer)
    }

    /// Apply a live slider change (from `SliderEvent::Change`).
    pub fn set_slider(&mut self, id: SliderId, value: f64) {
        self.dragging = Some(id);
        let clamped = id.spec().map(|spec| spec.clamp(value)).unwrap_or(value);
        id.set(&mut self.buffer, clamped);
    }

    /// The drag ended (`SliderEvent::Release`): final value, no drag live.
    pub fn end_drag(&mut self, id: SliderId, value: f64) {
        self.set_slider(id, value);
        self.dragging = None;
    }

    /// Replace the buffer with `item`'s edit (navigation or a persistence
    /// load). Live drags report against the item that owned them, so reset
    /// drag tracking too.
    pub fn load_edit(&mut self, item: i64, buffer: DevelopSettings) {
        self.buffer = buffer;
        self.owner = Some(item);
        self.dragging = None;
    }

    /// The edit could not be read: keep the previous buffer, but it no longer
    /// belongs to anything, so it can neither be edited nor saved over the
    /// unreadable row.
    pub fn disown(&mut self) {
        self.owner = None;
        self.dragging = None;
    }
}

/// Latest-request render scheduling for one worker (plan section 2.3):
/// slider changes coalesce into the newest wanted size, a stale class of
/// render is cancelled, and a failed item is terminal for the paint-time
/// kick until a new edit or navigation.
#[derive(Default)]
pub struct RenderQueue {
    wanted: Option<RenderSize>,
    /// Rendered once the current render succeeds, unless a newer request
    /// arrived first (the drag-edge proxy on selection upgrades to the
    /// release edge).
    followup: Option<RenderSize>,
    inflight: Option<RenderSize>,
    token: CancelToken,
    worker: bool,
    failed: HashSet<i64>,
    /// Item whose load or render the paint-time kick already started.
    pending: Option<i64>,
}

impl RenderQueue {
    /// Ask for a render. The newest request wins; an in-flight render of a
    /// different size class is cancelled as stale, while one of the same
    /// class finishes so a steady drag still shows frames. Returns whether
    /// the caller must spawn the worker.
    pub fn request(&mut self, size: RenderSize, then: Option<RenderSize>) -> bool {
        self.wanted = Some(size);
        self.followup = then;
        if self.inflight.is_some_and(|running| running != size) {
            self.token.cancel();
        }
        !std::mem::replace(&mut self.worker, true)
    }

    /// The worker's next job: the wanted size and a fresh token. `None` when
    /// nothing is wanted, which also stops the worker.
    pub fn take(&mut self) -> Option<(RenderSize, CancelToken)> {
        let Some(size) = self.wanted.take() else {
            self.worker = false;
            return None;
        };
        self.token = CancelToken::new();
        self.inflight = Some(size);
        Some((size, self.token.clone()))
    }

    /// The taken render ended. A success moves on to its follow-up when
    /// nothing newer was asked for; a cancel or failure drops it.
    pub fn complete(&mut self, succeeded: bool) {
        self.inflight = None;
        let followup = self.followup.take();
        if succeeded && self.wanted.is_none() {
            self.wanted = followup;
        }
    }

    /// The cursor moved: cancel the in-flight render, forget what was wanted,
    /// and give failed items another chance.
    pub fn navigate(&mut self) {
        self.token.cancel();
        self.wanted = None;
        self.followup = None;
        self.failed.clear();
        self.pending = None;
    }

    /// Whether a render is running or wanted.
    pub fn busy(&self) -> bool {
        self.inflight.is_some() || self.wanted.is_some()
    }

    /// Whether the paint should start loading `item`: nothing resolved for
    /// it, nothing started, no failure on record, and no render underway.
    pub fn should_kick(&self, item: i64, resolved: Option<i64>) -> bool {
        resolved != Some(item)
            && self.pending != Some(item)
            && !self.failed.contains(&item)
            && !self.busy()
    }

    /// The paint kicked off `item`'s load.
    pub fn begin(&mut self, item: i64) {
        self.pending = Some(item);
    }

    /// `item`'s load or render ended; a stale item leaves the slot alone.
    pub fn finish(&mut self, item: i64) {
        if self.pending == Some(item) {
            self.pending = None;
        }
    }

    /// The item's load or render failed: the paint stops re-kicking it.
    pub fn mark_failed(&mut self, item: i64) {
        self.failed.insert(item);
        self.finish(item);
    }

    /// A new edit retries a failed item.
    pub fn clear_failed(&mut self, item: i64) {
        self.failed.remove(&item);
    }

    pub fn is_failed(&self, item: i64) -> bool {
        self.failed.contains(&item)
    }
}

/// Sidecar writes serialized per item: one in flight per item, and the
/// newest edit queued behind it replaces an older queued one.
#[derive(Default)]
pub struct SidecarQueue {
    inflight: HashSet<i64>,
    queued: HashMap<i64, String>,
}

impl SidecarQueue {
    /// Submit an edit's JSON. `Some` when the caller should write it now;
    /// `None` when it queued behind a write already in flight for the item.
    pub fn submit(&mut self, item: i64, json: String) -> Option<String> {
        if self.inflight.insert(item) {
            Some(json)
        } else {
            self.queued.insert(item, json);
            None
        }
    }

    /// The item's write finished: the queued JSON to write next, if any.
    pub fn ack(&mut self, item: i64) -> Option<String> {
        let next = self.queued.remove(&item);
        if next.is_none() {
            self.inflight.remove(&item);
        }
        next
    }

    /// Everything still queued, for the quit flush.
    pub fn drain_queued(&mut self) -> Vec<(i64, String)> {
        self.queued.drain().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sliders_address_the_six_tone_controls() {
        let mut session = DevelopSession::new(DevelopSettings::default());
        assert_eq!(session.slider_value(SliderId::Exposure), 0.0);
        session.set_slider(SliderId::Exposure, 0.5);
        session.set_slider(SliderId::Shadows, 30.0);
        session.set_slider(SliderId::Highlights, -20.0);
        assert_eq!(session.slider_value(SliderId::Exposure), 0.5);
        assert_eq!(session.slider_value(SliderId::Shadows), 30.0);
        assert_eq!(session.slider_value(SliderId::Highlights), -20.0);
        // Exposure clamps to the spec range (-5..5).
        session.set_slider(SliderId::Exposure, 99.0);
        assert_eq!(session.slider_value(SliderId::Exposure), 5.0);
    }

    #[test]
    fn release_clears_the_drag_and_keeps_the_final_value() {
        let mut session = DevelopSession::new(DevelopSettings::default());
        session.set_slider(SliderId::Whites, 10.0);
        assert_eq!(session.dragging, Some(SliderId::Whites));
        session.end_drag(SliderId::Whites, 12.0);
        assert_eq!(session.dragging, None);
        assert_eq!(session.slider_value(SliderId::Whites), 12.0);
    }

    #[test]
    fn navigation_replaces_the_buffer_and_resets_the_drag() {
        let mut session = DevelopSession::new(DevelopSettings::default());
        session.set_slider(SliderId::Contrast, 40.0);
        let fresh = DevelopSettings::default();
        session.load_edit(7, fresh);
        assert_eq!(session.dragging, None);
        assert_eq!(session.owner(), Some(7));
        assert_eq!(session.slider_value(SliderId::Contrast), 0.0);
    }

    #[test]
    fn a_failed_load_disowns_the_previous_buffer() {
        let mut session = DevelopSession::new(DevelopSettings::default());
        assert_eq!(session.owner(), None);
        let mut edited = DevelopSettings::default();
        SliderId::Exposure.set(&mut edited, 1.5);
        session.load_edit(3, edited);
        session.disown();
        // The previous values stay (nothing silently reverts to defaults),
        // but no item owns them any more.
        assert_eq!(session.owner(), None);
        assert_eq!(session.slider_value(SliderId::Exposure), 1.5);
    }

    const DRAG: RenderSize = RenderSize::Proxy(edit_render::PROXY_DRAG_LONG_EDGE);
    const RELEASE: RenderSize = RenderSize::Proxy(edit_render::PROXY_RELEASE_LONG_EDGE);

    #[test]
    fn slider_changes_coalesce_into_one_latest_request() {
        let mut queue = RenderQueue::default();
        // Only the first request spawns the worker.
        assert!(queue.request(DRAG, None));
        for _ in 0..50 {
            assert!(!queue.request(DRAG, None));
        }
        let (size, _) = queue.take().expect("a render is wanted");
        assert_eq!(size, DRAG);
        // The 50 requests were one render.
        queue.complete(true);
        assert!(queue.take().is_none());
    }

    #[test]
    fn a_second_request_cancels_the_first_when_it_is_stale() {
        let mut queue = RenderQueue::default();
        queue.request(DRAG, None);
        let (_, first) = queue.take().unwrap();
        // Same class while dragging: the running render finishes.
        queue.request(DRAG, None);
        assert!(!first.is_cancelled());
        // Release supersedes the drag-edge render: cancelled.
        queue.request(RELEASE, None);
        assert!(first.is_cancelled());
        queue.complete(false);
        let (size, second) = queue.take().unwrap();
        assert_eq!(size, RELEASE);
        assert!(!second.is_cancelled());
        // A drag after release cancels the release render in turn.
        queue.request(DRAG, None);
        assert!(second.is_cancelled());
    }

    #[test]
    fn selection_upgrades_from_the_drag_edge_to_the_release_edge() {
        let mut queue = RenderQueue::default();
        queue.request(DRAG, Some(RELEASE));
        assert_eq!(queue.take().unwrap().0, DRAG);
        queue.complete(true);
        assert_eq!(queue.take().unwrap().0, RELEASE);
        queue.complete(true);
        assert!(queue.take().is_none());

        // A newer request replaces the follow-up; a failure drops it.
        queue.request(DRAG, Some(RELEASE));
        queue.take().unwrap();
        queue.request(DRAG, None);
        queue.complete(true);
        assert_eq!(queue.take().unwrap().0, DRAG);
        queue.complete(true);
        queue.request(DRAG, Some(RELEASE));
        queue.take().unwrap();
        queue.complete(false);
        assert!(queue.take().is_none());
    }

    #[test]
    fn navigation_cancels_and_forgets_what_was_wanted() {
        let mut queue = RenderQueue::default();
        queue.request(DRAG, Some(RELEASE));
        let (_, token) = queue.take().unwrap();
        queue.request(RELEASE, None);
        queue.mark_failed(4);
        queue.navigate();
        assert!(token.is_cancelled());
        assert!(!queue.is_failed(4));
        queue.complete(false);
        assert!(queue.take().is_none());
        assert!(!queue.busy());
    }

    #[test]
    fn a_failed_item_is_not_rekicked_by_the_paint() {
        let mut queue = RenderQueue::default();
        assert!(queue.should_kick(7, None));
        queue.begin(7);
        assert!(!queue.should_kick(7, None), "already started");
        queue.mark_failed(7);
        // Frame after frame: the failure is terminal for the paint kick.
        for _ in 0..10 {
            assert!(!queue.should_kick(7, None));
        }
        assert!(queue.is_failed(7));
        // Other items are unaffected.
        assert!(queue.should_kick(8, None));
        // A new edit retries it...
        queue.clear_failed(7);
        assert!(queue.should_kick(7, None));
        // ...and so does navigating away and back.
        queue.mark_failed(7);
        queue.navigate();
        assert!(queue.should_kick(7, None));
        // A resolved item never kicks, and nothing kicks mid-render.
        assert!(!queue.should_kick(7, Some(7)));
        queue.request(DRAG, None);
        assert!(!queue.should_kick(7, None));
    }

    #[test]
    fn a_stale_finish_leaves_the_pending_slot_alone() {
        let mut queue = RenderQueue::default();
        queue.begin(2);
        queue.finish(1);
        assert!(!queue.should_kick(2, None));
        queue.finish(2);
        assert!(queue.should_kick(2, None));
    }

    #[test]
    fn sidecar_writes_serialize_per_item_and_the_newest_wins() {
        let mut queue = SidecarQueue::default();
        assert_eq!(queue.submit(1, "a".into()), Some("a".into()));
        // Behind the in-flight write for item 1: queued, newest replaces.
        assert_eq!(queue.submit(1, "b".into()), None);
        assert_eq!(queue.submit(1, "c".into()), None);
        // Another item is independent.
        assert_eq!(queue.submit(2, "x".into()), Some("x".into()));
        assert_eq!(queue.ack(1), Some("c".into()));
        assert_eq!(queue.ack(1), None);
        // Item 1 is idle again.
        assert_eq!(queue.submit(1, "d".into()), Some("d".into()));
        assert_eq!(queue.submit(2, "y".into()), None);
        assert_eq!(queue.drain_queued(), vec![(2, "y".to_string())]);
    }
}
