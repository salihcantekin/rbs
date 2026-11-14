//! The cost of a start before it runs (ADR-0087 decisions 1 to 3, spec
//! `docs/specs/output-estimate.md`).
//!
//! [`estimate`] is a pure function of an [`EstimatePlan`] (what a start would run,
//! built by the studio host from the output profile and the collection), the
//! [`Limits`] known for this computer (declared capabilities, the last calibration,
//! the disk probe and the render partition's previous-tick time) and nothing else: no
//! clock read, no OS call, no thread. It answers per resource a need, a limit or
//! *unknown*, a [`Level`], and how many separate video and audio tracks fit
//! ([`MaxTracks`]). Nothing here runs unless the caller asks (P13).

mod probe;

pub use probe::{CACHE_FOR, DiskProbe, FreeSpace, PROBE_TIMEOUT, ReadFreeSpace};

use std::time::Duration;

use rbs_scene::model::SourceId;

use crate::iso::{IsoRole, MAX_ISO_AUDIO, MAX_ISO_VIDEO};

/// A rate resource is `heavy` from this share of its limit (ADR-0087 decision 2).
pub const HEAVY_SHARE: f64 = 0.7;

/// The duration the disk space row plans for unless the profile says otherwise
/// (ADR-0087 decision 1).
pub const DEFAULT_PLANNED_DURATION: Duration = Duration::from_secs(3600);

/// Separate video files allowed while what an encoder can take is not known: a
/// hardware encoder's session limit neither measured nor declared, or a software
/// encoder's load not measured (owner's choice, 2026-11-14, ADR-0087 Review). Three
/// 1080p60 files kept the master whole on an M4 Pro; four did not.
pub const UNKNOWN_SESSIONS_VIDEO_LIMIT: usize = 3;

/// How many separate video files a start may open, from its estimate: the
/// estimate's "at most N", and at most [`UNKNOWN_SESSIONS_VIDEO_LIMIT`] while
/// `encoder_sessions` or `cpu_load` is unknown (ADR-0087 Review, 2026-11-14).
pub fn separate_video_limit(estimate: &Estimate) -> usize {
    let max = &estimate.max_video;
    let count = max.count.map(|c| c as usize);
    if max.unknown.contains(&Resource::EncoderSessions) || max.unknown.contains(&Resource::CpuLoad)
    {
        count.map_or(UNKNOWN_SESSIONS_VIDEO_LIMIT, |c| {
            c.min(UNKNOWN_SESSIONS_VIDEO_LIMIT)
        })
    } else {
        count.unwrap_or(UNKNOWN_SESSIONS_VIDEO_LIMIT)
    }
}

/// What a row measures.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Resource {
    /// Concurrent encoder sessions on one adapter.
    EncoderSessions,
    /// Pixels per second through one hardware adapter.
    EncoderThroughput,
    /// The busiest ISO encode worker's share of a frame interval.
    IsoWorkers,
    /// The render partition's tick time with the ISO exports, against the share of
    /// the tick the exports may use (ADR-0086 decision 9).
    IsoRenderShare,
    /// Software video encoding against the calibrated software throughput.
    CpuLoad,
    /// Pooled export buffers of the ISO video tracks. Informational.
    Memory,
    /// Bytes per second written against the folder's measured write speed.
    DiskSpeed,
    /// Bytes for the planned duration against the folder's free space.
    DiskSpace,
    /// The ISO track ceilings ([`MAX_ISO_VIDEO`], [`MAX_ISO_AUDIO`]).
    IsoCeiling,
    /// Sources kept active for the take. Informational.
    ActiveSources,
}

impl Resource {
    /// The control API name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::EncoderSessions => "encoder_sessions",
            Self::EncoderThroughput => "encoder_throughput",
            Self::IsoWorkers => "iso_workers",
            Self::IsoRenderShare => "iso_render_share",
            Self::CpuLoad => "cpu_load",
            Self::Memory => "memory",
            Self::DiskSpeed => "disk_speed",
            Self::DiskSpace => "disk_space",
            Self::IsoCeiling => "iso_ceiling",
            Self::ActiveSources => "active_sources",
        }
    }

    /// Whether *Test this computer* can measure this resource, so an unknown limit
    /// comes with that advice (ADR-0087 decision 2).
    pub const fn calibration_measures(self) -> bool {
        matches!(
            self,
            Self::EncoderSessions
                | Self::EncoderThroughput
                | Self::IsoWorkers
                | Self::CpuLoad
                | Self::DiskSpeed
        )
    }
}

