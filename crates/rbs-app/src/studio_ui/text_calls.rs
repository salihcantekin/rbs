//! Text operations of the window, sent as control API commands (ADR-0163): the
//! font picker's `text.fonts` question, and the answers to the window's questions.

use rbs_api::{ApiError, ErrorKind, Response};
use rbs_scene::model::SourceId;

use super::{StudioUi, add_picker};

impl StudioUi {
    /// Opens the font picker for a text source; the families are asked for.
    pub fn open_font_picker(&mut self, id: i32) {
        let Some(settings) = self.text_settings.get(&id) else {
            return;
        };
        let current = match settings.get(rbs_sources::types::keys::FONT) {
            Some(rbs_scene::model::SettingValue::Text(f)) => f.clone(),
            _ => String::new(),
        };
        self.add_picker = Some(add_picker::AddPicker::fonts(
            SourceId::from_raw(id as u32),
            &current,
        ));
        self.font.0 = current;
        self.picker_dirty = true;
        self.font.1 = Some(self.ask_quietly(rbs_api::Command::TextFonts));
    }

    /// The answer to a question the window asked by ticket (`Update::ApiAnswer`);
    /// one no longer asked is dropped.
    pub(super) fn api_answer(&mut self, ticket: u64, result: Result<Response, ApiError>) {
        if self.font.1 != Some(ticket) {
            return;
        }
        self.font.1 = None;
        let families = match result {
            Ok(Response::TextFonts(fonts)) => fonts.families,
            // Being read: the list comes as `Update::TextFonts` once it is.
            Err(e) if e.kind == ErrorKind::NotReady => return,
            // No text renderer: the picker shows only "System font".
            _ => Vec::new(),
        };
        if let Some(p) = &mut self.add_picker
            && p.answer_fonts(&families, &self.font.0)
        {
            self.picker_dirty = true;
        }
    }
}
