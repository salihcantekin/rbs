//! A source entry of the running studio as it is first bound: no instance yet, not
//! scheduled, and its picture mapping (ADR-0165 decision 5). Every instance of the
//! entry is lent that mapping, and a replacement entry (the binder rebuilds a source
//! whose settings changed) takes it over from the entry it replaces, so the switch's
//! state and the cell the source's filters read survive the rebuild.

use rbs_core::registry::AdapterId;
use rbs_graph::description::NodeId;
use rbs_scene::model::SourceId;
use rbs_sources::SourceMapping;

use super::{Entry, SourceState};

impl Entry {
    /// The entry of `source`, bound to `node` and named `name`. Takes the mapping of
    /// the entry of the same source among `gone` (the entries this reconcile retired),
    /// else a new one.
    pub(super) fn new(source: SourceId, node: NodeId, name: &str, gone: &[Entry]) -> Self {
        let mapping = gone
            .iter()
            .find(|e| e.source == source)
            .map_or_else(SourceMapping::new, |e| e.mapping.clone());
        Self {
            source,
            state: SourceState {
                source,
                node,
                name: name.to_owned(),
                adapter: AdapterId::new(""),
                target: String::new(),
                failed: None,
                fault: None,
                reason: None,
            },
            instance: None,
            scheduled: false,
            mapping,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Test code.
mod tests {
    use std::sync::Arc;

    use rbs_graph::description::GraphDescription;
    use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};

    use super::*;

    #[test]
    fn a_replacement_entry_keeps_the_mapping_of_the_one_it_replaces() {
        let mut graph = GraphDescription::new();
        let descriptor = Arc::new(NodeDescriptor::new(
            NodeTypeId::new_static("org.rbs.test.source"),
            NodeKind::Source,
            vec![],
            vec![Pin::one("video", LinkKind::Video)],
        ));
        let mut node = || graph.add_node(Arc::clone(&descriptor)).unwrap();
        let (a, b) = (SourceId::from_raw(1), SourceId::from_raw(2));
        let old = Entry::new(a, node(), "a", &[]);
        old.mapping.switch.set(true);
        let gone = [old];
        let replacement = Entry::new(a, node(), "a", &gone);
        assert!(
            replacement.mapping.switch.is_on(),
            "the switch carries over"
        );
        replacement.mapping.switch.set(false);
        assert!(!gone[0].mapping.switch.is_on(), "one shared switch");

        gone[0].mapping.switch.set(true);
        let other = Entry::new(b, node(), "b", &gone);
        assert!(!other.mapping.switch.is_on(), "another source gets its own");
    }
}
