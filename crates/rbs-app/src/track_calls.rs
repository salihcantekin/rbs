//! The Mixer's master and track strips' callbacks (ADR-0081, ADR-0170 decision 9), what
//! the track entries run, and the name prompt of Add track and Rename track.

use slint::ComponentHandle as _;

use super::{App, Ask, Ctx};
use crate::menu_bar::TrackOp;
use crate::ui::{Overlays, Studio};

/// The strips' callbacks, on one window's global.
pub fn install(g: &Studio<'_>, ctx: &Ctx) {
    macro_rules! on {
        ($name:ident, |$app:ident $(, $arg:ident : $ty:ty)*| $body:block) => {{
            let ctx = ctx.clone();
            g.$name(move |$($arg: $ty),*| ctx.update(move |$app: &mut App| $body));
        }};
    }
    on!(on_set_master_volume, |app, position: f32| {
        app.studio.set_master_volume(position);
    });
    on!(on_set_master_muted, |app, muted: bool| {
        app.studio.set_master_muted(muted);
    });
    on!(on_reset_master_clip, |app| {
        app.studio.reset_master_clip();
    });
    on!(on_set_track_volume, |app, i: i32, position: f32| {
        app.studio.set_track_volume(i, position);
    });
    on!(on_set_track_muted, |app, i: i32, muted: bool| {
        app.studio.set_track_muted(i, muted);
    });
    on!(on_reset_track_clip, |app, i: i32| {
        app.studio.reset_track_clip(i);
    });
    on!(on_add_track, |app| {
        app.run_track(TrackOp::Add(None), true);
    });
    on!(on_set_audio_track, |app, i: i32, on: bool| {
        app.studio.set_audio_track(i, on);
    });
    on!(on_set_stream_audio, |app, i: i32| {
        app.studio.set_stream_audio(i);
    });
}

impl App {
    /// Runs a track entry. Asking for a name needs someone at the window, so a control
    /// surface or a global shortcut (`confirm` false) does not ask.
    pub(crate) fn run_track(&mut self, op: TrackOp, confirm: bool) {
        match op {
            TrackOp::Add(None) | TrackOp::Rename(_, None) if !confirm => {}
            TrackOp::Add(None) => self.ask_track_name(None),
            TrackOp::Rename(i, None) => self.ask_track_name(Some(i)),
            TrackOp::Add(Some(name)) => self.answer(None, &name),
            TrackOp::Rename(i, Some(name)) => self.answer(Some(i), &name),
            TrackOp::Remove(i) => self.studio.remove_track(i),
            TrackOp::Send(source, bus) => self.studio.toggle_send(source, &bus),
        }
    }

    fn answer(&mut self, track: Option<usize>, name: &str) {
        if let Err(why) = self.track_named(track, name) {
            self.show_error(crate::UiError::Other(why));
        }
    }

    /// Opens the name prompt for a new track (`None`, a free "Track N" filled in) or
    /// to rename track `i`.
    fn ask_track_name(&mut self, track: Option<usize>) {
        let tracks = self.studio.tracks();
        let (title, confirm, value) = match track {
            Some(i) => match tracks.name(i) {
                Some(name) => ("Rename track", "Rename", name.to_owned()),
                None => return,
            },
            None => ("Add track", "Add", tracks.next_name()),
        };
        let o = self.main.global::<Overlays>();
        o.set_name_error("".into());
        o.set_name_value(value.into());
        o.set_name_title(title.into());
        o.set_name_confirm(confirm.into());
        o.set_name_ask(true);
        o.set_name_open(true);
        self.name_for = Some(Ask::Track(track));
    }

    /// The name prompt's answer for a new track (`None`) or track `i`: a name the
    /// canvas cannot take is refused in the prompt.
    pub(crate) fn track_named(&mut self, track: Option<usize>, name: &str) -> Result<(), String> {
        let name = name.trim();
        if let Some(why) = self.studio.tracks().name_problem(name, track) {
            return Err(why.to_owned());
        }
        match track {
            Some(i) => self.studio.rename_track(i, name),
            None => self.studio.add_track(name),
        }
        Ok(())
    }
}