/// The unit of a row's need and limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit {
    /// Encoder sessions.
    Sessions,
    /// Pixels per second.
    PixelsPerSecond,
    /// A share: 1.0 is the whole frame interval or tick.
    Ratio,
    /// Bytes per second.
    BytesPerSecond,
    /// Bytes.
    Bytes,
    /// Tracks.
    Tracks,
    /// Sources.
    Sources,
}

impl Unit {
    /// The control API name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sessions => "sessions",
            Self::PixelsPerSecond => "pixels_per_second",
            Self::Ratio => "ratio",
            Self::BytesPerSecond => "bytes_per_second",
            Self::Bytes => "bytes",
            Self::Tracks => "tracks",
            Self::Sources => "sources",
        }
    }
}

/// How a need compares with its limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    /// Fits with room.
    Fits,
    /// Fits, near the limit, or a track falls to a CPU encoder.
    Heavy,
    /// Does not fit.
    Over,
    /// The limit is not known.
    Unknown,
}

impl Level {
    /// The control API name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fits => "fits",
            Self::Heavy => "heavy",
            Self::Over => "over",
            Self::Unknown => "unknown",
        }
    }

    const fn rank(self) -> u8 {
        match self {
            Self::Fits | Self::Unknown => 0,
            Self::Heavy => 1,
            Self::Over => 2,
        }
    }
}

/// Why a row is not `fits`, as a closed code clients localise (ADR-0087 decision 7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReasonCode {
    /// A limit is not known and no calibration measures it (or none ran yet).
    Unknown,
    /// No adapter can encode a track.
    NoAdapter,
    /// A file could not open.
    OpenFailed,
    /// More encoder sessions than the adapter allows, or near it.
    Sessions,
    /// A hardware adapter does not report its session limit.
    SessionsUnknown,
    /// Encoder throughput is near or over the limit.
    Throughput,
    /// A track would fall to a CPU encoder.
    SoftwareFallback,
    /// Software encoding is near or over the calibrated CPU throughput.
    CpuLoad,
    /// An ISO encode worker is near or over its frame interval.
    Workers,
    /// The render partition has no room for the ISO exports.
    RenderShare,
    /// Free space or write speed of the folder is not known.
    DiskUnknown,
    /// The planned duration does not fit on the disk.
    DiskFull,
    /// The folder writes slower than the take needs, or near it.
    DiskSlow,
    /// More tracks than the ISO ceiling.
    Ceiling,
    /// A marked source cannot be kept active.
    SourceInactive,
}

impl ReasonCode {
    /// The control API name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::NoAdapter => "no_adapter",
            Self::OpenFailed => "open_failed",
            Self::Sessions => "sessions",
            Self::SessionsUnknown => "sessions_unknown",
            Self::Throughput => "throughput",
            Self::SoftwareFallback => "software_fallback",
            Self::CpuLoad => "cpu_load",
            Self::Workers => "workers",
            Self::RenderShare => "render_share",
            Self::DiskUnknown => "disk_unknown",
            Self::DiskFull => "disk_full",
            Self::DiskSlow => "disk_slow",
            Self::Ceiling => "ceiling",
            Self::SourceInactive => "source_inactive",
        }
    }
}

/// One separate track of the plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrackRef {
    /// The marked source.
    pub source: SourceId,
    /// Picture, sound or images.
    pub role: IsoRole,
}

/// Who uses an encoder session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Use {
    /// The main recording (or a recording of another canvas).
    Recording,
    /// A stream.
    Stream,
    /// A separate track (ADR-0086).
    Iso(TrackRef),
}

/// Size class of a calibration entry: the smallest of 720, 1080, 1440 and 2160 lines
/// at or above the frame's height (ADR-0087 decision 4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SizeClass {
    /// Up to 720 lines.
    L720,
    /// Up to 1080 lines.
    L1080,
    /// Up to 1440 lines.
    L1440,
    /// Up to 2160 lines.
    L2160,
    /// Above 2160 lines.
    Above2160,
}

impl SizeClass {
    /// The class of a frame `height` lines high.
    pub const fn of(height: u32) -> Self {
        match height {
            0..=720 => Self::L720,
            721..=1080 => Self::L1080,
            1081..=1440 => Self::L1440,
            1441..=2160 => Self::L2160,
            _ => Self::Above2160,
        }
    }

