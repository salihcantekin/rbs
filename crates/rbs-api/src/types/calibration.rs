//! Calibration state and results.

use serde::{Deserialize, Serialize};

/// A code of `calibration.state` (protocol 1.55, ADR-0087 decision 4): why a series
/// or the write has no full result, or why a run failed. A client reads an unknown
/// code as `other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum CalibrationCode {
    /// The adapter does not support the class.
    Unsupported,
    /// No free session at the first open: other programs may hold them.
    Busy,
    /// A session did not open.
    OpenFailed,
    /// An encode failed.
    EncodeFailed,
    /// The folder has no room for the write test.
    NoRoom,
    /// The write test failed.
    WriteFailed,
    /// The run's time ended it first.
    TimeLimit,
    /// A cancel ended it first.
    Cancelled,
    /// A device was lost: nothing was written.
    DeviceLost,
    /// The run panicked.
    Panicked,
    /// The run ended without an answer.
    Ended,
    /// The run did not answer by its deadline.
    Hung,
    /// A code this client does not know.
    #[serde(other)]
    Other,
}

/// Why a calibration series stopped, which says what its `sessions` means (protocol
/// 1.55, spec `output-estimate.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum CalibrationStop {
    /// An open was refused: the session cap.
    Refused,
    /// The next step was late: a throughput limit.
    Late,
    /// The plan's ceiling passed: no cap.
    Ceiling,
    /// The time ended it: a lower bound.
    Limit,
    /// A cancel ended it: a lower bound.
    Cancelled,
    /// An error after step 1 ended it: a lower bound.
    Error,
    /// A reason this client does not know.
    #[serde(other)]
    Other,
}

/// The part of a calibration run that runs (protocol 1.55).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum CalibrationPart {
    /// The encoder sessions.
    Encoder,
    /// The folder write.
    Write,
    /// The export of separate video files (protocol 1.57).
    Export,
    /// A part this client does not know.
    #[serde(other)]
    Other,
}

/// A class of video encodes (protocol 1.55).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CalibrationClass {
    /// Codec name.
    pub codec: String,
    /// Size class (`720`, `1080`, `1440`, `2160`, `above-2160`).
    pub size: String,
    /// Frame-rate class (`30`, `60`, `above-60`).
    pub fps: String,
    /// Rate-control class (`cbr`, `vbr`, `cq`).
    pub rate: String,
}

/// One series' result in `calibration.state` (protocol 1.55).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CalibrationEncoder {
    /// Adapter id.
    pub adapter: String,
    /// The class.
    #[serde(flatten)]
    pub class: CalibrationClass,
    /// The highest passing session count.
    pub sessions: u32,
    /// `sessions × width × height × fps`.
    pub pixels_per_second: u64,
    /// Why the series stopped.
    pub stopped_by: CalibrationStop,
}

/// A part with no full result (protocol 1.55).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CalibrationSkip {
    /// Why.
    pub code: CalibrationCode,
    /// The series' adapter; absent for the write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter: Option<String>,
    /// The series' class; absent for the write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<CalibrationClass>,
}

/// One measured export class (protocol 1.57, ADR-0087 step D2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CalibrationExport {
    /// Size class: `720`, `1080`, `1440`, `2160` or `above-2160`.
    pub size: String,
    /// The export format.
    pub format: CalibrationExportFormat,
    /// The render thread's time of one export per frame, in microseconds (at least a
    /// frame period when it did not keep up).
    pub time_us: u64,
    /// The GPU's sustained time per export in microseconds, for diagnosis; judged by
    /// nothing. `null` with too few conversions.
    pub gpu_us: Option<u64>,
    /// No conversion found every readback in flight and the frames arrived.
    pub kept_up: bool,
}

/// The format of an export (protocol 1.57).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum CalibrationExportFormat {
    /// 8-bit 4:2:0.
    Nv12,
    /// 10-bit 4:2:0.
    P010,
    /// A format this client does not know.
    #[serde(other)]
    Other,
}

/// An export class with no result (protocol 1.57).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct CalibrationExportSkip {
    /// Size class.
    pub size: String,
    /// The export format.
    pub format: CalibrationExportFormat,
    /// Why.
    pub code: CalibrationExportCode,
}

/// Why an export class has no result (protocol 1.57): its own closed list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum CalibrationExportCode {
    /// No throwaway GPU device could be made.
    GpuUnavailable,
    /// The export could not be built or failed.
    ExportFailed,
    /// The throwaway device was lost; the run went on.
    DeviceLost,
    /// The time budget ended it first, or the GPU did not answer in time.
    TimeLimit,
    /// A cancel ended it first.
    Cancelled,
    /// A code this client does not know.
    #[serde(other)]
    Other,
}

/// `calibration.status` and the topic `calibration.state` (protocol 1.55, ADR-0087
/// decisions 4 and 7). No member carries a path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CalibrationState {
    /// No run in this app run.
    #[default]
    Idle,
    /// A run is active.
    Running {
        /// Steps started so far.
        step: u32,
        /// The upper bound of steps.
        steps: u32,
        /// What runs now.
        part: CalibrationPart,
        /// The adapter measured now; `null` during the write.
        adapter: Option<String>,
        /// A cancel was asked for and the run is ending.
        cancelling: bool,
    },
    /// The last run ended; it stays until the next run.
    Done {
        /// Some part stopped early or was skipped for time.
        partial: bool,
        /// A cancel ended it.
        cancelled: bool,
        /// `calibration.toml` was written.
        saved: bool,
        /// The series' results.
        encoders: Vec<CalibrationEncoder>,
        /// The folder's synced write speed in bytes per second.
        write_speed: Option<u64>,
        /// The write stopped early.
        write_partial: bool,
        /// The parts with no full result.
        skipped: Vec<CalibrationSkip>,
        /// The export classes measured (protocol 1.57).
        #[serde(default)]
        exports: Vec<CalibrationExport>,
        /// The export classes with no result (protocol 1.57).
        #[serde(default)]
        exports_skipped: Vec<CalibrationExportSkip>,
    },
    /// The last run failed; nothing was written. It stays until the next run.
    Failed {
        /// Why.
        code: CalibrationCode,
        /// A short message with no path.
        message: String,
    },
    /// A state this client does not know.
    #[serde(other)]
    Other,
}
