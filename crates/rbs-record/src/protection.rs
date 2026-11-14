//! Recording protection, phase one (ADR-0090, spec `docs/specs/recording-protection.md`):
//! early warnings for a disk that is about to fill, outputs that lose frames, late
//! render ticks and low system memory, and a clean stop before the disk is full.
//!
//! [`Guard`] is the whole decision logic as a pure state machine: the caller hands it a
//! [`Sample`] about once a second with the master-clock time, and it answers which
//! recordings to stop. It reads counters the studio already keeps
//! ([`crate::Studio::output_counters`]) and never touches a hot path (P15). Free space
//! and memory come from a [`SystemProbe`] on one worker thread ([`ProbeWorker`]), so a
//! slow or hung drive never holds the control thread.
//!
//! A guard exists only while a recording or stream runs; the composition root creates
//! it with the first output and drops it with the last (P13).

mod guard;
mod probe;

pub use guard::{
    Alert, AlertKind, AtCritical, DiskState, Guard, GuardStatus, Level, OutputSample, OutputState,
    ProbeStep, ProtectionSettings, Sample, StopRequest, VolumeSample, frames_lost,
};
pub use probe::{
    PROBE_INTERVAL, PROBE_TIMEOUT, ProbeOutcome, ProbeReading, ProbeWorker, Probed, SystemProbe,
    VolumeKey, VolumeSpace,
};
