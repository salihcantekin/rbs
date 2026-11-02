//! Binding a scene collection to a media graph description.
//!
//! The [`SceneBinder`] keeps a graph description in step with a collection: one node
//! per source (one instance however many scenes and canvases place it), one node per
//! filter chained after its source, and one compositor node per canvas for the program
//! scene (and one for the preview scene in studio mode). Node ids stay the same across
//! edits, so the runtime keeps running nodes across a swap (ADR-0020). The wanted
//! lifecycle state of every node comes from [`SceneCollection::wanted_sources`].
//!
//! The graph gets no output nodes from here: outputs (stream, recording, preview
//! panels, projectors) are attached by the application to
//! [`SceneBinder::compositor`], and a compositor without one is not scheduled.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use rbs_core::lifecycle::Lifecycle;
use rbs_graph::description::{GraphDescription, GraphError, Link, NodeId, SchedulePlan};
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeTypeId};

use crate::collection::{SceneCollection, SceneError};
use crate::flatten::{Affine2, FlatLayer};
use crate::model::{CanvasId, Crop, FilterId, ItemId, SourceId};

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
    /// Item path, as in [`FlatLayer::path`].
    pub path: Vec<ItemId>,
    /// Maps the unit square of the cropped source to canvas pixels.
    pub transform: Affine2,
    /// Source pixels cut before drawing.
    pub crop: Crop,
    /// 0.0..=1.0.
    pub opacity: f32,
}

/// Something the binder could not represent; the rest of the graph is still bound.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindProblem {
    /// No node type with this id is installed; the source is left out of the graph.
    UnknownSourceType(SourceId, NodeTypeId),
    /// No node type with this id is installed; the filter is skipped.
    UnknownFilterType(FilterId, NodeTypeId),
    /// The source's node type has no video output; it is left out.
    NoVideoOutput(SourceId),
    /// The filter's node type lacks a video input or output; it is skipped.
    NoVideoPins(FilterId),
}

/// What [`SceneBinder::sync`] produced.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BindReport {
    /// Layers of every bound compositor, bottom first.
    pub layers: BTreeMap<(CanvasId, Role), Vec<BoundLayer>>,
    /// Parts of the collection that could not be bound.
    pub problems: Vec<BindProblem>,
}

/// Why binding failed. The graph and the binder are unchanged.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindError {
    /// The compositor node type is not installed or has no many-input and output for
    /// video.
    NoCompositor,
    /// A graph edit was refused (for example the description was changed by someone
    /// else in a way that conflicts with the binding).
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

/// Pin names used to wire a node.
#[derive(Debug, Clone)]
struct Pins {
    input: Option<String>,
    output: Option<String>,
}

