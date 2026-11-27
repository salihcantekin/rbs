//! State snapshots and their sequence numbers.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A change counter per state key (`"record.state"`, later `"source.changed:7"`).
pub type SeqMap = BTreeMap<String, u64>;

/// A state query's answer: the data and the `seq` of each state key it includes, so a
/// client ignores events with an equal or lower `seq`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Snapshot<T> {
    /// `seq` per state key included in `data`.
    pub seq: SeqMap,
    /// The state.
    pub data: T,
}
