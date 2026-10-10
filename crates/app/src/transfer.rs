//! Copying develop settings between photos by group, like Lightroom's "Copy
//! settings" dialog. The groups are the develop panel's own sections plus the
//! crop, so what the dialog lists is what the panel shows. [`transfer`] is the
//! pure copy; [`TransferDialog`] is the checkbox list shown in a modal.

use std::rc::Rc;

use edit_core::{Crop, DevelopSettings};
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::DialogButtonProps;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::*;
use lightcraft_develop::controls;

use crate::panel::{self, Item, PANEL};

/// Key of the crop and straighten group.
pub const CROP_KEY: &str = "crop";

/// One checkbox of the dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Group {
    pub key: &'static str,
    pub title: &'static str,
    /// Checked when the dialog opens.
    pub default_on: bool,
}

/// Every group in panel order, then the crop.
pub fn groups() -> Vec<Group> {
    let mut groups: Vec<Group> = PANEL
        .iter()
        .map(|section| Group {
            key: section.key,
            title: section.title,
            default_on: true,
        })
        .collect();
    groups.push(Group {
        key: CROP_KEY,
        title: "Crop and straighten",
        default_on: false,
    });
    groups
}

/// The keys checked when the dialog opens.
pub fn default_keys() -> Vec<&'static str> {
    groups()
        .into_iter()
        .filter(|g| g.default_on)
        .map(|g| g.key)
        .collect()
}

/// A photo's settings as copied.
#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    pub develop: DevelopSettings,
    pub crop: Option<Crop>,
}

/// Copy the checked groups of `source` onto a photo's settings. Groups that
/// are not checked, and anything outside the groups, stay as they are.
pub fn transfer(
    source: &Snapshot,
    develop: &mut DevelopSettings,
    crop: &mut Option<Crop>,
    keys: &[&str],
) {
    for section in PANEL.iter().filter(|s| keys.contains(&s.key)) {
        for item in section.items {
            match item {
                Item::Slider(id, _) => {
                    if let Some(value) = controls::get(&source.develop, id) {
                        controls::set(develop, id, value);
                    }
                }
                Item::Toggle(id, _) => {
                    panel::toggle_set(develop, id, panel::toggle_get(&source.develop, id));
                }
                Item::WhiteBalanceMode => develop.wb = source.develop.wb,
                Item::Heading(_) => {}
            }
        }
        if section.key == "tone-curve" {
            develop.curve = source.develop.curve.clone();
        }
    }
    if keys.contains(&CROP_KEY) {
        *crop = source.crop.clone();
    }
}

/// Photos still waiting for a transfer. Taken a few at a time so a long
/// selection never holds the UI thread; dropping it stops the rest.
#[derive(Clone, Debug, PartialEq)]
pub struct Batch {
    pub source: Snapshot,
    pub keys: Vec<&'static str>,
    remaining: std::collections::VecDeque<i64>,
    total: usize,
}

impl Batch {
    pub fn new(source: Snapshot, keys: Vec<&'static str>, items: Vec<i64>) -> Self {
        Self {
            source,
            keys,
            total: items.len(),
            remaining: items.into(),
        }
    }

    /// The next `n` photos.
    pub fn take(&mut self, n: usize) -> Vec<i64> {
        let n = n.min(self.remaining.len());
        self.remaining.drain(..n).collect()
    }

    pub fn is_done(&self) -> bool {
        self.remaining.is_empty()
    }

    pub fn done(&self) -> usize {
        self.total - self.remaining.len()
    }

    pub fn total(&self) -> usize {
        self.total
    }
}

/// What the modal is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferKind {
    Copy,
    Sync,
}

impl TransferKind {
    fn title(self) -> &'static str {
        match self {
            Self::Copy => "Copy settings",
            Self::Sync => "Sync settings",
        }
    }

    fn confirm(self) -> &'static str {
        match self {
            Self::Copy => "Copy",
            Self::Sync => "Sync",
        }
    }
}

/// The checkbox list.
pub struct TransferDialog {
    checked: Vec<&'static str>,
}

