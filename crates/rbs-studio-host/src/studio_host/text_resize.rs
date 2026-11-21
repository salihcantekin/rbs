//! Crisp text resizing (ADR-0129 decision 12): a box edit of a text item on the main
//! canvas becomes a box change, so the text is drawn again at the size it is shown.
//! During a drag the item is stretched; the box changes once the drag rests for
//! [`CRISP_SETTLE`], in the drag's own undo step.

use std::time::Duration;

use rbs_core::time::Timestamp;
use rbs_scene::model::{ItemContent, ItemId, SourceId};
use rbs_scene::{SceneError, Undo};
use rbs_sources::types::{SourceClass, class_of};

use super::{EditKind, Host, Refusal, Update};

/// How long a drag rests before its text item gets its new box.
pub(super) const CRISP_SETTLE: Duration = Duration::from_millis(250);

/// How often the font picker asks again while the families are being read.
pub(super) const FONTS_POLL: Duration = Duration::from_millis(100);

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
                self.edits
                    .history
                    .extend_last(EditKind::Place, name, vec![undo]);
            }
            return;
        }
        self.texts.crisp_pending = Some(CrispPending {
            item,
            gesture,
            due: self.clock.now() + CRISP_SETTLE,
        });
    }

    /// Gives a rested drag's text its new box. How long until the next check, while
    /// one waits.
    pub(super) fn drive_crisp(&mut self, notify: &dyn Fn(Update)) -> Option<Duration> {
        let pending = self.texts.crisp_pending?;
        let now = self.clock.now();
        if now < pending.due {
            return Some(pending.due.saturating_since(now));
        }
        self.texts.crisp_pending = None;
        let name = self
            .collection
            .item(pending.item)
            .and_then(|(_, it)| match it.content {
                ItemContent::Source(id) => Some(self.name_of(id)),
                _ => None,
            })
            .unwrap_or_default();
        if let Ok(Some(undo)) = self.crisp_resize(pending.item) {
            self.edits
                .history
                .record(EditKind::Place, &name, pending.gesture, vec![undo]);
            self.apply(notify);
        }
        None
    }

    /// Fit box to text (ADR-0129 decision 13): the box becomes the text's natural size
    /// (wrapped at the box's width when wrapping is on), the item keeps its centre and
    /// its scale, all in one undo step. A live kind is measured with its pattern,
    /// every letter drawn as a wide figure, so the box holds every value.
    pub(super) fn fit_text_box(&mut self, id: SourceId) -> Result<bool, Refusal> {
        use rbs_scene::model::SettingValue;
        use rbs_sources::text::TextKind;
        use rbs_sources::types::keys;
        let source = self
            .collection
            .source(id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no source {}.", id.get())))?;
        if class_of(&source.type_id) != Some(SourceClass::Text) {
            return Err(Refusal::Invalid(format!(
                "{} is not a text source.",
                source.name
            )));
        }
        let name = source.name.clone();
        let (parsed, _) = rbs_sources::text::read_text_lenient(&source.settings);
        let mut block = parsed.block.clone();
        if parsed.kind != TextKind::Text {
            block.text = fit_sample(&parsed);
        }
        let Some(renderer) =
            rbs_sources::text::pick_renderer(&self.adapters.sources.text_renderers(), &block)
        else {
            return Err(Refusal::Failed("No text renderer is installed.".into()));
        };
        let wrap_at = block.wrap.then_some(block.box_size[0]);
        let Some([w, h]) = super::measure_text_at(renderer, block, wrap_at) else {
            return Err(Refusal::Failed("The text could not be measured.".into()));
        };
        let [ow, oh] = parsed.block.box_size;
        if [w, h] == [ow, oh] {
            return Ok(false);
        }
        let mut settings = source.settings.clone();
        settings.insert(keys::WIDTH.into(), SettingValue::Int(i64::from(w)));
        settings.insert(keys::HEIGHT.into(), SettingValue::Int(i64::from(h)));
        rbs_sources::validate_settings(&source.type_id, &settings)
            .map_err(|e| Refusal::Invalid(e.to_string()))?;
        // Every item that places it keeps its centre and its scale.
        let mut undo = Vec::new();
        let items: Vec<ItemId> = self
            .collection
            .placements_of(id)
            .iter()
            .map(|(_, item)| *item)
            .collect();
        let mut first = true;
        for item in items {
            let Some((_, it)) = self.collection.item(item) else {
                continue;
            };
            let mut layout = it.layout;
            let sx = layout.size[0] / ow.max(1) as f32;
            let sy = layout.size[1] / oh.max(1) as f32;
            let centre = [
                layout.position[0] + layout.size[0] * 0.5,
                layout.position[1] + layout.size[1] * 0.5,
            ];
            layout.size = [w as f32 * sx, h as f32 * sy];
            layout.position = [
                centre[0] - layout.size[0] * 0.5,
                centre[1] - layout.size[1] * 0.5,
            ];
            let step = if first {
                self.collection.resize_text(item, settings.clone(), layout)
            } else {
                self.collection.set_item_layout(item, None, layout)
            };
            undo.push(step.map_err(|e| Refusal::of(&e))?);
            first = false;
        }
        if first {
            let step = self
                .collection
                .set_source_settings(id, settings)
                .map_err(|e| Refusal::of(&e))?;
            undo.push(step);
        }
        self.edits.history.record(EditKind::Place, &name, 0, undo);
        Ok(true)
    }

    /// Answers the font picker with the families, or asks again on the next service
    /// while they are being read.
    pub(super) fn list_text_fonts(&mut self, notify: &dyn Fn(Update)) {
        match self.studio.text_fonts(&self.adapters.sources) {
            rbs_record::TextFonts::Ready(families) => {
                self.texts.fonts_wanted = false;
                notify(Update::TextFonts(families.as_ref().clone()));
            }
            rbs_record::TextFonts::NotReady => self.texts.fonts_wanted = true,
            _ => {
                self.texts.fonts_wanted = false;
                notify(Update::TextFonts(Vec::new()));
            }
        }
    }
}

/// What Fit box to text measures for a live kind: its pattern (or the default one)
/// with every letter a wide figure, and the end text when that is longer.
fn fit_sample(parsed: &rbs_sources::text::TextSettings) -> String {
    use rbs_sources::text::TextKind;
    let pattern = parsed.format.clone().unwrap_or_else(|| {
        match parsed.kind {
            TextKind::Clock => "HH:mm:ss",
            _ => "mm:ss",
        }
        .to_owned()
    });
    let sample: String = pattern
        .chars()
        .map(|c| if "HmsdMy".contains(c) { '8' } else { c })
        .collect();
    match &parsed.end_text {
        Some(end) if end.chars().count() > sample.chars().count() => end.clone(),
        _ => sample,
    }
}
