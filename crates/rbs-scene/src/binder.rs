//! Binding a scene collection to a media graph description.
//!
//! The [`SceneBinder`] keeps a graph description in step with a collection: one node
//! per source (one instance however many scenes and canvases place it), one node per
//! filter chained after its source, and one compositor node per canvas for the program
//! scene (and one for the preview scene in studio mode). Node ids stay the same across
//! edits, so the runtime keeps running nodes across a swap (ADR-0020). The wanted
//! lifecycle state of every node comes from [`SceneCollection::wanted_sources`].
//!
//! The binder owns the nodes and links it creates and touches nothing else. Outputs
//! (stream, recording, preview panels, projectors) are attached by the application to
//! [`SceneBinder::compositor`]; a compositor without one is not scheduled.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use rbs_core::lifecycle::Lifecycle;
use rbs_graph::description::{GraphDescription, GraphError, Link, NodeId, SchedulePlan};
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId};

use crate::collection::{SceneCollection, SceneError};
use crate::flatten::Affine2;
use crate::model::{CanvasId, Crop, FilterId, ItemId, SceneId, SourceId};

/// Node type id of the canvas compositor.
pub const COMPOSITOR_TYPE: &str = "org.rbs.compositor";

/// Looks up the descriptor of a node type (from the adapter registry or plugin
/// manifests). Any `Fn(&NodeTypeId) -> Option<Arc<NodeDescriptor>>` works.
pub trait NodeTypes {
    /// The descriptor of `type_id`, or `None` if no such type is installed.
    fn descriptor(&self, type_id: &NodeTypeId) -> Option<Arc<NodeDescriptor>>;
}

impl<F> NodeTypes for F
where
    F: Fn(&NodeTypeId) -> Option<Arc<NodeDescriptor>>,
{
    fn descriptor(&self, type_id: &NodeTypeId) -> Option<Arc<NodeDescriptor>> {
        self(type_id)
    }
}

/// Which picture of a canvas a compositor draws.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Role {
    /// The scene on air.
    Program,
    /// The preview scene in studio mode.
    Preview,
}

/// One layer for a compositor node, naming the graph node that feeds it.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct BoundLayer {
    /// The node whose video this layer draws: the last enabled filter of the source,
    /// or the source itself.
    pub producer: NodeId,
    /// The source.
    pub source: SourceId,
    /// Item path, as in [`FlatLayer::path`](crate::FlatLayer::path).
    pub path: Vec<ItemId>,
    /// Maps the unit square of the cropped source to canvas pixels.
    pub transform: Affine2,
    /// Source pixels cut before drawing.
    pub crop: Crop,
    /// 0.0..=1.0.
    pub opacity: f32,
}

/// A bound compositor and what it draws.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct BoundCompositor {
    /// Its node.
    pub node: NodeId,
    /// Index of the input pin that carries the layers (the compositor type's first
    /// many-input video pin); pass it to [`resolve`].
    pub layers_pin: usize,
    /// Layers, bottom first.
    pub layers: Vec<BoundLayer>,
    /// True if this sync created the node (attach outputs to it).
    pub created: bool,
}

/// Something the binder could not represent; the rest of the graph is still bound.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindProblem {
    /// No node type with this id is installed; the source is left out of the graph.
    UnknownSourceType(SourceId, NodeTypeId),
    /// No node type with this id is installed; the filter is skipped.
    UnknownFilterType(FilterId, NodeTypeId),
    /// The source's node type is not a `Source` with a video output; it is left out.
    /// (Audio sources are bound with the mixer, later.)
    NoVideoOutput(SourceId),
    /// The filter's node type is not a `VideoFilter` with video input and output; it
    /// is skipped.
    NoVideoPins(FilterId),
}

/// What [`SceneBinder::sync`] produced.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BindReport {
    /// Every bound compositor.
    pub compositors: BTreeMap<(CanvasId, Role), BoundCompositor>,
    /// Compositor nodes this sync removed (with every link into and out of them,
    /// including outputs the application attached).
    pub retired_compositors: Vec<NodeId>,
    /// Parts of the collection that could not be bound.
    pub problems: Vec<BindProblem>,
}

