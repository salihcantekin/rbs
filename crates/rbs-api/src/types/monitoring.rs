//! Monitor, loudness and heard-level values.

use serde::{Deserialize, Serialize};

/// The volume and mute a source is heard at (protocol 1.48, ADR-0091).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Heard {
    /// A linear factor from 0 to 1.
    pub volume: f32,
    /// Muted.
    pub muted: bool,
}

/// Whether a source is heard on the monitor output (protocol 1.30, ADR-0097).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MonitorMode {
    /// Not monitored.
    #[default]
    Off,
    /// Monitored and taken off the stream and recordings while a monitor plays.
    Only,
    /// Monitored and mixed.
    Also,
}

impl MonitorMode {
    #[allow(clippy::trivially_copy_pass_by_ref)] // serde passes a reference.
    pub(super) fn is_off(&self) -> bool {
        *self == Self::Off
    }
}

/// `loudness.status` (protocol 1.47, ADR-0092 stage 1): the loudness of the main
/// canvas's mix after ITU-R BS.1770-4 and EBU R 128. Each value is `null` while it has
/// no data: the meter is off, its window has not filled since it started or was
/// reset, or the programme so far is below the -70 LUFS gate.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct LoudnessStatus {
    /// The meter runs.
    pub enabled: bool,
    /// Momentary loudness (400 ms), LUFS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub momentary: Option<f32>,
    /// Short-term loudness (3 s), LUFS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short_term: Option<f32>,
    /// Integrated loudness since the meter started or was reset, LUFS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integrated: Option<f32>,
    /// Loudness range (EBU Tech 3342) over the same time, LU.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<f32>,
    /// Highest true peak over the same time, dBTP.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub true_peak_max: Option<f32>,
    /// Highest momentary loudness over the same time, LUFS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub momentary_max: Option<f32>,
    /// Highest short-term loudness over the same time, LUFS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short_term_max: Option<f32>,
    /// The saved target, LUFS (protocol 1.59; `settings.get` `loudness`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<f32>,
    /// The saved true-peak limit, dBTP (protocol 1.59).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub true_peak_limit: Option<f32>,
}

/// `monitor.status` and `monitor.changed` (protocol 1.30, ADR-0097).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MonitorStatus {
    /// Some audio adapter can play to a device on this computer.
    pub available: bool,
    /// The chosen device (`monitor.devices` `device`); `null`: System default.
    pub device: Option<String>,
    /// The monitor volume, a linear factor from 0 to 1.
    pub volume: f32,
    /// A source is monitored or soloed, so the device should play.
    pub wanted: bool,
    /// The device plays the monitor mix.
    pub playing: bool,
    /// How far the monitor plays behind real time, in milliseconds, while it plays.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u32>,
    /// The soloed sources (session state, never saved).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub solo: Vec<u32>,
    /// Why the device failed to open or play, until it plays again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One playback device of `monitor.devices` (protocol 1.30).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorDevice {
    /// Its id, for `monitor.set_device`.
    pub device: String,
    /// The name the system gives it.
    pub name: String,
    /// The system's default output.
    pub is_default: bool,
}

/// `monitor.devices` (protocol 1.30).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorDeviceList {
    /// The playback devices, best adapter first.
    pub devices: Vec<MonitorDevice>,
}
