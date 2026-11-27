//! Shows the searchable picker's content in a window's overlays (design system,
//! "Searchable picker"): the header, the status or what to show when nothing is
//! listed, the option and the rows with their pictures.

use slint::{ModelRc, VecModel};

use crate::studio_ui::PickerView;
use crate::studio_ui::add_picker::Status;
use crate::ui::{Overlays, PickerRow};

/// Fills the overlays from `view` and opens the picker.
pub fn show(o: &Overlays<'_>, view: PickerView) {
    let (status, empty_title, empty_text) = match view.status {
        Status::Looking if view.fonts => ("Reading the fonts...", "", ""),
        Status::Looking => ("Looking for devices...", "", ""),
        Status::Nothing => (
            "",
            "Nothing of this type was found",
            "Connect the device or open what you want to capture, then look again.",
        ),
        Status::NoMatch => ("", "Nothing matches", "Try another word."),
        Status::Ready => ("", "", ""),
    };
    let rows: Vec<PickerRow> = view
        .rows
        .into_iter()
        .zip(view.pictures)
        .map(|(r, picture)| PickerRow {
            name: r.name.into(),
            note: r.note.into(),
            index: r.index as i32,
            icon: r.icon.into(),
            has_picture: picture.is_some(),
            picture: picture.unwrap_or_default(),
            badge: r.badge.into(),
            badge_icon: r.badge_icon.into(),
            badge_accent: r.badge_accent,
            group: r.group.into(),
            groups_before: r.groups_before as i32,
            font: r.font.into(),
            action: r.action.into(),
        })
        .collect();
    let (label, hint) = if view.offers_audio {
        (
            "Capture audio too",
            "Adds the sound that goes with the picture as its own audio source: a display's system sound without RBS's own sound, a window's application sound. Capturing the system sound and another app's sound can play the same audio twice.",
        )
    } else if view.offers_mic {
        (
            "Add its microphone too",
            "A microphone that belongs with a camera was found; the rows that have one show 'Mic'. Adds it as its own audio source with the camera.",
        )
    } else {
        ("", "")
    };
    o.set_picker_title(view.title.into());
    o.set_picker_icon(view.icon.into());
    o.set_picker_hint(view.hint.into());
    o.set_picker_fonts(view.fonts);
    o.set_picker_looking(view.status == Status::Looking);
    o.set_picker_status(status.into());
    o.set_picker_empty_title(empty_title.into());
    o.set_picker_empty_text(empty_text.into());
    o.set_picker_can_retry(view.can_retry && view.status == Status::Nothing);
    o.set_picker_option_label(label.into());
    o.set_picker_option_hint(hint.into());
    o.set_picker_option(view.audio);
    o.set_picker_rows(ModelRc::new(VecModel::from(rows)));
    o.set_picker_open(true);
}
