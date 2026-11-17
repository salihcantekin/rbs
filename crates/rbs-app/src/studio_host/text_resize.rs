//! Crisp text resizing (ADR-0129 decision 12): a box edit of a text item on the main
//! canvas becomes a box change, so the text is drawn again at the size it is shown.
//! During a drag the item is stretched; the box changes once the drag rests for
//! [`CRISP_SETTLE`], in the drag's own undo step.

use std::time::Duration;

use rbs_core::time::Timestamp;
use rbs_scene::model::{ItemContent, ItemId, SourceId};
use rbs_scene::{SceneError, Undo};
use rbs_sources::types::{SourceClass, class_of};

use super::{EditKind, Host, Update};

/// How long a drag rests before its text item gets its new box.
pub(super) const CRISP_SETTLE: Duration = Duration::from_millis(250);

/// A crisp resize waiting for its drag to rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CrispPending {
    item: ItemId,
    gesture: u64,
    due: Timestamp,
}

impl Host {
    /// The text source of `item` when a box edit of it may be crisp: a text, placed
    /// once in the whole collection, with no layout of its own on any canvas, and not
    /// written to a separate picture track of a running take. Otherwise it stretches.
    fn crisp_text(&self, item: ItemId) -> Option<SourceId> {
        let (_, it) = self.collection.item(item)?;
        let ItemContent::Source(id) = it.content else {
            return None;
        };
        let source = self.collection.source(id)?;
        let crisp = class_of(&source.type_id) == Some(SourceClass::Text)
            && it.overrides.is_empty()
            && self.collection.placements_of(id).len() == 1
            && !self.recorded_text_boxes().iter().any(|(s, ..)| *s == id);
        crisp.then_some(id)
    }

    /// Turns the item's stretch on the main canvas into a new box and scaled
    /// settings, one record. `None` when it is not a crisp text or nothing changes.
    pub(super) fn crisp_resize(&mut self, item: ItemId) -> Result<Option<Undo>, SceneError> {
        let Some(source) = self.crisp_text(item) else {
            return Ok(None);
        };
        let (Some((_, it)), Some(src)) =
            (self.collection.item(item), self.collection.source(source))
        else {
            return Ok(None);
        };
        let live = self.shown_layout(it);
        let Some(r) = rbs_sources::text_resize::resized(&src.settings, live.crop, live.size) else {
            return Ok(None);
        };
        let mut layout = live.with_crop(r.crop);
        layout.size = [
            r.size[0].copysign(live.size[0]),
            r.size[1].copysign(live.size[1]),
        ];
        let stored = self.collection.stored_layout(self.canvas, &layout);
        self.collection
            .resize_text(item, r.settings, stored)
            .map(Some)
    }

    /// After a box edit of `item`: a single edit (`gesture` 0) gets its new box in the
    /// same undo step now; a drag's waits until it rests.
    pub(super) fn crisp_after_box(&mut self, item: ItemId, gesture: u64, name: &str) {
        if self.crisp_text(item).is_none() {
            return;
        }
        if gesture == 0 {
            if let Ok(Some(undo)) = self.crisp_resize(item) {
                self.history.extend_last(EditKind::Place, name, vec![undo]);
            }
            return;
        }
        self.crisp_pending = Some(CrispPending {
            item,
            gesture,
            due: self.clock.now() + CRISP_SETTLE,
        });
    }

    /// Gives a rested drag's text its new box. How long until the next check, while
    /// one waits.
    pub(super) fn drive_crisp(&mut self, notify: &dyn Fn(Update)) -> Option<Duration> {
        let pending = self.crisp_pending?;
        let now = self.clock.now();
        if now < pending.due {
            return Some(pending.due.saturating_since(now));
        }
        self.crisp_pending = None;
        let name = self
            .collection
            .item(pending.item)
            .and_then(|(_, it)| match it.content {
                ItemContent::Source(id) => Some(self.name_of(id)),
                _ => None,
            })
            .unwrap_or_default();
        if let Ok(Some(undo)) = self.crisp_resize(pending.item) {
            self.history
                .record(EditKind::Place, &name, pending.gesture, vec![undo]);
            self.apply(notify);
        }
        None
    }
}
