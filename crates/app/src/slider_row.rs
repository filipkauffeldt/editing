//! The slider row shared by the Develop sidebar and the crop panel: the name
//! on the left, a value and a reset button on the right, the slider below.
//! The caller owns the value (an input in the sidebar, a readout in the crop
//! panel) and what a reset does.

use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::label::Label;
use gpui_kit::component::slider::{Slider, SliderState};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

type ResetHandler = Rc<dyn Fn(&mut Window, &mut App)>;

#[derive(IntoElement)]
pub struct SliderRow {
    scope: SharedString,
    key: SharedString,
    label: SharedString,
    value: Option<AnyElement>,
    slider: Entity<SliderState>,
    at_default: bool,
    disabled: bool,
    on_reset: Option<ResetHandler>,
    reset_on_double_click: bool,
}

impl SliderRow {
    /// The ids of the row's parts are `{scope}-row-{key}` and
    /// `{scope}-reset-{key}`, so the pair must be unique among the rows on
    /// screen.
    pub fn new(
        scope: impl Into<SharedString>,
        key: impl Into<SharedString>,
        label: impl Into<SharedString>,
        slider: &Entity<SliderState>,
    ) -> Self {
        Self {
            scope: scope.into(),
            key: key.into(),
            label: label.into(),
            value: None,
            slider: slider.clone(),
            at_default: false,
            disabled: false,
            on_reset: None,
            reset_on_double_click: false,
        }
    }

    /// What shows beside the reset button.
    pub fn value(mut self, value: impl IntoElement) -> Self {
        self.value = Some(value.into_any_element());
        self
    }

    /// Whether the control already sits at its default, which disables reset.
    pub fn at_default(mut self, at_default: bool) -> Self {
        self.at_default = at_default;
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn on_reset(mut self, handler: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_reset = Some(Rc::new(handler));
        self
    }

    /// Double-clicking the row's name line resets it, like the button.
    pub fn reset_on_double_click(mut self) -> Self {
        self.reset_on_double_click = true;
        self
    }
}

impl RenderOnce for SliderRow {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let spacing = cx.theme().spacing_tokens();
        let Self {
            scope,
            key,
            label,
            value,
            slider,
            at_default,
            disabled,
            on_reset,
            reset_on_double_click,
        } = self;
        let button_handler = on_reset.clone();
        let double_handler = on_reset.filter(|_| reset_on_double_click && !disabled);
        v_flex()
            .w_full()
            .gap(spacing.xs)
            .child(
                h_flex()
                    .id(SharedString::from(format!("{scope}-row-{key}")))
                    .w_full()
                    .h_6()
                    .items_center()
                    .justify_between()
                    .gap(spacing.sm)
                    .when_some(double_handler, |row, handler| {
                        row.on_click(move |event, window, cx| {
                            if event.click_count() == 2 {
                                handler(window, cx);
                            }
                        })
                    })
                    .child(Label::new(label).text_xs())
                    .child(
                        h_flex()
                            .items_center()
                            .gap(spacing.xs)
                            .children(value)
                            .child(
                                Button::new(SharedString::from(format!("{scope}-reset-{key}")))
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Undo2)
                                    .tooltip("Reset")
                                    .disabled(at_default || disabled || button_handler.is_none())
                                    .on_click(move |_, window, cx| {
                                        if let Some(handler) = &button_handler {
                                            handler(window, cx);
                                        }
                                    }),
                            ),
                    ),
            )
            .child(Slider::new(&slider).disabled(disabled))
    }
}
