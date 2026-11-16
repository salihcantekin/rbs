//! *Test this computer*: the calibration run of ADR-0087 decision 4, step D1 (spec
//! `docs/specs/output-estimate.md`, "Calibration run").
//!
//! It measures how many encoder sessions of each class of a recording's plan keep up
//! on each adapter, and how fast the recording folder writes. Everything here is a
//! plain function over injected parts: the [`Lanes`] that run the sessions (three
//! threads in the app, inline in tests), a [`Clock`](rbs_core::time::Clock), a
//! [`Pacer`] and a stop flag. Threads belong to the caller. Nothing here runs unless a
//! run is asked for (P13).

mod driver;
mod export;
mod session;
mod write;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rbs_codec::{VideoEncoderConfig, VideoEncoderFactory};
use rbs_core::registry::AdapterId;

pub use driver::{
    CalibrationEvent, CalibrationOutcome, EncoderResult, Lanes, LanesFailure, Pacer, RoundRequest,
    SeriesKey, SkipCode, Skipped, run_encoders,
};
pub use export::{
    EXPORT_FEED_FRAMES, EXPORT_LIMIT, ExportClass, ExportCounters, ExportResult, ExportRig,
    ExportSkip, ExportSkipCode, ExportsEnd, GPU_FRAMES, GPU_MIN_FRAMES, GPU_PART_LIMIT,
    GpuExportRig, RigError, SIBLING_RESERVE, WAIT_SLICE, class_estimate, export_rounds,
    export_warmup, run_exports,
};
pub use session::{FrameOutcome, SessionReport, SessionRunner, moving_ring};
pub use write::{
    CALIBRATION_WRITE_BYTES, FileSink, FolderResult, LEFTOVER_AGE, PARTIAL_MIN_BYTES, WriteSink,
    measure_write_speed, temp_name,
};

use crate::estimate::{FpsClass, RateClass, SizeClass};

/// The whole run's limit on the run's clock (ADR-0087 decision 4).
pub const CALIBRATION_LIMIT: Duration = Duration::from_secs(60);

/// Kept for the folder write, which runs last: 256 MiB at 17 MiB/s.
pub const WRITE_RESERVE: Duration = Duration::from_secs(15);

/// Of the encoder time, the part of the first pass (the likely adapter of each
/// class); the second pass (the other candidates) gets the rest.
pub const FIRST_PASS: Duration = Duration::from_secs(30);

/// The most sessions a plan can put on one adapter of one class: one per encoder
/// chain the studio runs and one per separate video file, `MAX_CHAINS +
/// MAX_ISO_VIDEO` (24). ADR-0087 wrote it as `MAX_OUTPUTS + MAX_ISO_VIDEO` (20) while
/// every output had its own chain; since ADR-0127 several outputs share one chain, so
/// the chains bound the sessions. A series that passes it has no session cap.
pub const SESSION_CEILING: u32 = (crate::MAX_CHAINS + crate::MAX_ISO_VIDEO) as u32;

/// The jitter depth of the late-round rule: a single round of up to `Q` frame
/// periods passes.
pub const JITTER_DEPTH: u32 = 3;

/// Warm-up rounds of a step: at least this many.
pub const WARMUP_MIN: u32 = 10;

/// Warm-up rounds of a step: at most this many; a backlog still growing then fails the
/// step as late.
pub const WARMUP_MAX: u32 = 120;

/// The warm-up ends once no session's backlog has grown for this many rounds.
pub const WARMUP_STABLE: u32 = 10;

/// A warm-up round's bound: past it the step fails as late.
pub const WARMUP_ROUND_LIMIT: Duration = Duration::from_secs(1);

/// Judged video per step.
pub const JUDGED_SECONDS: u32 = 2;

/// Paced rounds after step 1 whose call time is `frame_time`.
pub const FRAME_TIME_ROUNDS: u32 = 30;

/// Paced rounds before those, not counted (a ring still full from the judged rounds
/// drains).
pub const FRAME_TIME_SETTLE: u32 = 5;

/// Pre-filled frames per class, used in order.
pub const RING_FRAMES: usize = 8;

/// A frame older than the newest drained one by more than this many frames and still
/// without a packet is lost (the deepest reorder or in-flight window of any adapter).
pub const LOST_WINDOW: u64 = 16;

/// One class of the plan's video encodes (ADR-0087 decision 4).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ClassKey {
    /// Codec name.
    pub codec: String,
    /// Size class.
    pub size: SizeClass,
    /// Frame-rate class.
    pub fps: FpsClass,
    /// Rate-control class.
    pub rate: RateClass,
}

/// An adapter a class may be measured on, snapshotted on the control thread.
#[derive(Clone)]
pub struct Candidate {
    /// Adapter id.
    pub adapter: AdapterId,
    /// `adapter.version` when declared, part of the merge key.
    pub version: Option<String>,
    /// A hardware adapter.
    pub hardware: bool,
    /// The adapter the plan's encodes of this class use.
    pub likely: bool,
    /// Its factory.
    pub factory: Arc<dyn VideoEncoderFactory>,
}

impl std::fmt::Debug for Candidate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Candidate")
            .field("adapter", &self.adapter)
            .field("version", &self.version)
            .field("hardware", &self.hardware)
            .field("likely", &self.likely)
            .finish_non_exhaustive()
    }
}

/// One class to measure.
#[derive(Clone, Debug)]
pub struct ClassPlan {
    /// The class.
    pub key: ClassKey,
    /// The encoder settings at the largest planned size of the class.
    pub config: VideoEncoderConfig,
    /// The candidates in measuring order (selection order, hardware first).
    pub candidates: Vec<Candidate>,
}

/// What a run measures, from the plan `record.estimate` would build.
#[derive(Clone, Debug)]
pub struct CalibrationPlan {
    /// The classes, the master's first; empty for a profile with no picture.
    pub classes: Vec<ClassPlan>,
    /// The profile's recording folder for the canvas.
    pub folder: PathBuf,
    /// The export classes of the separate video files (step D2), largest first.
    pub exports: Vec<ExportClass>,
    /// The canvas frame rate the exports are paced at.
    pub frame_rate: rbs_core::timing::FrameRate,
}

impl CalibrationPlan {
    /// The upper bound of steps: one per export class, every series to the ceiling,
    /// and the folder.
    pub fn steps(&self) -> u32 {
        let series: usize = self.classes.iter().map(|c| c.candidates.len()).sum();
        u32::try_from(series)
            .unwrap_or(u32::MAX)
            .saturating_mul(SESSION_CEILING)
            .saturating_add(1)
            .saturating_add(u32::try_from(self.exports.len()).unwrap_or(u32::MAX))
    }
}
