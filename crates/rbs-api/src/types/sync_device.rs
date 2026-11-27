//! Remembered audio device synchronization offsets.

use serde::{Deserialize, Serialize};

/// An audio device whose offset is remembered (protocol 1.66, ADR-0126 decision 7):
/// the source type and its `adapter` and `target` settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct DeviceKey {
    /// Source type id.
    #[serde(rename = "type")]
    pub type_id: String,
    /// Adapter id.
    pub adapter: String,
    /// The adapter's device id.
    pub target: String,
}

/// How a remembered offset was set (protocol 1.66). A client reads an unknown value
/// as `other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum DeviceSyncOrigin {
    /// Applied from a measurement.
    Measured,
    /// Set by hand or through `source.set_sync_offset` without a measurement.
    Manual,
    /// A value this client does not know.
    #[serde(other)]
    Other,
}

/// What a measured offset was measured against (protocol 1.66).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DeviceSyncReference {
    /// The reference's device, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<DeviceKey>,
    /// The reference's name when measured.
    pub name: String,
}

/// One entry of `sync.devices` (protocol 1.66).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DeviceSyncEntry {
    /// The device.
    pub key: DeviceKey,
    /// The remembered offset in milliseconds.
    pub offset_ms: i32,
    /// How it was set.
    pub origin: DeviceSyncOrigin,
    /// For a measured value, what it was measured against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<DeviceSyncReference>,
    /// When it was set, in seconds since the Unix epoch (UTC).
    pub set_at: u64,
}

/// `sync.devices`'s answer (protocol 1.66).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SyncDeviceList {
    /// One entry per remembered device.
    pub devices: Vec<DeviceSyncEntry>,
}
