//! Measuring a new text source off the control thread.
//!
//! Adding the source uses the default box at once, so the control thread does not
//! wait on the renderer. The measure thread posts the size back. It is applied only
//! when the text and the box are still that default, and it is not its own undo step:
//! undoing the add still removes the source.

use rbs_scene::model::SourceId;
use rbs_scene::model::SettingValue;

use rbs_sources::types::keys;

use super::{Host, SourceKind};

/// Starts a measure of source `id` and posts [`super::Msg::TextMeasured`] when it
/// finishes. A missing renderer, a failed measure or a dropped host does nothing.
pub(super) fn begin(host: &Host, id: SourceId) {
    let Some(source) = host.collection.source(id) else {
        return;
    };
    let Some(SettingValue::Text(text)) = source.settings.get(keys::TEXT) else {
        return;
    };
    let text = text.clone();
    let parsed = rbs_sources::text::validate_text(&source.settings);
    let Ok(parsed) = parsed else {
        return;
    };
    let Some(renderer) =
        rbs_sources::text::pick_renderer(&host.adapters.sources.text_renderers(), &parsed.block)
    else {
        return;
    };
    let inbox = host.inbox.clone();
    let block = parsed.block;
    let _ = std::thread::Builder::new()
        .name("rbs-text-measure".into())
        .spawn(move || {
            let measured = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                renderer.measure(&block, None)
            }));
            let Some(size) = measured.ok().and_then(Result::ok) else {
                return;
            };
            if let Some(tx) = inbox.upgrade() {
                let _ = tx.send(super::Msg::TextMeasured { id, text, size });
            }
        });
}

/// Writes `size` onto `id` when it still shows `text` in the default box.
/// `true` when the collection changed.
pub(super) fn apply(host: &mut Host, id: SourceId, text: &str, size: [u32; 2]) -> bool {
    let Some(source) = host.collection.source(id) else {
        return false;
    };
    if SourceKind::of(&source.type_id) != Some(SourceKind::Text) {
        return false;
    }
    let Some(SettingValue::Text(current)) = source.settings.get(keys::TEXT) else {
        return false;
    };
    if current != text || !box_is_default(&source.settings) {
        return false;
    }
    let Some(canvas) = host.collection.canvas(host.canvas) else {
        return false;
    };
    let [w, h] = [
        size[0].clamp(1, canvas.size.width.max(1)),
        size[1].clamp(1, canvas.size.height.max(1)),
    ];
    let [dw, dh] = rbs_sources::text::DEFAULT_BOX;
    if [w, h] == [dw, dh] {
        return false;
    }
    let mut settings = source.settings.clone();
    settings.insert(keys::WIDTH.into(), SettingValue::Int(i64::from(w)));
    settings.insert(keys::HEIGHT.into(), SettingValue::Int(i64::from(h)));
    if rbs_sources::validate_settings(&source.type_id, &settings).is_err() {
        return false;
    }
    let placements = host.collection.placements_of(id);
    if host.collection.set_source_settings(id, settings).is_err() {
        return false;
    }
    let fitted = [w as f32, h as f32];
    for (_, item_id) in placements {
        let Some((_, item)) = host.collection.item(item_id) else {
            continue;
        };
        if !size_is_default(item.layout.size) {
            continue;
        }
        let mut layout = item.layout.clone();
        layout.position = [
            layout.position[0] + layout.size[0] * 0.5 - fitted[0] * 0.5,
            layout.position[1] + layout.size[1] * 0.5 - fitted[1] * 0.5,
        ];
        layout.size = fitted;
        let _ = host.collection.set_item_layout(item_id, None, layout);
    }
    true
}

fn box_is_default(settings: &rbs_scene::model::Settings) -> bool {
    let [dw, dh] = rbs_sources::text::DEFAULT_BOX;
    int_is(settings, keys::WIDTH, i64::from(dw)) && int_is(settings, keys::HEIGHT, i64::from(dh))
}

fn int_is(settings: &rbs_scene::model::Settings, key: &str, value: i64) -> bool {
    matches!(settings.get(key), Some(SettingValue::Int(n)) if *n == value)
}

fn size_is_default(size: [f32; 2]) -> bool {
    let [dw, dh] = rbs_sources::text::DEFAULT_BOX;
    size == [dw as f32, dh as f32]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_moved_default_box_keeps_its_centre() {
        let layout = rbs_scene::model::Layout::new([10.0, 20.0], [640.0, 160.0]);
        let fitted = [100.0, 40.0];
        let position = [
            layout.position[0] + layout.size[0] * 0.5 - fitted[0] * 0.5,
            layout.position[1] + layout.size[1] * 0.5 - fitted[1] * 0.5,
        ];
        assert_eq!(position, [280.0, 80.0]);
    }

    #[test]
    fn the_default_box_is_the_one_a_later_measure_may_replace() {
        let [dw, dh] = rbs_sources::text::DEFAULT_BOX;
        let mut settings = rbs_scene::model::Settings::new();
        settings.insert(keys::WIDTH.into(), SettingValue::Int(i64::from(dw)));
        settings.insert(keys::HEIGHT.into(), SettingValue::Int(i64::from(dh)));
        assert!(box_is_default(&settings));
        settings.insert(keys::WIDTH.into(), SettingValue::Int(100));
        assert!(!box_is_default(&settings));
        assert!(size_is_default([dw as f32, dh as f32]));
        assert!(!size_is_default([dw as f32 + 1.0, dh as f32]));
    }
}
