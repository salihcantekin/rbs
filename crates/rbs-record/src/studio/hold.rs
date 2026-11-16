//! Holding a stream on a running studio (ADR-0124 decisions 12 to 14, spec
//! `docs/specs/recording.md`, "Pause and hold").
//!
//! A stream started with [`OutputSettings::holdable`] runs on chains that follow a
//! hold switch: its export and its audio encoder node. [`Studio::hold_output`] turns
//! the switches on and wakes the export, which from its next tick reads nothing
//! back and sends a pinned picture (the last frame, or a black one made once)
//! stamped with each tick, while the audio encoder node sends silence with the
//! blocks' own timestamps. The connection stays up and timestamps keep running.
//! [`Studio::release_hold`] turns them off: the export goes back to the program on
//! its next tick and raises its run-path value again.
//!
//! The hold is on the chain, so every stream sharing it is held with it. A chain
//! shared with an output that cannot be held (a recording) is held only while that
//! output is paused, so the hold picture never reaches its file (decision 14); the
//! host pauses the recording first, and [`Studio::resume_output`] refuses while a
//! hold of its chains is on ([`PauseRefused::Held`](super::PauseRefused::Held)).

use std::fmt;

use rbs_core::pause::HoldPicture;

use super::*;

/// Why [`Studio::hold_output`] or [`Studio::release_hold`] did nothing.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldRefused {
    /// No output with this id runs.
    NotRunning,
    /// The output was not started [`OutputSettings::holdable`] (a recording, the
    /// replay buffer), or a chain it uses was not built to hold.
    NotHoldable,
    /// The black picture cannot be made for the export's pixel format.
    PictureUnsupported,
    /// Its encoder is shared with an output that cannot be held and is not paused
    /// (ADR-0124 decision 14): pause that recording first.
    SharedWithRecording,
    /// The output was ended (ADR-0045).
    Ending,
    /// The stream is already on hold with this picture.
    AlreadyHeld,
    /// The stream is not on hold.
    NotHeld,
}

impl fmt::Display for HoldRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotRunning => "the output does not run",
            Self::NotHoldable => "the output cannot be held",
            Self::PictureUnsupported => "the hold picture cannot be made for this stream",
            Self::SharedWithRecording => {
                "the stream shares its encoder with a recording that is not paused"
            }
            Self::Ending => "the output is ending",
            Self::AlreadyHeld => "the stream is already on hold",
            Self::NotHeld => "the stream is not on hold",
        })
    }
}

impl std::error::Error for HoldRefused {}

/// Where a holdable output's holds stand, on the master clock.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HoldState {
    /// The picture sent while on hold; `None` while not held.
    pub picture: Option<HoldPicture>,
    /// When the open hold began.
    pub held_at: Option<Timestamp>,
    /// Time on hold so far, an open hold counted up to now.
    pub held_ns: u64,
}

/// An output's holds, kept on the control thread.
#[derive(Debug, Default)]
pub(super) struct Holds {
    /// Started [`OutputSettings::holdable`].
    holdable: bool,
    /// The open hold: its picture and when it began.
    on: Option<(HoldPicture, Timestamp)>,
    /// Time held by earlier holds.
    before_ns: u64,
}

impl Holds {
    pub(super) fn new(holdable: bool) -> Self {
        Self {
            holdable,
            ..Self::default()
        }
    }

    fn state(&self, now: Timestamp) -> HoldState {
        let open = self
            .on
            .map_or(0, |(_, at)| now.as_nanos().saturating_sub(at.as_nanos()));
        HoldState {
            picture: self.on.map(|(p, _)| p),
            held_at: self.on.map(|(_, at)| at),
            held_ns: self.before_ns.saturating_add(open),
        }
    }

    fn start(&mut self, picture: HoldPicture, now: Timestamp) {
        match &mut self.on {
            Some((p, _)) => *p = picture,
            None => self.on = Some((picture, now)),
        }
    }

    fn end(&mut self, now: Timestamp) {
        if let Some((_, at)) = self.on.take() {
            self.before_ns = self
                .before_ns
                .saturating_add(now.as_nanos().saturating_sub(at.as_nanos()));
        }
    }
}