    /// The `calibration.toml` name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::L720 => "720",
            Self::L1080 => "1080",
            Self::L1440 => "1440",
            Self::L2160 => "2160",
            Self::Above2160 => "above-2160",
        }
    }

    /// The class named `s` ([`SizeClass::as_str`]).
    pub fn parse(s: &str) -> Option<Self> {
        [
            Self::L720,
            Self::L1080,
            Self::L1440,
            Self::L2160,
            Self::Above2160,
        ]
        .into_iter()
        .find(|c| c.as_str() == s)
    }
}

/// Frame-rate class of a calibration entry: 30, 60 or above 60, at or below.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FpsClass {
    /// Up to 30 frames per second.
    F30,
    /// Up to 60 frames per second.
    F60,
    /// Above 60 frames per second.
    Above60,
}

impl FpsClass {
    /// The class of `fps` frames per second.
    pub fn of(fps: f64) -> Self {
        if fps <= 30.0 {
            Self::F30
        } else if fps <= 60.0 {
            Self::F60
        } else {
            Self::Above60
        }
    }

    /// The `calibration.toml` name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::F30 => "30",
            Self::F60 => "60",
            Self::Above60 => "above-60",
        }
    }

    /// The class named `s` ([`FpsClass::as_str`]).
    pub fn parse(s: &str) -> Option<Self> {
        [Self::F30, Self::F60, Self::Above60]
            .into_iter()
            .find(|c| c.as_str() == s)
    }
}

/// Rate-control class of a calibration entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RateClass {
    /// Constant bit rate.
    Cbr,
    /// Variable bit rate.
    Vbr,
    /// Constant quality (QP or CRF).
    Cq,
}

impl RateClass {
    /// The `calibration.toml` name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cbr => "cbr",
            Self::Vbr => "vbr",
            Self::Cq => "cq",
        }
    }

    /// The class named `s` ([`RateClass::as_str`]).
    pub fn parse(s: &str) -> Option<Self> {
        [Self::Cbr, Self::Vbr, Self::Cq]
            .into_iter()
            .find(|c| c.as_str() == s)
    }
}

/// One video encoder session the start would run. Encoders shared by several outputs
/// (ADR-0047) are one session: the caller lists them once.
#[derive(Clone, Debug, PartialEq)]
pub struct VideoEncode {
    /// Who uses it.
    pub used_by: Use,
    /// The adapter id the selection would pick.
    pub adapter: String,
    /// Whether that adapter is hardware (`adapter.hardware`).
    pub hardware: bool,
    /// The codec name.
    pub codec: String,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Frames per second.
    pub fps: f64,
    /// Rate control.
    pub rate: RateClass,
    /// Bytes per second written (the bit rate over 8; for constant quality, the
    /// caller's best guess).
    pub bytes_per_second: u64,
    /// The plan wants a hardware encoder for this codec, every hardware candidate is
    /// exhausted or refuses, and a software one would be used (ADR-0087 decision 2).
    pub falls_to_cpu: bool,
    /// The ISO encode worker it runs on, for separate tracks.
    pub worker: Option<usize>,
    /// Pooled export buffers it holds (separate tracks), for the memory row.
    pub export_buffers: u32,
}

impl VideoEncode {
    fn pixel_rate(&self) -> f64 {
        f64::from(self.width) * f64::from(self.height) * self.fps
    }

    fn class(&self) -> (SizeClass, FpsClass, RateClass) {
        (
            SizeClass::of(self.height),
            FpsClass::of(self.fps),
            self.rate,
        )
    }

    fn frame_bytes(&self) -> u64 {
        // RGBA 8-bit export frames (ADR-0086 decision 9).
        u64::from(self.width) * u64::from(self.height) * 4
    }
}

/// One audio encoder the start would run.
#[derive(Clone, Debug, PartialEq)]
pub struct AudioEncode {
    /// Who uses it.
    pub used_by: Use,
    /// Bytes per second written.
    pub bytes_per_second: u64,
}

/// What a start would run (ADR-0087 decision 1). Built by the studio host from the
/// output profile, the canvases and the collection, never from a caller's
/// description. Outputs already running are listed too.
#[derive(Clone, Debug, PartialEq)]
pub struct EstimatePlan {
    /// Video encoder sessions, each once.
    pub video: Vec<VideoEncode>,
    /// Audio encoders.
    pub audio: Vec<AudioEncode>,
    /// Image tracks (PNG, ADR-0117): listed, not counted against encoders.
    pub images: Vec<TrackRef>,
    /// Sources kept active for the take (ADR-0086 decision 8).
    pub active_sources: u32,
    /// Marked sources that cannot be kept active: a hard limit.
    pub inactive: Vec<TrackRef>,
    /// The shape of one more separate video track, for [`MaxTracks`]; `used_by` is a
    /// [`Use::Iso`]. `None` when the recording has no picture.
    pub video_template: Option<VideoEncode>,
    /// The shape of one more separate audio track, for [`MaxTracks`]; `used_by` is a
    /// [`Use::Iso`]. `None` when the recording has no sound.
    pub audio_template: Option<AudioEncode>,
    /// ISO encode workers the separate video tracks share.
    pub iso_workers: usize,
    /// The canvas tick interval.
    pub tick: Duration,
    /// The share of the tick the ISO exports may use (ADR-0086 decision 9).
    pub give_way_share: f64,
    /// The duration the disk space row plans for.
    pub planned_duration: Duration,
}

