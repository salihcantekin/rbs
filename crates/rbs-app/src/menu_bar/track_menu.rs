//! The tracks' entries (ADR-0170 decision 9): Add track, a track strip's Rename track
//! and Remove track (`track` is the track's place in the Mixer's Tracks group), and a
//! source's Send to tracks. They show at the Advanced level only (U16).

use rbs_ui::command::{CommandId, Disabled};

use super::{Run, Shown};
use crate::studio_ui::MAX_TRACKS;

/// What a track entry does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackOp {
    /// Ask for a new track's name, or add one called this.
    Add(Option<String>),
    /// Ask for track `i`'s new name, or rename it to this.
    Rename(usize, Option<String>),
    /// Remove track `i`.
    Remove(usize),
    /// Turn the source's send to the bus (`main` or a track's name) on or off.
    Send(i32, String),
}

/// Whether `base` is a track entry.
pub(super) fn is_track(base: &str) -> bool {
    matches!(
        base,
        "mixer.track.add" | "mixer.track.rename" | "mixer.track.remove" | "source.sends"
    )
}

fn track_of(id: &CommandId) -> Option<usize> {
    id.param("track")?.parse().ok()
}

impl Shown<'_> {
    /// Whether a track entry can run now.
    pub(super) fn track_enabled(&self, id: &CommandId) -> Result<(), Disabled> {
        self.running()?;
        let tracks = self.studio.tracks();
        match id.base() {
            "mixer.track.add" if tracks.names().count() >= MAX_TRACKS => {
                Err("There are five tracks with Master, the most a canvas has.".into())
            }
            "mixer.track.add" => Ok(()),
            "source.sends" if self.source(id)?.audio => Ok(()),
            "source.sends" => Err("The source has no sound in the Mixer.".into()),
            _ => match track_of(id).and_then(|i| tracks.name(i)) {
                Some(_) => Ok(()),
                None => Err("The track is gone.".into()),
            },
        }
    }

    /// Track entries show at the Advanced level; Send to tracks while there is a track.
    pub(super) fn track_applies(&self, id: &CommandId) -> bool {
        let (advanced, _) = self.experience.levels();
        advanced && (id.base() != "source.sends" || self.studio.tracks().names().next().is_some())
    }

    /// Send to tracks' choices: Master and each track, checked where the source sends.
    pub(super) fn send_choices(&self, id: &CommandId) -> Vec<(String, String, bool)> {
        match self.source(id) {
            Ok(f) => self.studio.send_choices(f.id),
            Err(_) => Vec::new(),
        }
    }
}

/// What a track entry runs; `bus` is Send to tracks' chosen value.
pub(super) fn run(id: &CommandId, state: &Shown<'_>, bus: Option<&str>) -> Option<Run> {
    let name = |key| id.param(key).map(str::to_owned);
    Some(Run::Track(match id.base() {
        "mixer.track.add" => TrackOp::Add(name("name")),
        "mixer.track.rename" => TrackOp::Rename(track_of(id)?, name("rename")),
        "mixer.track.remove" => TrackOp::Remove(track_of(id)?),
        "source.sends" => TrackOp::Send(state.source(id).ok()?.id, bus?.to_owned()),
        _ => return None,
    }))
}