impl Studio {
    /// Puts a running stream on hold (ADR-0124 decisions 12 and 13): from the next
    /// render tick its export sends `picture` stamped with each tick and its audio
    /// encoder sends silence; the connection, the encoder sessions and the
    /// timestamps run on. Every stream sharing its chains is held with it. Calling
    /// it again with another picture changes the picture. Returns when the hold
    /// began, on the master clock.
    pub fn hold_output(
        &mut self,
        id: OutputId,
        picture: HoldPicture,
    ) -> Result<Timestamp, HoldRefused> {
        let r = self.attached(id).ok_or(HoldRefused::NotRunning)?;
        if r.ended.is_some() {
            return Err(HoldRefused::Ending);
        }
        if !r.hold.holdable {
            return Err(HoldRefused::NotHoldable);
        }
        if r.hold.on.is_some_and(|(p, _)| p == picture) {
            return Err(HoldRefused::AlreadyHeld);
        }
        let (video, audio) = (r.video, r.audio);
        let video_chain = video.and_then(|v| self.chains.video[v].as_ref());
        let audio_chain = audio.and_then(|a| self.chains.audio[a].as_ref());
        let video_hold = match (video, video_chain) {
            (None, _) => None,
            (Some(_), Some(c)) => Some(c.hold.as_ref().ok_or(HoldRefused::NotHoldable)?),
            (Some(_), None) => return Err(HoldRefused::NotRunning),
        };
        let audio_hold = match (audio, audio_chain) {
            (None, _) => None,
            (Some(_), Some(c)) => Some(c.hold.as_ref().ok_or(HoldRefused::NotHoldable)?),
            (Some(_), None) => return Err(HoldRefused::NotRunning),
        };
        // Every other user of these chains is a stream held with it, or a paused
        // recording whose output cuts what the chains send (decision 14).
        let now = self.engine.clock().now();
        for o in self
            .outputs
            .iter()
            .filter(|o| o.id != id && shares(o, video, audio))
        {
            if o.hold.holdable {
                continue;
            }
            let paused = o
                .pause
                .as_ref()
                .and_then(|p| p.read())
                .is_some_and(|w| w.is_open() && w.pause_at <= now.as_nanos());
            if !paused {
                return Err(HoldRefused::SharedWithRecording);
            }
        }
        if let Some((hold, format)) = video_hold
            && picture == HoldPicture::Black
            && !hold.has_black()
        {
            // Allocates once, here on the control thread, never on a tick.
            let black = rbs_nodes::black_frame(*format).ok_or(HoldRefused::PictureUnsupported)?;
            hold.set_black(black);
        }
        if let Some((hold, _)) = video_hold {
            hold.switch().hold(picture);
        }
        if let Some(switch) = audio_hold {
            switch.hold(picture);
        }
        if let Some(c) = video_chain {
            // The export reads the switch on its slow path only.
            c.end.wake_at(Timestamp::ZERO);
        }
        for o in self.outputs.iter_mut().filter(|o| o.hold.holdable) {
            if o.id == id || shares(o, video, audio) {
                o.hold.start(picture, now);
            }
        }
        Ok(now)
    }

    /// Takes a held stream back to the program from the next render tick, with every
    /// stream sharing its chains. Returns when the hold ended, on the master clock.
    pub fn release_hold(&mut self, id: OutputId) -> Result<Timestamp, HoldRefused> {
        let r = self.attached(id).ok_or(HoldRefused::NotRunning)?;
        if !r.hold.holdable {
            return Err(HoldRefused::NotHoldable);
        }
        if r.hold.on.is_none() {
            return Err(HoldRefused::NotHeld);
        }
        let (video, audio) = (r.video, r.audio);
        if let Some((hold, _)) = video
            .and_then(|v| self.chains.video[v].as_ref())
            .and_then(|c| c.hold.as_ref())
        {
            hold.switch().release();
        }
        if let Some(switch) = audio
            .and_then(|a| self.chains.audio[a].as_ref())
            .and_then(|c| c.hold.as_ref())
        {
            switch.release();
        }
        let now = self.engine.clock().now();
        for o in self.outputs.iter_mut() {
            if o.id == id || shares(o, video, audio) {
                o.hold.end(now);
            }
        }
        Ok(now)
    }

    /// Where a running output's holds stand. `None` when `id` does not run or is not
    /// holdable.
    pub fn output_hold(&self, id: OutputId) -> Option<HoldState> {
        let r = self.attached(id)?;
        r.hold
            .holdable
            .then(|| r.hold.state(self.engine.clock().now()))
    }

    /// True when a hold is on for a chain `r` uses (a recording sharing a held
    /// stream's encoder must not resume, ADR-0124 decision 14).
    pub(super) fn chains_held(&self, r: &Attached) -> bool {
        let video = r
            .video
            .and_then(|v| self.chains.video[v].as_ref())
            .and_then(|c| c.hold.as_ref())
            .is_some_and(|(h, _)| h.switch().is_held());
        let audio = r
            .audio
            .and_then(|a| self.chains.audio[a].as_ref())
            .and_then(|c| c.hold.as_ref())
            .is_some_and(|h| h.is_held());
        video || audio
    }

    /// `p` with `r`'s hold fields filled in.
    pub(super) fn with_hold_progress(&self, r: &Attached, mut p: Progress) -> Progress {
        let state = r.hold.state(self.engine.clock().now());
        p.held = state.picture.is_some();
        p.held_ns = state.held_ns;
        p
    }
}

/// True when `o` takes packets from the video chain in slot `video` or the audio
/// chain in slot `audio`.
fn shares(o: &Attached, video: Option<usize>, audio: Option<usize>) -> bool {
    (video.is_some() && o.video == video) || (audio.is_some() && o.audio == audio)
}
