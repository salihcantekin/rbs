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
//!
//! With [`SceneBinder::with_audio`], each canvas also gets a mixer node (ADR-0039)
//! fed by the audio-only sources visible in its program scene. The application
//! attaches the canvas's audio encoder to [`SceneBinder::mixer`].

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use rbs_core::lifecycle::Lifecycle;
use rbs_graph::description::{
    GraphDescription, GraphError, Link, NodeId, PartitionId, SchedulePlan,
};
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId};

use crate::collection::{SceneCollection, SceneError};
use crate::flatten::Affine2;
use crate::model::{CanvasId, Crop, FilterId, ItemId, ItemStyle, SceneId, Settings, SourceId};

/// Node type id of the canvas compositor.
pub const COMPOSITOR_TYPE: &str = "org.rbs.compositor";
/// Node type id of the audio mixer (ADR-0037), bound when audio is enabled.
pub const MIXER_TYPE: &str = "org.rbs.audio.mixer";

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
    /// Shape, border and shadow, as in [`FlatLayer::style`](crate::FlatLayer::style).
    pub style: ItemStyle,
    /// The item's own box size, the units of `style`.
    pub box_size: [f32; 2],
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

/// One input of the mixer.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoundAudio {
    /// The source.
    pub source: SourceId,
    /// The node whose audio the mixer hears: the tail of the source's filter chain
    /// (the source's node when it has no enabled filter). It is in the mixer's
    /// partition, so it is also the strip's producer (ADR-0037, ADR-0082).
    pub producer: NodeId,
}

/// The bound mixer and what it mixes.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundMixer {
    /// Its node.
    pub node: NodeId,
    /// Its inputs, in source id order. The mixer does not hear an input without a
    /// strip (ADR-0037), so the application gives each one a strip.
    pub inputs: Vec<BoundAudio>,
    /// True if this sync created the node (attach the audio encoder to it).
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
    /// The source's node type is not a `Source` with a video output, nor (with audio
    /// enabled) with an audio output; it is left out.
    NoVideoOutput(SourceId),
    /// The filter's node type is not a `VideoFilter` with video input and output; it
    /// is skipped.
    NoVideoPins(FilterId),
    /// The filter's node type is not an `AudioFilter` with audio input and output (an
    /// audio source takes only audio filters); it is skipped.
    NoAudioPins(FilterId),
    /// The filter's key source (setting `key_source`) is deleted, has no bound audio,
    /// is the filter's own source, or would make a loop of key links (the link of the
    /// filter with the highest id in the loop is the one left out). The filter passes
    /// its sound unchanged until the key is valid (ADR-0095).
    KeyUnavailable(FilterId),
    /// The source has video and audio outputs; only its video is bound. Its audio
    /// would reach the mixer through a partition queue, whose reader the mixer sees
    /// as the producer, which the binder cannot name yet.
    AudioOnVideoSource(SourceId),
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
    /// Source nodes this sync created: the application adopts an instance for each
    /// into the node's partition (a source whose settings changed gets a new node).
    pub created_sources: Vec<(SourceId, NodeId)>,
    /// Source nodes this sync removed, or that someone else removed since the last
    /// sync: the application takes their instances back once the runtime released
    /// them (after the apply that removes them has been collected).
    pub retired_sources: Vec<NodeId>,
    /// Sources whose settings changed: `(source, old node, new node)`. Both nodes are
    /// also in `retired_sources` and `created_sources`. An exclusive device (most
    /// cameras) cannot open the new instance while the old one runs, so the
    /// application applies break-before-make: first without the new node, then with
    /// it (scene sources spec).
    pub replaced_sources: Vec<(SourceId, NodeId, NodeId)>,
    /// Filter nodes this sync created.
    pub created_filters: Vec<(FilterId, NodeId)>,
    /// Filter nodes this sync removed, or that someone else removed.
    pub retired_filters: Vec<NodeId>,
    /// Bound filters whose node was kept but whose settings changed since the last
    /// sync (ADR-0062 decision 3): the application sends the new values to the
    /// running instance; the node is never rebuilt for it.
    pub changed_filters: Vec<(FilterId, NodeId)>,
    /// The mixer of each canvas, when audio is enabled.
    pub mixers: BTreeMap<CanvasId, BoundMixer>,
    /// Mixer nodes this sync removed (their canvas is gone), with every link into
    /// and out of them.
    pub retired_mixers: Vec<NodeId>,
}

