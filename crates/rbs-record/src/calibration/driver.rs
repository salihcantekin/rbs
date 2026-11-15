//! The step driver of the encoder measurement (spec `output-estimate.md`,
//! "Calibration run"): series by series, step by step, judged on the run's clock.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use rbs_codec::{CreateError, EncodeError, VideoEncoderConfig, VideoEncoderFactory};
use rbs_core::media::SystemFrame;
use rbs_core::registry::AdapterId;
use rbs_core::time::{Clock, Timestamp};

use super::session::{FrameOutcome, SessionReport, moving_ring};
use super::{
    CALIBRATION_LIMIT, CalibrationPlan, Candidate, ClassKey, ClassPlan, FIRST_PASS,
    FRAME_TIME_ROUNDS, FRAME_TIME_SETTLE, JITTER_DEPTH, JUDGED_SECONDS, RING_FRAMES,
    SESSION_CEILING, WARMUP_MAX, WARMUP_MIN, WARMUP_ROUND_LIMIT, WARMUP_STABLE, WRITE_RESERVE,
};
use crate::estimate::StopReason;

/// Waiting, injected so tests never sleep: the app yields and sleeps, tests advance a
/// `TestClock`.
pub trait Pacer: Send + Sync {
    /// Lets other threads run between `Busy` retries.
    fn yield_now(&self);
    /// Returns at `t` or later.
    fn wait_until(&self, t: Timestamp);
}

/// One round of every open session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RoundRequest {
    /// A `Busy` retry past this time makes the frame late.
    pub deadline: Timestamp,
}

/// A lane thread panicked or went away: the run ends `failed`, code `panicked`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LanesFailure;

/// The sessions of a series on the take's thread layout: lane 0 is the master's own
/// partition, lanes 1 and 2 the shared workers. Session `i` (from 1) runs on lane 0
/// when `i = 1`, else on lane `1 + (i - 2) % 2`.
pub trait Lanes {
    /// Opens one more session on `lane`.
    fn open(
        &mut self,
        lane: usize,
        factory: &Arc<dyn VideoEncoderFactory>,
        config: &VideoEncoderConfig,
        ring: &Arc<[SystemFrame]>,
    ) -> Result<Result<(), CreateError>, LanesFailure>;

    /// Every open session encodes one frame; the lanes run at the same time, each its
    /// sessions in order. `out` gets one report per session, in open order.
    fn round(
        &mut self,
        request: RoundRequest,
        out: &mut Vec<SessionReport>,
    ) -> Result<(), LanesFailure>;

    /// Flushes and closes every session; per session, whether a frame was lost.
    fn close(&mut self) -> Result<Vec<Result<bool, EncodeError>>, LanesFailure>;
}

/// The lane of session `i` (from 1).
pub fn lane_of(i: u32) -> usize {
    if i <= 1 {
        0
    } else {
        1 + ((i - 2) % 2) as usize
    }
}

/// A series: one class on one adapter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeriesKey {
    /// The class.
    pub class: ClassKey,
    /// The adapter.
    pub adapter: AdapterId,
    /// Its `adapter.version`.
    pub version: Option<String>,
    /// A hardware adapter.
    pub hardware: bool,
}

/// One series' result (`[[encoders]]`).
#[derive(Clone, Debug, PartialEq)]
pub struct EncoderResult {
    /// What was measured.
    pub key: SeriesKey,
    /// The highest passing step.
    pub sessions: u32,
    /// `sessions × width × height × fps`.
    pub pixels_per_second: u64,
    /// Mean `submit` plus `drain` time of one paced session.
    pub frame_time: Duration,
    /// Why the series stopped.
    pub stopped_by: StopReason,
}

/// Why a series or the write has no (full) result: the closed code list of
/// `calibration.state`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SkipCode {
    /// The adapter does not support the class.
    Unsupported,
    /// No free session at step 1 (other programs may hold them).
    Busy,
    /// A session did not open.
    OpenFailed,
    /// An encode failed.
    EncodeFailed,
    /// The folder has no room for the write.
    NoRoom,
    /// The write failed.
    WriteFailed,
    /// The time budget ended it first.
    TimeLimit,
    /// A cancel ended it first.
    Cancelled,
}