impl TransferDialog {
    pub fn new(checked: Vec<&'static str>) -> Self {
        Self { checked }
    }

    pub fn checked(&self) -> Vec<&'static str> {
        self.checked.clone()
    }

    pub fn check_all(&mut self, cx: &mut Context<Self>) {
        self.checked = groups().into_iter().map(|g| g.key).collect();
        cx.notify();
    }

    pub fn check_none(&mut self, cx: &mut Context<Self>) {
        self.checked.clear();
        cx.notify();
    }

    pub fn toggle(&mut self, key: &'static str, on: bool, cx: &mut Context<Self>) {
        self.checked.retain(|k| *k != key);
        if on {
            self.checked.push(key);
        }
        cx.notify();
    }
}

impl Render for TransferDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let spacing = cx.theme().spacing_tokens();
        let rows = groups().into_iter().map(|group| {
            Checkbox::new(SharedString::from(format!("transfer-group-{}", group.key)))
                .label(group.title)
                .checked(self.checked.contains(&group.key))
                .on_click(cx.listener(move |this, on: &bool, _, cx| {
                    this.toggle(group.key, *on, cx);
                }))
        });
        v_flex()
            .id("transfer-dialog")
            .w_full()
            .gap(spacing.md)
            .child(
                h_flex()
                    .gap(spacing.sm)
                    .child(
                        Button::new("transfer-check-all")
                            .small()
                            .label("Check all")
                            .on_click(cx.listener(|this, _, _, cx| this.check_all(cx))),
                    )
                    .child(
                        Button::new("transfer-check-none")
                            .small()
                            .label("Check none")
                            .on_click(cx.listener(|this, _, _, cx| this.check_none(cx))),
                    ),
            )
            .child(
                v_flex()
                    .id("transfer-groups")
                    .max_h(px(360.))
                    .gap(spacing.sm)
                    .overflow_y_scrollbar()
                    .children(rows),
            )
    }
}

