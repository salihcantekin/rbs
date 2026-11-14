//! The pure decision logic of recording protection (ADR-0090 decisions 2 to 4 and 6).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::Duration;

use rbs_core::time::Timestamp;

use super::probe::{ProbeOutcome, Probed, VolumeKey};
use crate::{OutputId, Progress};

const GIB: u64 = 1024 * 1024 * 1024;

/// What happens when a volume reaches its reserve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AtCritical {
    /// Every recording on the volume ends cleanly through its tail (the default).
    StopCleanly,
    /// Only the warning.
    WarnOnly,
}

/// The guard's thresholds (ADR-0090 decision 8). Phase one runs
/// [`ProtectionSettings::default`]; the saved section and its UI are phase two.
#[derive(Debug, Clone, PartialEq)]
pub struct ProtectionSettings {
    /// `notice` when less disk time than this is left.
    pub notice_minutes: u32,
    /// `warning` when less disk time than this is left.
    pub warning_minutes: u32,
    /// Free space kept on the volume: `critical` at or below it.
    pub reserve_bytes: u64,
    /// What `critical` does.
    pub at_critical: AtCritical,
    /// Frames lost per second above which an output counts as lagging.
    pub lag_limit: u32,
    /// Seconds in a row above (or, to clear, at or below) `lag_limit`.
    pub lag_seconds: u32,
    /// `warning` when available memory is below this.
    pub min_memory_bytes: u64,
    /// How far back the write rate of a volume is measured.
    pub rate_window: Duration,
}

impl Default for ProtectionSettings {
    fn default() -> Self {
        Self {
            notice_minutes: 30,
            warning_minutes: 3,
            reserve_bytes: GIB,
            at_critical: AtCritical::StopCleanly,
            lag_limit: 1,
            lag_seconds: 3,
            min_memory_bytes: GIB,
            rate_window: Duration::from_secs(60),
        }
    }
}

/// A level of one watched thing. Ordered by severity; `Unavailable` (nothing could be
/// read) sorts lowest, so it never raises the overall level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    /// Nothing could be read (the platform has no reading, or the probe timed out).
    Unavailable,
    /// Fine.
    Ok,
    /// Worth knowing.
    Notice,
    /// Act soon.
    Warning,
    /// Acting now.
    Critical,
}

/// What an alert is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AlertKind {
    /// A volume is filling.
    Disk,
    /// An output loses frames.
    Lag,
    /// The render worker is late (studio-wide).
    Render,
    /// The audio worker is late (studio-wide).
    Audio,
    /// System memory is low.
    Memory,
}

/// One running output as the guard sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputSample {
    /// The output.
    pub output: OutputId,
    /// A stream (network) rather than a recording (file).
    pub stream: bool,
    /// The volume a recording writes to; `None` for a stream or when unknown.
    pub volume: Option<VolumeKey>,
    /// Its counters from [`crate::Studio::output_counters`].
    pub counters: Progress,
}

/// A volume's free space from a probe answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VolumeSample {
    /// The volume.
    pub volume: VolumeKey,
    /// Free bytes, if read.
    pub free_bytes: Probed<u64>,
}

/// What the guard is told on each step.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Sample {
    /// Every running recording and stream.
    pub outputs: Vec<OutputSample>,
    /// The render worker's late ticks (studio-wide, cumulative).
    pub late_render_ticks: u64,
    /// The audio worker's late ticks (studio-wide, cumulative).
    pub late_audio_ticks: u64,
    /// A fresh probe answer, or its timeout; `None` when nothing new arrived.
    pub probe: Option<ProbeStep>,
}

/// Probe news for one step, already mapped to volumes.
#[derive(Debug, Clone, PartialEq)]
pub enum ProbeStep {
    /// Free space per volume of the running recordings, and memory.
    Reading {
        /// One entry per volume.
        volumes: Vec<VolumeSample>,
        /// Available memory.
        memory: Probed<u64>,
    },
    /// The probe did not answer in time.
    TimedOut,
}

