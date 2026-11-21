//! A source entry of the running studio as it is first bound: no instance yet, not
//! scheduled, and its own picture mapping (ADR-0165 decision 5), which every instance
//! of the entry is lent so a rebuilt capture node keeps it.

use rbs_core::registry::AdapterId;
use rbs_graph::description::NodeId;
use rbs_scene::model::SourceId;
use rbs_sources::SourceMapping;

use super::{Entry, SourceState};

impl Entry {
    /// The entry of `source`, bound to `node` and named `name`.
    pub(super) fn new(source: SourceId, node: NodeId, name: String) -> Self {
        Self {
            source,
            state: SourceState {
                source,
                node,
                name,
                adapter: AdapterId::new(""),
                target: String::new(),
                failed: None,
                fault: None,
                reason: None,
            },
            instance: None,
            scheduled: false,
            mapping: SourceMapping::new(),
        }
    }
}
