//! The Mixer's master strip (ADR-0081) and its track strips (ADR-0170 decision 9):
//! the main canvas's buses other than `main`, each with a meter, a fader and a mute,
//! at the Advanced level. A track's name, fader and mute go through the control API's
//! `bus.*` methods and its sends through `source.set_sends` (protocol 1.115).

use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use rbs_api::request::{AddBus, BusCommand, BusName, SendLevel, SetBus, SetSourceSends};
use rbs_app::record_profile::RecordProfile;
use rbs_app::studio_host::{Command, OutputCanvas, TrackView};
use rbs_audio::LevelMeter;
use rbs_ui::fader;
use rbs_ui::meter::{self, Meter};
use slint::{Model, ModelRc, SharedString, VecModel};

use super::{MeterSnap, StudioUi, due_readout, meter_snap_row, meter_view, write_meter_row};
use crate::ui::{MeterRow, Studio};

/// Most tracks: six buses with `main` (ADR-0170 decision 1).
pub const MAX_TRACKS: usize = 5;

/// The control API's name of the master mix as a bus.
const MAIN: &str = "main";

/// The value of a meter that has heard nothing yet.
const SILENT: &str = "silent";

/// The master and track strips' state.
#[derive(Default)]
pub struct Strips {
    /// The mixer's master meter and its ballistics; `None` while there is no mixer.
    master: Option<(Arc<LevelMeter>, Meter)>,
    /// Snapped bar last written on the master strip (see `MeterSlot::shown`).
    master_shown: Option<MeterSnap>,
    /// Readout last written on the master strip (see `MeterSlot::readout`).
    master_readout: Option<meter::AccessibleLevel>,
    /// The master strip's one row (empty while the studio has no mixer).
    master_meter: Rc<VecModel<MeterRow>>,
    pub(super) master_audio: rbs_scene::model::MasterAudio,
    tracks: Vec<TrackSlot>,
    /// One row per track, `id` its index.
    track_meters: Rc<VecModel<MeterRow>>,
    /// The tracks' accessible values, apart from the rows (see `meter_values`).
    track_values: Rc<VecModel<SharedString>>,
}

struct TrackSlot {
    view: TrackView,
    ballistics: Meter,
    shown: Option<MeterSnap>,
    readout: Option<meter::AccessibleLevel>,
}

/// The key of track `i`'s level bar for the native meter layer (ADR-0035): -10, -11
/// and down, apart from the master (-1) and its loudness bar (-2).
pub fn track_key(i: usize) -> i32 {
    -10 - i as i32
}

impl Strips {
    /// Gives a window's `Studio` global the strips' models.
    pub(super) fn attach(&self, g: &Studio<'_>) {
        g.set_master_meter(ModelRc::from(self.master_meter.clone()));
        g.set_track_meters(ModelRc::from(self.track_meters.clone()));
        g.set_track_meter_values(ModelRc::from(self.track_values.clone()));
    }

    /// The studio has a mixer with a meter to refresh.
    pub(super) fn metered(&self) -> bool {
        self.master.is_some()
    }

    /// No master and no tracks (a collection switch; the host sends them again).
    pub(super) fn clear(&mut self) {
        self.master = None;
        self.master_meter.set_vec(Vec::new());
        self.master_shown = None;
        self.master_readout = None;
        self.tracks.clear();
        self.track_meters.set_vec(Vec::new());
        self.track_values.set_vec(Vec::new());
    }

    /// The tracks' names, in order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.tracks.iter().map(|t| t.view.name.as_str())
    }

    /// Track `i`'s name.
    pub fn name(&self, i: usize) -> Option<&str> {
        self.tracks.get(i).map(|t| t.view.name.as_str())
    }

    /// Why `name` cannot name a track (other than track `except`), as the name prompt
    /// shows it; `None` when it can.
    pub fn name_problem(&self, name: &str, except: Option<usize>) -> Option<&'static str> {
        let taken = self
            .tracks
            .iter()
            .enumerate()
            .any(|(i, t)| Some(i) != except && t.view.name == name);
        if name.is_empty() {
            Some("Give it a name.")
        } else if name == MAIN || name.eq_ignore_ascii_case("master") {
            Some("Master is the main mix: choose another name.")
        } else if taken {
            Some("A track has this name.")
        } else {
            None
        }
    }

    /// A name for a new track that no track has: "Track 2" and up (Master is 1).
    pub fn next_name(&self) -> String {
        (2..)
            .map(|n| format!("Track {n}"))
            .find(|n| self.names().all(|t| t != n))
            .unwrap_or_default()
    }
}

