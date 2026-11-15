//! Pausing and resuming a recording on a running studio (ADR-0124, spec
//! `docs/specs/recording.md`, "Pause and hold").
//!
//! A recording started with [`OutputSettings::pausable`] owns a [`PauseWindow`] that
//! its export and audio encoder follow. [`Studio::pause_output`] writes a window
//! opening on a render tick at least two render periods ahead, so no worker already
//! inside a tick has passed the instant, and wakes the export;
//! [`Studio::resume_output`] closes it the same way, and asks the video encoder for a
//! keyframe at the join. A resume before the pause began cancels it.
//!
//! This first step pauses a recording whose encoders are its own. A recording that
//! shares an encoder with another output is refused with
//! [`PauseRefused::SharedEncoder`] until the cut at the output node is built, and
//! one with separate files running with [`PauseRefused::SeparateFiles`] until those
//! follow the window.

use std::fmt;

use rbs_core::pause::{PauseError, Window};

use super::*;

/// Render ticks from the tick in progress to a pause's instant: the instant is then
/// at least two render periods after the request (ADR-0124 decision 2).
const LEAD_TICKS: u64 = 3;

/// Why [`Studio::pause_output`] or [`Studio::resume_output`] did nothing.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseRefused {
    /// No output with this id runs.
    NotRunning,
    /// The output was not started [`OutputSettings::pausable`] (a stream, the replay
    /// buffer).
    NotPausable,
    /// The recording shares its video or audio encoder with another output.
    SharedEncoder,
    /// Separate video or audio files run (ADR-0086); they do not follow a pause yet.
    SeparateFiles,
    /// The output was ended (ADR-0045).
    Ending,
    /// The recording is already paused.
    AlreadyPaused,
    /// The recording is not paused.
    NotPaused,
}

impl fmt::Display for PauseRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotRunning => "the output does not run",
            Self::NotPausable => "the output cannot be paused",
            Self::SharedEncoder => "the recording shares an encoder with another output",
            Self::SeparateFiles => "separate files are recorded; they cannot pause yet",
            Self::Ending => "the output is ending",
            Self::AlreadyPaused => "the recording is already paused",
            Self::NotPaused => "the recording is not paused",
        })
    }
}

impl std::error::Error for PauseRefused {}

/// Where a pausable output's pauses stand, on the master clock.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PauseState {
    /// Paused now, or a pause asked for has not begun yet.
    pub paused: bool,
    /// When the open (or last) pause begins.
    pub pause_at: Option<Timestamp>,
    /// When the last pause ended; `None` while one is open.
    pub resume_at: Option<Timestamp>,
    /// Time left out so far, an open pause counted up to now.
    pub paused_ns: u64,
}

impl PauseState {
    fn of(w: Window, now: u64) -> Self {
        if w.pause_at == u64::MAX {
            return Self {
                paused_ns: w.before,
                ..Self::default()
            };
        }
        let until = now.min(w.resume_at);
        Self {
            paused: w.is_open(),
            pause_at: Some(Timestamp::from_nanos(w.pause_at)),
            resume_at: (!w.is_open()).then(|| Timestamp::from_nanos(w.resume_at)),
            paused_ns: w.before + until.saturating_sub(w.pause_at),
        }
    }
}

impl From<PauseError> for PauseRefused {
    fn from(e: PauseError) -> Self {
        match e {
            PauseError::AlreadyPaused | PauseError::TooEarly => Self::AlreadyPaused,
            PauseError::NotPaused => Self::NotPaused,
        }
    }
}

impl Studio {
    /// Pauses a running recording (ADR-0124): from a render tick at least two render
    /// periods ahead, nothing reaches its file until [`Studio::resume_output`], and
    /// what follows continues the file with no gap. The encoder sessions stay open.
    /// Returns the instant the pause begins, on the master clock.
    pub fn pause_output(&mut self, id: OutputId) -> Result<Timestamp, PauseRefused> {
        let at = self.next_pause_instant();
        let pause = self.own_pause(id)?;
        pause.pause(at.as_nanos())?;
        if let Some(r) = self.attached(id)
            && let Some(c) = r.video.and_then(|v| self.chains.video[v].as_ref())
        {
            c.end.wake_at(at);
        }
        Ok(at)
    }