impl ProbeStep {
    /// The step for a [`ProbeOutcome`], with each folder's volume taken from the
    /// reading itself. Folders on the same volume give one entry.
    pub fn of(outcome: ProbeOutcome) -> Self {
        match outcome {
            ProbeOutcome::TimedOut => Self::TimedOut,
            ProbeOutcome::Reading(r) => {
                let mut volumes: Vec<VolumeSample> = Vec::new();
                for (_, space) in r.folders {
                    if let Probed::Value(s) = space
                        && !volumes.iter().any(|v| v.volume == s.volume)
                    {
                        volumes.push(VolumeSample {
                            volume: s.volume,
                            free_bytes: Probed::Value(s.free_bytes),
                        });
                    }
                }
                Self::Reading {
                    volumes,
                    memory: r.memory,
                }
            }
        }
    }
}

/// A volume's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskState {
    /// The volume.
    pub volume: VolumeKey,
    /// Its level.
    pub level: Level,
    /// Free bytes at the last reading.
    pub free_bytes: Option<u64>,
    /// Seconds until the reserve is reached at the measured rate; `None` while the
    /// rate is unknown or free space does not fall.
    pub seconds_left: Option<u64>,
}

/// An output's lag state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputState {
    /// The output.
    pub output: OutputId,
    /// `Ok` or `Warning` (phase one only warns on lag).
    pub level: Level,
}

/// One thing the user should know, with a plain sentence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    /// What it is about.
    pub kind: AlertKind,
    /// How bad.
    pub level: Level,
    /// The sentence for the user (English until the app has localisation).
    pub text: String,
    /// The output, for a lag alert.
    pub output: Option<OutputId>,
}

/// Everything the guard knows now: what `protection.status` reports.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct GuardStatus {
    /// Per volume of a running recording.
    pub disks: Vec<DiskState>,
    /// Per running output.
    pub outputs: Vec<OutputState>,
    /// The render worker.
    pub render: Option<Level>,
    /// The audio worker.
    pub audio: Option<Level>,
    /// System memory.
    pub memory: Option<Level>,
    /// Available memory at the last reading.
    pub available_memory: Option<u64>,
    /// Every state at `notice` or above, most severe first.
    pub alerts: Vec<Alert>,
}

impl GuardStatus {
    /// The most severe level of all; `Unavailable` never raises it above `Ok`.
    pub fn level(&self) -> Level {
        self.alerts
            .first()
            .map_or(Level::Ok, |a| a.level)
            .max(Level::Ok)
    }
}

/// A recording the guard asks to end cleanly, with the reason for its summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopRequest {
    /// The recording.
    pub output: OutputId,
    /// Why, for the user.
    pub reason: String,
}

/// Frames an output lost so far: video frames dropped before or at the encoder, and,
/// for a stream, items the transport dropped (ADR-0090 decision 4).
pub fn frames_lost(p: &Progress, stream: bool) -> u64 {
    let mut lost = p
        .export_drops
        .saturating_add(p.encoder_skipped)
        .saturating_add(p.encoder_drops);
    if stream {
        lost = lost.saturating_add(p.transport_drops);
    }
    lost
}

/// Rate of a cumulative counter, with the hysteresis of decision 4.
#[derive(Debug, Clone, Default)]
struct LagMeter {
    last: Option<(Timestamp, u64)>,
    over_for: Duration,
    under_for: Duration,
    lagging: bool,
}

impl LagMeter {
    fn step(&mut self, now: Timestamp, count: u64, s: &ProtectionSettings) {
        let Some((then, before)) = self.last.replace((now, count)) else {
            return;
        };
        let elapsed = now.saturating_since(then);
        if elapsed.is_zero() {
            return;
        }
        let delta = count.saturating_sub(before);
        // Per second, without floats: delta / secs > limit.
        let over = u128::from(delta) * 1_000_000_000 > u128::from(s.lag_limit) * elapsed.as_nanos();
        let hold = Duration::from_secs(u64::from(s.lag_seconds));
        if over {
            self.over_for += elapsed;
            self.under_for = Duration::ZERO;
            if self.over_for >= hold {
                self.lagging = true;
            }
        } else {
            self.under_for += elapsed;
            self.over_for = Duration::ZERO;
            if self.under_for >= hold {
                self.lagging = false;
            }
        }
    }

