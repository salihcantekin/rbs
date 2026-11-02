//! Graph descriptions and schedule plans.
//!
//! The control thread edits a [`GraphDescription`] (allocation is fine there), then
//! calls [`GraphDescription::plan`]. The resulting [`SchedulePlan`] lists only Active
//! nodes, in dependency order, and is what the render and audio threads swap to at the
//! next tick (media graph spec, "Live editing"). Inactive nodes are never in a plan, so
//! hot loops need no "is it enabled" checks (ADR-0013).

use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;

use crate::lifecycle::{InvalidTransition, Lifecycle};
use crate::node::{DescriptorError, LinkKind, NodeDescriptor};

/// Identifies a node within one graph description. Ids are never reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(u32);

impl NodeId {
    /// The raw id.
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "node#{}", self.0)
    }
}

/// One pin of one node: an index into the node's inputs or outputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PinRef {
    /// The node.
    pub node: NodeId,
    /// Index into the descriptor's `inputs` or `outputs`.
    pub pin: usize,
}

/// A link from an output pin to an input pin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Link {
    /// Producer output.
    pub from: PinRef,
    /// Consumer input.
    pub to: PinRef,
}

/// A node in a description.
#[derive(Debug, Clone)]
pub struct NodeEntry {
    /// Its id.
    pub id: NodeId,
    /// Its type's descriptor, shared between nodes of the same type.
    pub descriptor: Arc<NodeDescriptor>,
    /// Its lifecycle state.
    pub state: Lifecycle,
}

/// Why an edit or a plan was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphError {
    /// The descriptor is invalid.
    Descriptor(DescriptorError),
    /// No node with this id.
    UnknownNode(NodeId),
    /// The node has no pin with this name.
    UnknownPin {
        /// The node.
        node: NodeId,
        /// The requested pin name.
        name: String,
    },
    /// The link connects pins of different kinds.
    KindMismatch {
        /// Output kind.
        from: LinkKind,
        /// Input kind.
        to: LinkKind,
    },
    /// The input takes one link and already has one.
    InputOccupied(PinRef),
    /// The same link already exists.
    DuplicateLink(Link),
    /// The link would create a cycle.
    Cycle(Link),
    /// A lifecycle transition is not allowed.
    Lifecycle(NodeId, InvalidTransition),
}

impl fmt::Display for GraphError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GraphError::Descriptor(e) => write!(f, "invalid descriptor: {e}"),
            GraphError::UnknownNode(id) => write!(f, "unknown {id}"),
            GraphError::UnknownPin { node, name } => write!(f, "{node} has no pin {name:?}"),
            GraphError::KindMismatch { from, to } => {
                write!(f, "cannot link a {from} output to a {to} input")
            }
            GraphError::InputOccupied(p) => {
                write!(f, "input {} of {} already has a link", p.pin, p.node)
            }
            GraphError::DuplicateLink(_) => f.write_str("link already exists"),
            GraphError::Cycle(_) => f.write_str("link would create a cycle"),
            GraphError::Lifecycle(id, e) => write!(f, "{id}: {e}"),
        }
    }
}

impl std::error::Error for GraphError {}

/// The editable description of a media graph.
#[derive(Debug, Clone, Default)]
pub struct GraphDescription {
    nodes: Vec<NodeEntry>,
    links: Vec<Link>,
    next_id: u32,
}

impl GraphDescription {
    /// An empty graph.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a node in the Installed state, after validating its descriptor.
    pub fn add_node(&mut self, descriptor: Arc<NodeDescriptor>) -> Result<NodeId, GraphError> {
        descriptor.validate().map_err(GraphError::Descriptor)?;
        let id = NodeId(self.next_id);
        self.next_id += 1;
        self.nodes.push(NodeEntry {
            id,
            descriptor,
            state: Lifecycle::Installed,
        });
        Ok(id)
    }

    /// Removes a node and every link touching it.
    pub fn remove_node(&mut self, id: NodeId) -> Result<NodeEntry, GraphError> {
        let index = self.index(id)?;
        self.links.retain(|l| l.from.node != id && l.to.node != id);
        Ok(self.nodes.remove(index))
    }

    /// Moves a node one lifecycle step.
    pub fn set_state(&mut self, id: NodeId, to: Lifecycle) -> Result<(), GraphError> {
        let index = self.index(id)?;
        let entry = &mut self.nodes[index];
        entry.state = entry
            .state
            .transition(to)
            .map_err(|e| GraphError::Lifecycle(id, e))?;
        Ok(())
    }

    /// The node with this id.
    pub fn node(&self, id: NodeId) -> Option<&NodeEntry> {
        self.nodes.iter().find(|n| n.id == id)
    }

    /// All nodes in insertion order.
    pub fn nodes(&self) -> &[NodeEntry] {
        &self.nodes
    }

