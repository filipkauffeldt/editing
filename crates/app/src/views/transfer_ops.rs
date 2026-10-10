//! Copy, paste, apply previous and sync of develop settings between photos.
//! The open photo goes through the edit buffer (live render and the usual
//! commit); every other photo is changed in its stored edit a few at a time,
//! and the run can be stopped between photos.

use std::rc::Rc;
use std::time::Duration;

use gpui_kit::*;

use super::{Browser, Mode};
use crate::develop_panel::TransferAvailability;
use crate::smart;
use crate::transfer::{
    Batch, Snapshot, TransferKind, default_keys, open_transfer_dialog, transfer,
};

/// Photos changed per step of a run.
const CHUNK: usize = 4;

pub(super) struct TransferState {
    copied: Option<Snapshot>,
    /// Groups pasted from the copied settings.
    keys: Vec<&'static str>,
    /// The last settings and groups applied.
    previous: Option<(Snapshot, Vec<&'static str>)>,
    batch: Option<Batch>,
    task: Option<Task<()>>,
}

impl Default for TransferState {
    fn default() -> Self {
        Self {
            copied: None,
            keys: default_keys(),
            previous: None,
            batch: None,
            task: None,
        }
    }
}

impl Browser {
    /// What the panel menu may offer.
    pub(super) fn transfer_availability(&self) -> TransferAvailability {
        TransferAvailability {
            copy: self.current_item().is_some(),
            paste: self.transfer.copied.is_some(),
            previous: self.transfer.previous.is_some(),
            sync: self.selection.selected.len() > 1,
            running: self.transfer.batch.is_some(),
        }
    }

    /// The current photo's settings: the open buffer, else the stored edit.
    fn current_snapshot(&self) -> Option<Snapshot> {
        let item = self.current_item()?.item_id;
        if self.develop.owner() == Some(item) {
            return Some(Snapshot {
                develop: self.develop.buffer().clone(),
                crop: self.develop.crop().cloned(),
            });
        }
        let stored = match self.unsaved.get(&item) {
            Some(edit) => Some((edit.develop.clone(), edit.crop.clone())),
            None => self
                .catalog
                .get_edit(item)
                .ok()?
                .map(|edit| (edit.develop_json, edit.crop_json)),
        };
        match stored {
            Some((develop, crop)) => {
                let edit = super::LoadedEdit::parse(&develop, crop.as_deref()).ok()?;
                Some(Snapshot {
                    develop: edit.develop,
                    crop: edit.crop,
                })
            }
            None => Some(Snapshot {
                develop: Default::default(),
                crop: None,
            }),
        }
    }

    /// Open the group dialog the panel asked for.
    pub(super) fn open_pending_transfer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.pending_dialog.take() {
            Some(TransferKind::Copy) => self.copy_settings(window, cx),
            Some(TransferKind::Sync) => self.sync_settings(window, cx),
            None => {}
        }
    }