impl StudioUi {
    /// The master strip's row: its name, fader and mute from the model, its meter
    /// parts from the last refresh (ADR-0081). One row while the studio has a mixer.
    pub(super) fn show_master(&mut self) {
        let s = &mut self.strips;
        self.meter_values.show_master(s.master.is_some());
        if s.master.is_none() {
            s.master_meter.set_vec(Vec::new());
            s.master_shown = None;
            s.master_readout = None;
            return;
        }
        let m = s.master_audio;
        let mut row = s.master_meter.row_data(0).unwrap_or_default();
        row.muted = m.muted;
        row.volume = fader::position_of(m.volume);
        row.volume_text = fader::volume_text(m.volume).into();
        row.icon = "speaker".into();
        row.live = true;
        s.master_shown = Some(meter_snap_row(&row));
        if s.master_meter.row_count() == 0 {
            s.master_meter.set_vec(vec![row]);
        } else {
            s.master_meter.set_row_data(0, row);
        }
    }

    /// The master meter, made again or gone (`Update::MasterMeter`).
    pub(super) fn set_master_meter(&mut self, level: Option<Arc<LevelMeter>>) {
        self.strips.master = level.map(|l| (l, Meter::new()));
        self.show_master();
    }

    /// The master volume from the fader position (0 to 1, the fader law of the strips).
    pub fn set_master_volume(&mut self, position: f32) {
        self.send(Command::SetMasterVolume {
            volume: fader::volume_at(position),
        });
    }

    /// Mutes or unmutes the master.
    pub fn set_master_muted(&mut self, muted: bool) {
        self.send(Command::SetMasterMuted { muted });
    }

    /// Clears the master's clip latch.
    pub fn reset_master_clip(&mut self) {
        if let Some((_, ballistics)) = &mut self.strips.master {
            ballistics.reset_clip();
        }
    }

