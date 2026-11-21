//! A source entry's picture mapping (ADR-0165 decision 5), lent to every instance of
//! the entry through [`InstanceContext::with_mapping`](crate::InstanceContext::with_mapping).

use rbs_capture::{MappingCell, MappingSwitch};

/// A source entry's picture mapping (ADR-0165 decision 5): the switch the host sets to
/// "a pointer filter of this source is scheduled" and the cell the capture node writes
/// the newest frame's mapping to. Created once per source entry and passed to every
/// instance of it, so a rebuilt capture node keeps both. Clones share them.
#[non_exhaustive]
#[derive(Clone, Debug, Default)]
pub struct SourceMapping {
    /// Turns the adapter's mapping work on and off (control thread).
    pub switch: MappingSwitch,
    /// The newest frame's mapping, read by the source's pointer filters.
    pub cell: MappingCell,
}

impl SourceMapping {
    /// A switch that is off and an empty cell.
    pub fn new() -> Self {
        Self::default()
    }
}