    fn level(&self) -> Level {
        if self.lagging {
            Level::Warning
        } else {
            Level::Ok
        }
    }
}

#[derive(Debug, Clone)]
struct Volume {
    /// Free-space readings inside the rate window, oldest first.
    history: VecDeque<(Timestamp, u64)>,
    level: Level,
    seconds_left: Option<u64>,
}

/// The guard (ADR-0090 decision 2). One per take; see the module text.
#[derive(Debug, Clone)]
pub struct Guard {
    settings: ProtectionSettings,
    volumes: BTreeMap<VolumeKey, Volume>,
    outputs: BTreeMap<OutputId, (bool, LagMeter)>,
    render: LagMeter,
    audio: LagMeter,
    memory: Option<(Level, Option<u64>)>,
    probe_lost: bool,
    asked_to_stop: BTreeSet<OutputId>,
    status: GuardStatus,
}

/// Readings needed in the window before a rate is trusted.
const MIN_READINGS: usize = 3;

impl Guard {
    /// A guard with `settings` and nothing seen yet.
    pub fn new(settings: ProtectionSettings) -> Self {
        Self {
            settings,
            volumes: BTreeMap::new(),
            outputs: BTreeMap::new(),
            render: LagMeter::default(),
            audio: LagMeter::default(),
            memory: None,
            probe_lost: false,
            asked_to_stop: BTreeSet::new(),
            status: GuardStatus::default(),
        }
    }

    /// The thresholds in use.
    pub fn settings(&self) -> &ProtectionSettings {
        &self.settings
    }

    /// New thresholds; they apply from the next step (ADR-0090 decision 9).
    pub fn set_settings(&mut self, settings: ProtectionSettings) {
        self.settings = settings;
    }

    /// The state after the last step.
    pub fn status(&self) -> &GuardStatus {
        &self.status
    }

    /// One step at `now`: takes `sample`, updates every level and returns the
    /// recordings to end cleanly now. A recording is asked once.
    pub fn step(&mut self, now: Timestamp, sample: &Sample) -> Vec<StopRequest> {
        self.step_outputs(now, sample);
        self.render
            .step(now, sample.late_render_ticks, &self.settings);
        self.audio
            .step(now, sample.late_audio_ticks, &self.settings);
        match &sample.probe {
            Some(ProbeStep::Reading { volumes, memory }) => {
                self.probe_lost = false;
                for v in volumes {
                    self.step_volume(now, v);
                }
                self.step_memory(*memory);
            }
            Some(ProbeStep::TimedOut) => self.probe_lost = true,
            None => {}
        }
        // Volumes no running recording uses any more are forgotten.
        let used: BTreeSet<VolumeKey> = sample.outputs.iter().filter_map(|o| o.volume).collect();
        self.volumes.retain(|k, _| used.contains(k));
        let stops = self.stops(sample);
        self.status = self.build_status();
        stops
    }

    fn step_outputs(&mut self, now: Timestamp, sample: &Sample) {
        self.outputs
            .retain(|id, _| sample.outputs.iter().any(|o| o.output == *id));
        self.asked_to_stop
            .retain(|id| sample.outputs.iter().any(|o| o.output == *id));
        for o in &sample.outputs {
            let (_, meter) = self
                .outputs
                .entry(o.output)
                .or_insert_with(|| (o.stream, LagMeter::default()));
            meter.step(now, frames_lost(&o.counters, o.stream), &self.settings);
        }
    }

