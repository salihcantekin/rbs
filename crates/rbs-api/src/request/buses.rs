//! The main canvas's mix: its master (protocol 1.15) and its audio buses and send
//! levels (protocol 1.115, ADR-0097 decisions 9 to 13, ADR-0170 decision 9).

use serde::{Deserialize, Serialize};

/// `canvas.set_master_volume` (protocol 1.15, ADR-0081): the studio canvas's master.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct SetMasterVolume {
    /// A linear factor from 0 to 1.
    pub volume: f32,
}

/// `canvas.set_master_muted` (protocol 1.15, ADR-0081).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct SetMasterMuted {
    /// Muted.
    pub muted: bool,
}

/// `bus.list` and topic `bus.changed`: the main canvas's buses other than `main`, in
/// the order they were added.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BusList {
    /// The buses.
    pub buses: Vec<BusRow>,
}

/// One bus of [`BusList`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct BusRow {
    /// Its name, unique on the canvas, never `main`.
    pub name: String,
    /// Its fader, a linear factor from 0 to 1.
    pub volume: f32,
    /// Muted.
    pub muted: bool,
}

/// `bus.add`: a new bus, silent until a source sends to it. One undo step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct AddBus {
    /// Its name: not empty, not `main`, not taken.
    pub name: String,
}

/// `bus.remove`: the bus, its filters and the sends to it, in one undo step; refused
/// while the loaded output profile or a running output uses it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct BusName {
    /// The bus.
    pub name: String,
}

/// `bus.set`: at least one of `rename`, `volume`, `muted`; all or none. A rename is
/// one undo step and renames the loaded output profile's references; the fader and
/// mute are live.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct SetBus {
    /// The bus.
    pub name: String,
    /// Its new name, under the rules of `bus.add`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rename: Option<String>,
    /// Its fader, a linear factor from 0 to 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<f32>,
    /// Muted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub muted: Option<bool>,
}

/// `source.set_sends`: post-fader send levels of one source, live; buses not listed
/// keep theirs. All or none.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct SetSourceSends {
    /// The source's id.
    pub source: u32,
    /// The levels to set, not empty, each bus once.
    pub sends: Vec<SendLevel>,
}

/// One send level: in `source.set_sends`, and in source rows for the levels that
/// differ from the default (1 to `main`, 0 to every other bus).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct SendLevel {
    /// The bus, by name; `main` is the master mix.
    pub bus: String,
    /// A linear factor from 0 to 1.
    pub level: f32,
}

/// The bus methods and `source.set_sends` (protocol 1.115): one [`Command`](super::Command)
/// variant, so the namespace's dispatch stays in one place.
#[derive(Debug, Clone, PartialEq)]
pub enum BusCommand {
    /// `bus.list`.
    List,
    /// `bus.add`.
    Add(AddBus),
    /// `bus.remove`.
    Remove(BusName),
    /// `bus.set`.
    Set(SetBus),
    /// `source.set_sends`.
    SetSends(SetSourceSends),
}

impl BusCommand {
    /// The method's wire name.
    pub fn method(&self) -> &'static str {
        match self {
            Self::List => "bus.list",
            Self::Add(_) => "bus.add",
            Self::Remove(_) => "bus.remove",
            Self::Set(_) => "bus.set",
            Self::SetSends(_) => "source.set_sends",
        }
    }
}