impl SkipCode {
    /// The wire name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unsupported => "unsupported",
            Self::Busy => "busy",
            Self::OpenFailed => "open_failed",
            Self::EncodeFailed => "encode_failed",
            Self::NoRoom => "no_room",
            Self::WriteFailed => "write_failed",
            Self::TimeLimit => "time_limit",
            Self::Cancelled => "cancelled",
        }
    }
}

/// A skipped series (or the write when `series` is `None`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skipped {
    /// Why.
    pub code: SkipCode,
    /// The series.
    pub series: Option<SeriesKey>,
}

/// Progress of a run, sent as it happens.
#[derive(Clone, Debug, PartialEq)]
pub enum CalibrationEvent {
    /// A step started (`step` counts from 1 over the run).
    Step {
        /// The step.
        step: u32,
        /// The adapter it measures.
        adapter: AdapterId,
    },
    /// A series finished with a result (and possibly a skip code beside it).
    Series(EncoderResult),
    /// A series or part was skipped.
    Skipped(Skipped),
    /// An export class started (`step` counts from 1 over the export part; step D2).
    ExportStep {
        /// The step.
        step: u32,
    },
    /// An export class was measured.
    Export(super::ExportResult),
    /// An export class was skipped.
    ExportSkipped(super::ExportSkip),
}

/// How the encoder part ended.
#[derive(Clone, Debug, PartialEq)]
pub enum CalibrationOutcome {
    /// It ran to its end, a cancel or its time.
    Done {
        /// The cancel ended it.
        cancelled: bool,
    },
    /// A device was lost: the whole run fails and nothing is written.
    DeviceLost,
    /// A lane panicked: the whole run fails.
    Panicked,
}

enum StepEnd {
    Pass,
    Late,
    Limit,
    Cancelled,
    Error(EncodeError),
    Panicked,
}

enum SeriesEnd {
    Result(EncoderResult, Option<SkipCode>),
    Skip(SkipCode),
    DeviceLost,
    Panicked,
}

struct Ctx<'a> {
    lanes: &'a mut dyn Lanes,
    clock: &'a dyn Clock,
    pacer: &'a dyn Pacer,
    stop: &'a AtomicBool,
    events: &'a mut dyn FnMut(CalibrationEvent),
    step: u32,
    reports: Vec<SessionReport>,
    durations: Vec<Duration>,
    max_backlog: Vec<u64>,
}