    fn step_volume(&mut self, now: Timestamp, v: &VolumeSample) {
        let s = &self.settings;
        let vol = self.volumes.entry(v.volume).or_insert_with(|| Volume {
            history: VecDeque::new(),
            level: Level::Ok,
            seconds_left: None,
        });
        let Probed::Value(free) = v.free_bytes else {
            vol.level = Level::Unavailable;
            vol.seconds_left = None;
            return;
        };
        vol.history.push_back((now, free));
        while vol
            .history
            .front()
            .is_some_and(|(t, _)| now.saturating_since(*t) > s.rate_window)
        {
            vol.history.pop_front();
        }
        vol.seconds_left = seconds_left(&vol.history, s.reserve_bytes);
        vol.level = disk_level(vol.level, free, vol.seconds_left, s);
    }

    fn step_memory(&mut self, memory: Probed<u64>) {
        let s = &self.settings;
        let prev = self.memory.map_or(Level::Ok, |(l, _)| l);
        self.memory = Some(match memory {
            Probed::Value(avail) => {
                let level = if avail < s.min_memory_bytes
                    || (prev == Level::Warning && avail < margin(s.min_memory_bytes))
                {
                    Level::Warning
                } else {
                    Level::Ok
                };
                (level, Some(avail))
            }
            Probed::Unknown | Probed::Unsupported => (Level::Unavailable, None),
        });
    }

    fn stops(&mut self, sample: &Sample) -> Vec<StopRequest> {
        if self.settings.at_critical != AtCritical::StopCleanly {
            return Vec::new();
        }
        let mut out = Vec::new();
        for o in &sample.outputs {
            let Some(key) = o.volume.filter(|_| !o.stream) else {
                continue;
            };
            let Some(vol) = self.volumes.get(&key) else {
                continue;
            };
            if vol.level == Level::Critical && self.asked_to_stop.insert(o.output) {
                let free = vol.history.back().map_or(0, |(_, f)| *f);
                out.push(StopRequest {
                    output: o.output,
                    reason: format!(
                        "Recording stopped before the disk was full: {} left on the disk.",
                        gib(free)
                    ),
                });
            }
        }
        out
    }

    fn build_status(&self) -> GuardStatus {
        let s = &self.settings;
        let mut alerts = Vec::new();
        let disks: Vec<DiskState> = self
            .volumes
            .iter()
            .map(|(k, v)| {
                let free = v.history.back().map(|(_, f)| *f);
                let level = if self.probe_lost {
                    Level::Unavailable
                } else {
                    v.level
                };
                if let Some(text) = disk_text(level, free, v.seconds_left, s.at_critical) {
                    alerts.push(Alert {
                        kind: AlertKind::Disk,
                        level,
                        text,
                        output: None,
                    });
                }
                DiskState {
                    volume: *k,
                    level,
                    free_bytes: free.filter(|_| !self.probe_lost),
                    seconds_left: v.seconds_left.filter(|_| !self.probe_lost),
                }
            })
            .collect();
        let outputs = self
            .outputs
            .iter()
            .map(|(id, (stream, m))| {
                let level = m.level();
                if level >= Level::Notice {
                    alerts.push(Alert {
                        kind: AlertKind::Lag,
                        level,
                        text: if *stream {
                            "The stream is losing frames: the encoder or the connection cannot keep up."
                        } else {
                            "The recording is losing frames: the encoder or the disk cannot keep up."
                        }
                        .to_owned(),
                        output: Some(*id),
                    });
                }
                OutputState {
                    output: *id,
                    level,
                }
            })
            .collect();
        let render = self.render.level();
        if render >= Level::Notice {
            alerts.push(Alert {
                kind: AlertKind::Render,
                level: render,
                text: "The picture is late: the computer cannot render every frame.".to_owned(),
                output: None,
            });
        }
        let audio = self.audio.level();
        if audio >= Level::Notice {
            alerts.push(Alert {
                kind: AlertKind::Audio,
                level: audio,
                text: "The sound is late: the computer cannot mix every block in time.".to_owned(),
                output: None,
            });
        }
        let (memory, available_memory) = match self.memory {
            _ if self.probe_lost => (Some(Level::Unavailable), None),
            Some((l, a)) => (Some(l), a),
            None => (None, None),
        };
        if memory == Some(Level::Warning) {
            alerts.push(Alert {
                kind: AlertKind::Memory,
                level: Level::Warning,
                text: format!(
                    "Memory is low: {} available.",
                    gib(available_memory.unwrap_or(0))
                ),
                output: None,
            });
        }
        // Most severe first; equal levels keep the order disk, lag, render, audio, memory.
        alerts.sort_by_key(|a| std::cmp::Reverse(a.level));
        GuardStatus {
            disks,
            outputs,
            render: Some(render),
            audio: Some(audio),
            memory,
            available_memory,
            alerts,
        }
    }
}