/// What an adapter declares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeclaredAdapter {
    /// Adapter id.
    pub id: String,
    /// `adapter.hardware`.
    pub hardware: bool,
    /// `codec.sessions.max`; absent on a hardware adapter is unknown, on a software
    /// one unlimited.
    pub sessions_max: Option<u32>,
    /// `codec.video.pixel_rate.max`.
    pub pixel_rate_max: Option<u64>,
}

/// One calibration result for an encoder (ADR-0087 decision 4).
#[derive(Clone, Debug, PartialEq)]
pub struct EncoderMeasure {
    /// Adapter id.
    pub adapter: String,
    /// Codec name.
    pub codec: String,
    /// Size class.
    pub size: SizeClass,
    /// Frame-rate class.
    pub fps: FpsClass,
    /// Rate-control class.
    pub rate: RateClass,
    /// Highest session count that ran with no late frame.
    pub sessions: u32,
    /// Highest pixel rate that ran with no late frame.
    pub pixels_per_second: u64,
    /// Encode time of one frame with one session.
    pub frame_time: Duration,
}

/// What the last calibration measured, already filtered to this computer's adapters
/// and versions by the reader of `calibration.toml`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Calibration {
    /// Encoder results.
    pub encoders: Vec<EncoderMeasure>,
    /// ISO export time per frame per size class.
    pub exports: Vec<(SizeClass, Duration)>,
    /// The recording folder's write speed in bytes per second.
    pub write_speed: Option<u64>,
}

impl Calibration {
    fn encoder(&self, e: &VideoEncode) -> Option<&EncoderMeasure> {
        let (size, fps, rate) = e.class();
        self.encoders.iter().find(|m| {
            m.adapter == e.adapter
                && m.codec == e.codec
                && m.size == size
                && m.fps == fps
                && m.rate == rate
        })
    }

    fn export(&self, height: u32) -> Option<Duration> {
        let size = SizeClass::of(height);
        self.exports
            .iter()
            .find(|(s, _)| *s == size)
            .map(|(_, t)| *t)
    }
}

/// What is known about this computer when the estimate runs.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Limits {
    /// Declared capabilities of the adapters the plan uses.
    pub adapters: Vec<DeclaredAdapter>,
    /// The last calibration, if any.
    pub calibration: Option<Calibration>,
    /// Free space of the recording folder from the cached probe; `None` is unknown.
    pub free_space: Option<u64>,
    /// The median of the render partition's last previous-tick times; `None` until
    /// it is published (ADR-0087 decision 1).
    pub previous_tick: Option<Duration>,
}

/// One resource of the answer.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    /// What it measures.
    pub resource: Resource,
    /// The adapter id for per-adapter rows, `"video"` or `"audio"` for the ceilings.
    pub scope: Option<String>,
    /// What the plan needs.
    pub need: f64,
    /// The limit, `None` when unknown or unlimited (see `level`).
    pub limit: Option<f64>,
    /// Unit of `need` and `limit`.
    pub unit: Unit,
    /// `None` for informational rows (`memory`, `active_sources`).
    pub level: Option<Level>,
    /// The limit was measured on this computer, not declared.
    pub measured: bool,
}

/// One reason a row is not `fits`.
#[derive(Clone, Debug, PartialEq)]
pub struct Reason {
    /// The code.
    pub code: ReasonCode,
    /// The resource it is about.
    pub resource: Option<Resource>,
    /// The track it is about.
    pub track: Option<TrackRef>,
}

/// How many tracks of one kind fit (ADR-0087 decision 3).
#[derive(Clone, Debug, PartialEq)]
pub struct MaxTracks {
    /// Total tracks of that kind that fit before a known resource is `over`; an upper
    /// bound when `partial`; `None` when no resource that grows with the tracks is
    /// known.
    pub count: Option<u32>,
    /// The resource that sets the count.
    pub limit: Option<Resource>,
    /// Some resources that grow with the tracks are unknown.
    pub partial: bool,
    /// Those resources.
    pub unknown: Vec<Resource>,
}

