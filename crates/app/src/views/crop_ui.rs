//! Crop mode on the single view (plan sections 2.3 and 2.5): enter and leave,
//! the panel, the canvas overlay, and the pointer wiring.
//!
//! The drag, aspect and fit rules live in [`crate::crop`] and are covered by
//! `cargo test`. This module only turns pointer positions into the
//! straightened canvas's normalized coordinates and paints what the session
//! holds.
//!
//! Nothing in crop mode runs the develop pipeline per pointer event. Entering
//! renders the whole photo once; the canvas paints that image and the frame,
//! handles and grid over it, so frame drags cost one repaint. A straighten
//! angle turns a small copy of that image on a background thread (newest
//! request only) and the full-size copy when the pointer is let go. The
//! pipeline renders the committed crop when the mode ends.

use std::sync::Arc;

use edit_core::{Aspect, Rect, RenderSize};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::label::Label;
use gpui_kit::component::slider::{
    Slider as StraightenSlider, SliderEvent, SliderState, SliderValue,
};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, IconName, Selectable as _, Sizable as _, h_flex, v_flex,
};
use gpui_kit::*;
use image::RgbaImage;

use super::{Browser, Mode, PROXY_DRAG_LONG_EDGE, PROXY_RELEASE_LONG_EDGE};
use crate::crop::{CropSession, Grab, Handle};

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

    /// The draft crop, while crop mode is active.
    pub fn crop_draft(&self) -> Option<edit_core::Crop> {
        self.crop.as_ref().map(|session| session.draft().clone())
    }

    /// Whether the crop canvas has its image, so the frame can be edited.
    pub fn crop_ready(&self) -> bool {
        self.crop_base.is_some() && self.crop.as_ref().is_some_and(CropSession::has_source)
    }

    /// Where the canvas last painted the image, as `(x, y, w, h)` in window
    /// pixels.
    pub fn canvas_image_frame(&self) -> Option<(f32, f32, f32, f32)> {
        self.crop_frame.get().map(|f| (f.x, f.y, f.w, f.h))
    }

    /// Turn the image to `degrees` as if the slider were released there.
    pub fn straighten_to(&mut self, degrees: f32, cx: &mut Context<Self>) {
        self.on_straighten(degrees, true, cx);
        cx.notify();
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
        self.crop_base = None;
        self.crop_base_image = None;
        self.crop_small = None;
        self.crop_hover = None;
        self.zoom.reset();
        self.queue.clear_failed(item.item_id);
        // The one pipeline render of crop mode: the whole photo, uncropped.
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
        self.crop_base = None;
        self.crop_base_image = None;
        self.crop_small = None;
        self.crop_hover = None;
        self.crop_turn.clear();
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

    /// `X`: the frame and its lock turn between landscape and portrait.
    pub(super) fn flip_crop_orientation(&mut self, cx: &mut Context<Self>) {
        if let Some(session) = self.crop.as_mut() {
            session.flip_orientation();
            cx.notify();
        }
    }

    /// The crop and framing the next render uses: the whole uncropped photo
    /// in crop mode, the committed crop otherwise.
    pub(super) fn render_crop(&self) -> (Option<edit_core::Crop>, bool) {
        match &self.crop {
            Some(_) => (None, true),
            None => (self.develop.crop().cloned(), false),
        }
    }

    /// Whether a finished render belongs on the canvas. Crop mode takes only
    /// its own uncropped render, and the normal view takes the others, so a
    /// render that raced a mode change is dropped.
    pub(super) fn takes_frame(&self, uncropped: bool) -> bool {
        self.crop.is_some() == uncropped
    }

    /// The uncropped render landed: it becomes the crop canvas. A small copy
    /// for live straightening is made off the UI thread.
    pub(super) fn set_crop_base(&mut self, frame: edit_core::Frame, cx: &mut Context<Self>) {
        let (width, height) = (frame.width, frame.height);
        let Some(base) = RgbaImage::from_raw(width, height, frame.pixels) else {
            return;
        };
        let base = Arc::new(base);
        if let Some(session) = self.crop.as_mut() {
            session.set_source(width, height);
        }
        self.crop_base_image = Some(Arc::new(RenderImage::new([image::Frame::new(
            (*base).clone(),
        )])));
        self.crop_base = Some(Arc::clone(&base));
        self.crop_small = None;
        let angle = self
            .crop
            .as_ref()
            .map_or(0.0, |s| s.draft().rotation_degrees);
        self.show_turned(angle, false, cx);
        let item = self.develop_item_id();
        cx.spawn(async move |this, cx| {
            let small = cx
                .background_executor()
                .spawn(async move { shrink(&base, LIVE_LONG_EDGE) })
                .await;
            this.update(cx, |this, cx| {
                if this.crop.is_some() && this.develop_item_id() == item {
                    this.crop_small = Some(Arc::new(small));
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Put the photo turned to `degrees` on the canvas. At zero it is the
    /// render itself; otherwise the turn runs on a background thread, on the
    /// small copy while the angle is moving and on the full one when it
    /// settles, and only the newest request ever runs.
    fn show_turned(&mut self, degrees: f32, live: bool, cx: &mut Context<Self>) {
        let Some(item) = self.current_item().map(|row| row.item_id) else {
            return;
        };
        if degrees == 0.0 {
            self.crop_turn.clear();
            if let Some(image) = self.crop_base_image.clone() {
                self.show_canvas(item, image);
                cx.notify();
            }
            return;
        }
        if self.crop_turn.request((degrees, !live)) {
            self.spawn_turn_worker(cx);
        }
    }

    fn show_canvas(&mut self, item: i64, image: Arc<RenderImage>) {
        self.develop_image = Some(image);
        self.develop_image_item = Some(item);
        self.straightened_image = true;
    }

    fn spawn_turn_worker(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                let job = this
                    .update(cx, |this, _| {
                        let (degrees, full) = this.crop_turn.take()?;
                        let source = if full {
                            this.crop_base.clone()
                        } else {
                            this.crop_small.clone().or_else(|| this.crop_base.clone())
                        }?;
                        Some((degrees, source))
                    })
                    .ok()
                    .flatten();
                let Some((degrees, source)) = job else {
                    break;
                };
                let turned =
                    cx.background_executor()
                        .spawn(async move {
                            edit_render::straighten::straighten_preview(&source, degrees)
                        })
                        .await;
                let applied = this
                    .update(cx, |this, cx| {
                        let item = this.develop_item_id();
                        if this.crop.is_some() {
                            let image = Arc::new(RenderImage::new([image::Frame::new(turned)]));
                            this.show_canvas(item, image);
                        }
                        cx.notify();
                    })
                    .is_ok();
                if !applied {
                    break;
                }
            }
        })
        .detach();
    }

    /// The frame overlay, once the canvas shows the crop image.
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

    /// The pointer shape for what is under it (or held).
    pub(super) fn crop_cursor(&self) -> Option<CursorStyle> {
        let session = self.crop.as_ref()?;
        let grab = if session.is_rotating() {
            return Some(CursorStyle::ClosedHand);
        } else {
            self.crop_hover?
        };
        Some(match grab {
            Grab::Move => CursorStyle::OpenHand,
            Grab::Rotate => CursorStyle::PointingHand,
            Grab::Draw => CursorStyle::Crosshair,
            Grab::Handle(Handle::Top | Handle::Bottom) => CursorStyle::ResizeUpDown,
            Grab::Handle(Handle::Left | Handle::Right) => CursorStyle::ResizeLeftRight,
            Grab::Handle(Handle::TopLeft | Handle::BottomRight) => {
                CursorStyle::ResizeUpLeftDownRight
            }
            Grab::Handle(Handle::TopRight | Handle::BottomLeft) => {
                CursorStyle::ResizeUpRightDownLeft
            }
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
        let Some((at, view)) = self.pointer_in_canvas(position) else {
            return true;
        };
        if session.is_dragging() {
            let before = session.draft().rotation_degrees;
            let Some(session) = self.crop.as_mut() else {
                return true;
            };
            session.drag_to(at);
            let after = session.draft().rotation_degrees;
            if after != before {
                self.show_turned(after, true, cx);
            }
            cx.notify();
        } else {
            let hover = session.hit(at, view);
            if hover != self.crop_hover {
                self.crop_hover = hover;
                cx.notify();
            }
        }
        true
    }

    pub(super) fn crop_pointer_up(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(session) = self.crop.as_mut() else {
            return false;
        };
        let turned = session.is_rotating();
        let angle = session.draft().rotation_degrees;
        session.end_drag();
        if turned {
            // Settled: swap the small preview for the full-size turn.
            self.show_turned(angle, false, cx);
        }
        cx.notify();
        true
    }

    /// Slider moved: turn the image under the frame. The small copy follows
    /// the drag; the full one is made when the slider is let go.
    fn on_straighten(&mut self, degrees: f32, release: bool, cx: &mut Context<Self>) {
        let Some(session) = self.crop.as_mut() else {
            return;
        };
        session.set_angle(degrees);
        let angle = session.draft().rotation_degrees;
        self.straighten_dragging = !release;
        self.show_turned(angle, !release, cx);
    }

    pub fn set_crop_aspect(&mut self, aspect: Option<Aspect>, cx: &mut Context<Self>) {
        if let Some(session) = self.crop.as_mut() {
            session.set_aspect(aspect);
            cx.notify();
        }
    }

    /// The panel's Reset: the whole photo, level and free.
    pub fn reset_crop(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.crop.as_mut() else {
            return;
        };
        session.reset();
        self.straighten_dragging = false;
        self.show_turned(0.0, false, cx);
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

    /// The crop panel, laid out like the Develop panel it replaces: a title
    /// and hint, the aspect presets in an even grid, the straighten row
    /// (name, readout and reset on one line, slider below), and the actions.
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
        let presets: Vec<Option<Aspect>> = std::iter::once(None)
            .chain(Aspect::ALL.into_iter().map(Some))
            .collect();
        let mut preset_rows = Vec::new();
        // Free, Original and 1:1 on the first line; the ratios on the second.
        for (row, chunk) in [&presets[..3], &presets[3..]].into_iter().enumerate() {
            let mut line = h_flex().w_full().gap_1();
            for (col, preset) in chunk.iter().enumerate() {
                let preset = *preset;
                line = line.child(
                    div().flex_1().min_w_0().child(
                        Button::new(("crop-aspect", row * 3 + col))
                            .label(preset.map(Aspect::label).unwrap_or("Free"))
                            .xsmall()
                            .w_full()
                            .selected(aspect == preset)
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.set_crop_aspect(preset, cx);
                            })),
                    ),
                );
            }
            preset_rows.push(line);
        }
        v_flex()
            .w(px(240.0))
            .h_full()
            .p_3()
            .gap_3()
            .bg(bg)
            .text_color(fg)
            .border_l_1()
            .border_color(border)
            .child(
                v_flex().gap_1().child(Label::new("Crop").text_lg()).child(
                    Label::new(
                        "Drag on the photo to draw a frame, or outside the frame to straighten.",
                    )
                    .text_xs()
                    .text_color(muted),
                ),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        h_flex()
                            .w_full()
                            .items_center()
                            .justify_between()
                            .child(Label::new("Aspect ratio").text_xs().text_color(muted))
                            .child(
                                Button::new("crop-flip")
                                    .ghost()
                                    .xsmall()
                                    .label("Flip")
                                    .tooltip("Landscape or portrait (X)")
                                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                        this.flip_crop_orientation(cx);
                                    })),
                            ),
                    )
                    .children(preset_rows),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        h_flex()
                            .w_full()
                            .items_center()
                            .justify_between()
                            .child(Label::new("Straighten").text_xs())
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap_1()
                                    .child(
                                        Label::new(format!("{angle:+.1}°"))
                                            .text_xs()
                                            .text_color(muted),
                                    )
                                    .child(
                                        Button::new("crop-straighten-reset")
                                            .ghost()
                                            .xsmall()
                                            .icon(IconName::Undo2)
                                            .tooltip("Reset straighten")
                                            .disabled(angle == 0.0)
                                            .on_click(cx.listener(
                                                |this, _: &ClickEvent, _, cx| {
                                                    this.on_straighten(0.0, true, cx);
                                                    this.straighten_dragging = false;
                                                    cx.notify();
                                                },
                                            )),
                                    ),
                            ),
                    )
                    .child(StraightenSlider::new(&self.straighten)),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .justify_between()
                    .child(
                        Button::new("crop-reset")
                            .ghost()
                            .small()
                            .label("Reset")
                            .tooltip("Back to the whole photo")
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.reset_crop(cx);
                            })),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("crop-cancel")
                                    .outline()
                                    .small()
                                    .label("Cancel")
                                    .tooltip("Cancel (Esc)")
                                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                        this.finish_crop(false, cx);
                                    })),
                            )
                            .child(
                                Button::new("crop-apply")
                                    .primary()
                                    .small()
                                    .label("Apply")
                                    .tooltip("Apply (Enter)")
                                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                        this.finish_crop(true, cx);
                                    })),
                            ),
                    ),
            )
    }
}

/// Long edge of the copy turned while an angle is moving. Small enough to
/// turn well inside a frame, large enough to look like the photo.
const LIVE_LONG_EDGE: u32 = 900;

/// `image` shrunk so its long edge is at most `edge`.
fn shrink(image: &RgbaImage, edge: u32) -> RgbaImage {
    let long = image.width().max(image.height());
    if long <= edge {
        return image.clone();
    }
    let scale = edge as f32 / long as f32;
    let w = ((image.width() as f32 * scale).round() as u32).max(1);
    let h = ((image.height() as f32 * scale).round() as u32).max(1);
    image::imageops::resize(image, w, h, image::imageops::FilterType::Triangle)
}