/// `x` plus 10 %: the margin a level needs to fall back (decisions 3 and 6).
fn margin(x: u64) -> u64 {
    x.saturating_add(x / 10)
}

/// Seconds until `reserve` at the rate free space fell over `history`.
fn seconds_left(history: &VecDeque<(Timestamp, u64)>, reserve: u64) -> Option<u64> {
    if history.len() < MIN_READINGS {
        return None;
    }
    let (t0, f0) = *history.front()?;
    let (t1, f1) = *history.back()?;
    let span = t1.saturating_since(t0).as_nanos();
    let fell = u128::from(f0.saturating_sub(f1));
    if span == 0 || fell == 0 {
        return None;
    }
    let above = u128::from(f1.saturating_sub(reserve));
    // above / (fell / span) in seconds.
    let secs = above * span / fell / 1_000_000_000;
    Some(u64::try_from(secs).unwrap_or(u64::MAX))
}

fn disk_level(prev: Level, free: u64, left: Option<u64>, s: &ProtectionSettings) -> Level {
    if free <= s.reserve_bytes || (prev == Level::Critical && free <= margin(s.reserve_bytes)) {
        return Level::Critical;
    }
    let below = |minutes: u32, level: Level| {
        let limit = u64::from(minutes) * 60;
        left.is_some_and(|l| l < limit || (prev >= level && l < margin_secs(limit)))
    };
    if below(s.warning_minutes, Level::Warning) {
        Level::Warning
    } else if below(s.notice_minutes, Level::Notice) {
        Level::Notice
    } else {
        Level::Ok
    }
}

/// `secs` plus 20 % (decision 3: a level falls back with 20 % more time left).
fn margin_secs(secs: u64) -> u64 {
    secs.saturating_add(secs / 5)
}

fn gib(bytes: u64) -> String {
    let tenths = bytes.saturating_mul(10) / GIB;
    format!("{}.{} GiB", tenths / 10, tenths % 10)
}

fn minutes(secs: u64) -> u64 {
    secs.div_ceil(60).max(1)
}

fn disk_text(level: Level, free: Option<u64>, left: Option<u64>, at: AtCritical) -> Option<String> {
    let free = gib(free.unwrap_or(0));
    Some(match level {
        Level::Ok => return None,
        Level::Unavailable => "Free disk space could not be read.".to_owned(),
        Level::Notice => format!(
            "About {} minutes of disk space left for the recording.",
            minutes(left.unwrap_or(0))
        ),
        Level::Warning => match at {
            AtCritical::StopCleanly => format!(
                "Recording will stop in about {} minutes: the disk is almost full.",
                minutes(left.unwrap_or(0))
            ),
            AtCritical::WarnOnly => format!(
                "The disk will be full in about {} minutes.",
                minutes(left.unwrap_or(0))
            ),
        },
        Level::Critical => match at {
            AtCritical::StopCleanly => {
                format!("Recording stopped before the disk was full: {free} left on the disk.")
            }
            AtCritical::WarnOnly => format!("The disk is almost full: {free} left."),
        },
    })
}

#[cfg(test)]
mod tests;
