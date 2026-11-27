//! The searchable picker's operations (Add source, Add existing source, fonts; design
//! system, "Searchable picker"): opening and closing it, the filter, a pick, "Look
//! again", the pictures of display and window rows (ADR-0077) and what it shows.

use rbs_app::studio_host::{Command, SourceKind};

use super::{StudioUi, THUMB_REQUEST, THUMBS_PER_LIST, add_picker};

impl StudioUi {
    /// Opens the Add source picker for a source type (its devices are listed by the
    /// studio and fill the rows when they arrive) or for the sources of other scenes.
    /// A picker that is open is replaced.
    pub fn open_add_picker(&mut self, what: crate::menu_bar::AddWhat) {
        use crate::menu_bar::AddWhat;
        let picker = match what {
            AddWhat::Kind(code) => {
                let Some(kind) = self.kind_list.get(code as usize).copied() else {
                    return;
                };
                self.list_kind_targets(kind);
                add_picker::AddPicker::for_kind(kind)
            }
            AddWhat::Existing => add_picker::AddPicker::existing(&self.existing),
        };
        self.add_picker = Some(picker);
        self.picker_dirty = true;
    }

    /// The picker was closed or cancelled: a late answer fills nothing.
    pub fn close_add_picker(&mut self) {
        let pictured = self
            .add_picker
            .as_ref()
            .is_some_and(|p| matches!(p.kind(), Some(SourceKind::Screen | SourceKind::Window)));
        self.add_picker = None;
        self.picker_dirty = false;
        // The grabs end with the list (ADR-0077), unless the Source tab still shows one.
        if pictured && self.source_targets_for.is_none() {
            self.send(Command::StopThumbnails);
        }
    }

    /// "Look again" after an empty answer: the kind's targets are listed again.
    pub fn retry_add_picker(&mut self) {
        if let Some(kind) = self.add_picker.as_mut().and_then(|p| p.look_again()) {
            self.picker_dirty = true;
            self.list_kind_targets(kind);
        }
    }

    /// Asks for the pictures of the picker's display or window rows that have none yet,
    /// a few at a time, while it is open (ADR-0077: the grabs are the host's, off the
    /// control thread, and end when the list closes).
    pub(super) fn ask_picker_pictures(&mut self) {
        let Some(p) = &self.add_picker else { return };
        let Some(kind) = p
            .kind()
            .filter(|k| matches!(k, SourceKind::Screen | SourceKind::Window))
        else {
            return;
        };
        let asked: Vec<(String, String)> = p
            .rows()
            .into_iter()
            .filter_map(|r| r.picture)
            .filter(|key| !self.source_pictures.contains_key(key))
            .take(THUMBS_PER_LIST)
            .collect();
        if !asked.is_empty() {
            self.send(Command::GrabThumbnails {
                kind,
                rows: asked,
                max: (THUMB_REQUEST.0, THUMB_REQUEST.1),
            });
        }
    }

    /// "Capture audio too" in the Add source dialog was checked or unchecked.
    pub fn set_picker_audio(&mut self, on: bool) {
        if let Some(p) = &mut self.add_picker {
            p.set_audio(on);
            self.picker_dirty = true;
        }
    }

    pub fn filter_add_picker(&mut self, text: &str) {
        if let Some(p) = &mut self.add_picker {
            p.set_query(text);
            self.picker_dirty = true;
            self.ask_picker_pictures();
        }
    }

    /// A row was chosen (`index` is of the picker's listing, see [`add_picker::Row`]).
    /// Returns whether something was added, so the caller closes the dialog.
    pub fn choose_add_picker(&mut self, index: usize) -> bool {
        let pick = self
            .add_picker
            .as_ref()
            .and_then(|p| p.pick(index))
            .cloned();
        match pick {
            Some(add_picker::Pick::Target(kind, target)) => {
                let audio = self.add_picker.as_ref().is_some_and(|p| p.audio());
                self.adding = true;
                self.deleted.clear();
                // A camera without a microphone of its own adds just the camera.
                let with_sound =
                    audio && !target.picker && (kind != SourceKind::Camera || target.mic.is_some());
                self.add_target(kind, target, with_sound, false);
            }
            Some(add_picker::Pick::Existing(id)) => self.add_existing_source(id.get() as i32),
            Some(add_picker::Pick::Font(id, family)) => {
                self.set_text_value(id.get() as i32, rbs_sources::types::keys::FONT, &family);
            }
            None => return false,
        }
        self.add_picker = None;
        true
    }

    /// What the dialog shows now, once after each change.
    pub fn take_picker_view(&mut self) -> Option<PickerView> {
        if !std::mem::take(&mut self.picker_dirty) {
            return None;
        }
        let p = self.add_picker.as_ref()?;
        let what = p
            .kind()
            .map_or("existing source".to_owned(), |k| k.label().to_lowercase());
        let rows = p.rows();
        let pictures = rows
            .iter()
            .map(|r| {
                r.picture
                    .as_ref()
                    .and_then(|key| self.source_pictures.get(key).cloned())
            })
            .collect();
        let (icon, hint) = p.header();
        Some(PickerView {
            title: if p.is_fonts() {
                "Choose a font".to_owned()
            } else {
                format!("Add {what}")
            },
            icon,
            hint,
            fonts: p.is_fonts(),
            status: p.status(),
            can_retry: p.kind().is_some(),
            rows,
            pictures,
            offers_audio: p.offers_audio(),
            offers_mic: p.offers_microphone(),
            audio: p.audio(),
        })
    }
}

/// The Add source dialog's content ([`StudioUi::take_picker_view`]).
pub struct PickerView {
    pub title: String,
    /// The picker lists font families (a text source's Font).
    pub fonts: bool,
    pub status: add_picker::Status,
    pub rows: Vec<add_picker::Row>,
    /// Each row's picture, once it came (displays and windows, ADR-0077).
    pub pictures: Vec<Option<slint::Image>>,
    /// The header's icon name and the line that says what to pick.
    pub icon: &'static str,
    pub hint: &'static str,
    /// An empty answer can be asked again ("Look again").
    pub can_retry: bool,
    /// Show "Capture audio too" (displays and windows) and whether it is checked.
    pub offers_audio: bool,
    /// The dialog offers "Add its microphone too" (a camera with a microphone of its own).
    pub offers_mic: bool,
    pub audio: bool,
}
