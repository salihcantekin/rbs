//! Graph descriptions and schedule plans.
//!
//! The control thread edits a [`GraphDescription`] (allocation is fine there), then
//! calls [`GraphDescription::plan`]. The resulting [`SchedulePlan`] lists only Active
//! nodes that can do useful work, in dependency order, and is what the render and
//! audio threads swap to at the next tick (media graph spec, "Live editing"). Inactive,
//! starved and dead-end nodes are never in a plan, so hot loops need no "is it
//! enabled" checks (ADR-0013).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::sync::Arc;

use rbs_core::lifecycle::{InvalidTransition, Lifecycle};

use crate::node::{DescriptorError, LinkKind, NodeDescriptor, NodeKind};

/// Identifies a node within one graph description. Ids are issued in increasing order
/// and never reused; [`GraphDescription::restore`] brings a removed node back under
/// its old id.
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
///
/// Indices are for the plan only. Anything persisted or logged (scene documents, the
/// command log) names pins by name, see [`GraphDescription::link_pin_names`].
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PinRef {
    /// The node.
    pub node: NodeId,
    /// Index into the descriptor's `inputs` or `outputs`.
    pub pin: usize,
}

/// A link from an output pin to an input pin.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Link {
    /// Producer output.
    pub from: PinRef,
    /// Consumer input.
    pub to: PinRef,
    /// What the link carries.
    pub kind: LinkKind,
}

/// A node in a description.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct NodeEntry {
    /// Its id.
    pub id: NodeId,
    /// Its type's descriptor, shared between nodes of the same type.
    pub descriptor: Arc<NodeDescriptor>,
    /// Its lifecycle state.
    pub state: Lifecycle,
}

/// A node taken out of a description together with its links, so an undo can put both
/// back with [`GraphDescription::restore`].
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct RemovedNode {
    /// The node as it was (state Standby or Installed).
    pub entry: NodeEntry,
    /// Every link that touched it.
    pub links: Vec<Link>,
}

/// Why an edit was refused.
#[non_exhaustive]
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
    /// The link would create a cycle. Feedback goes through the control plane.
    Cycle(Link),
    /// A lifecycle transition is not allowed.
    Lifecycle(NodeId, InvalidTransition),
    /// Active nodes must move to Standby before removal.
    NodeIsActive(NodeId),
    /// `restore` was given an id that is in use or was never issued.
    IdNotRestorable(NodeId),
    /// All `u32` node ids have been issued.
    IdsExhausted,
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
            GraphError::NodeIsActive(id) => write!(f, "{id} is active; move it to standby first"),
            GraphError::IdNotRestorable(id) => write!(f, "{id} cannot be restored"),
            GraphError::IdsExhausted => f.write_str("node ids exhausted"),
        }
    }
}

impl std::error::Error for GraphError {}

/// Why an Active node was left out of a plan.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SkipReason {
    /// It has media inputs but no Active producer feeds any of them.
    Starved,
    /// Nothing it produces reaches an Active sink (a node without media outputs).
    NoSink,
}

/// What the real-time threads run: Active nodes that can do useful work, in dependency
/// order.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SchedulePlan {
    /// Scheduled nodes; every producer comes before its consumers.
    pub order: Vec<NodeId>,
    /// Links between scheduled nodes.
    pub links: Vec<Link>,
    /// Active nodes left out, with the reason, for diagnostics.
    pub skipped: Vec<(NodeId, SkipReason)>,
}