/// Why binding failed. The graph and the binder are unchanged.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindError {
    /// The compositor node type is not installed, is not a `Compositor`, or lacks a
    /// many-input video pin or a video output.
    NoCompositor,
    /// Audio is enabled but the mixer node type is not installed, is not an
    /// `AudioMixer`, or lacks a many-input audio pin or an audio output.
    NoMixer,
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
            BindError::NoMixer => write!(f, "no usable {MIXER_TYPE} node type"),
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
    /// First audio output.
    audio: Option<String>,
    /// The audio input named `key` of a filter that takes another source's signal
    /// (ADR-0095).
    key: Option<String>,
}

/// The name of the audio input pin that carries a key signal (ADR-0095).
const KEY_PIN: &str = "key";

/// The setting that names the key source: an `Int` source id; absent means none.
const KEY_SETTING: &str = "key_source";

fn pins_of(descriptor: &NodeDescriptor, kind: LinkKind, many_input: bool) -> Pins {
    let first_output = |kind: LinkKind| {
        descriptor
            .outputs
            .iter()
            .find(|p| p.kind == kind)
            .map(|p| p.name.to_string())
    };
    Pins {
        input: descriptor
            .inputs
            .iter()
            .enumerate()
            .find(|(_, p)| p.kind == kind && (p.many || !many_input))
            .map(|(i, p)| (i, p.name.to_string())),
        output: first_output(kind),
        audio: first_output(LinkKind::Audio),
        key: descriptor
            .inputs
            .iter()
            .find(|p| p.kind == LinkKind::Audio && p.name == KEY_PIN)
            .map(|p| p.name.to_string()),
    }
}

/// Whether `to` can be reached from `from` along `deps` (source depends on its keys).
fn reaches(deps: &BTreeMap<SourceId, BTreeSet<SourceId>>, from: SourceId, to: SourceId) -> bool {
    let mut seen = BTreeSet::new();
    let mut stack = vec![from];
    while let Some(s) = stack.pop() {
        if s == to {
            return true;
        }
        if seen.insert(s)
            && let Some(next) = deps.get(&s)
        {
            stack.extend(next.iter().copied());
        }
    }
    false
}

fn video_pins(descriptor: &NodeDescriptor, many_input: bool) -> Pins {
    pins_of(descriptor, LinkKind::Video, many_input)
}

#[derive(Debug, Clone)]
struct BoundNode {
    node: NodeId,
    pins: Pins,
}

#[derive(Debug, Clone)]
struct BoundSource {
    source: BoundNode,
    /// The settings the node was made for; other settings get a new node.
    settings: Settings,
    /// The settings each bound filter was last synced with.
    filter_settings: BTreeMap<FilterId, Settings>,
    filters: BTreeMap<FilterId, BoundNode>,
    /// Enabled, bound filters in chain order.
    chain: Vec<FilterId>,
    /// Links this binder made for the chain.
    chain_links: Vec<Link>,
    /// The key links of this source's filters: the producer node they come from and
    /// the link (ADR-0095).
    key_links: BTreeMap<FilterId, (NodeId, Link)>,
}

impl BoundSource {
    fn tail(&self) -> &BoundNode {
        self.chain
            .last()
            .and_then(|f| self.filters.get(f))
            .unwrap_or(&self.source)
    }
}

#[derive(Debug, Clone)]
struct BoundComp {
    node: BoundNode,
    /// Links this binder made into the layers pin.
    links: Vec<Link>,
}

#[derive(Debug, Clone)]
struct BoundMix {
    node: BoundNode,
    /// Links this binder made into the mixer's input pin.
    links: Vec<Link>,
}

