//! What a backend offers: levels, cost hints and capabilities.

use rbs_core::media::MemoryKind;

/// A quality level the user can choose. Backends offer one to three of them.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum LevelId {
    /// The cheapest level.
    Low,
    /// The balanced level.
    Medium,
    /// The best edge quality, at the highest cost.
    High,
}

/// Where the work of a level runs.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ComputeUnit {
    /// The graphics processor.
    Gpu,
    /// The processor.
    Cpu,
    /// A neural engine or NPU.
    Npu,
}

/// The measured cost of a level, shown to the user next to it (ADR-0179 decision 12).
/// Numbers come from measurements on the reference machines, never from guesses, and
/// engine code never decides anything from them.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CostHint {
    /// Typical milliseconds per frame at 1080p.
    pub ms_per_frame: f32,
    /// Where the work runs.
    pub unit: ComputeUnit,
    /// A short English note for the UI to localise, such as "mask 512x384".
    pub note: &'static str,
}

impl CostHint {
    /// Creates a cost hint.
    pub const fn new(ms_per_frame: f32, unit: ComputeUnit, note: &'static str) -> Self {
        Self {
            ms_per_frame,
            unit,
            note,
        }
    }
}

/// One level of a backend with its cost.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QualityLevel {
    /// Which level.
    pub id: LevelId,
    /// What it costs.
    pub cost: CostHint,
}

impl QualityLevel {
    /// Creates a level.
    pub const fn new(id: LevelId, cost: CostHint) -> Self {
        Self { id, cost }
    }
}

/// What a backend can do on this computer, returned by `probe`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct BackendInfo {
    /// False: the backend is not listed in the UI.
    pub available: bool,
    /// One to three levels, ordered low to high.
    pub levels: Vec<QualityLevel>,
    /// The kind of frame `open` wants. `MemoryKind::System` makes the filter downscale
    /// on the GPU and read the small image back (ADR-0179 decision 3).
    pub input: MemoryKind,
    /// The kind a matte comes in.
    pub output: MemoryKind,
    /// The backend keeps state between frames.
    pub temporal: bool,
    /// The work runs on dedicated hardware (GPU, neural engine).
    pub hardware_assisted: bool,
}

impl BackendInfo {
    /// A backend that is not available here.
    pub fn unavailable() -> Self {
        Self {
            available: false,
            levels: Vec::new(),
            input: MemoryKind::System,
            output: MemoryKind::System,
            temporal: false,
            hardware_assisted: false,
        }
    }

    /// The level with this id, when the backend offers it.
    pub fn level(&self, id: LevelId) -> Option<&QualityLevel> {
        self.levels.iter().find(|level| level.id == id)
    }
}