/// Why binding failed. The graph and the binder are unchanged.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindError {
    /// The compositor node type is not installed, is not a `Compositor`, or lacks a
    /// many-input video pin or a video output.
    NoCompositor,
    /// A graph edit was refused.
    Graph(GraphError),
    /// The collection could not be flattened (cannot happen for a validated
    /// collection; reported instead of panicking).
    Scene(SceneError),
}

impl fmt::Display for BindError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BindError::NoCompositor => write!(f, "no usable {COMPOSITOR_TYPE} node type"),
            BindError::Graph(e) => write!(f, "graph edit refused: {e}"),
            BindError::Scene(e) => write!(f, "scene error: {e}"),
        }
    }
}

impl std::error::Error for BindError {}

impl From<GraphError> for BindError {
    fn from(e: GraphError) -> Self {
        BindError::Graph(e)
    }
}

/// Pin names (and the input's index) used to wire a node.
#[derive(Debug, Clone)]
struct Pins {
    input: Option<(usize, String)>,
    output: Option<String>,
}

fn video_pins(descriptor: &NodeDescriptor, many_input: bool) -> Pins {
    Pins {
        input: descriptor
            .inputs
            .iter()
            .enumerate()
            .find(|(_, p)| p.kind == LinkKind::Video && (p.many || !many_input))
            .map(|(i, p)| (i, p.name.to_string())),
        output: descriptor
            .outputs
            .iter()
            .find(|p| p.kind == LinkKind::Video)
            .map(|p| p.name.to_string()),
    }
}

#[derive(Debug, Clone)]
struct BoundNode {
    node: NodeId,
    pins: Pins,
}

#[derive(Debug, Clone)]
struct BoundSource {
    source: BoundNode,
    filters: BTreeMap<FilterId, BoundNode>,
    /// Enabled, bound filters in chain order.
    chain: Vec<FilterId>,
    /// Links this binder made for the chain.
    chain_links: Vec<Link>,
}

impl BoundSource {
    fn tail(&self) -> &BoundNode {
        self.chain
            .last()
            .and_then(|f| self.filters.get(f))
            .unwrap_or(&self.source)
    }

    fn nodes(&self) -> impl Iterator<Item = NodeId> + '_ {
        std::iter::once(self.source.node).chain(self.filters.values().map(|b| b.node))
    }
}

#[derive(Debug, Clone)]
struct BoundComp {
    node: BoundNode,
    /// Links this binder made into the layers pin.
    links: Vec<Link>,
}

/// Keeps a graph description in step with a scene collection (scene model spec,
/// "Graph binding").
pub struct SceneBinder {
    types: Box<dyn NodeTypes + Send>,
    state: State,
}

#[derive(Debug, Clone, Default)]
struct State {
    sources: BTreeMap<SourceId, BoundSource>,
    compositors: BTreeMap<(CanvasId, Role), BoundComp>,
}

impl fmt::Debug for SceneBinder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SceneBinder")
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

/// Moves a node to `to` one lifecycle step at a time.
fn set_state(graph: &mut GraphDescription, id: NodeId, to: Lifecycle) -> Result<(), GraphError> {
    let order = [Lifecycle::Installed, Lifecycle::Standby, Lifecycle::Active];
    let rank = |s: Lifecycle| order.iter().position(|&o| o == s).unwrap_or(0);
    let mut now = graph
        .node(id)
        .map(|n| n.state)
        .ok_or(GraphError::UnknownNode(id))?;
    while now != to {
        let next = if rank(now) < rank(to) {
            order[rank(now) + 1]
        } else {
            order[rank(now) - 1]
        };
        graph.set_state(id, next)?;
        now = next;
    }
    Ok(())
}

fn add_node(
    graph: &mut GraphDescription,
    descriptor: Arc<NodeDescriptor>,
) -> Result<NodeId, GraphError> {
    let id = graph.add_node(descriptor)?;
    graph.set_state(id, Lifecycle::Standby)?;
    Ok(id)
}