    /// The tracks the host sent (`Update::Tracks`): a track whose meter stays keeps
    /// its ballistics, so a fader move does not reset its peak.
    pub(super) fn set_tracks(&mut self, tracks: Vec<TrackView>) {
        let s = &mut self.strips;
        let mut old = std::mem::take(&mut s.tracks);
        s.tracks = tracks
            .into_iter()
            .map(|view| {
                let kept = old.iter().position(|o| match (&o.view.meter, &view.meter) {
                    (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                    _ => false,
                });
                let (ballistics, readout) = kept.map_or((Meter::new(), None), |i| {
                    let o = old.swap_remove(i);
                    (o.ballistics, o.readout)
                });
                TrackSlot {
                    view,
                    ballistics,
                    shown: None,
                    readout,
                }
            })
            .collect();
        let rows: Vec<MeterRow> = s
            .tracks
            .iter_mut()
            .enumerate()
            .map(|(i, t)| {
                let mut row = s.track_meters.row_data(i).unwrap_or_default();
                row.id = i as i32;
                row.name = t.view.name.as_str().into();
                row.muted = t.view.muted;
                row.volume = fader::position_of(t.view.volume);
                row.volume_text = fader::volume_text(t.view.volume).into();
                row.icon = "speaker".into();
                row.live = t.view.meter.is_some();
                t.shown = Some(meter_snap_row(&row));
                row
            })
            .collect();
        let values: Vec<SharedString> = s
            .tracks
            .iter()
            .enumerate()
            .map(|(i, _)| s.track_values.row_data(i).unwrap_or_else(|| SILENT.into()))
            .collect();
        super::set_rows(&s.track_meters, rows);
        super::set_rows(&s.track_values, values);
    }

    /// Track `i`'s fader from its position (0 to 1, the strips' fader law).
    pub fn set_track_volume(&mut self, i: i32, position: f32) {
        self.set_track(i, |set| set.volume = Some(fader::volume_at(position)));
    }

    /// Mutes or unmutes track `i`.
    pub fn set_track_muted(&mut self, i: i32, muted: bool) {
        self.set_track(i, |set| set.muted = Some(muted));
    }

    /// Clears track `i`'s clip latch.
    pub fn reset_track_clip(&mut self, i: i32) {
        if let Some(t) = usize::try_from(i)
            .ok()
            .and_then(|i| self.strips.tracks.get_mut(i))
        {
            t.ballistics.reset_clip();
        }
    }

    /// Adds a track called `name` (`bus.add`).
    pub fn add_track(&self, name: &str) {
        self.call(rbs_api::Command::Bus(BusCommand::Add(AddBus {
            name: name.to_owned(),
        })));
    }

    /// Renames track `i` (`bus.set` with `rename`).
    pub fn rename_track(&mut self, i: usize, name: &str) {
        let to = name.to_owned();
        self.set_track(i as i32, move |set| set.rename = Some(to));
    }

    /// Removes track `i` (`bus.remove`); a used track is refused, and the refusal
    /// names its uses.
    pub fn remove_track(&self, i: usize) {
        if let Some(name) = self.strips.name(i) {
            self.call(rbs_api::Command::Bus(BusCommand::Remove(BusName {
                name: name.to_owned(),
            })));
        }
    }

    fn set_track(&mut self, i: i32, edit: impl FnOnce(&mut SetBus)) {
        let Some(name) = usize::try_from(i).ok().and_then(|i| self.strips.name(i)) else {
            return;
        };
        let mut set = SetBus {
            name: name.to_owned(),
            rename: None,
            volume: None,
            muted: None,
        };
        edit(&mut set);
        self.call(rbs_api::Command::Bus(BusCommand::Set(set)));
    }

    /// The tracks, for the name prompt and the menus.
    pub fn tracks(&self) -> &Strips {
        &self.strips
    }

    /// Send to tracks (`source.sends`): Master and each track, checked where the source
    /// sends to it. Empty while the canvas has no track.
    pub fn send_choices(&self, source: i32) -> Vec<(String, String, bool)> {
        if self.strips.tracks.is_empty() {
            return Vec::new();
        }
        let Some(row) = self.rows.iter().find(|r| r.id.get() as i32 == source) else {
            return Vec::new();
        };
        std::iter::once(("Master".to_owned(), MAIN))
            .chain(self.strips.names().map(|n| (n.to_owned(), n)))
            .map(|(title, bus)| {
                let on = send_level(&row.sends, bus) > 0.0;
                (title, bus.to_owned(), on)
            })
            .collect()
    }

    /// Turns the source's send to `bus` on (level 1) or off (0).
    pub fn toggle_send(&self, source: i32, bus: &str) {
        let Some(row) = self.rows.iter().find(|r| r.id.get() as i32 == source) else {
            return;
        };
        let on = send_level(&row.sends, bus) > 0.0;
        self.call(rbs_api::Command::Bus(BusCommand::SetSends(
            SetSourceSends {
                source: row.id.get(),
                sends: vec![SendLevel {
                    bus: bus.to_owned(),
                    level: if on { 0.0 } else { 1.0 },
                }],
            },
        )));
    }

    /// One refresh of the master and track meters (see `tick_meters`).
    pub(super) fn tick_strips(&mut self, now: Duration, report: bool) {
        let s = &mut self.strips;
        if let Some((level, ballistics)) = &mut s.master {
            let view = meter_view(level, ballistics, now);
            if let Some(text) = due_readout(report, ballistics, &mut s.master_readout) {
                self.meter_values.set_master(&text);
            }
            let native = crate::meter_bridge::set_view(-1, &view);
            write_meter_row(&*s.master_meter, 0, &mut s.master_shown, &view, native);
        }
        for (i, t) in s.tracks.iter_mut().enumerate() {
            let Some(level) = &t.view.meter else {
                continue;
            };
            let view = meter_view(level, &mut t.ballistics, now);
            if let Some(text) = due_readout(report, &mut t.ballistics, &mut t.readout)
                && s.track_values
                    .row_data(i)
                    .is_some_and(|v| v != text.as_str())
            {
                s.track_values.set_row_data(i, text.into());
            }
            let native = crate::meter_bridge::set_view(track_key(i), &view);
            write_meter_row(&*s.track_meters, i, &mut t.shown, &view, native);
        }
    }
}

impl StudioUi {
    /// The Recording section's audio rows (sample width, Audio tracks) and the Stream
    /// section's Audio choice, for the canvas the Settings show.
    pub(super) fn push_audio_rows(&self, g: &Studio<'_>, p: &RecordProfile) {
        g.set_out_bit_depth_default(
            p.format
                .widths()
                .iter()
                .position(|b| *b == RecordProfile::default().bit_depth)
                .unwrap_or(0) as i32,
        );
        g.set_out_pcm_widths(super::strings(
            p.format.width_labels().iter().map(|s| (*s).to_owned()),
        ));
        let mixes = self.settings_mixes();
        let titles = || mixes.iter().map(|(title, _)| (*title).to_owned());
        let on: Vec<bool> = mixes
            .iter()
            .map(|(_, bus)| p.audio_tracks.iter().any(|t| t == bus))
            .collect();
        g.set_out_tracks(super::strings(titles()));
        g.set_out_tracks_on(ModelRc::new(VecModel::from(on)));
        g.set_out_one_track(p.format.is_audio_only());
        let bus = &self.stream_form().audio_bus;
        g.set_st_audio_buses(super::strings(titles()));
        g.set_st_audio_bus(
            mixes
                .iter()
                .position(|(_, b)| b == bus)
                .map_or(-1, |i| i as i32),
        );
    }