    /// Resumes a paused recording from a render tick at least two render periods
    /// ahead, and asks its video encoder for a keyframe at the join. A resume before
    /// the pause began cancels it: nothing is left out. Returns the instant the
    /// recording continues from, on the master clock.
    pub fn resume_output(&mut self, id: OutputId) -> Result<Timestamp, PauseRefused> {
        let at = self.next_pause_instant();
        let pause = self.own_pause(id)?;
        let w = pause.resume(at.as_nanos())?;
        if w.pause_at != u64::MAX
            && let Some(r) = self.attached(id)
            && let Some(c) = r.video.and_then(|v| self.chains.video[v].as_ref())
            && let Some(keyframes) = &c.keyframes
        {
            // The first frame after the join is stamped at the pause's start in file
            // time (ADR-0124 decision 7).
            keyframes.request(Timestamp::from_nanos(w.pause_at - w.before));
        }
        Ok(at)
    }

    /// Where a running output's pauses stand. `None` when `id` does not run or is not
    /// pausable.
    pub fn output_pause(&self, id: OutputId) -> Option<PauseState> {
        let w = self.attached(id)?.pause.as_ref()?.read()?;
        Some(PauseState::of(w, self.engine.clock().now().as_nanos()))
    }

    /// `p` with `r`'s pause fields filled in.
    pub(super) fn with_pause_progress(&self, r: &Attached, mut p: Progress) -> Progress {
        let now = self.engine.clock().now();
        let state = r
            .pause
            .as_ref()
            .and_then(|pause| pause.read())
            .map(|w| PauseState::of(w, now.as_nanos()))
            .unwrap_or_default();
        p.paused = state.paused;
        p.paused_ns = state.paused_ns;
        p.recorded_ns = now
            .as_nanos()
            .saturating_sub(r.started.as_nanos())
            .saturating_sub(state.paused_ns);
        p
    }

    /// The render tick [`LEAD_TICKS`] after the one in progress.
    fn next_pause_instant(&self) -> Timestamp {
        let ticks = Cadence::frames(Timestamp::ZERO, self.fps);
        let now = self.engine.clock().now();
        ticks.time_of(ticks.index_at_or_before(now).unwrap_or(0) + LEAD_TICKS)
    }

    /// The pause window of a running, pausable output whose chains are its own.
    fn own_pause(&self, id: OutputId) -> Result<Arc<PauseWindow>, PauseRefused> {
        let r = self.attached(id).ok_or(PauseRefused::NotRunning)?;
        if r.ended.is_some() {
            return Err(PauseRefused::Ending);
        }
        let pause = r.pause.as_ref().ok_or(PauseRefused::NotPausable)?;
        if self.iso_video_running() || self.iso_audio_running() {
            return Err(PauseRefused::SeparateFiles);
        }
        let users = |video: bool, slot: usize| {
            self.outputs
                .iter()
                .filter(|o| (if video { o.video } else { o.audio }) == Some(slot))
                .count()
        };
        if let Some(v) = r.video {
            let own = self.chains.video[v]
                .as_ref()
                .and_then(|c| c.pause.as_ref())
                .is_some_and(|p| Arc::ptr_eq(p, pause));
            if !own || users(true, v) != 1 {
                return Err(PauseRefused::SharedEncoder);
            }
        }
        if let Some(a) = r.audio {
            let own = self.chains.audio[a]
                .as_ref()
                .and_then(|c| c.pause.as_ref())
                .is_some_and(|p| Arc::ptr_eq(p, pause));
            if !own || users(false, a) != 1 {
                return Err(PauseRefused::SharedEncoder);
            }
        }
        Ok(Arc::clone(pause))
    }
}