/// Moves a node to Standby and removes it with its links; a node someone else already
/// removed is fine.
fn retire(graph: &mut GraphDescription, id: NodeId) -> Result<(), GraphError> {
    if graph.node(id).is_none() {
        return Ok(());
    }
    set_state(graph, id, Lifecycle::Standby)?;
    graph.remove_node(id)?;
    Ok(())
}

impl SceneBinder {
    /// A binder that looks node types up in `types`. Nothing is bound until
    /// [`SceneBinder::sync`].
    pub fn new(types: impl NodeTypes + Send + 'static) -> Self {
        Self {
            types: Box::new(types),
            state: State::default(),
        }
    }

    /// The node of a bound source.
    pub fn source_node(&self, id: SourceId) -> Option<NodeId> {
        self.state.sources.get(&id).map(|b| b.source.node)
    }

    /// The node of a bound filter.
    pub fn filter_node(&self, source: SourceId, id: FilterId) -> Option<NodeId> {
        self.state
            .sources
            .get(&source)
            .and_then(|b| b.filters.get(&id))
            .map(|b| b.node)
    }

    /// The compositor node drawing `role` of `canvas`, if that canvas has a scene in
    /// that role. Outputs connect to its video output.
    pub fn compositor(&self, canvas: CanvasId, role: Role) -> Option<NodeId> {
        self.state
            .compositors
            .get(&(canvas, role))
            .map(|b| b.node.node)
    }

    /// Updates `graph` so it matches `collection`, keeping node ids of everything that
    /// still exists. On error neither `graph` nor the binder changes. See the scene
    /// model spec, "Graph binding", for the rules.
    pub fn sync(
        &mut self,
        collection: &SceneCollection,
        graph: &mut GraphDescription,
    ) -> Result<BindReport, BindError> {
        let mut draft_graph = graph.clone();
        let mut draft = self.state.clone();
        let report = draft.sync(&*self.types, collection, &mut draft_graph)?;
        *graph = draft_graph;
        self.state = draft;
        Ok(report)
    }
}

impl State {
    /// Forgets bindings whose nodes were removed from the graph by someone else, so
    /// they are bound again instead of failing forever.
    fn heal(&mut self, graph: &mut GraphDescription) -> Result<(), GraphError> {
        let lost: Vec<SourceId> = self
            .sources
            .iter()
            .filter(|(_, b)| graph.node(b.source.node).is_none())
            .map(|(id, _)| *id)
            .collect();
        for id in lost {
            if let Some(b) = self.sources.remove(&id) {
                // Its filters would be orphans; remove them so the source is bound
                // again from scratch.
                for f in b.filters.values() {
                    retire(graph, f.node)?;
                }
            }
        }
        for b in self.sources.values_mut() {
            let before = b.filters.len();
            b.filters.retain(|_, f| graph.node(f.node).is_some());
            b.chain_links.retain(|l| graph.links().contains(l));
            if b.filters.len() != before {
                // Rebuild the chain on this sync.
                for link in b.chain_links.drain(..) {
                    graph.disconnect(link);
                }
                b.chain.clear();
            }
        }
        self.compositors
            .retain(|_, c| graph.node(c.node.node).is_some());
        for c in self.compositors.values_mut() {
            c.links.retain(|l| graph.links().contains(l));
        }
        Ok(())
    }