    /// All links in insertion order.
    pub fn links(&self) -> &[Link] {
        &self.links
    }

    fn index(&self, id: NodeId) -> Result<usize, GraphError> {
        self.nodes
            .iter()
            .position(|n| n.id == id)
            .ok_or(GraphError::UnknownNode(id))
    }

    /// Connects output `from_pin` of `from` to input `to_pin` of `to`, by pin name.
    pub fn connect(
        &mut self,
        from: NodeId,
        from_pin: &str,
        to: NodeId,
        to_pin: &str,
    ) -> Result<Link, GraphError> {
        let producer = &self.nodes[self.index(from)?].descriptor;
        let consumer = &self.nodes[self.index(to)?].descriptor;
        let out_index = producer
            .outputs
            .iter()
            .position(|p| p.name == from_pin)
            .ok_or_else(|| GraphError::UnknownPin {
                node: from,
                name: from_pin.to_owned(),
            })?;
        let in_index = consumer
            .inputs
            .iter()
            .position(|p| p.name == to_pin)
            .ok_or_else(|| GraphError::UnknownPin {
                node: to,
                name: to_pin.to_owned(),
            })?;
        let (out_pin, in_pin) = (&producer.outputs[out_index], &consumer.inputs[in_index]);
        if out_pin.kind != in_pin.kind {
            return Err(GraphError::KindMismatch {
                from: out_pin.kind,
                to: in_pin.kind,
            });
        }
        let link = Link {
            from: PinRef {
                node: from,
                pin: out_index,
            },
            to: PinRef {
                node: to,
                pin: in_index,
            },
        };
        if self.links.contains(&link) {
            return Err(GraphError::DuplicateLink(link));
        }
        if !in_pin.many && self.links.iter().any(|l| l.to == link.to) {
            return Err(GraphError::InputOccupied(link.to));
        }
        if from == to || self.reaches(to, from) {
            return Err(GraphError::Cycle(link));
        }
        self.links.push(link);
        Ok(link)
    }

    /// Removes a link. Returns whether it existed.
    pub fn disconnect(&mut self, link: Link) -> bool {
        let before = self.links.len();
        self.links.retain(|l| *l != link);
        self.links.len() != before
    }

    /// True if `target` can be reached from `start` along links.
    fn reaches(&self, start: NodeId, target: NodeId) -> bool {
        let mut stack = vec![start];
        let mut seen = BTreeSet::new();
        while let Some(node) = stack.pop() {
            if node == target {
                return true;
            }
            if seen.insert(node) {
                stack.extend(
                    self.links
                        .iter()
                        .filter(|l| l.from.node == node)
                        .map(|l| l.to.node),
                );
            }
        }
        false
    }

    /// Builds the plan the real-time threads run: Active nodes in dependency order
    /// (ties broken by insertion order, so plans are deterministic) and the links
    /// between Active nodes. Links from or to inactive nodes are left out; the
    /// consumer sees no input on that pin (for example a hidden layer).
    pub fn plan(&self) -> SchedulePlan {
        let active: Vec<usize> = (0..self.nodes.len())
            .filter(|&i| self.nodes[i].state.is_scheduled())
            .collect();
        let is_active = |id: NodeId| {
            self.nodes
                .iter()
                .any(|n| n.id == id && n.state.is_scheduled())
        };
        let links: Vec<Link> = self
            .links
            .iter()
            .copied()
            .filter(|l| is_active(l.from.node) && is_active(l.to.node))
            .collect();

        // Kahn's algorithm keyed by insertion index. `connect` refuses cycles, so every
        // active node is emitted.
        let position = |id: NodeId| self.nodes.iter().position(|n| n.id == id);
        let mut indegree = vec![0_usize; self.nodes.len()];
        for l in &links {
            if let Some(i) = position(l.to.node) {
                indegree[i] += 1;
            }
        }
        let mut ready: BTreeSet<usize> = active
            .iter()
            .copied()
            .filter(|&i| indegree[i] == 0)
            .collect();
        let mut order = Vec::with_capacity(active.len());
        while let Some(i) = ready.pop_first() {
            let id = self.nodes[i].id;
            order.push(id);
            for l in links.iter().filter(|l| l.from.node == id) {
                if let Some(j) = position(l.to.node) {
                    indegree[j] -= 1;
                    if indegree[j] == 0 {
                        ready.insert(j);
                    }
                }
            }
        }
        SchedulePlan { order, links }
    }
}