impl Ctx<'_> {
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// The end of a round's reports, or `None` when every frame was fine.
    fn judge(&self) -> Option<StepEnd> {
        for r in &self.reports {
            match &r.outcome {
                FrameOutcome::Accepted => {}
                FrameOutcome::Late => return Some(StepEnd::Late),
                FrameOutcome::Stopped => return Some(StepEnd::Cancelled),
                FrameOutcome::Error(e) => return Some(StepEnd::Error(e.clone())),
            }
            if r.lost {
                return Some(StepEnd::Late);
            }
        }
        None
    }

    fn round(&mut self, deadline: Timestamp) -> Result<(), StepEnd> {
        self.lanes
            .round(RoundRequest { deadline }, &mut self.reports)
            .map_err(|LanesFailure| StepEnd::Panicked)
    }

    /// One step of `n` open sessions at `period`: warm-up, judged rounds, sustained
    /// rate (spec "A late round").
    fn step(&mut self, n: u32, period: Duration, judged: u32, deadline: Timestamp) -> StepEnd {
        // Warm-up: until no backlog has grown for WARMUP_STABLE rounds.
        self.max_backlog.clear();
        self.max_backlog.resize(n as usize, 0);
        let mut stable = 0;
        let mut rounds = 0;
        loop {
            if self.stopped() {
                return StepEnd::Cancelled;
            }
            if self.clock.now() > deadline {
                return StepEnd::Limit;
            }
            let start = self.clock.now();
            if let Err(e) = self.round(start + WARMUP_ROUND_LIMIT) {
                return e;
            }
            if self.clock.now().saturating_since(start) > WARMUP_ROUND_LIMIT {
                return StepEnd::Late;
            }
            if let Some(end) = self.judge() {
                return end;
            }
            let mut grew = false;
            for (m, r) in self.max_backlog.iter_mut().zip(&self.reports) {
                if r.backlog > *m {
                    *m = r.backlog;
                    grew = true;
                }
            }
            rounds += 1;
            stable = if grew { 0 } else { stable + 1 };
            if rounds >= WARMUP_MIN && stable >= WARMUP_STABLE {
                break;
            }
            if rounds >= WARMUP_MAX {
                return StepEnd::Late;
            }
        }

        // Judged rounds on the virtual paced schedule.
        let q = u64::from(JITTER_DEPTH);
        let t_start = self.clock.now();
        let mut e_prev = t_start;
        self.durations.clear();
        for k in 0..u64::from(judged) {
            if self.stopped() {
                return StepEnd::Cancelled;
            }
            if self.clock.now() > deadline {
                return StepEnd::Limit;
            }
            let tick = t_start + period.saturating_mul(k as u32);
            let latest = t_start + period.saturating_mul((k + q) as u32);
            let begin = e_prev.max(tick);
            let allowed = latest.saturating_since(begin);
            let s = self.clock.now();
            if let Err(e) = self.round(s + allowed) {
                return e;
            }
            let d = self.clock.now().saturating_since(s);
            let e = begin + d;
            if e > latest {
                return StepEnd::Late;
            }
            if let Some(end) = self.judge() {
                return end;
            }
            if self
                .reports
                .iter()
                .zip(&self.max_backlog)
                .any(|(r, l)| r.backlog > l + q)
            {
                return StepEnd::Late;
            }
            self.durations.push(d);
            e_prev = e;
        }
        // Sustained rate: the mean without the Q longest rounds stays within P.
        self.durations.sort_unstable();
        let keep = self.durations.len().saturating_sub(JITTER_DEPTH as usize);
        if keep > 0 {
            let total: Duration = self.durations[..keep].iter().sum();
            if total / keep as u32 > period {
                return StepEnd::Late;
            }
        }
        StepEnd::Pass
    }

    /// Paced rounds of session 1 after step 1: the mean call time.
    fn frame_time(&mut self, period: Duration) -> Result<Duration, StepEnd> {
        let base = self.clock.now();
        let mut total = Duration::ZERO;
        let mut counted = 0u32;
        for i in 0..FRAME_TIME_SETTLE + FRAME_TIME_ROUNDS {
            if self.stopped() {
                return Err(StepEnd::Cancelled);
            }
            let tick = base + period.saturating_mul(i + 1);
            self.pacer.wait_until(tick);
            self.round(tick + WARMUP_ROUND_LIMIT)?;
            match self.reports.first().map(|r| r.outcome.clone()) {
                Some(FrameOutcome::Error(e)) => return Err(StepEnd::Error(e)),
                Some(FrameOutcome::Stopped) => return Err(StepEnd::Cancelled),
                _ => {}
            }
            if i >= FRAME_TIME_SETTLE {
                total += self.reports.first().map_or(Duration::ZERO, |r| r.call_time);
                counted += 1;
            }
        }
        Ok(if counted > 0 {
            total / counted
        } else {
            Duration::ZERO
        })
    }

    fn series(&mut self, class: &ClassPlan, cand: &Candidate, deadline: Timestamp) -> SeriesEnd {
        let key = SeriesKey {
            class: class.key.clone(),
            adapter: cand.adapter.clone(),
            version: cand.version.clone(),
            hardware: cand.hardware,
        };
        let Some(ring) = moving_ring(class.config.format, RING_FRAMES) else {
            return SeriesEnd::Skip(SkipCode::OpenFailed);
        };
        let period = class.config.frame_rate.frame_duration();
        let fps = 1.0 / period.as_secs_f64().max(1e-9);
        let judged = (f64::from(JUDGED_SECONDS) * fps).round().max(1.0) as u32;
        let mut passed = 0u32;
        let mut frame_time = None;
        let mut beside = None;
        let mut stop_reason = StopReason::Ceiling;
        let mut opened = 0u32;
        for n in 1..=SESSION_CEILING {
            if self.stopped() {
                stop_reason = StopReason::Cancelled;
                break;
            }
            if self.clock.now() > deadline {
                stop_reason = StopReason::Limit;
                break;
            }
            match self
                .lanes
                .open(lane_of(n), &cand.factory, &class.config, &ring)
            {
                Err(LanesFailure) => return SeriesEnd::Panicked,
                Ok(Ok(())) => opened = n,
                Ok(Err(e)) => {
                    let first = n == 1;
                    let (reason, code) = match e {
                        CreateError::ResourceExhausted if first => {
                            (StopReason::Refused, Some(SkipCode::Busy))
                        }
                        CreateError::Unsupported { .. } if first => {
                            (StopReason::Refused, Some(SkipCode::Unsupported))
                        }
                        CreateError::ResourceExhausted | CreateError::Unsupported { .. } => {
                            (StopReason::Refused, None)
                        }
                        _ => (StopReason::Error, Some(SkipCode::OpenFailed)),
                    };
                    stop_reason = reason;
                    beside = code;
                    break;
                }
            }
            self.step += 1;
            (self.events)(CalibrationEvent::Step {
                step: self.step,
                adapter: cand.adapter.clone(),
            });
            let end = self.step(n, period, judged, deadline);
            if n == 1 && matches!(end, StepEnd::Pass | StepEnd::Late) {
                match self.frame_time(period) {
                    Ok(t) => frame_time = Some(t),
                    Err(StepEnd::Panicked) => return SeriesEnd::Panicked,
                    Err(StepEnd::Error(EncodeError::DeviceLost)) => return SeriesEnd::DeviceLost,
                    Err(StepEnd::Error(_)) => {
                        stop_reason = StopReason::Error;
                        beside = Some(SkipCode::EncodeFailed);
                        break;
                    }
                    Err(StepEnd::Cancelled) => {
                        stop_reason = StopReason::Cancelled;
                        break;
                    }
                    Err(_) => {}
                }
            }
            match end {
                StepEnd::Pass => passed = n,
                StepEnd::Late => {
                    stop_reason = StopReason::Late;
                    break;
                }
                StepEnd::Limit => {
                    stop_reason = StopReason::Limit;
                    break;
                }
                StepEnd::Cancelled => {
                    stop_reason = StopReason::Cancelled;
                    break;
                }
                StepEnd::Panicked => return SeriesEnd::Panicked,
                StepEnd::Error(EncodeError::DeviceLost) => return SeriesEnd::DeviceLost,
                StepEnd::Error(_) => {
                    stop_reason = StopReason::Error;
                    beside = Some(SkipCode::EncodeFailed);
                    break;
                }
            }
        }
        // Close the series: flush, and check the tail of the last passing step.
        let closed = if opened > 0 {
            match self.lanes.close() {
                Err(LanesFailure) => return SeriesEnd::Panicked,
                Ok(r) => r,
            }
        } else {
            Vec::new()
        };
        let mut tail_lost = false;
        for r in &closed {
            match r {
                Err(EncodeError::DeviceLost) => return SeriesEnd::DeviceLost,
                Err(_) => beside = beside.or(Some(SkipCode::EncodeFailed)),
                Ok(lost) => tail_lost |= *lost,
            }
        }
        // The tail belongs to the last passing step when the series ended between
        // steps (a refused open or the ceiling).
        if tail_lost
            && passed > 0
            && matches!(stop_reason, StopReason::Refused | StopReason::Ceiling)
        {
            passed -= 1;
            stop_reason = StopReason::Late;
        }

        // A series that did not finish step 1 has no result.
        let step1_done = passed > 0 || stop_reason == StopReason::Late;
        if !step1_done {
            return SeriesEnd::Skip(match (stop_reason, beside) {
                (_, Some(code)) => code,
                (StopReason::Cancelled, None) => SkipCode::Cancelled,
                _ => SkipCode::TimeLimit,
            });
        }
        let size = class.config.format.size;
        let pixels = u64::from(size.width) * u64::from(size.height);
        SeriesEnd::Result(
            EncoderResult {
                key,
                sessions: passed,
                pixels_per_second: (f64::from(passed) * pixels as f64 * fps) as u64,
                frame_time: frame_time.unwrap_or(Duration::ZERO),
                stopped_by: stop_reason,
            },
            beside,
        )
    }
}