    fn sync(
        &mut self,
        types: &dyn NodeTypes,
        collection: &SceneCollection,
        graph: &mut GraphDescription,
    ) -> Result<BindReport, BindError> {
        self.heal(graph)?;
        let mut report = BindReport::default();
        let compositor_desc = types
            .descriptor(&NodeTypeId::new(COMPOSITOR_TYPE))
            .filter(|d| d.kind == NodeKind::Compositor)
            .ok_or(BindError::NoCompositor)?;
        let compositor_pins = video_pins(&compositor_desc, true);
        let Some((layers_pin, _)) = compositor_pins.input.clone() else {
            return Err(BindError::NoCompositor);
        };
        if compositor_pins.output.is_none() {
            return Err(BindError::NoCompositor);
        }

        // Sources that are gone.
        let gone: Vec<SourceId> = self
            .sources
            .keys()
            .copied()
            .filter(|id| collection.source(*id).is_none())
            .collect();
        for id in gone {
            if let Some(bound) = self.sources.remove(&id) {
                for node in bound.nodes().collect::<Vec<_>>() {
                    retire(graph, node)?;
                }
            }
        }

        // Sources and their filter chains.
        for source in collection.sources() {
            if let std::collections::btree_map::Entry::Vacant(slot) = self.sources.entry(source.id)
            {
                let Some(descriptor) = types.descriptor(&source.type_id) else {
                    report.problems.push(BindProblem::UnknownSourceType(
                        source.id,
                        source.type_id.clone(),
                    ));
                    continue;
                };
                let pins = video_pins(&descriptor, false);
                if descriptor.kind != NodeKind::Source || pins.output.is_none() {
                    report.problems.push(BindProblem::NoVideoOutput(source.id));
                    continue;
                }
                let node = add_node(graph, descriptor)?;
                slot.insert(BoundSource {
                    source: BoundNode { node, pins },
                    filters: BTreeMap::new(),
                    chain: Vec::new(),
                    chain_links: Vec::new(),
                });
            }
            let Some(bound) = self.sources.get_mut(&source.id) else {
                continue;
            };
            let present: BTreeSet<FilterId> = source.filters.iter().map(|f| f.id).collect();
            let removed: Vec<FilterId> = bound
                .filters
                .keys()
                .copied()
                .filter(|f| !present.contains(f))
                .collect();
            for f in removed {
                if let Some(b) = bound.filters.remove(&f) {
                    retire(graph, b.node)?;
                }
            }
            for filter in &source.filters {
                if bound.filters.contains_key(&filter.id) {
                    continue;
                }
                let Some(descriptor) = types.descriptor(&filter.type_id) else {
                    report.problems.push(BindProblem::UnknownFilterType(
                        filter.id,
                        filter.type_id.clone(),
                    ));
                    continue;
                };
                let pins = video_pins(&descriptor, false);
                if descriptor.kind != NodeKind::VideoFilter
                    || pins.input.is_none()
                    || pins.output.is_none()
                {
                    report.problems.push(BindProblem::NoVideoPins(filter.id));
                    continue;
                }
                let node = add_node(graph, descriptor)?;
                bound.filters.insert(filter.id, BoundNode { node, pins });
            }
            let chain: Vec<FilterId> = source
                .filters
                .iter()
                .filter(|f| f.enabled && bound.filters.contains_key(&f.id))
                .map(|f| f.id)
                .collect();
            if chain != bound.chain {
                // Only the links this binder made; links into removed filters went
                // with their node.
                for link in bound.chain_links.drain(..) {
                    graph.disconnect(link);
                }
                let mut prev = bound.source.clone();
                for f in &chain {
                    if let Some(next) = bound.filters.get(f) {
                        if let (Some(out), Some((_, input))) = (&prev.pins.output, &next.pins.input)
                        {
                            let link = graph.connect(prev.node, out, next.node, input)?;
                            bound.chain_links.push(link);
                        }
                        prev = next.clone();
                    }
                }
                bound.chain = chain;
            }
        }

        // Compositors and their layers.
        let mut needed: BTreeMap<(CanvasId, Role), SceneId> = BTreeMap::new();
        for canvas in collection.canvases() {
            if let Some(scene) = canvas.program {
                needed.insert((canvas.id, Role::Program), scene);
            }
            if let Some(scene) = canvas.preview {
                needed.insert((canvas.id, Role::Preview), scene);
            }
        }
        let unneeded: Vec<(CanvasId, Role)> = self
            .compositors
            .keys()
            .copied()
            .filter(|k| !needed.contains_key(k))
            .collect();
        for key in unneeded {
            if let Some(b) = self.compositors.remove(&key) {
                retire(graph, b.node.node)?;
                report.retired_compositors.push(b.node.node);
            }
        }
        for (&(canvas, role), &scene) in &needed {
            let mut created = false;
            let comp = match self.compositors.entry((canvas, role)) {
                std::collections::btree_map::Entry::Occupied(e) => e.into_mut(),
                std::collections::btree_map::Entry::Vacant(e) => {
                    created = true;
                    let node = add_node(graph, Arc::clone(&compositor_desc))?;
                    e.insert(BoundComp {
                        node: BoundNode {
                            node,
                            pins: compositor_pins.clone(),
                        },
                        links: Vec::new(),
                    })
                }
            };
            let flat = collection
                .flatten(canvas, scene)
                .map_err(BindError::Scene)?;
            let mut layers = Vec::with_capacity(flat.len());
            for layer in flat {
                if let Some(bound) = self.sources.get(&layer.source) {
                    layers.push(BoundLayer {
                        producer: bound.tail().node,
                        source: layer.source,
                        path: layer.path,
                        transform: layer.transform,
                        crop: layer.crop,
                        opacity: layer.opacity,
                    });
                }
            }
            // Producers come from visibility, not from the drawn layers, so a source
            // faded to opacity 0 stays linked and scheduled.
            let mut producers: Vec<BoundNode> = Vec::new();
            let visible = collection
                .visible_sources(canvas, scene)
                .map_err(BindError::Scene)?;
            let ordered = layers
                .iter()
                .map(|l| l.source)
                .chain(visible.iter().copied());
            for source in ordered {
                if let Some(bound) = self.sources.get(&source) {
                    let tail = bound.tail();
                    if !producers.iter().any(|p| p.node == tail.node) {
                        producers.push(tail.clone());
                    }
                }
            }
            let wanted: BTreeSet<NodeId> = producers.iter().map(|p| p.node).collect();
            comp.links.retain(|link| {
                let keep = wanted.contains(&link.from.node);
                if !keep {
                    graph.disconnect(*link);
                }
                keep
            });
            let linked: BTreeSet<NodeId> = comp.links.iter().map(|l| l.from.node).collect();
            if let Some((_, input)) = &comp.node.pins.input {
                for p in producers.iter().filter(|p| !linked.contains(&p.node)) {
                    if let Some(out) = &p.pins.output {
                        let link = graph.connect(p.node, out, comp.node.node, input)?;
                        comp.links.push(link);
                    }
                }
            }
            report.compositors.insert(
                (canvas, role),
                BoundCompositor {
                    node: comp.node.node,
                    layers_pin,
                    layers,
                    created,
                },
            );
        }

        // Wanted states.
        let wanted = collection.wanted_sources();
        for (id, bound) in &self.sources {
            let state = if wanted.contains(id) {
                Lifecycle::Active
            } else {
                Lifecycle::Standby
            };
            set_state(graph, bound.source.node, state)?;
            for (f, b) in &bound.filters {
                let s = if bound.chain.contains(f) {
                    state
                } else {
                    Lifecycle::Standby
                };
                set_state(graph, b.node, s)?;
            }
        }
        for b in self.compositors.values() {
            set_state(graph, b.node.node, Lifecycle::Active)?;
        }
        Ok(report)
    }
}

/// The layers of a compositor as the running node sees them: each with the index of
/// its link among the node's scheduled links on `layers_pin` (the order
/// `rbs_graph::runtime::Inputs` presents them in). Layers whose producer is not
/// scheduled are left out.
///
/// Pass the plan the runtime actually runs, `ApplyReport::plan` from
/// `rbs_graph::runtime::Controller::apply`: it can leave out nodes whose `prepare`
/// failed, so it can differ from `GraphDescription::plan`.
pub fn resolve<'a>(
    compositor: &'a BoundCompositor,
    plan: &SchedulePlan,
) -> Vec<(usize, &'a BoundLayer)> {
    let inputs: Vec<NodeId> = plan
        .links
        .iter()
        .filter(|l| l.to.node == compositor.node && l.to.pin == compositor.layers_pin)
        .map(|l| l.from.node)
        .collect();
    compositor
        .layers
        .iter()
        .filter_map(|layer| {
            inputs
                .iter()
                .position(|&p| p == layer.producer)
                .map(|index| (index, layer))
        })
        .collect()
}