/// What one separate track adds.
#[derive(Clone, Debug, PartialEq)]
pub struct TrackCost {
    /// The track.
    pub track: TrackRef,
    /// Need per resource, in each row's unit.
    pub cost: Vec<(Resource, f64)>,
}

/// The answer (ADR-0087 decision 1).
#[derive(Clone, Debug, PartialEq)]
pub struct Estimate {
    /// The worst row level (`fits`, `heavy` or `over`).
    pub level: Level,
    /// Resources whose limit is unknown, reported beside `level`.
    pub unknown: Vec<Resource>,
    /// The rows.
    pub resources: Vec<Row>,
    /// Why rows are not `fits`.
    pub reasons: Vec<Reason>,
    /// Separate video tracks that fit.
    pub max_video: MaxTracks,
    /// Separate audio tracks that fit.
    pub max_audio: MaxTracks,
    /// What each separate track adds.
    pub tracks: Vec<TrackCost>,
}

/// The level of a count resource.
fn count_level(need: f64, limit: f64) -> Level {
    if need <= limit {
        Level::Fits
    } else {
        Level::Over
    }
}

/// The level of a rate resource.
fn rate_level(need: f64, limit: f64) -> Level {
    if need > limit {
        Level::Over
    } else if need >= HEAVY_SHARE * limit {
        Level::Heavy
    } else {
        Level::Fits
    }
}

fn iso_video_count(plan: &EstimatePlan) -> usize {
    plan.video
        .iter()
        .filter(|e| matches!(e.used_by, Use::Iso(_)))
        .count()
}

fn iso_audio_count(plan: &EstimatePlan) -> usize {
    plan.audio
        .iter()
        .filter(|e| matches!(e.used_by, Use::Iso(_)))
        .count()
}

struct Rows {
    rows: Vec<Row>,
    reasons: Vec<Reason>,
}

impl Rows {
    fn push(&mut self, row: Row, reason: Option<ReasonCode>) {
        let resource = row.resource;
        match row.level {
            Some(Level::Unknown) => {
                let code = reason.unwrap_or(ReasonCode::Unknown);
                self.reasons.push(Reason {
                    code,
                    resource: Some(resource),
                    track: None,
                });
            }
            Some(Level::Heavy | Level::Over) => {
                if let Some(code) = reason {
                    self.reasons.push(Reason {
                        code,
                        resource: Some(resource),
                        track: None,
                    });
                }
            }
            _ => {}
        }
        self.rows.push(row);
    }
}

/// The adapters the plan uses, in first-use order.
fn adapters_of(plan: &EstimatePlan) -> Vec<&str> {
    let mut ids: Vec<&str> = Vec::new();
    for e in &plan.video {
        if !ids.contains(&e.adapter.as_str()) {
            ids.push(&e.adapter);
        }
    }
    ids
}

/// Pixel rate need, and the limit that holds the same share, of `encodes` against
/// per-session limits (`None` when one is unknown).
fn throughput<'a>(
    encodes: impl Iterator<Item = &'a VideoEncode>,
    limit_of: impl Fn(&VideoEncode) -> Option<(f64, bool)>,
) -> (f64, Option<f64>, bool) {
    let mut need = 0.0;
    let mut share = 0.0;
    let mut known = true;
    let mut measured = true;
    for e in encodes {
        let rate = e.pixel_rate();
        need += rate;
        match limit_of(e) {
            Some((limit, m)) if limit > 0.0 => {
                share += rate / limit;
                measured &= m;
            }
            _ => known = false,
        }
    }
    if !known {
        return (need, None, false);
    }
    // A single class gives its own limit; mixed classes give the limit at which the
    // same share is used.
    let limit = if share > 0.0 { need / share } else { 0.0 };
    (need, Some(limit), measured)
}