fn video_pins(descriptor: &NodeDescriptor, many_input: bool) -> Pins {
    Pins {
        input: descriptor
            .inputs
            .iter()
            .find(|p| p.kind == LinkKind::Video && (p.many || !many_input))
            .map(|p| p.name.to_string()),
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
}

impl BoundSource {
    fn tail(&self) -> &BoundNode {
        self.chain
            .last()
            .and_then(|f| self.filters.get(f))
            .unwrap_or(&self.source)
    }
}

/// Keeps a graph description in step with a scene collection (scene model spec,
/// "Graph binding").
pub struct SceneBinder {
    types: Box<dyn NodeTypes + Send>,
    sources: BTreeMap<SourceId, BoundSource>,
    compositors: BTreeMap<(CanvasId, Role), BoundNode>,
}

impl fmt::Debug for SceneBinder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SceneBinder")
            .field("sources", &self.sources)
            .field("compositors", &self.compositors)
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

fn retire(graph: &mut GraphDescription, id: NodeId) -> Result<(), GraphError> {
    set_state(graph, id, Lifecycle::Standby)?;
    graph.remove_node(id)?;
    Ok(())
}

fn links_into(graph: &GraphDescription, node: NodeId) -> Vec<Link> {
    graph
        .links()
        .iter()
        .copied()
        .filter(|l| l.to.node == node)
        .collect()
}

impl SceneBinder {
    /// A binder that looks node types up in `types`. Nothing is bound until
    /// [`SceneBinder::sync`].
    pub fn new(types: impl NodeTypes + Send + 'static) -> Self {
        Self {
            types: Box::new(types),
            sources: BTreeMap::new(),
            compositors: BTreeMap::new(),
        }
    }

    /// The node of a bound source.
    pub fn source_node(&self, id: SourceId) -> Option<NodeId> {
        self.sources.get(&id).map(|b| b.source.node)
    }

    /// The node of a bound filter.
    pub fn filter_node(&self, source: SourceId, id: FilterId) -> Option<NodeId> {
        self.sources
            .get(&source)
            .and_then(|b| b.filters.get(&id))
            .map(|b| b.node)
    }

    /// The compositor node drawing `role` of `canvas`, if that canvas has a scene in
    /// that role. Outputs connect to its video output.
    pub fn compositor(&self, canvas: CanvasId, role: Role) -> Option<NodeId> {
        self.compositors.get(&(canvas, role)).map(|b| b.node)
    }

    /// Updates `graph` so it matches `collection`, keeping node ids of everything that
    /// still exists. On error neither `graph` nor the binder changes.
    ///
    /// Nodes are created Standby; sources the scenes want (`wanted_sources`) and their
    /// enabled filters, and every bound compositor, are set Active; the rest Standby.
    /// Nodes for removed sources, filters and compositors are moved to Standby and
    /// removed. Links: each source's enabled filters are chained in order; each
    /// compositor gets one link per distinct producer in its scene, in order of first
    /// appearance, and links it no longer needs are removed.
    pub fn sync(
        &mut self,
        collection: &SceneCollection,
        graph: &mut GraphDescription,
    ) -> Result<BindReport, BindError> {
        let mut draft_graph = graph.clone();
        let mut draft = SceneBinderState {
            sources: self.sources.clone(),
            compositors: self.compositors.clone(),
        };
        let report = draft.sync(&*self.types, collection, &mut draft_graph)?;
        *graph = draft_graph;
        self.sources = draft.sources;
        self.compositors = draft.compositors;
        Ok(report)
    }
}

/// The mutable part of a binder, so `sync` can work on a copy.
struct SceneBinderState {
    sources: BTreeMap<SourceId, BoundSource>,
    compositors: BTreeMap<(CanvasId, Role), BoundNode>,
}

impl SceneBinderState {
    fn sync(
        &mut self,
        types: &dyn NodeTypes,
        collection: &SceneCollection,
        graph: &mut GraphDescription,
    ) -> Result<BindReport, BindError> {
        let mut report = BindReport::default();
        let compositor_type = NodeTypeId::new(COMPOSITOR_TYPE);
        let compositor_desc = types
            .descriptor(&compositor_type)
            .ok_or(BindError::NoCompositor)?;
        let compositor_pins = video_pins(&compositor_desc, true);
        if compositor_pins.input.is_none() || compositor_pins.output.is_none() {
            return Err(BindError::NoCompositor);
        }

        // Sources and filters that are gone.
        let gone: Vec<SourceId> = self
            .sources
            .keys()
            .copied()
            .filter(|id| collection.source(*id).is_none())
            .collect();
        for id in gone {
            if let Some(bound) = self.sources.remove(&id) {
                for f in bound.filters.values() {
                    retire(graph, f.node)?;
                }
                retire(graph, bound.source.node)?;
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
                if pins.output.is_none() {
                    report.problems.push(BindProblem::NoVideoOutput(source.id));
                    continue;
                }
                let node = add_node(graph, descriptor)?;
                slot.insert(BoundSource {
                    source: BoundNode { node, pins },
                    filters: BTreeMap::new(),
                    chain: Vec::new(),
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
                if pins.input.is_none() || pins.output.is_none() {
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
                // Unhook the old chain: every link into one of this source's filters.
                for b in bound.filters.values() {
                    for link in links_into(graph, b.node) {
                        graph.disconnect(link);
                    }
                }
                let mut prev = bound.source.clone();
                for f in &chain {
                    if let Some(next) = bound.filters.get(f) {
                        if let (Some(out), Some(input)) = (&prev.pins.output, &next.pins.input) {
                            graph.connect(prev.node, out, next.node, input)?;
                        }
                        prev = next.clone();
                    }
                }
                bound.chain = chain;
            }
        }

        // Compositors and their layers.
        let mut needed: BTreeMap<(CanvasId, Role), crate::model::SceneId> = BTreeMap::new();
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
                retire(graph, b.node)?;
            }
        }
        for (&(canvas, role), &scene) in &needed {
            let comp = match self.compositors.entry((canvas, role)) {
                std::collections::btree_map::Entry::Occupied(e) => e.get().clone(),
                std::collections::btree_map::Entry::Vacant(e) => {
                    let node = add_node(graph, Arc::clone(&compositor_desc))?;
                    e.insert(BoundNode {
                        node,
                        pins: compositor_pins.clone(),
                    })
                    .clone()
                }
            };
            let flat: Vec<FlatLayer> = collection
                .flatten(canvas, scene)
                .map_err(BindError::Scene)?;
            let mut layers = Vec::with_capacity(flat.len());
            let mut producers: Vec<BoundNode> = Vec::new();
            for layer in flat {
                let Some(bound) = self.sources.get(&layer.source) else {
                    continue;
                };
                let tail = bound.tail().clone();
                if !producers.iter().any(|p| p.node == tail.node) {
                    producers.push(tail.clone());
                }
                layers.push(BoundLayer {
                    producer: tail.node,
                    source: layer.source,
                    path: layer.path,
                    transform: layer.transform,
                    crop: layer.crop,
                    opacity: layer.opacity,
                });
            }
            let wanted: BTreeSet<NodeId> = producers.iter().map(|p| p.node).collect();
            let current = links_into(graph, comp.node);
            for link in &current {
                if !wanted.contains(&link.from.node) {
                    graph.disconnect(*link);
                }
            }
            let linked: BTreeSet<NodeId> = links_into(graph, comp.node)
                .iter()
                .map(|l| l.from.node)
                .collect();
            if let Some(input) = &comp.pins.input {
                for p in producers.iter().filter(|p| !linked.contains(&p.node)) {
                    if let Some(out) = &p.pins.output {
                        graph.connect(p.node, out, comp.node, input)?;
                    }
                }
            }
            report.layers.insert((canvas, role), layers);
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
            set_state(graph, b.node, Lifecycle::Active)?;
        }
        Ok(report)
    }
}

/// The layers of `compositor` as the running compositor node sees them: each with the
/// index of its link among the node's scheduled input links (the order
/// `rbs_graph::runtime::Inputs` presents them in). Layers whose producer is not
/// scheduled (for example its `prepare` failed) are left out.
pub fn resolve<'a>(
    compositor: NodeId,
    layers: &'a [BoundLayer],
    plan: &SchedulePlan,
) -> Vec<(usize, &'a BoundLayer)> {
    let inputs: Vec<NodeId> = plan
        .links
        .iter()
        .filter(|l| l.to.node == compositor && l.kind == LinkKind::Video)
        .map(|l| l.from.node)
        .collect();
    layers
        .iter()
        .filter_map(|layer| {
            inputs
                .iter()
                .position(|&p| p == layer.producer)
                .map(|index| (index, layer))
        })
        .collect()
}