/// Keeps a graph description in step with a scene collection (scene model spec,
/// "Graph binding").
pub struct SceneBinder {
    types: Box<dyn NodeTypes + Send>,
    /// The partition of the mixer and of audio-only sources; `None`: audio is not
    /// bound.
    audio: Option<PartitionId>,
    state: State,
}

#[derive(Debug, Clone, Default)]
struct State {
    sources: BTreeMap<SourceId, BoundSource>,
    compositors: BTreeMap<(CanvasId, Role), BoundComp>,
    mixers: BTreeMap<CanvasId, BoundMix>,
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
            audio: None,
            state: State::default(),
        }
    }

    /// Also binds audio (ADR-0039): one mixer node per canvas and the audio-only
    /// source nodes go into `partition`, an audio partition (ADR-0037; never
    /// `PartitionId::RENDER`, which ticks at frame rate). Each mixer gets a link from
    /// every audio-only source visible in its canvas's program scene.
    pub fn with_audio(mut self, partition: PartitionId) -> Self {
        self.audio = Some(partition);
        self
    }

    /// The mixer node of `canvas`, when audio is bound. That canvas's audio encoder
    /// connects to its output.
    pub fn mixer(&self, canvas: CanvasId) -> Option<NodeId> {
        self.state.mixers.get(&canvas).map(|m| m.node.node)
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
        let report = draft.sync(&*self.types, self.audio, collection, &mut draft_graph)?;
        *graph = draft_graph;
        self.state = draft;
        Ok(report)
    }
}