fn rows_of(plan: &EstimatePlan, limits: &Limits) -> Rows {
    let cal = limits.calibration.as_ref();
    let mut out = Rows {
        rows: Vec::new(),
        reasons: Vec::new(),
    };

    // Encoder sessions and hardware throughput, per adapter.
    for id in adapters_of(plan) {
        let encodes: Vec<&VideoEncode> = plan.video.iter().filter(|e| e.adapter == id).collect();
        let declared = limits.adapters.iter().find(|a| a.id == id);
        let hardware = encodes.iter().any(|e| e.hardware);
        let need = encodes.len() as f64;
        let measured_sessions = encodes
            .iter()
            .filter_map(|e| cal.and_then(|c| c.encoder(e)).map(|m| m.sessions))
            .min();
        let (limit, measured, level, reason) =
            match (measured_sessions, declared.and_then(|d| d.sessions_max)) {
                (Some(m), _) => {
                    let l = f64::from(m);
                    (Some(l), true, count_level(need, l), ReasonCode::Sessions)
                }
                (None, Some(d)) => {
                    let l = f64::from(d);
                    (Some(l), false, count_level(need, l), ReasonCode::Sessions)
                }
                (None, None) if hardware => {
                    (None, false, Level::Unknown, ReasonCode::SessionsUnknown)
                }
                (None, None) => (None, false, Level::Fits, ReasonCode::Sessions),
            };
        out.push(
            Row {
                resource: Resource::EncoderSessions,
                scope: Some(id.to_owned()),
                need,
                limit,
                unit: Unit::Sessions,
                level: Some(level),
                measured,
            },
            Some(reason),
        );

        if hardware {
            let declared_rate = declared.and_then(|d| d.pixel_rate_max).map(|r| r as f64);
            let (need, limit, measured) = throughput(encodes.iter().copied(), |e| {
                match cal.and_then(|c| c.encoder(e)) {
                    Some(m) => Some((m.pixels_per_second as f64, true)),
                    None => declared_rate.map(|r| (r, false)),
                }
            });
            let level = limit.map_or(Level::Unknown, |l| rate_level(need, l));
            out.push(
                Row {
                    resource: Resource::EncoderThroughput,
                    scope: Some(id.to_owned()),
                    need,
                    limit,
                    unit: Unit::PixelsPerSecond,
                    level: Some(level),
                    measured,
                },
                (level != Level::Unknown).then_some(ReasonCode::Throughput),
            );
        }
    }

    // CPU load of software video encoding.
    if plan.video.iter().any(|e| !e.hardware) {
        let (need, limit, _) = throughput(plan.video.iter().filter(|e| !e.hardware), |e| {
            cal.and_then(|c| c.encoder(e))
                .map(|m| (m.pixels_per_second as f64, true))
        });
        let level = limit.map_or(Level::Unknown, |l| rate_level(need, l));
        out.push(
            Row {
                resource: Resource::CpuLoad,
                scope: None,
                need,
                limit,
                unit: Unit::PixelsPerSecond,
                level: Some(level),
                measured: limit.is_some(),
            },
            (level != Level::Unknown).then_some(ReasonCode::CpuLoad),
        );
    }

    let iso: Vec<&VideoEncode> = plan
        .video
        .iter()
        .filter(|e| matches!(e.used_by, Use::Iso(_)))
        .collect();
    if !iso.is_empty() {
        // The busiest ISO encode worker.
        let mut busy = vec![0.0_f64; plan.iso_workers.max(1)];
        let mut known = true;
        for e in &iso {
            match cal.and_then(|c| c.encoder(e)) {
                Some(m) => {
                    let slot = e.worker.unwrap_or(0) % busy.len();
                    busy[slot] += m.frame_time.as_secs_f64() * e.fps;
                }
                None => known = false,
            }
        }
        let need = busy.iter().copied().fold(0.0, f64::max);
        let level = if known {
            rate_level(need, 1.0)
        } else {
            Level::Unknown
        };
        out.push(
            Row {
                resource: Resource::IsoWorkers,
                scope: None,
                need,
                limit: known.then_some(1.0),
                unit: Unit::Ratio,
                level: Some(level),
                measured: known,
            },
            known.then_some(ReasonCode::Workers),
        );

        // The render partition's share.
        let tick = plan.tick.as_secs_f64();
        let exports: Option<f64> = iso
            .iter()
            .map(|e| {
                cal.and_then(|c| c.export(e.height))
                    .map(|t| t.as_secs_f64())
            })
            .sum();
        let (need, level) = match (limits.previous_tick, exports) {
            (Some(prev), Some(ex)) if tick > 0.0 => {
                let need = (prev.as_secs_f64() + ex) / tick;
                (need, rate_level(need, plan.give_way_share))
            }
            (_, ex) if tick > 0.0 => (ex.unwrap_or(0.0) / tick, Level::Unknown),
            _ => (0.0, Level::Unknown),
        };
        out.push(
            Row {
                resource: Resource::IsoRenderShare,
                scope: None,
                need,
                limit: (level != Level::Unknown).then_some(plan.give_way_share),
                unit: Unit::Ratio,
                level: Some(level),
                measured: level != Level::Unknown,
            },
            (level != Level::Unknown).then_some(ReasonCode::RenderShare),
        );

        // Export memory, informational.
        let bytes: u64 = iso
            .iter()
            .map(|e| e.frame_bytes() * u64::from(e.export_buffers))
            .sum();
        out.push(
            Row {
                resource: Resource::Memory,
                scope: None,
                need: bytes as f64,
                limit: None,
                unit: Unit::Bytes,
                level: None,
                measured: false,
            },
            None,
        );
    }

    // Disk write rate and space.
    let rate: u64 = plan.video.iter().map(|e| e.bytes_per_second).sum::<u64>()
        + plan.audio.iter().map(|e| e.bytes_per_second).sum::<u64>();
    let rate = rate as f64;
    let speed = cal.and_then(|c| c.write_speed).map(|s| s as f64);
    let level = speed.map_or(Level::Unknown, |s| rate_level(rate, s));
    out.push(
        Row {
            resource: Resource::DiskSpeed,
            scope: None,
            need: rate,
            limit: speed,
            unit: Unit::BytesPerSecond,
            level: Some(level),
            measured: speed.is_some(),
        },
        Some(if level == Level::Unknown {
            ReasonCode::DiskUnknown
        } else {
            ReasonCode::DiskSlow
        }),
    );
    let space = rate * plan.planned_duration.as_secs_f64();
    let free = limits.free_space.map(|f| f as f64);
    // Planned against a guessed duration: never `over` here; a disk with no room for
    // the start itself is found when files open (ADR-0087 decision 2).
    let level = free.map_or(Level::Unknown, |f| match rate_level(space, f) {
        Level::Over => Level::Heavy,
        l => l,
    });
    out.push(
        Row {
            resource: Resource::DiskSpace,
            scope: None,
            need: space,
            limit: free,
            unit: Unit::Bytes,
            level: Some(level),
            measured: free.is_some(),
        },
        Some(if level == Level::Unknown {
            ReasonCode::DiskUnknown
        } else {
            ReasonCode::DiskFull
        }),
    );

    // The ISO ceilings.
    for (scope, need, limit) in [
        ("video", iso_video_count(plan), MAX_ISO_VIDEO),
        ("audio", iso_audio_count(plan), MAX_ISO_AUDIO),
    ] {
        let (need, limit) = (need as f64, limit as f64);
        out.push(
            Row {
                resource: Resource::IsoCeiling,
                scope: Some(scope.to_owned()),
                need,
                limit: Some(limit),
                unit: Unit::Tracks,
                level: Some(count_level(need, limit)),
                measured: false,
            },
            Some(ReasonCode::Ceiling),
        );
    }

    out.rows.push(Row {
        resource: Resource::ActiveSources,
        scope: None,
        need: f64::from(plan.active_sources),
        limit: None,
        unit: Unit::Sources,
        level: None,
        measured: false,
    });
    out
}