/// Measures every series of `plan` from `start` (the run's start on `clock`), in two
/// passes (the likely adapter of each class, then the other candidates), within the
/// encoder part of [`CALIBRATION_LIMIT`]. Results and skips go to `events` as each
/// series ends.
pub fn run_encoders(
    plan: &CalibrationPlan,
    lanes: &mut dyn Lanes,
    clock: &dyn Clock,
    pacer: &dyn Pacer,
    stop: &AtomicBool,
    start: Timestamp,
    events: &mut dyn FnMut(CalibrationEvent),
) -> CalibrationOutcome {
    let encoder_end = start + CALIBRATION_LIMIT.saturating_sub(WRITE_RESERVE);
    // The export part runs first (step D2): the first pass keeps its whole time after
    // it, and the second pass keeps its end, so the export time comes out of pass 2.
    let pass1_end = (clock.now().max(start) + FIRST_PASS).min(encoder_end);
    let first: Vec<(&ClassPlan, &Candidate)> = plan
        .classes
        .iter()
        .filter_map(|c| c.candidates.iter().find(|x| x.likely).map(|x| (c, x)))
        .collect();
    let second: Vec<(&ClassPlan, &Candidate)> = plan
        .classes
        .iter()
        .flat_map(|c| {
            let likely = c.candidates.iter().position(|x| x.likely);
            c.candidates
                .iter()
                .enumerate()
                .filter(move |(i, _)| Some(*i) != likely)
                .map(move |(_, x)| (c, x))
        })
        .collect();
    let mut ctx = Ctx {
        lanes,
        clock,
        pacer,
        stop,
        events,
        step: 0,
        reports: Vec::with_capacity(SESSION_CEILING as usize),
        durations: Vec::with_capacity(2 * 240),
        max_backlog: Vec::with_capacity(SESSION_CEILING as usize),
    };
    let mut class_frame_time: Vec<(ClassKey, Duration)> = Vec::new();
    let mut cancelled = false;
    for (series, end) in [(first, pass1_end), (second, encoder_end)] {
        let count = series.len();
        for (i, (class, cand)) in series.into_iter().enumerate() {
            let key = || SeriesKey {
                class: class.key.clone(),
                adapter: cand.adapter.clone(),
                version: cand.version.clone(),
                hardware: cand.hardware,
            };
            if ctx.stopped() {
                cancelled = true;
                (ctx.events)(CalibrationEvent::Skipped(Skipped {
                    code: SkipCode::Cancelled,
                    series: Some(key()),
                }));
                continue;
            }
            let now = clock.now();
            let left = (count - i) as u32;
            let share = end.saturating_since(now) / left.max(1);
            let period = class.config.frame_rate.frame_duration();
            let fps = 1.0 / period.as_secs_f64().max(1e-9);
            let rounds = f64::from(WARMUP_MIN) + f64::from(JUDGED_SECONDS) * fps;
            let estimate = class_frame_time
                .iter()
                .find(|(k, _)| *k == class.key)
                .map_or(Duration::from_secs(1), |(_, t)| t.mul_f64(rounds));
            if share < estimate || share.is_zero() {
                (ctx.events)(CalibrationEvent::Skipped(Skipped {
                    code: SkipCode::TimeLimit,
                    series: Some(key()),
                }));
                continue;
            }
            match ctx.series(class, cand, now + share) {
                SeriesEnd::Panicked => return CalibrationOutcome::Panicked,
                SeriesEnd::DeviceLost => return CalibrationOutcome::DeviceLost,
                SeriesEnd::Skip(code) => {
                    cancelled |= code == SkipCode::Cancelled;
                    (ctx.events)(CalibrationEvent::Skipped(Skipped {
                        code,
                        series: Some(key()),
                    }));
                }
                SeriesEnd::Result(r, beside) => {
                    cancelled |= r.stopped_by == StopReason::Cancelled;
                    if !class_frame_time.iter().any(|(k, _)| *k == class.key) {
                        class_frame_time.push((class.key.clone(), r.frame_time));
                    }
                    (ctx.events)(CalibrationEvent::Series(r));
                    if let Some(code) = beside {
                        (ctx.events)(CalibrationEvent::Skipped(Skipped {
                            code,
                            series: Some(key()),
                        }));
                    }
                }
            }
        }
    }
    CalibrationOutcome::Done { cancelled }
}