impl State {
    /// Forgets bindings whose nodes were removed from the graph by someone else, so
    /// they are bound again instead of failing forever.
    fn heal(
        &mut self,
        graph: &mut GraphDescription,
        report: &mut BindReport,
    ) -> Result<(), GraphError> {
        let lost: Vec<SourceId> = self
            .sources
            .iter()
            .filter(|(_, b)| graph.node(b.source.node).is_none())
            .map(|(id, _)| *id)
            .collect();
        for id in lost {
            if let Some(b) = self.sources.remove(&id) {
                report.retired_sources.push(b.source.node);
                // Its filters would be orphans; remove them so the source is bound
                // again from scratch.
                for f in b.filters.values() {
                    retire(graph, f.node)?;
                    report.retired_filters.push(f.node);
                }
            }
        }
        for b in self.sources.values_mut() {
            let before = b.filters.len();
            b.filters.retain(|_, f| {
                let present = graph.node(f.node).is_some();
                if !present {
                    report.retired_filters.push(f.node);
                }
                present
            });
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
        self.mixers.retain(|_, m| graph.node(m.node.node).is_some());
        for m in self.mixers.values_mut() {
            m.links.retain(|l| graph.links().contains(l));
        }
        Ok(())
    }

    /// Links the `key` input of every filter that has one (a ducker, ADR-0095) from the
    /// tail of its key source's chain, the signal before that source's fader. A key that
    /// is none, deleted, without bound audio, the filter's own source or part of a loop
    /// is left unlinked (the filter hears silence and so changes nothing); loops are
    /// found before any link is made, over all key settings in filter id order, so the
    /// result does not depend on the order of edits.
    fn sync_key_links(
        &mut self,
        graph: &mut GraphDescription,
        collection: &SceneCollection,
        report: &mut BindReport,
    ) -> Result<(), BindError> {
        // (filter, owner source, wanted key source) for every bound filter with a key pin.
        let mut wanted: Vec<(FilterId, SourceId, Option<SourceId>)> = Vec::new();
        for (owner, bound) in &self.sources {
            let Some(source) = collection.source(*owner) else {
                continue;
            };
            for f in &source.filters {
                if bound
                    .filters
                    .get(&f.id)
                    .is_some_and(|b| b.pins.key.is_some())
                {
                    let key = match f.settings.get(KEY_SETTING) {
                        Some(crate::model::SettingValue::Int(i)) => {
                            u32::try_from(*i).ok().map(SourceId::from_raw)
                        }
                        _ => None,
                    };
                    wanted.push((f.id, *owner, key));
                }
            }
        }
        wanted.sort_by_key(|(f, _, _)| *f);
        // Accept the edges in filter id order; one that closes a loop is left out.
        let mut deps: BTreeMap<SourceId, BTreeSet<SourceId>> = BTreeMap::new();
        let mut accepted: BTreeMap<FilterId, (NodeId, String)> = BTreeMap::new();
        for (fid, owner, key) in wanted.iter().copied() {
            let Some(key) = key else { continue };
            let producer = self
                .sources
                .get(&key)
                .filter(|k| k.source.pins.output.is_none())
                .and_then(|k| {
                    let tail = k.tail();
                    tail.pins.audio.clone().map(|out| (tail.node, out))
                });
            let loops = key == owner || reaches(&deps, key, owner);
            match (producer, loops) {
                (Some(p), false) => {
                    deps.entry(owner).or_default().insert(key);
                    accepted.insert(fid, p);
                }
                _ => report.problems.push(BindProblem::KeyUnavailable(fid)),
            }
        }
        for bound in self.sources.values_mut() {
            // Drop links that are no longer wanted or come from another node now (the
            // key source's chain tail changed).
            let stale: Vec<FilterId> = bound
                .key_links
                .iter()
                .filter(|(f, (node, _))| {
                    accepted.get(f).map(|(n, _)| n) != Some(node) || !bound.filters.contains_key(f)
                })
                .map(|(f, _)| *f)
                .collect();
            for f in stale {
                if let Some((_, link)) = bound.key_links.remove(&f) {
                    graph.disconnect(link);
                }
            }
            for (fid, filter) in &bound.filters {
                let (Some((producer, out)), Some(pin)) = (accepted.get(fid), &filter.pins.key)
                else {
                    continue;
                };
                if !bound.key_links.contains_key(fid) {
                    let link = graph.connect(*producer, out, filter.node, pin)?;
                    bound.key_links.insert(*fid, (*producer, link));
                }
            }
        }
        Ok(())
    }

    fn sync(
        &mut self,
        types: &dyn NodeTypes,
        audio: Option<PartitionId>,
        collection: &SceneCollection,
        graph: &mut GraphDescription,
    ) -> Result<BindReport, BindError> {
        let mut report = BindReport::default();
        self.heal(graph, &mut report)?;
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
        let mixer_desc = match audio {
            None => None,
            Some(_) => {
                let desc = types
                    .descriptor(&NodeTypeId::new(MIXER_TYPE))
                    .filter(|d| d.kind == NodeKind::AudioMixer)
                    .ok_or(BindError::NoMixer)?;
                let pins = pins_of(&desc, LinkKind::Audio, true);
                if pins.input.is_none() || pins.output.is_none() {
                    return Err(BindError::NoMixer);
                }
                Some((desc, pins))
            }
        };

        // Sources whose settings changed get a new node (same type and partition), so
        // the application creates an instance for the new settings. Their filters
        // stay; the chain is linked again to the new node.
        for (id, bound) in &mut self.sources {
            let Some(source) = collection.source(*id) else {
                continue;
            };
            if source.settings == bound.settings {
                continue;
            }
            let old = bound.source.node;
            let (descriptor, partition) = graph
                .node(old)
                .map(|n| (Arc::clone(&n.descriptor), n.partition))
                .ok_or(GraphError::UnknownNode(old))?;
            for link in bound.chain_links.drain(..) {
                graph.disconnect(link);
            }
            bound.chain.clear();
            retire(graph, old)?;
            let node = add_node(graph, descriptor)?;
            graph.set_partition(node, partition)?;
            bound.source.node = node;
            bound.settings = source.settings.clone();
            report.retired_sources.push(old);
            report.created_sources.push((*id, node));
            report.replaced_sources.push((*id, old, node));
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
                retire(graph, bound.source.node)?;
                report.retired_sources.push(bound.source.node);
                for f in bound.filters.values() {
                    retire(graph, f.node)?;
                    report.retired_filters.push(f.node);
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
                let audio_only = pins.output.is_none() && pins.audio.is_some();
                let usable = pins.output.is_some() || (audio.is_some() && audio_only);
                if descriptor.kind != NodeKind::Source || !usable {
                    report.problems.push(BindProblem::NoVideoOutput(source.id));
                    continue;
                }
                let node = add_node(graph, descriptor)?;
                if let (true, Some(partition)) = (audio_only, audio) {
                    graph.set_partition(node, partition)?;
                }
                report.created_sources.push((source.id, node));
                slot.insert(BoundSource {
                    source: BoundNode { node, pins },
                    settings: source.settings.clone(),
                    filter_settings: BTreeMap::new(),
                    filters: BTreeMap::new(),
                    chain: Vec::new(),
                    chain_links: Vec::new(),
                    key_links: BTreeMap::new(),
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
                bound.filter_settings.remove(&f);
                if let Some(b) = bound.filters.remove(&f) {
                    retire(graph, b.node)?;
                    report.retired_filters.push(b.node);
                }
            }
            // A source without a video output is an audio source (bound only with audio
            // enabled): its chain is an audio chain in the source's partition.
            let is_audio = bound.source.pins.output.is_none();
            for filter in &source.filters {
                if let Some(b) = bound.filters.get(&filter.id) {
                    if bound.filter_settings.get(&filter.id) != Some(&filter.settings) {
                        bound
                            .filter_settings
                            .insert(filter.id, filter.settings.clone());
                        report.changed_filters.push((filter.id, b.node));
                    }
                    continue;
                }
                let Some(descriptor) = types.descriptor(&filter.type_id) else {
                    report.problems.push(BindProblem::UnknownFilterType(
                        filter.id,
                        filter.type_id.clone(),
                    ));
                    continue;
                };
                let (kind, pins) = if is_audio {
                    (
                        NodeKind::AudioFilter,
                        pins_of(&descriptor, LinkKind::Audio, false),
                    )
                } else {
                    (NodeKind::VideoFilter, video_pins(&descriptor, false))
                };
                let usable = pins.input.is_some()
                    && if is_audio {
                        pins.audio.is_some()
                    } else {
                        pins.output.is_some()
                    };
                if descriptor.kind != kind || !usable {
                    report.problems.push(if is_audio {
                        BindProblem::NoAudioPins(filter.id)
                    } else {
                        BindProblem::NoVideoPins(filter.id)
                    });
                    continue;
                }
                let node = add_node(graph, descriptor)?;
                if let (true, Some(partition)) = (is_audio, audio) {
                    // The filter runs in the source's partition: no queue between them.
                    graph.set_partition(node, partition)?;
                }
                report.created_filters.push((filter.id, node));
                bound
                    .filter_settings
                    .insert(filter.id, filter.settings.clone());
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
                        let out = if is_audio {
                            &prev.pins.audio
                        } else {
                            &prev.pins.output
                        };
                        if let (Some(out), Some((_, input))) = (out, &next.pins.input) {
                            let link = graph.connect(prev.node, out, next.node, input)?;
                            bound.chain_links.push(link);
                        }
                        prev = next.clone();
                    }
                }
                bound.chain = chain;
            }
        }

        self.sync_key_links(graph, collection, &mut report)?;

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
                if let Some(bound) = self
                    .sources
                    .get(&layer.source)
                    .filter(|b| b.source.pins.output.is_some())
                {
                    layers.push(BoundLayer {
                        producer: bound.tail().node,
                        source: layer.source,
                        path: layer.path,
                        transform: layer.transform,
                        crop: layer.crop,
                        opacity: layer.opacity,
                        style: layer.style,
                        box_size: layer.box_size,
                    });
                }
            }
            // Producers keep wanted sources on this canvas scheduled (linked), including
            // hidden and off-scene ones, so devices stay open. Drawn layers still come
            // only from flatten; a faded (opacity 0) source is wanted and linked too.
            let mut producers: Vec<BoundNode> = Vec::new();
            let warm = match role {
                Role::Program => collection.warm_sources(canvas),
                Role::Preview => collection
                    .visible_sources(canvas, scene)
                    .map_err(BindError::Scene)?,
            };
            let ordered = layers.iter().map(|l| l.source).chain(warm.iter().copied());
            for source in ordered {
                if let Some(bound) = self
                    .sources
                    .get(&source)
                    .filter(|b| b.source.pins.output.is_some())
                {
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

        // One mixer per canvas; its inputs are the audio-only sources visible in the
        // canvas's program scene.
        if let (Some((desc, pins)), Some(partition)) = (mixer_desc, audio) {
            let canvases: BTreeSet<CanvasId> = collection.canvases().map(|c| c.id).collect();
            let unneeded: Vec<CanvasId> = self
                .mixers
                .keys()
                .copied()
                .filter(|c| !canvases.contains(c))
                .collect();
            for c in unneeded {
                if let Some(m) = self.mixers.remove(&c) {
                    retire(graph, m.node.node)?;
                    report.retired_mixers.push(m.node.node);
                }
            }
            for (id, bound) in &self.sources {
                if bound.source.pins.output.is_some() && bound.source.pins.audio.is_some() {
                    report.problems.push(BindProblem::AudioOnVideoSource(*id));
                }
            }
            for canvas in collection.canvases() {
                let mut created = false;
                let mix = match self.mixers.entry(canvas.id) {
                    std::collections::btree_map::Entry::Occupied(e) => e.into_mut(),
                    std::collections::btree_map::Entry::Vacant(e) => {
                        created = true;
                        let node = add_node(graph, Arc::clone(&desc))?;
                        graph.set_partition(node, partition)?;
                        e.insert(BoundMix {
                            node: BoundNode {
                                node,
                                pins: pins.clone(),
                            },
                            links: Vec::new(),
                        })
                    }
                };
                let heard: BTreeSet<SourceId> = match canvas.program {
                    Some(scene) => collection
                        .visible_sources(canvas.id, scene)
                        .map_err(BindError::Scene)?
                        .into_iter()
                        .collect(),
                    None => BTreeSet::new(),
                };
                let mut inputs = Vec::new();
                let mut outs: BTreeMap<NodeId, String> = BTreeMap::new();
                for id in &heard {
                    let Some(bound) = self.sources.get(id) else {
                        continue;
                    };
                    if bound.source.pins.output.is_none() {
                        // The mixer hears the tail of the audio chain (ADR-0082).
                        let tail = bound.tail();
                        if let Some(out) = &tail.pins.audio {
                            inputs.push(BoundAudio {
                                source: *id,
                                producer: tail.node,
                            });
                            outs.insert(tail.node, out.clone());
                        }
                    }
                }
                mix.links.retain(|link| {
                    let keep = outs.contains_key(&link.from.node);
                    if !keep {
                        graph.disconnect(*link);
                    }
                    keep
                });
                let linked: BTreeSet<NodeId> = mix.links.iter().map(|l| l.from.node).collect();
                if let Some((_, input)) = &mix.node.pins.input {
                    for (producer, out) in outs.iter().filter(|(p, _)| !linked.contains(p)) {
                        let link = graph.connect(*producer, out, mix.node.node, input)?;
                        mix.links.push(link);
                    }
                }
                report.mixers.insert(
                    canvas.id,
                    BoundMixer {
                        node: mix.node.node,
                        inputs,
                        created,
                    },
                );
            }
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
        for m in self.mixers.values() {
            set_state(graph, m.node.node, Lifecycle::Active)?;
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
///
/// The index is a hint for the plan given. Parameters sent to a running compositor can
/// meet an older or newer schedule (ADR-0023), so the node checks
/// `Inputs::source(pin, index) == Some(layer.producer)` and otherwise looks the
/// producer up with `Inputs::find`.
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