    /// Ctrl+C: copy the current photo's settings and choose which groups to
    /// keep for pasting.
    pub fn copy_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(source) = self.current_snapshot() else {
            return;
        };
        self.transfer.copied = Some(source);
        let browser = cx.entity();
        open_transfer_dialog(
            TransferKind::Copy,
            self.transfer.keys.clone(),
            Rc::new(move |keys, _, cx| {
                browser.update(cx, |this, cx| {
                    this.develop_status = format!("Copied {} groups", keys.len());
                    this.transfer.keys = keys;
                    cx.notify();
                });
            }),
            self.focus.clone(),
            window,
            cx,
        );
    }

    /// Ctrl+V: paste the checked groups onto the current photo, or onto the
    /// whole selection.
    pub fn paste_settings(&mut self, cx: &mut Context<Self>) {
        let Some(source) = self.transfer.copied.clone() else {
            self.develop_status = "Nothing copied yet".into();
            cx.notify();
            return;
        };
        let keys = self.transfer.keys.clone();
        self.start_transfer(source, keys, true, cx);
    }

    /// Apply the last applied groups again, to the current photo or the
    /// selection.
    pub fn apply_previous(&mut self, cx: &mut Context<Self>) {
        let Some((source, keys)) = self.transfer.previous.clone() else {
            self.develop_status = "Nothing applied yet".into();
            cx.notify();
            return;
        };
        self.start_transfer(source, keys, true, cx);
    }

    /// Choose groups of the current photo and apply them to every other
    /// selected photo.
    pub fn sync_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.selection.selected.len() < 2 {
            self.develop_status = "Select more than one photo to sync".into();
            cx.notify();
            return;
        }
        let Some(source) = self.current_snapshot() else {
            return;
        };
        let browser = cx.entity();
        open_transfer_dialog(
            TransferKind::Sync,
            self.transfer.keys.clone(),
            Rc::new(move |keys, _, cx| {
                browser.update(cx, |this, cx| {
                    this.start_transfer(source.clone(), keys, false, cx);
                });
            }),
            self.focus.clone(),
            window,
            cx,
        );
    }

    /// Photos a transfer reaches: the selection, or the current photo.
    fn transfer_targets(&self, include_current: bool) -> Vec<i64> {
        let current = self.current_item().map(|row| row.item_id);
        let mut targets: Vec<i64> = self
            .selection
            .selected
            .iter()
            .filter_map(|index| self.rows.get(*index).map(|row| row.item_id))
            .collect();
        if targets.is_empty() {
            targets.extend(current);
        }
        if !include_current {
            targets.retain(|item| Some(*item) != current);
        }
        targets
    }

    fn start_transfer(
        &mut self,
        source: Snapshot,
        keys: Vec<&'static str>,
        include_current: bool,
        cx: &mut Context<Self>,
    ) {
        self.cancel_transfer(cx);
        let targets = self.transfer_targets(include_current);
        if targets.is_empty() || keys.is_empty() {
            return;
        }
        self.transfer.previous = Some((source.clone(), keys.clone()));
        // The buffer is live only for the photo on screen. After a move to
        // the grid it can still belong to another photo; that photo is
        // changed in its stored edit, so the buffer is committed and dropped
        // and the next visit reads the new edit.
        let live = self.current_item().map(|row| row.item_id);
        let (open_target, stored): (Vec<i64>, Vec<i64>) = targets
            .iter()
            .partition(|item| Some(**item) == live && self.develop.owner() == live);
        if let Some(owner) = self.develop.owner()
            && Some(owner) != live
            && stored.contains(&owner)
        {
            self.persist_current_edit(cx);
            self.develop.disown();
        }
        if !open_target.is_empty() {
            self.apply_to_open(&source, &keys, cx);
        }
        if stored.is_empty() {
            self.develop_status = "Settings pasted".into();
            cx.notify();
            return;
        }
        self.develop_status = format!("Applying to {} photos", stored.len());
        self.transfer.batch = Some(Batch::new(source, keys, stored));
        self.transfer.task = Some(cx.spawn(async move |this, cx| {
            loop {
                let more = this
                    .update(cx, |this, cx| this.transfer_chunk(cx))
                    .unwrap_or(false);
                if !more {
                    break;
                }
                cx.background_executor()
                    .timer(Duration::from_millis(2))
                    .await;
            }
        }));
        cx.notify();
    }

    /// The open photo: change the buffer so the render and the commit follow
    /// like any other edit.
    fn apply_to_open(&mut self, source: &Snapshot, keys: &[&str], cx: &mut Context<Self>) {
        if self.mode == Mode::Single && self.crop.is_some() {
            self.finish_crop(true, cx);
        }
        let item = self.develop_item_id();
        if self.develop.owner() != Some(item) {
            return;
        }
        let mut settings = self.develop.buffer().clone();
        let mut crop = self.develop.crop().cloned();
        transfer(source, &mut settings, &mut crop, keys);
        let crop_changed = crop.as_ref() != self.develop.crop();
        if crop_changed {
            self.develop.set_crop(crop);
        }
        if self.develop.edit(|buffer| *buffer = settings) || crop_changed {
            self.develop_dirty = true;
            self.want_smart_preview();
            self.queue.clear_failed(item);
            self.request_render(
                super::RenderSize::Proxy(super::PROXY_RELEASE_LONG_EDGE),
                None,
                cx,
            );
            self.persist_current_edit(cx);
        }
    }

    /// One step of a run. `false` when nothing is left.
    fn transfer_chunk(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(mut batch) = self.transfer.batch.take() else {
            return false;
        };
        let (source, keys) = (batch.source.clone(), batch.keys.clone());
        for item in batch.take(CHUNK) {
            self.edit_stored(
                item,
                |edit| transfer(&source, &mut edit.develop, &mut edit.crop, &keys),
                cx,
            );
            self.want_smart_preview_for(item);
        }
        let more = !batch.is_done();
        self.develop_status = if more {
            format!("Applied to {} of {} photos", batch.done(), batch.total())
        } else {
            format!("Applied to {} photos", batch.total())
        };
        if more {
            self.transfer.batch = Some(batch);
        }
        cx.notify();
        more
    }

    /// Stop a run between photos. `true` when one was running.
    pub fn cancel_transfer(&mut self, cx: &mut Context<Self>) -> bool {
        self.transfer.task = None;
        let Some(batch) = self.transfer.batch.take() else {
            return false;
        };
        self.develop_status = format!("Stopped after {} of {} photos", batch.done(), batch.total());
        cx.notify();
        true
    }

    /// A raw-only photo that just got an edit needs its smart preview.
    fn want_smart_preview_for(&mut self, item: i64) {
        let Some(row) = self.items.iter().find(|row| row.item_id == item) else {
            return;
        };
        if row.has_raw && !row.has_jpeg && !self.smart_failed.contains(&row.display_path) {
            smart::enqueue(&self.catalog, &row.display_path, false);
        }
    }
}