/// The editable description of a media graph. `Clone` is cheap enough for snapshots
/// (descriptors are shared).
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
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or(GraphError::IdsExhausted)?;
        self.nodes.push(NodeEntry {
            id,
            descriptor,
            state: Lifecycle::Installed,
        });
        Ok(id)
    }

    /// Removes a Standby or Installed node and every link touching it. Returns both so
    /// the removal can be undone with [`GraphDescription::restore`].
    pub fn remove_node(&mut self, id: NodeId) -> Result<RemovedNode, GraphError> {
        let index = self.index(id)?;
        if self.nodes[index].state.is_scheduled() {
            return Err(GraphError::NodeIsActive(id));
        }
        let (links, kept) = self
            .links
            .iter()
            .partition(|l| l.from.node == id || l.to.node == id);
        self.links = kept;
        Ok(RemovedNode {
            entry: self.nodes.remove(index),
            links,
        })
    }

    /// Puts a removed node back under its old id, with its links. All or nothing: if
    /// any link no longer fits (for example its other end was removed too), nothing
    /// changes and the error is returned.
    pub fn restore(&mut self, removed: RemovedNode) -> Result<NodeId, GraphError> {
        let id = removed.entry.id;
        if id.0 >= self.next_id || self.node(id).is_some() {
            return Err(GraphError::IdNotRestorable(id));
        }
        if removed.entry.state.is_scheduled() {
            return Err(GraphError::NodeIsActive(id));
        }
        let mut next = self.clone();
        next.nodes.push(removed.entry);
        next.nodes.sort_by_key(|n| n.id);
        for link in removed.links {
            next.insert_link(link)?;
        }
        *self = next;
        Ok(id)
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

    /// All nodes in id order.
    pub fn nodes(&self) -> &[NodeEntry] {
        &self.nodes
    }

    /// All links in insertion order.
    pub fn links(&self) -> &[Link] {
        &self.links
    }

    /// The `(output name, input name)` of a link: the stable form for scene documents
    /// and the command log.
    pub fn link_pin_names(&self, link: &Link) -> Option<(&str, &str)> {
        let from = self
            .node(link.from.node)?
            .descriptor
            .outputs
            .get(link.from.pin)?;
        let to = self
            .node(link.to.node)?
            .descriptor
            .inputs
            .get(link.to.pin)?;
        Some((&from.name, &to.name))
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
        let link = Link {
            from: PinRef {
                node: from,
                pin: out_index,
            },
            to: PinRef {
                node: to,
                pin: in_index,
            },
            kind: producer.outputs[out_index].kind,
        };
        self.insert_link(link)?;
        Ok(link)
    }

    /// Removes a link. Returns whether it existed.
    pub fn disconnect(&mut self, link: Link) -> bool {
        let before = self.links.len();
        self.links.retain(|l| *l != link);
        self.links.len() != before
    }

    /// Validates and adds a link given by indices.
    fn insert_link(&mut self, link: Link) -> Result<(), GraphError> {
        let unknown_pin = |node: NodeId, pin: usize| GraphError::UnknownPin {
            node,
            name: format!("#{pin}"),
        };
        let producer = &self.nodes[self.index(link.from.node)?].descriptor;
        let consumer = &self.nodes[self.index(link.to.node)?].descriptor;
        let out_pin = producer
            .outputs
            .get(link.from.pin)
            .ok_or_else(|| unknown_pin(link.from.node, link.from.pin))?;
        let in_pin = consumer
            .inputs
            .get(link.to.pin)
            .ok_or_else(|| unknown_pin(link.to.node, link.to.pin))?;
        if out_pin.kind != in_pin.kind || link.kind != out_pin.kind {
            return Err(GraphError::KindMismatch {
                from: out_pin.kind,
                to: in_pin.kind,
            });
        }
        if self.links.contains(&link) {
            return Err(GraphError::DuplicateLink(link));
        }
        if !in_pin.many && self.links.iter().any(|l| l.to == link.to) {
            return Err(GraphError::InputOccupied(link.to));
        }
        if link.from.node == link.to.node || self.reaches(link.to.node, link.from.node) {
            return Err(GraphError::Cycle(link));
        }
        self.links.push(link);
        Ok(())
    }

    /// True if `target` can be reached from `start` along links. O(nodes + links).
    fn reaches(&self, start: NodeId, target: NodeId) -> bool {
        let mut next: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
        for l in &self.links {
            next.entry(l.from.node).or_default().push(l.to.node);
        }
        let mut stack = vec![start];
        let mut seen = HashSet::new();
        while let Some(node) = stack.pop() {
            if node == target {
                return true;
            }
            if seen.insert(node) {
                stack.extend(next.get(&node).into_iter().flatten().copied());
            }
        }
        false
    }

    /// Builds the plan the real-time threads run. O(nodes + links) per pruning round.
    ///
    /// 1. Start from the Active nodes and the links between them. Links from or to a
    ///    non-Active node are left out.
    /// 2. Repeatedly drop nodes that are [`SkipReason::Starved`] (a non-source with
    ///    media inputs but no remaining media link into any of them) or
    ///    [`SkipReason::NoSink`] (no path along remaining links to a node without media
    ///    outputs), until nothing changes.
    /// 3. Order the rest so every producer precedes its consumers, ties by node id, so
    ///    the same description always gives the same plan.
    ///
    /// A scheduled node may still see no input on some pins (a hidden layer, the second
    /// input of a muxer); handling that is part of its normal `process`, not a feature
    /// check.
    pub fn plan(&self) -> SchedulePlan {
        let kinds: HashMap<NodeId, &NodeEntry> = self
            .nodes
            .iter()
            .filter(|n| n.state.is_scheduled())
            .map(|n| (n.id, n))
            .collect();
        let mut alive: BTreeSet<NodeId> = kinds.keys().copied().collect();
        let mut skipped = Vec::new();

        loop {
            let links: Vec<&Link> = self
                .links
                .iter()
                .filter(|l| alive.contains(&l.from.node) && alive.contains(&l.to.node))
                .collect();
            let mut fed: HashSet<NodeId> = HashSet::new();
            let mut prev: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
            for l in &links {
                if l.kind.is_media() {
                    fed.insert(l.to.node);
                }
                prev.entry(l.to.node).or_default().push(l.from.node);
            }
            let starved: Vec<NodeId> = alive
                .iter()
                .copied()
                .filter(|id| {
                    let d = &kinds[id].descriptor;
                    d.kind != NodeKind::Source
                        && d.inputs.iter().any(|p| p.kind.is_media())
                        && !fed.contains(id)
                })
                .collect();

            // Reverse reachability from sinks.
            let mut reaches_sink: HashSet<NodeId> = HashSet::new();
            let mut stack: Vec<NodeId> = alive
                .iter()
                .copied()
                .filter(|id| {
                    !kinds[id]
                        .descriptor
                        .outputs
                        .iter()
                        .any(|p| p.kind.is_media())
                })
                .collect();
            while let Some(id) = stack.pop() {
                if reaches_sink.insert(id) {
                    stack.extend(prev.get(&id).into_iter().flatten().copied());
                }
            }

            let mut changed = false;
            for id in starved {
                alive.remove(&id);
                skipped.push((id, SkipReason::Starved));
                changed = true;
            }
            for id in alive.clone() {
                if !reaches_sink.contains(&id) {
                    alive.remove(&id);
                    skipped.push((id, SkipReason::NoSink));
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }

        let links: Vec<Link> = self
            .links
            .iter()
            .copied()
            .filter(|l| alive.contains(&l.from.node) && alive.contains(&l.to.node))
            .collect();
        let mut indegree: HashMap<NodeId, usize> = alive.iter().map(|&id| (id, 0)).collect();
        let mut next: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
        for l in &links {
            *indegree.entry(l.to.node).or_default() += 1;
            next.entry(l.from.node).or_default().push(l.to.node);
        }
        let mut ready: BTreeSet<NodeId> = alive
            .iter()
            .copied()
            .filter(|id| indegree.get(id) == Some(&0))
            .collect();
        let mut order = Vec::with_capacity(alive.len());
        while let Some(id) = ready.pop_first() {
            order.push(id);
            for to in next.get(&id).into_iter().flatten() {
                if let Some(d) = indegree.get_mut(to) {
                    *d -= 1;
                    if *d == 0 {
                        ready.insert(*to);
                    }
                }
            }
        }
        skipped.sort_by_key(|(id, _)| *id);
        SchedulePlan {
            order,
            links,
            skipped,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::{NodeTypeId, Pin};

    fn d(kind: NodeKind, inputs: Vec<Pin>, outputs: Vec<Pin>) -> Arc<NodeDescriptor> {
        Arc::new(NodeDescriptor::new(
            NodeTypeId::new_static("test.node"),
            kind,
            inputs,
            outputs,
        ))
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
    fn output() -> Arc<NodeDescriptor> {
        d(
            NodeKind::Output,
            vec![Pin::many("in", LinkKind::Packets)],
            vec![],
        )
    }
    fn preview() -> Arc<NodeDescriptor> {
        d(
            NodeKind::Output,
            vec![Pin::one("video", LinkKind::Video)],
            vec![],
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
        let out = g.add_node(output())?;
        let enc = g.add_node(encoder())?;
        let comp = g.add_node(compositor())?;
        let blur = g.add_node(filter())?;
        let cam = g.add_node(source())?;
        let screen = g.add_node(source())?;
        g.connect(enc, "out", out, "in")?;
        g.connect(comp, "out", enc, "in")?;
        g.connect(blur, "out", comp, "layers")?;
        g.connect(cam, "video", blur, "in")?;
        g.connect(screen, "video", comp, "layers")?;
        activate(&mut g, &[out, enc, comp, blur, cam, screen]);

        let plan = g.plan();
        assert_eq!(plan.order, [cam, blur, screen, comp, enc, out]);
        assert_eq!(plan.links.len(), 5);
        assert!(plan.skipped.is_empty());
        Ok(())
    }

    #[test]
    fn inactive_starved_and_dead_end_nodes_are_not_scheduled() -> Result<(), GraphError> {
        let mut g = GraphDescription::new();
        let pv = g.add_node(preview())?;
        let comp = g.add_node(compositor())?;
        let visible = g.add_node(source())?;
        let hidden = g.add_node(source())?;
        let lonely_filter = g.add_node(filter())?;
        let enc = g.add_node(encoder())?;
        let dead_source = g.add_node(source())?;
        g.connect(comp, "out", pv, "video")?;
        g.connect(visible, "video", comp, "layers")?;
        g.connect(hidden, "video", comp, "layers")?;
        g.connect(hidden, "video", lonely_filter, "in")?;
        g.connect(lonely_filter, "out", enc, "in")?;
        activate(
            &mut g,
            &[pv, comp, visible, lonely_filter, enc, dead_source],
        );
        g.set_state(hidden, Lifecycle::Standby)?;

        let plan = g.plan();
        assert_eq!(plan.order, [visible, comp, pv]);
        assert!(plan.links.iter().all(|l| l.from.node != hidden));
        assert_eq!(
            plan.skipped,
            [
                (lonely_filter, SkipReason::Starved),
                (enc, SkipReason::NoSink),
                (dead_source, SkipReason::NoSink)
            ]
        );

        // An Active producer feeding only an inactive consumer has no sink.
        let mut g = GraphDescription::new();
        let s = g.add_node(source())?;
        let p = g.add_node(preview())?;
        g.connect(s, "video", p, "video")?;
        activate(&mut g, &[s]);
        g.set_state(p, Lifecycle::Standby)?;
        assert_eq!(g.plan().skipped, [(s, SkipReason::NoSink)]);
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
        let ghost = NodeId(999);
        assert_eq!(
            g.connect(ghost, "out", a, "in").err(),
            Some(GraphError::UnknownNode(ghost))
        );
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

        activate(&mut g, &[b]);
        assert_eq!(g.remove_node(b).err(), Some(GraphError::NodeIsActive(b)));
        Ok(())
    }

    #[test]
    fn removal_can_be_undone_with_the_same_id() -> Result<(), GraphError> {
        let mut g = GraphDescription::new();
        let s = g.add_node(source())?;
        let f = g.add_node(filter())?;
        let p = g.add_node(preview())?;
        g.connect(s, "video", f, "in")?;
        let link = g.connect(f, "out", p, "video")?;
        assert_eq!(g.link_pin_names(&link), Some(("out", "video")));
        assert!(g.disconnect(link));
        assert!(!g.disconnect(link));
        g.connect(f, "out", p, "video")?;

        let removed = g.remove_node(f)?;
        assert_eq!(removed.links.len(), 2);
        assert!(g.links().is_empty());
        let added = g.add_node(filter())?;
        assert!(added > f, "ids are never reused");

        assert_eq!(g.restore(removed.clone())?, f);
        assert_eq!(g.links().len(), 2);
        assert_eq!(
            g.nodes().iter().map(|n| n.id).collect::<Vec<_>>(),
            [s, f, p, added]
        );
        assert_eq!(
            g.restore(removed).err(),
            Some(GraphError::IdNotRestorable(f))
        );

        // A restore whose links no longer fit changes nothing.
        let removed_f = g.remove_node(f)?;
        g.remove_node(p)?;
        let before = g.links().len();
        assert!(matches!(
            g.restore(removed_f),
            Err(GraphError::UnknownNode(_))
        ));
        assert!(g.node(f).is_none());
        assert_eq!(g.links().len(), before);
        Ok(())
    }

    #[test]
    fn ids_exhaust_with_an_error() -> Result<(), GraphError> {
        let mut g = GraphDescription::new();
        g.next_id = u32::MAX - 1;
        let last = g.add_node(source())?;
        assert_eq!(last.get(), u32::MAX - 1);
        assert_eq!(g.add_node(source()).err(), Some(GraphError::IdsExhausted));
        Ok(())
    }

    /// Small deterministic generator so tests need no dependency.
    struct Lcg(u64);

    impl Lcg {
        fn below(&mut self, n: u64) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 33) % n
        }
    }

    #[test]
    fn random_graphs_stay_acyclic_and_plans_respect_dependencies() -> Result<(), GraphError> {
        let mut rng = Lcg(2026);
        let mixer = d(
            NodeKind::AudioMixer,
            vec![Pin::many("in", LinkKind::Audio)],
            vec![Pin::one("out", LinkKind::Audio)],
        );
        let sink = d(
            NodeKind::Output,
            vec![Pin::many("in", LinkKind::Audio)],
            vec![],
        );
        for _ in 0..50 {
            let mut g = GraphDescription::new();
            let mut ids = Vec::new();
            for i in 0..30 {
                let desc = if i % 7 == 0 {
                    sink.clone()
                } else {
                    mixer.clone()
                };
                ids.push(g.add_node(desc)?);
            }
            for _ in 0..120 {
                let a = ids[rng.below(30) as usize];
                let b = ids[rng.below(30) as usize];
                // Errors (cycles, sinks as producers, duplicates) are expected; the
                // graph must stay valid whatever is refused.
                let _ = g.connect(a, "out", b, "in");
            }
            for &id in &ids {
                if rng.below(4) != 0 {
                    activate(&mut g, &[id]);
                }
            }
            let plan = g.plan();
            let position: HashMap<NodeId, usize> = plan
                .order
                .iter()
                .enumerate()
                .map(|(i, id)| (*id, i))
                .collect();
            assert_eq!(position.len(), plan.order.len());
            for l in &plan.links {
                assert!(position[&l.from.node] < position[&l.to.node]);
            }
            for (id, _) in &plan.skipped {
                assert!(!position.contains_key(id));
            }
            assert_eq!(plan, g.plan(), "plans are deterministic");
        }
        Ok(())
    }
}
