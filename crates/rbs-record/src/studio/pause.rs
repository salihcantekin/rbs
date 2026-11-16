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
//! A chain the recording uses alone is cut before its encoder (the export and the
//! audio encoder node follow the chain's window). A chain it shares with another
//! output is cut at the recording's output node ([`OutputCut`]), and its video
//! encoder is asked for a keyframe at the join in master time; an adapter that
//! cannot make one refuses the pause ([`PauseRefused::KeyframeUnsupported`]).
//! Separate files started with [`IsoVideoStart::pause_with`](super::IsoVideoStart::pause_with)
//! naming the recording, or with [`IsoAudioStart::pause`](super::IsoAudioStart::pause)
//! holding the [`SharedPause`] the recording was started with
//! ([`OutputSettings::shared_pause`]), follow the same window: their exports, audio encoders and image tracks
//! cut at the same instants, and their encoders get a keyframe at each join. Other
//! separate files make a pause [`PauseRefused::SeparateFiles`].

use std::fmt;

use rbs_core::pause::{PauseError, Window};
use rbs_nodes::output_cut::OutputCut;

use super::*;

/// Render ticks from the tick in progress to a pause's instant: the instant is then
/// at least two render periods after the request (ADR-0124 decision 2).
const LEAD_TICKS: u64 = 3;

/// A pause window made before its recording starts, so separate audio files that
/// open first can follow it (ADR-0124): give the same value to
/// [`IsoAudioStart::pause`](super::IsoAudioStart::pause) and
/// [`OutputSettings::shared_pause`]. Two values are equal only when they are the
/// same window.
#[derive(Debug, Clone, Default)]
pub struct SharedPause(Arc<PauseWindow>);

impl SharedPause {
    /// A new window with no pause.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub(super) fn window(&self) -> &Arc<PauseWindow> {
        &self.0
    }
}

impl PartialEq for SharedPause {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for SharedPause {}

/// Why [`Studio::pause_output`] or [`Studio::resume_output`] did nothing.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseRefused {
    /// No output with this id runs.
    NotRunning,
    /// The output was not started [`OutputSettings::pausable`] (a stream, the replay
    /// buffer).
    NotPausable,
    /// The recording shares its video encoder with another output, and that
    /// encoder's adapter does not declare `codec.reconfigure: keyframe`: the file
    /// could not resume at the join (ADR-0124 decision 4).
    KeyframeUnsupported,
    /// Separate video or audio files run that were not started with this recording
    /// ([`IsoVideoStart::pause_with`](super::IsoVideoStart::pause_with)), so they
    /// would not pause with it.
    SeparateFiles,
    /// The output was ended (ADR-0045).
    Ending,
    /// The recording is already paused.
    AlreadyPaused,
    /// The recording is not paused.
    NotPaused,
    /// A stream sharing the recording's encoder is on hold (ADR-0124 decision 14):
    /// resuming now would put the hold picture in the file. Resume the stream first.
    Held,
}

impl fmt::Display for PauseRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotRunning => "the output does not run",
            Self::NotPausable => "the output cannot be paused",
            Self::KeyframeUnsupported => {
                "the recording shares a video encoder that cannot make a keyframe on request"
            }
            Self::SeparateFiles => "separate files run that do not belong to this recording",
            Self::Ending => "the output is ending",
            Self::AlreadyPaused => "the recording is already paused",
            Self::NotPaused => "the recording is not paused",
            Self::Held => "a stream sharing the recording's encoder is on hold",
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

/// Where one pause of a recording was written (ADR-0124 decision 4): a chain the
/// recording uses alone is cut before its encoder, a shared one at the output node.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Targets {
    video_chain: bool,
    audio_chain: bool,
    /// Bit per output stream cut at the output node.
    output_mask: u64,
}

/// How a pause of one running recording is applied, worked out before anything is
/// written.
struct Plan {
    pause: Arc<PauseWindow>,
    targets: Targets,
    video_chain: Option<usize>,
    audio_chain: Option<usize>,
    cut: Option<Arc<OutputCut>>,
}

impl Studio {
    /// Pauses a running recording (ADR-0124): from a render tick at least two render
    /// periods ahead, nothing reaches its files until [`Studio::resume_output`], and
    /// what follows continues them with no gap. The encoder sessions stay open. A
    /// chain the recording uses alone is cut before its encoder; one it shares with
    /// another output (a stream) is cut at the recording's output node, which needs
    /// that encoder to honour keyframe requests. Returns the instant the pause
    /// begins, on the master clock.
    pub fn pause_output(&mut self, id: OutputId) -> Result<Timestamp, PauseRefused> {
        let at = self.next_pause_instant();
        let plan = self.plan_pause(id)?;
        plan.pause.pause(at.as_nanos())?;
        // The recording's window took it: the chains and the cut follow it, so they
        // are never paused while it is not.
        let n = at.as_nanos();
        if plan.targets.video_chain
            && let Some(c) = plan.video_chain.and_then(|v| self.chains.video[v].as_ref())
        {
            if let Some(w) = &c.pause {
                let _ = w.pause(n);
            }
            c.end.wake_at(at);
        }
        if plan.targets.audio_chain
            && let Some(w) = plan
                .audio_chain
                .and_then(|a| self.chains.audio[a].as_ref())
                .and_then(|c| c.pause.as_ref())
        {
            let _ = w.pause(n);
        }
        if plan.targets.output_mask != 0
            && let Some(cut) = &plan.cut
        {
            let _ = cut.pause(n, plan.targets.output_mask);
        }
        if let Some(v) = &self.iso_video {
            for end in &v.ends {
                end.wake_at(at);
            }
        }
        if let Some(r) = self.outputs.iter_mut().find(|r| r.id == id) {
            r.paused_at = plan.targets;
        }
        Ok(at)
    }