    /// Master and each track as the Settings list them (title, bus name); none while
    /// the main canvas has no track or the Settings show the vertical canvas.
    fn settings_mixes(&self) -> Vec<(&str, &str)> {
        if self.settings_canvas != OutputCanvas::Main || self.strips.tracks.is_empty() {
            return Vec::new();
        }
        std::iter::once(("Master", MAIN))
            .chain(self.strips.names().map(|n| (n, n)))
            .collect()
    }

    /// Audio tracks: the recording writes mix `i` of [`Self::settings_mixes`] or not.
    /// The last track stays; in a format with one track, choosing one replaces it.
    pub fn set_audio_track(&mut self, i: i32, on: bool) {
        let Some(bus) = usize::try_from(i)
            .ok()
            .and_then(|i| self.settings_mixes().get(i).map(|(_, b)| (*b).to_owned()))
        else {
            return;
        };
        let order: Vec<String> = self
            .settings_mixes()
            .iter()
            .map(|(_, b)| (*b).to_owned())
            .collect();
        let p = self.record_form_mut();
        let mut tracks = std::mem::take(&mut p.audio_tracks);
        match on {
            true if p.format.is_audio_only() => tracks = vec![bus],
            true if !tracks.contains(&bus) => tracks.push(bus),
            false if tracks.len() > 1 => tracks.retain(|t| *t != bus),
            _ => {}
        }
        // Canvas order first, then names the canvas does not have (kept as they were).
        let rank = |t: &String| order.iter().position(|o| o == t).unwrap_or(order.len());
        tracks.sort_by_key(rank);
        p.audio_tracks = tracks;
        self.save_profile_form();
    }

    /// The stream's Audio: mix `i` of [`Self::settings_mixes`].
    pub fn set_stream_audio(&mut self, i: i32) {
        let Some(bus) = usize::try_from(i)
            .ok()
            .and_then(|i| self.settings_mixes().get(i).map(|(_, b)| (*b).to_owned()))
        else {
            return;
        };
        self.stream_form_mut().audio_bus = bus;
        self.save_stream_form();
    }
}

/// A source's send level to `bus` from its row's sends (only the levels that differ
/// from the default: 1 to `main`, 0 to any other bus).
fn send_level(sends: &[(String, f32)], bus: &str) -> f32 {
    sends
        .iter()
        .find(|(b, _)| b == bus)
        .map_or(if bus == MAIN { 1.0 } else { 0.0 }, |(_, l)| *l)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_send_not_listed_has_the_default_level() {
        let sends = vec![("main".to_owned(), 0.0), ("Music".to_owned(), 0.5)];
        assert_eq!(send_level(&sends, "main"), 0.0);
        assert_eq!(send_level(&sends, "Music"), 0.5);
        assert_eq!(send_level(&[], "main"), 1.0);
        assert_eq!(send_level(&[], "Music"), 0.0);
    }

    #[test]
    fn a_track_name_is_new_and_not_the_master() {
        let s = Strips::default();
        assert_eq!(s.name_problem("", None), Some("Give it a name."));
        assert!(s.name_problem("main", None).is_some());
        assert!(s.name_problem("Master", None).is_some());
        assert_eq!(s.name_problem("Music", None), None);
        assert_eq!(s.next_name(), "Track 2");
        assert_eq!(track_key(0), -10);
        assert_eq!(track_key(4), -14);
    }
}