/// The worst level of `rows`, and the resources whose limit is unknown.
fn worst(rows: &[Row]) -> (Level, Vec<Resource>) {
    let mut level = Level::Fits;
    let mut unknown = Vec::new();
    for row in rows {
        match row.level {
            Some(Level::Unknown) => {
                if !unknown.contains(&row.resource) {
                    unknown.push(row.resource);
                }
            }
            Some(l) if l.rank() > level.rank() => level = l,
            _ => {}
        }
    }
    (level, unknown)
}

/// The resources that grow with the separate tracks of one kind.
fn grows_with(resource: Resource, video: bool) -> bool {
    match resource {
        Resource::DiskSpeed | Resource::IsoCeiling => true,
        Resource::EncoderSessions
        | Resource::EncoderThroughput
        | Resource::IsoWorkers
        | Resource::IsoRenderShare
        | Resource::CpuLoad => video,
        // Never `over` here, or informational.
        Resource::DiskSpace | Resource::Memory | Resource::ActiveSources => false,
    }
}

/// The plan with `n` separate tracks of one kind: the first of the plan's own, then
/// copies of the template.
fn with_tracks(plan: &EstimatePlan, n: usize, video: bool) -> Option<EstimatePlan> {
    let mut p = plan.clone();
    if video {
        let template = plan.video_template.as_ref()?;
        let mut kept = 0;
        p.video.retain(|e| {
            if matches!(e.used_by, Use::Iso(_)) {
                kept += 1;
                kept <= n
            } else {
                true
            }
        });
        for i in kept.min(n)..n {
            let mut e = template.clone();
            e.worker = Some(i % plan.iso_workers.max(1));
            p.video.push(e);
        }
    } else {
        let template = plan.audio_template.as_ref()?;
        let mut kept = 0;
        p.audio.retain(|e| {
            if matches!(e.used_by, Use::Iso(_)) {
                kept += 1;
                kept <= n
            } else {
                true
            }
        });
        for _ in kept.min(n)..n {
            p.audio.push(template.clone());
        }
    }
    Some(p)
}