    /// Resumes a paused recording from a render tick at least two render periods
    /// ahead, and asks its video encoders for a keyframe at the join. A resume before
    /// the pause began cancels it: nothing is left out. Returns the instant the
    /// recording continues from, on the master clock.
    pub fn resume_output(&mut self, id: OutputId) -> Result<Timestamp, PauseRefused> {
        let at = self.next_pause_instant();
        let r = self.attached(id).ok_or(PauseRefused::NotRunning)?;
        if r.ended.is_some() {
            return Err(PauseRefused::Ending);
        }
        let pause = r.pause.clone().ok_or(PauseRefused::NotPausable)?;
        if self.chains_held(r) {
            return Err(PauseRefused::Held);
        }
        let (targets, video, audio, cut) = (r.paused_at, r.video, r.audio, r.cut.clone());
        let w = pause.resume(at.as_nanos())?;
        let n = at.as_nanos();
        let resumed = w.pause_at != u64::MAX;
        if let Some(c) = video.and_then(|v| self.chains.video[v].as_ref()) {
            if targets.video_chain
                && let Some(window) = &c.pause
                && let Ok(cw) = window.resume(n)
                && cw.pause_at != u64::MAX
            {
                // The first frame after the join is stamped at the pause's start in
                // file time (ADR-0124 decision 7).
                c.keyframes
                    .request(Timestamp::from_nanos(cw.pause_at - cw.before));
            }
            if targets.output_mask & 1 != 0 && resumed {
                // A shared encoder sees master time: the join keyframe at `resume_at`.
                c.keyframes.request(at);
            }
        }
        if targets.audio_chain
            && let Some(window) = audio
                .and_then(|a| self.chains.audio[a].as_ref())
                .and_then(|c| c.pause.as_ref())
        {
            let _ = window.resume(n);
        }
        if targets.output_mask != 0
            && let Some(cut) = &cut
        {
            let _ = cut.resume(n);
        }
        if resumed && let Some(v) = &self.iso_video {
            for keyframes in &v.keyframes {
                keyframes.request(Timestamp::from_nanos(w.pause_at - w.before));
            }
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

    /// Where a pause of `id` goes, refusing what cannot be paused.
    fn plan_pause(&self, id: OutputId) -> Result<Plan, PauseRefused> {
        let r = self.attached(id).ok_or(PauseRefused::NotRunning)?;
        if r.ended.is_some() {
            return Err(PauseRefused::Ending);
        }
        let pause = r.pause.as_ref().ok_or(PauseRefused::NotPausable)?;
        // Separate files pause with their recording, or no pause is allowed.
        let follows = |iso: Option<&Arc<PauseWindow>>| iso.is_some_and(|p| Arc::ptr_eq(p, pause));
        if let Some(v) = &self.iso_video
            && !follows(v.pause.as_ref())
        {
            return Err(PauseRefused::SeparateFiles);
        }
        if let Some(a) = &self.iso_audio
            && !follows(a.pause.as_ref())
        {
            return Err(PauseRefused::SeparateFiles);
        }
        let alone = |video: bool, slot: usize| {
            self.outputs
                .iter()
                .filter(|o| (if video { o.video } else { o.audio }) == Some(slot))
                .count()
                == 1
        };
        let mut targets = Targets::default();
        // Stream 0 is video when there is video; audio follows it.
        if let Some(v) = r.video {
            let c = self.chains.video[v]
                .as_ref()
                .ok_or(PauseRefused::NotRunning)?;
            if c.pause.is_some() && alone(true, v) {
                targets.video_chain = true;
            } else if c.keyframe_capable {
                targets.output_mask |= 1;
            } else {
                return Err(PauseRefused::KeyframeUnsupported);
            }
        }
        if let Some(a) = r.audio {
            let c = self.chains.audio[a]
                .as_ref()
                .ok_or(PauseRefused::NotRunning)?;
            if c.pause.is_some() && alone(false, a) {
                targets.audio_chain = true;
            } else {
                targets.output_mask |= 1 << u32::from(r.video.is_some());
            }
        }
        Ok(Plan {
            pause: Arc::clone(pause),
            targets,
            video_chain: r.video,
            audio_chain: r.audio,
            cut: r.cut.clone(),
        })
    }
}
