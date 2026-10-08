//! Crop mode on the single view (plan sections 2.3 and 2.5): enter and leave,
//! the panel, the canvas overlay, and the pointer wiring.
//!
//! The drag, aspect and fit rules live in [`crate::crop`] and are covered by
//! `cargo test`. This module only turns pointer positions into the
//! straightened canvas's normalized coordinates and paints what the session
//! holds. Renders go through the same coalescing worker as the sliders: while
//! the mode is active they show the whole straightened canvas, so the frame
//! sits over the image and the image turns under it.

use edit_core::{Aspect, Rect, RenderSize};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::label::Label;
use gpui_kit::component::slider::{
    Slider as StraightenSlider, SliderEvent, SliderState, SliderValue,
};
use gpui_kit::component::{ActiveTheme as _, Selectable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::*;

use super::{Browser, Mode, PROXY_DRAG_LONG_EDGE, PROXY_RELEASE_LONG_EDGE};
use crate::crop::{CropSession, Handle};

/// Where the canvas last painted the image, in window pixels. The pointer
/// maps into the straightened canvas through it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct ImageFrame {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Colors for the frame overlay, from the active theme.
#[derive(Clone, Copy)]
pub(super) struct OverlayColors {
    dim: Hsla,
    border: Hsla,
    grid: Hsla,
    handle: Hsla,
    handle_border: Hsla,
}

/// What the canvas paints over the image while the frame is edited.
#[derive(Clone, Copy)]
pub(super) struct Overlay {
    pub rect: Rect,
    pub colors: OverlayColors,
}

fn bounds(x: f32, y: f32, w: f32, h: f32) -> Bounds<Pixels> {
    Bounds::new(
        point(px(x), px(y)),
        gpui_kit::size(px(w.max(0.0)), px(h.max(0.0))),
    )
}

/// Dim everything outside the frame, then the border, thirds grid and eight
/// handles. Custom painting: the plan maps the crop surface to `canvas()`.
pub(super) fn paint_overlay(window: &mut Window, frame: ImageFrame, overlay: Overlay) {
    let Overlay { rect, colors } = overlay;
    let x0 = frame.x + rect.x * frame.w;
    let y0 = frame.y + rect.y * frame.h;
    let x1 = x0 + rect.w * frame.w;
    let y1 = y0 + rect.h * frame.h;
    let (right, bottom) = (frame.x + frame.w, frame.y + frame.h);
    for (x, y, w, h) in [
        (frame.x, frame.y, frame.w, y0 - frame.y),
        (frame.x, y1, frame.w, bottom - y1),
        (frame.x, y0, x0 - frame.x, y1 - y0),
        (x1, y0, right - x1, y1 - y0),
    ] {
        window.paint_quad(fill(bounds(x, y, w, h), colors.dim));
    }
    for third in 1..3 {
        let t = third as f32 / 3.0;
        window.paint_quad(fill(
            bounds(x0 + (x1 - x0) * t, y0, 1.0, y1 - y0),
            colors.grid,
        ));
        window.paint_quad(fill(
            bounds(x0, y0 + (y1 - y0) * t, x1 - x0, 1.0),
            colors.grid,
        ));
    }
    window.paint_quad(outline(
        bounds(x0, y0, x1 - x0, y1 - y0),
        colors.border,
        BorderStyle::Solid,
    ));
    for handle in Handle::ALL {
        let (hx, hy) = handle.position(rect);
        let (cx, cy) = (frame.x + hx * frame.w, frame.y + hy * frame.h);
        window.paint_quad(quad(
            bounds(cx - 4.0, cy - 4.0, 8.0, 8.0),
            px(1.0),
            colors.handle,
            px(1.0),
            colors.handle_border,
            BorderStyle::Solid,
        ));
    }
}

/// The straighten slider, with its range from LightCraft's control table
/// like the tone sliders, and its events routed to the browser.
pub(super) fn straighten_slider(cx: &mut Context<Browser>) -> Entity<SliderState> {
    let spec = lightcraft_develop::CONTROLS
        .iter()
        .find(|spec| spec.id == "crop.angle");
    let slider = cx.new(|_| match spec {
        Some(spec) => SliderState::new()
            .min(spec.min as f32)
            .max(spec.max as f32)
            .step(spec.step as f32),
        None => SliderState::new().min(-45.0).max(45.0).step(0.01),
    });
    cx.subscribe(&slider, |this, _, event: &SliderEvent, cx| {
        let (value, release) = match event {
            SliderEvent::Change(value) => (value, false),
            SliderEvent::Release(value) => (value, true),
        };
        if let SliderValue::Single(value) = value {
            this.on_straighten(*value, release, cx);
        }
        cx.notify();
    })
    .detach();
    slider
}

impl Browser {
    /// Whether crop mode is active.
    pub fn crop_active(&self) -> bool {
        self.crop.is_some()
    }

    /// `C`: enter crop mode, or apply the frame and leave.
    pub(super) fn toggle_crop(&mut self, cx: &mut Context<Self>) {
        if self.crop.is_some() {
            self.finish_crop(true, cx);
        } else {
            self.enter_crop(cx);
        }
    }

    fn enter_crop(&mut self, cx: &mut Context<Self>) {
        let Some(item) = self.current_item() else {
            return;
        };
        if self.mode != Mode::Single || self.develop.owner() != Some(item.item_id) {
            self.develop_status = "Crop needs a loaded photo".to_string();
            cx.notify();
            return;
        }
        self.crop = Some(CropSession::enter(self.develop.crop()));
        self.straightened_image = false;
        self.zoom.reset();
        self.queue.clear_failed(item.item_id);
        self.request_render(
            RenderSize::Proxy(PROXY_DRAG_LONG_EDGE),
            Some(RenderSize::Proxy(PROXY_RELEASE_LONG_EDGE)),
            cx,
        );
        cx.notify();
    }

    /// Leave crop mode, committing the draft when `apply` is set. The render
    /// that follows shows the item as committed again.
    pub(super) fn finish_crop(&mut self, apply: bool, cx: &mut Context<Self>) {
        let Some(session) = self.crop.take() else {
            return;
        };
        self.straighten_dragging = false;
        if apply {
            let crop = session.result();
            if crop.as_ref() != self.develop.crop() {
                self.develop.set_crop(crop);
                self.develop_dirty = true;
            }
        }
        let item = self.develop_item_id();
        if self.develop.owner() == Some(item) {
            self.queue.clear_failed(item);
            self.request_render(RenderSize::Proxy(PROXY_RELEASE_LONG_EDGE), None, cx);
        }
        if apply {
            self.persist_current_edit(cx);
        }
        cx.notify();
    }

    /// The crop and framing the next render uses: the whole straightened
    /// canvas in crop mode, the committed crop otherwise.
    pub(super) fn render_crop(&self) -> (Option<edit_core::Crop>, bool) {
        match &self.crop {
            Some(session) => (Some(session.draft().straightened_canvas()), true),
            None => (self.develop.crop().cloned(), false),
        }
    }

    /// A rendered frame landed: remember what framing it shows, and tell the
    /// session the source's size once it is the straightened canvas.
    pub(super) fn note_frame_framing(&mut self, straightened: bool, width: u32, height: u32) {
        self.straightened_image = straightened;
        if straightened && let Some(session) = self.crop.as_mut() {
            session.set_source(width, height);
        }
    }

    /// The frame overlay, once the canvas shows the straightened image.
    pub(super) fn crop_overlay(&self, cx: &App) -> Option<Overlay> {
        let session = self.crop.as_ref().filter(|_| self.straightened_image)?;
        let theme = cx.theme();
        Some(Overlay {
            rect: session.draft().rect,
            colors: OverlayColors {
                dim: theme.background.opacity(0.6),
                border: theme.primary,
                grid: theme.foreground.opacity(0.35),
                handle: theme.foreground,
                handle_border: theme.background,
            },
        })
    }

    fn pointer_in_canvas(&self, position: Point<Pixels>) -> Option<((f32, f32), (f32, f32))> {
        let frame = self.crop_frame.get()?;
        if frame.w <= 0.0 || frame.h <= 0.0 {
            return None;
        }
        let (x, y): (f32, f32) = (position.x.into(), position.y.into());
        Some((
            ((x - frame.x) / frame.w, (y - frame.y) / frame.h),
            (frame.w, frame.h),
        ))
    }

    /// Pointer down on the canvas. `true` when crop mode took it, so the
    /// view does not start a pan.
    pub(super) fn crop_pointer_down(
        &mut self,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.crop.is_none() {
            return false;
        }
        if let Some((at, view)) = self.pointer_in_canvas(position)
            && let Some(session) = self.crop.as_mut()
            && session.begin_drag(at, view)
        {
            cx.notify();
        }
        true
    }

    pub(super) fn crop_pointer_move(
        &mut self,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(session) = self.crop.as_ref() else {
            return false;
        };
        if session.is_dragging()
            && let Some((at, _)) = self.pointer_in_canvas(position)
            && let Some(session) = self.crop.as_mut()
        {
            session.drag_to(at);
            cx.notify();
        }
        true
    }

    pub(super) fn crop_pointer_up(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(session) = self.crop.as_mut() else {
            return false;
        };
        session.end_drag();
        cx.notify();
        true
    }

    /// Slider moved: turn the image under the frame and re-render, at the
    /// drag edge while held and the release edge when let go.
    fn on_straighten(&mut self, degrees: f32, release: bool, cx: &mut Context<Self>) {
        let Some(session) = self.crop.as_mut() else {
            return;
        };
        session.set_angle(degrees);
        self.straighten_dragging = !release;
        let size = if release {
            PROXY_RELEASE_LONG_EDGE
        } else {
            PROXY_DRAG_LONG_EDGE
        };
        self.request_render(RenderSize::Proxy(size), None, cx);
    }

    fn set_crop_aspect(&mut self, aspect: Option<Aspect>, cx: &mut Context<Self>) {
        if let Some(session) = self.crop.as_mut() {
            session.set_aspect(aspect);
            cx.notify();
        }
    }

    fn reset_crop(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.crop.as_mut() else {
            return;
        };
        session.reset();
        self.straighten_dragging = false;
        self.request_render(RenderSize::Proxy(PROXY_RELEASE_LONG_EDGE), None, cx);
        cx.notify();
    }

    /// Keep the straighten slider on the draft's angle unless it is being
    /// dragged.
    pub(super) fn sync_straighten(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.crop.as_ref() else {
            return;
        };
        let angle = session.draft().rotation_degrees;
        let matches = matches!(
            self.straighten.read(cx).value(),
            SliderValue::Single(v) if (v - angle).abs() < 1e-3
        );
        if !matches && !self.straighten_dragging {
            self.straighten.update(cx, |slider, cx| {
                slider.set_value(angle, window, cx);
            });
        }
    }

    /// The crop panel: aspect presets, straighten, and the way out.
    pub(super) fn render_crop_panel(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let (aspect, angle) = self
            .crop
            .as_ref()
            .map(|s| (s.draft().aspect, s.draft().rotation_degrees))
            .unwrap_or((None, 0.0));
        let theme = cx.theme();
        let (bg, fg, border, muted) = (
            theme.sidebar,
            theme.sidebar_foreground,
            theme.sidebar_border,
            theme.muted_foreground,
        );
        let presets = std::iter::once(None)
            .chain(Aspect::ALL.into_iter().map(Some))
            .enumerate()
            .map(|(i, preset)| {
                Button::new(("crop-aspect", i))
                    .label(preset.map(Aspect::label).unwrap_or("Free"))
                    .compact()
                    .small()
                    .selected(aspect == preset)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.set_crop_aspect(preset, cx);
                    }))
            });
        v_flex()
            .w(px(240.0))
            .h_full()
            .p_3()
            .gap_3()
            .bg(bg)
            .text_color(fg)
            .border_l_1()
            .border_color(border)
            .child(Label::new("Crop"))
            .child(
                Label::new("Drag the frame to move it, or drag a handle to resize.")
                    .text_xs()
                    .text_color(muted),
            )
            .child(Label::new("Aspect").text_sm())
            .child(h_flex().flex_wrap().gap_1().children(presets))
            .child(Label::new("Straighten").text_sm())
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .id("crop-straighten")
                            .flex_1()
                            .h(px(20.0))
                            .child(StraightenSlider::new(&self.straighten)),
                    )
                    .child(Label::new(format!("{angle:+.2}°")).text_xs()),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        Button::new("crop-reset").label("Reset").small().on_click(
                            cx.listener(|this, _: &ClickEvent, _, cx| this.reset_crop(cx)),
                        ),
                    )
                    .child(Button::new("crop-cancel").label("Cancel").small().on_click(
                        cx.listener(|this, _: &ClickEvent, _, cx| this.finish_crop(false, cx)),
                    ))
                    .child(
                        Button::new("crop-done")
                            .label("Done")
                            .small()
                            .primary()
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.finish_crop(true, cx);
                            })),
                    ),
            )
    }
}