fn max_tracks(plan: &EstimatePlan, limits: &Limits, video: bool) -> MaxTracks {
    let ceiling = if video { MAX_ISO_VIDEO } else { MAX_ISO_AUDIO };
    let mut unknown: Vec<Resource> = Vec::new();
    let mut known_growing = false;
    let mut result = None;
    for n in 0..=ceiling + 1 {
        let Some(p) = with_tracks(plan, n, video) else {
            return MaxTracks {
                count: Some(0),
                limit: None,
                partial: false,
                unknown: Vec::new(),
            };
        };
        let rows = rows_of(&p, limits).rows;
        let over = rows.iter().find(|r| {
            grows_with(r.resource, video)
                && r.level == Some(Level::Over)
                && (r.resource != Resource::IsoCeiling
                    || r.scope.as_deref() == Some(if video { "video" } else { "audio" }))
        });
        for r in &rows {
            if !grows_with(r.resource, video) || r.resource == Resource::IsoCeiling {
                continue;
            }
            if r.level == Some(Level::Unknown) {
                if !unknown.contains(&r.resource) {
                    unknown.push(r.resource);
                }
            } else if n > 0 {
                known_growing = true;
            }
        }
        if let Some(r) = over {
            result = Some((n.saturating_sub(1), r.resource));
            break;
        }
    }
    let (count, limit) = result.unwrap_or((ceiling, Resource::IsoCeiling));
    let count = u32::try_from(count).unwrap_or(u32::MAX);
    let partial = !unknown.is_empty();
    if !known_growing && limit == Resource::IsoCeiling {
        return MaxTracks {
            count: None,
            limit: None,
            partial,
            unknown,
        };
    }
    MaxTracks {
        count: Some(count),
        limit: Some(limit),
        partial,
        unknown,
    }
}

fn track_costs(plan: &EstimatePlan, limits: &Limits) -> Vec<TrackCost> {
    let cal = limits.calibration.as_ref();
    let tick = plan.tick.as_secs_f64();
    let mut out = Vec::new();
    for e in &plan.video {
        let Use::Iso(track) = e.used_by else { continue };
        let mut cost = vec![
            (Resource::EncoderSessions, 1.0),
            (Resource::EncoderThroughput, e.pixel_rate()),
            (Resource::DiskSpeed, e.bytes_per_second as f64),
            (
                Resource::Memory,
                (e.frame_bytes() * u64::from(e.export_buffers)) as f64,
            ),
        ];
        if let Some(t) = cal.and_then(|c| c.export(e.height))
            && tick > 0.0
        {
            cost.push((Resource::IsoRenderShare, t.as_secs_f64() / tick));
        }
        out.push(TrackCost { track, cost });
    }
    for a in &plan.audio {
        let Use::Iso(track) = a.used_by else { continue };
        out.push(TrackCost {
            track,
            cost: vec![(Resource::DiskSpeed, a.bytes_per_second as f64)],
        });
    }
    out
}

/// The estimate of `plan` on this computer (ADR-0087 decisions 1 to 3).
pub fn estimate(plan: &EstimatePlan, limits: &Limits) -> Estimate {
    let Rows { rows, mut reasons } = rows_of(plan, limits);
    let (mut level, unknown) = worst(&rows);
    for e in plan.video.iter().filter(|e| e.falls_to_cpu) {
        if level == Level::Fits {
            level = Level::Heavy;
        }
        reasons.push(Reason {
            code: ReasonCode::SoftwareFallback,
            resource: Some(Resource::CpuLoad),
            track: match e.used_by {
                Use::Iso(t) => Some(t),
                _ => None,
            },
        });
    }
    for t in &plan.inactive {
        level = Level::Over;
        reasons.push(Reason {
            code: ReasonCode::SourceInactive,
            resource: Some(Resource::ActiveSources),
            track: Some(*t),
        });
    }
    Estimate {
        level,
        unknown,
        max_video: max_tracks(plan, limits, true),
        max_audio: max_tracks(plan, limits, false),
        tracks: track_costs(plan, limits),
        resources: rows,
        reasons,
    }
}