/// What the real-time threads run: only Active nodes, in dependency order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SchedulePlan {
    /// Active nodes; every producer comes before its consumers.
    pub order: Vec<NodeId>,
    /// Links between Active nodes.
    pub links: Vec<Link>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::{NodeKind, NodeTypeId, Pin};

    fn d(kind: NodeKind, inputs: Vec<Pin>, outputs: Vec<Pin>) -> Arc<NodeDescriptor> {
        Arc::new(NodeDescriptor {
            type_id: NodeTypeId::new_static("test.node"),
            kind,
            inputs,
            outputs,
        })
    }

    fn source() -> Arc<NodeDescriptor> {
        d(
            NodeKind::Source,
            vec![],
            vec![Pin::one("video", LinkKind::Video)],
        )
    }
    fn filter() -> Arc<NodeDescriptor> {
        d(
            NodeKind::VideoFilter,
            vec![Pin::one("in", LinkKind::Video)],
            vec![Pin::one("out", LinkKind::Video)],
        )
    }
    fn compositor() -> Arc<NodeDescriptor> {
        d(
            NodeKind::Compositor,
            vec![Pin::many("layers", LinkKind::Video)],
            vec![Pin::one("out", LinkKind::Video)],
        )
    }
    fn encoder() -> Arc<NodeDescriptor> {
        d(
            NodeKind::Encoder,
            vec![Pin::one("in", LinkKind::Video)],
            vec![Pin::one("out", LinkKind::Packets)],
        )
    }

    fn activate(g: &mut GraphDescription, ids: &[NodeId]) {
        for &id in ids {
            assert_eq!(g.set_state(id, Lifecycle::Standby), Ok(()));
            assert_eq!(g.set_state(id, Lifecycle::Active), Ok(()));
        }
    }

    #[test]
    fn plan_orders_active_nodes_by_dependency() -> Result<(), GraphError> {
        let mut g = GraphDescription::new();
        let enc = g.add_node(encoder())?;
        let comp = g.add_node(compositor())?;
        let blur = g.add_node(filter())?;
        let cam = g.add_node(source())?;
        let screen = g.add_node(source())?;
        g.connect(comp, "out", enc, "in")?;
        g.connect(blur, "out", comp, "layers")?;
        g.connect(cam, "video", blur, "in")?;
        g.connect(screen, "video", comp, "layers")?;
        activate(&mut g, &[enc, comp, blur, cam, screen]);

        let plan = g.plan();
        assert_eq!(plan.order, [cam, blur, screen, comp, enc]);
        assert_eq!(plan.links.len(), 4);
        Ok(())
    }

    #[test]
    fn inactive_nodes_are_not_scheduled() -> Result<(), GraphError> {
        let mut g = GraphDescription::new();
        let comp = g.add_node(compositor())?;
        let visible = g.add_node(source())?;
        let hidden = g.add_node(source())?;
        g.connect(visible, "video", comp, "layers")?;
        g.connect(hidden, "video", comp, "layers")?;
        activate(&mut g, &[comp, visible]);
        g.set_state(hidden, Lifecycle::Standby)?;

        let plan = g.plan();
        assert_eq!(plan.order, [visible, comp]);
        assert!(plan.links.iter().all(|l| l.from.node != hidden));
        assert_eq!(GraphDescription::new().plan(), SchedulePlan::default());
        Ok(())
    }

    #[test]
    fn edits_are_validated() -> Result<(), GraphError> {
        let mut g = GraphDescription::new();
        let a = g.add_node(filter())?;
        let b = g.add_node(filter())?;
        let s = g.add_node(source())?;
        let e = g.add_node(encoder())?;
        g.connect(a, "out", b, "in")?;

        assert!(matches!(
            g.connect(b, "out", a, "in"),
            Err(GraphError::Cycle(_))
        ));
        assert!(matches!(
            g.connect(a, "out", a, "in"),
            Err(GraphError::Cycle(_))
        ));
        assert!(matches!(
            g.connect(s, "video", b, "in"),
            Err(GraphError::InputOccupied(_))
        ));
        assert!(matches!(
            g.connect(a, "out", b, "in"),
            Err(GraphError::DuplicateLink(_))
        ));
        assert!(matches!(
            g.connect(e, "out", a, "in"),
            Err(GraphError::KindMismatch { .. })
        ));
        assert!(matches!(
            g.connect(s, "nope", a, "in"),
            Err(GraphError::UnknownPin { .. })
        ));
        assert!(matches!(
            g.set_state(a, Lifecycle::Active),
            Err(GraphError::Lifecycle(..))
        ));

        let bad = d(
            NodeKind::Source,
            vec![Pin::one("in", LinkKind::Video)],
            vec![],
        );
        assert!(matches!(g.add_node(bad), Err(GraphError::Descriptor(_))));

        let removed = g.remove_node(b)?;
        assert_eq!(removed.id, b);
        assert!(g.links().is_empty());
        assert_eq!(g.remove_node(b).err(), Some(GraphError::UnknownNode(b)));
        let c = g.add_node(filter())?;
        assert_ne!(c, b, "ids are never reused");
        let link = g.connect(s, "video", c, "in")?;
        assert!(g.disconnect(link));
        assert!(!g.disconnect(link));
        Ok(())
    }
}
