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
    pub(super) fn api_answer(
        &mut self,
        ticket: u64,
        result: Result<Response, ApiError>,
        out: &mut super::Applied,
    ) {
        if ticket == super::add_calls::BROWSER_TICKET {
            return self.browser_answer(result, out);
        }
        if ticket & super::crop_calls::EDIT_TICKET != 0 {
            return self.crop_answer(ticket & !super::crop_calls::EDIT_TICKET, result, out);
        }
        if ticket == super::target_calls::TARGETS_TICKET {
            return self.targets_answer(result);
        }
        if ticket == super::add_calls::ADD_ONS_TICKET {
            if let Ok(Response::AddOnList(list)) = result {
                self.add_ons = list.add_ons;
            }
            return;
        }
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

impl StudioUi {
    /// Start (0), Pause (1) or Reset (2) of a countdown's or stopwatch's timer.
    pub fn text_timer(&mut self, id: i32, action: i32) {
        use rbs_api::request::TimerAction;
        let action = match action {
            0 => TimerAction::Start,
            1 => TimerAction::Pause,
            2 => TimerAction::Reset,
            _ => return,
        };
        let call = rbs_api::Command::TextTimer(rbs_api::request::TextTimer {
            source: id as u32,
            action,
        });
        // A source removed meanwhile is not worth an error, as before (ADR-0163).
        self.call_as(
            call,
            rbs_app::studio_host::ApiReply::QuietIf(rbs_api::ErrorKind::NotFound),
        );
    }
}

/// `source.add` for a text source showing `text` in the scene the window edits.
pub(super) fn add_text_call(text: &str) -> rbs_api::request::AddSource {
    rbs_api::request::AddSource {
        stamp: None,
        path: None,
        url: None,
        kind: rbs_api::types::SourceKind::Text,
        adapter: String::new(),
        target: String::new(),
        name: None,
        scene: None,
        global: false,
        with_audio: false,
        text: Some(text.to_owned()),
    }
}
