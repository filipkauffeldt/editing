//! Develop session for M2 (plan sections 1.5, 2.3): the edit buffer, the
//! six LR-style tone sliders, and drag tracking for proxy scheduling.
//!
//! Pure, UI-free state covered by `cargo test`; the GPUI wiring lives in
//! [`crate::views`]. Sliders address `DevelopSettings` through LightCraft's
//! own control table — the only edit model.

use edit_core::DevelopSettings;
use lightcraft_develop::ControlSpec;

/// Long edge of the downscaled proxy rendered during a drag.
pub const PROXY_DRAG_LONG_EDGE: u32 = edit_render::PROXY_DRAG_LONG_EDGE;

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
    /// Slider a drag is live on, if any.
    pub dragging: Option<crate::develop::SliderId>,
}

impl DevelopSession {
    pub fn new(buffer: DevelopSettings) -> Self {
        Self {
            buffer,
            dragging: None,
        }
    }

    /// The edit buffer (never mutated in place by callers).
    pub fn buffer(&self) -> &DevelopSettings {
        &self.buffer
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

    /// Replace the buffer (navigation or a persistence load). Live drags
    /// report against the item that owned them, so reset drag tracking too.
    pub fn load_edit(&mut self, buffer: DevelopSettings) {
        self.buffer = buffer;
        self.dragging = None;
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
        session.load_edit(fresh);
        assert_eq!(session.dragging, None);
        assert_eq!(session.slider_value(SliderId::Contrast), 0.0);
    }
}