/// Called with the checked group keys when the dialog is confirmed.
pub type ConfirmHandler = Rc<dyn Fn(Vec<&'static str>, &mut Window, &mut App)>;

/// Open the group dialog as a modal. `on_confirm` gets the checked keys.
pub fn open_transfer_dialog(
    kind: TransferKind,
    checked: Vec<&'static str>,
    on_confirm: ConfirmHandler,
    restore_focus: FocusHandle,
    window: &mut Window,
    cx: &mut App,
) {
    if window.has_active_dialog(cx) {
        return;
    }
    let body = cx.new(|_| TransferDialog::new(checked));
    window.open_dialog(cx, move |dialog, _, _| {
        let chosen = body.clone();
        let on_confirm = on_confirm.clone();
        let restore = restore_focus.clone();
        dialog
            .title(kind.title())
            .w(px(360.))
            .margin_top(px(48.))
            .button_props(
                DialogButtonProps::default()
                    .ok_text(kind.confirm())
                    .show_cancel(true)
                    .cancel_text("Cancel"),
            )
            .on_close(move |_, window, cx| restore.focus(window, cx))
            .on_ok(move |_, window, cx| {
                let keys = chosen.read(cx).checked();
                on_confirm(keys, window, cx);
                true
            })
            .child(body.clone())
    });
}

#[cfg(test)]
mod tests {
    // Explicit imports: `super::*` would pull in the `gpui_kit::*` glob,
    // whose `test` macro shadows the built-in `#[test]`.
    use super::{Batch, CROP_KEY, Snapshot, default_keys, groups, transfer};
    use crate::panel::{self, Item, PANEL, sliders};
    use edit_core::{Crop, DevelopSettings};
    use lightcraft_develop::controls;

    fn all_moved() -> Snapshot {
        let mut develop = DevelopSettings::default();
        for (id, _) in sliders() {
            let spec = panel::spec(id).unwrap();
            // Not the default, still in range.
            let value = if spec.default == spec.max {
                spec.min
            } else {
                spec.max
            };
            controls::set(&mut develop, id, value);
        }
        develop.optics.lens_profile = true;
        develop.optics.remove_ca = true;
        Snapshot {
            develop,
            crop: Some(Crop {
                rect: edit_core::Rect::new(0.1, 0.1, 0.8, 0.8),
                rotation_degrees: 1.5,
                aspect: None,
                aspect_flipped: false,
            }),
        }
    }

    fn group_sliders(key: &str) -> Vec<&'static str> {
        panel::section(key)
            .unwrap()
            .items
            .iter()
            .filter_map(|i| match i {
                Item::Slider(id, _) => Some(*id),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn crop_is_a_group_and_off_by_default() {
        let all = groups();
        assert_eq!(all.last().unwrap().key, CROP_KEY);
        assert!(!all.last().unwrap().default_on);
        assert!(!default_keys().contains(&CROP_KEY));
        assert_eq!(all.len(), PANEL.len() + 1);
    }

    #[test]
    fn every_slider_belongs_to_exactly_one_group() {
        for (id, _) in sliders() {
            let owners = PANEL
                .iter()
                .filter(|s| group_sliders(s.key).contains(&id))
                .count();
            assert_eq!(owners, 1, "{id}");
        }
    }

    #[test]
    fn a_group_copies_its_own_controls_and_nothing_else() {
        let source = all_moved();
        for section in PANEL {
            let mut develop = DevelopSettings::default();
            let mut crop = None;
            transfer(&source, &mut develop, &mut crop, &[section.key]);
            let own = group_sliders(section.key);
            for (id, _) in sliders() {
                let want = if own.contains(&id) {
                    controls::get(&source.develop, id)
                } else {
                    controls::get(&DevelopSettings::default(), id)
                };
                assert_eq!(controls::get(&develop, id), want, "{} {id}", section.key);
            }
            assert_eq!(crop, None, "{}", section.key);
        }
    }

    #[test]
    fn crop_moves_only_when_checked_and_toggles_follow_their_group() {
        let source = all_moved();
        let mut develop = DevelopSettings::default();
        let mut crop = None;
        transfer(&source, &mut develop, &mut crop, &default_keys());
        assert_eq!(crop, None);
        assert!(develop.optics.lens_profile && develop.optics.remove_ca);
        transfer(&source, &mut develop, &mut crop, &[CROP_KEY]);
        assert_eq!(crop, source.crop);

        // An unchecked lens group leaves the target's toggles alone.
        let mut target = DevelopSettings::default();
        target.optics.lens_profile = true;
        transfer(&source, &mut target, &mut None, &["tone"]);
        assert!(target.optics.lens_profile);
    }

    #[test]
    fn white_balance_and_curve_groups_copy_their_state() {
        let mut source = all_moved();
        source.develop.wb.mode = lightcraft_develop::WbMode::Daylight;
        let mut develop = DevelopSettings::default();
        transfer(&source, &mut develop, &mut None, &["white-balance"]);
        assert_eq!(develop.wb, source.develop.wb);
        transfer(&source, &mut develop, &mut None, &["tone-curve"]);
        assert_eq!(develop.curve, source.develop.curve);
    }

    #[test]
    fn a_batch_hands_out_photos_in_order_and_counts_progress() {
        let mut batch = Batch::new(all_moved(), vec!["tone"], vec![5, 6, 7, 8, 9]);
        assert_eq!((batch.done(), batch.total()), (0, 5));
        assert_eq!(batch.take(2), [5, 6]);
        assert_eq!(batch.done(), 2);
        assert!(!batch.is_done());
        assert_eq!(batch.take(10), [7, 8, 9]);
        assert!(batch.is_done());
        assert_eq!(batch.take(3), Vec::<i64>::new());
    }

    #[test]
    fn no_keys_copy_nothing() {
        let source = all_moved();
        let mut develop = DevelopSettings::default();
        let mut crop = None;
        transfer(&source, &mut develop, &mut crop, &[]);
        assert_eq!(develop, DevelopSettings::default());
    }
}
