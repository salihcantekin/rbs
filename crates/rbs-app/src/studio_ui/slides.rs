//! A slide show's rows on the Source tab of Properties (ADR-0171): its folder, the
//! playback buttons with "n / N", and its settings drawn from the type's schema.

use super::StudioUi;
use super::filters::{FilterUi, field_data, number_at};
use crate::ui::SourceData;
use rbs_app::studio_host::{MediaTick, SourceRow};
use rbs_params::{FieldKind, Value};
use rbs_scene::model::SettingValue;
use rbs_sources::slideshow::{self, FIELDS};

/// What the Folder row says under the path: how many pictures, or why there are none.
fn note(tick: Option<&MediaTick>) -> String {
    match tick {
        None => "The pictures are listed when the slide show is shown.".to_owned(),
        Some(t) if t.state == 5 => "The folder cannot be read.".to_owned(),
        Some(t) if t.state == 4 && t.count == 0 => {
            "The pictures are listed when the slide show is shown.".to_owned()
        }
        Some(t) if t.count == 0 => "No PNG or JPEG pictures in this folder.".to_owned(),
        Some(t) if t.count == 1 => "1 picture".to_owned(),
        Some(t) => format!("{} pictures", t.count),
    }
}

/// "3 / 24" for the picture shown; empty before the first.
fn position(tick: Option<&MediaTick>) -> String {
    match tick {
        Some(t) if t.index >= 0 && t.count > 0 => format!("{} / {}", t.index + 1, t.count),
        _ => String::new(),
    }
}

/// Writes what moves while a slide show plays into its row: the folder note, "n / N"
/// and Pause or Resume. True when something changed.
pub(super) fn patch_live(d: &mut SourceData, tick: Option<&MediaTick>) -> bool {
    let note = note(tick);
    let position = position(tick);
    let paused = tick.is_some_and(|t| t.state == 1);
    if d.slides_note.as_str() == note
        && d.slides_position.as_str() == position
        && d.slides_paused == paused
    {
        return false;
    }
    d.slides_note = note.into();
    d.slides_position = position.into();
    d.slides_paused = paused;
    true
}

/// `d` with the file rows of `r` filled in: a media file's path and Loop, or a slide
/// show's rows; other kinds are left as they are.
pub(super) fn fill(
    mut d: SourceData,
    r: &SourceRow,
    ticks: &[MediaTick],
    models: &mut FilterUi,
) -> SourceData {
    if let Some(m) = r.media.as_ref() {
        d.media = true;
        d.media_path = m.path.as_str().into();
        d.media_loop = m.looped;
    }
    let Some(settings) = r.slideshow.as_ref() else {
        return d;
    };
    let tick = ticks.iter().find(|t| t.id == r.id);
    d.slides = true;
    d.slides_folder = match settings.get(slideshow::FOLDER) {
        Some(SettingValue::Text(t)) => t.as_str().into(),
        _ => "".into(),
    };
    patch_live(&mut d, tick);
    let values = slideshow::values(settings);
    d.slides_fields = models.slides_rows(d.id, field_data(FIELDS, &values, r.id, &[]));
    d
}

/// The stored form of a schema value: an option key, a number or a switch.
fn stored(kind: &FieldKind, value: Value) -> Option<serde_json::Value> {
    Some(match (kind, value) {
        (FieldKind::Choice { options, .. }, Value::Choice(i)) => {
            serde_json::Value::String(options.get(usize::from(i))?.0.to_owned())
        }
        (FieldKind::Toggle { .. }, Value::Toggle(on)) => serde_json::Value::Bool(on),
        (FieldKind::Number { .. }, Value::Number(n)) => serde_json::json!(f64::from(n)),
        _ => return None,
    })
}

impl StudioUi {
    /// Previous (`direction` < 0) or Next picture: a live action, not an undo step.
    pub fn slides_step(&mut self, id: i32, direction: i32) {
        let on = rbs_api::request::MediaSource { source: id as u32 };
        self.call(if direction < 0 {
            rbs_api::Command::MediaPrevious(on)
        } else {
            rbs_api::Command::MediaNext(on)
        });
    }

    /// A choice picked or a switch flipped: one undo step.
    pub fn set_slides_option(&mut self, id: i32, field: i32, option: i32) {
        let Some(f) = FIELDS.get(field as usize) else {
            return;
        };
        let value = match f.kind {
            FieldKind::Choice { .. } => match u8::try_from(option) {
                Ok(i) => Value::Choice(i),
                Err(_) => return,
            },
            FieldKind::Toggle { .. } => Value::Toggle(option != 0),
            _ => return,
        };
        if let Some(v) = stored(&f.kind, value) {
            self.set_media_setting(id, f.key, v);
        }
    }

    /// A slider moved: the field shows the position; nothing is stored until release.
    pub fn preview_slides_number(&mut self, id: i32, field: i32, position: f32) {
        self.filter_ui.slides_drag = Some((id, field, position));
    }

    /// The slider was let go: its value is stored, one undo step.
    pub fn commit_slides_number(&mut self, id: i32, field: i32) {
        let Some((source, f, p)) = self.filter_ui.slides_drag.take() else {
            return;
        };
        if source != id || f != field {
            return;
        }
        let Some((key, v)) = FIELDS.get(field as usize).and_then(|f| number_at(f, p)) else {
            return;
        };
        self.set_media_setting(id, key, serde_json::json!(f64::from(v)));
    }

    /// The drag was cancelled: nothing changes.
    pub fn cancel_slides_number(&mut self, _id: i32) {
        self.filter_ui.slides_drag = None;
    }

    /// One field back to its schema default: one undo step.
    pub fn reset_slides_field(&mut self, id: i32, field: i32) {
        let Some(f) = FIELDS.get(field as usize) else {
            return;
        };
        if let Some(v) = stored(&f.kind, f.kind.default_value()) {
            self.set_media_setting(id, f.key, v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rbs_scene::model::SourceId;

    fn tick(state: u8, index: i32, count: u32) -> MediaTick {
        MediaTick {
            id: SourceId::from_raw(1),
            state,
            cursor_ms: 0,
            duration_ms: 0,
            index,
            count,
        }
    }

    #[test]
    fn the_folder_note_and_position_follow_the_state() {
        assert_eq!(note(Some(&tick(0, 2, 24))), "24 pictures");
        assert_eq!(position(Some(&tick(0, 2, 24))), "3 / 24");
        assert_eq!(note(Some(&tick(0, -1, 1))), "1 picture");
        assert_eq!(position(Some(&tick(0, -1, 1))), "");
        assert_eq!(note(Some(&tick(5, -1, 0))), "The folder cannot be read.");
        assert_eq!(
            note(Some(&tick(0, -1, 0))),
            "No PNG or JPEG pictures in this folder."
        );
        assert!(note(None).contains("listed when"));
    }

    #[test]
    fn a_new_tick_moves_the_picture_number_in_the_row() {
        let mut d = SourceData::default();
        assert!(patch_live(&mut d, Some(&tick(0, 2, 24))));
        assert_eq!(d.slides_position.as_str(), "3 / 24");
        assert!(!patch_live(&mut d, Some(&tick(0, 2, 24))));
        assert!(patch_live(&mut d, Some(&tick(1, 3, 24))));
        assert_eq!(d.slides_position.as_str(), "4 / 24");
        assert!(d.slides_paused);
    }

    #[test]
    fn every_schema_value_has_a_stored_form() {
        for f in FIELDS {
            assert!(
                stored(&f.kind, f.kind.default_value()).is_some(),
                "{}",
                f.key
            );
        }
    }
}
