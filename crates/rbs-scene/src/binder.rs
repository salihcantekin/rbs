//! Binding a scene collection to a media graph description.
//!
//! The [`SceneBinder`] keeps a graph description in step with a collection: one node
//! per source (one instance however many scenes and canvases place it), one node per
//! filter chained after its source, and one compositor node per canvas for the program
//! scene (and one for the preview scene in studio mode). Node ids stay the same across
//! edits, so the runtime keeps running nodes across a swap (ADR-0020). The wanted
//! lifecycle state of every node comes from [`SceneCollection::running_sources`]:
//! capture sources and stills from [`SceneCollection::wanted_sources`], and media
//! and browser sources only while a shown scene draws them (ADR-0107).
//!
//! The binder owns the nodes and links it creates and touches nothing else. Outputs
//! (stream, recording, preview panels, projectors) are attached by the application to
//! [`SceneBinder::compositor`]; a compositor without one is not scheduled.
//!
//! With [`SceneBinder::with_audio`], each canvas also gets a mixer node (ADR-0039)
//! fed by the sources with audio that are visible in its program scene. An
//! audio-only source is linked from its filter-chain tail. A source with picture
//! and sound is two nodes (ADR-0106): its picture node in the render partition and
//! its sound node in the audio partition, and the mixer is linked from the sound
//! node. The application attaches the canvas's audio encoder to
//! [`SceneBinder::mixer`].

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use rbs_core::lifecycle::Lifecycle;
use rbs_core::time::{Clock, Timestamp};

use crate::activity::{ActivityTraits, Level, OnReturn};
use rbs_graph::description::{
    GraphDescription, GraphError, Link, NodeId, PartitionId, SchedulePlan,
};
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId};

use crate::collection::{SceneCollection, SceneError};
use crate::flatten::Affine2;
use crate::flatten::ScrollKey;
use crate::model::{
    CanvasId, Crop, FilterId, ItemId, ItemStyle, SceneId, Scroll, Settings, SourceId,
};

mod cue_levels;
mod group_spans;
mod linger;
mod turned_on;
use turned_on::{drawn, note_joined, turned_on};

pub use group_spans::{GroupMark, SpanLimits, SpanRefusal};

/// Node type id of the canvas compositor.
pub const COMPOSITOR_TYPE: &str = "org.rbs.compositor";
/// Node type id of the audio mixer (ADR-0037), bound when audio is enabled.
pub const MIXER_TYPE: &str = "org.rbs.audio.mixer";

/// Looks up the descriptor of a node type (from the adapter registry or plugin
/// manifests). Any `Fn(&NodeTypeId) -> Option<Arc<NodeDescriptor>>` works.
pub trait NodeTypes {
    /// The descriptor of `type_id`, or `None` if no such type is installed.
    fn descriptor(&self, type_id: &NodeTypeId) -> Option<Arc<NodeDescriptor>>;

    /// The sound node type of a picture-and-sound source type (ADR-0106 decision 2),
    /// or `None` when `picture` has no sound node. With audio bound, the binder gives
    /// each source of such a type a second node of this type in the audio partition,
    /// and every mixer that hears the source is linked from it.
    fn sound_type(&self, picture: &NodeTypeId) -> Option<NodeTypeId> {
        let _ = picture;
        None
    }

    /// Whether a source of `type_id` takes every settings change in place: the binder
    /// keeps its node and reports it in [`BindReport::changed_sources`] instead of
    /// replacing it (ADR-0129 decision 5). False unless a type says so.
    fn live_settings(&self, _type_id: &NodeTypeId) -> bool {
        false
    }

    /// The settings keys of `type_id` that a running source takes in place (ADR-0108
    /// decision 12). When only these keys differ, the binder keeps the source's nodes
    /// and reports the source and the changed keys in [`BindReport::live_changed`].
    /// No keys unless a type says so.
    fn live_keys(&self, _type_id: &NodeTypeId) -> &'static [&'static str] {
        &[]
    }

    /// What `type_id` supports for when a source runs and what it does when it
    /// returns (ADR-0152 decision 5). [`ActivityTraits::PLACED`] unless a type says
    /// otherwise: Active while placed, linked while warm, as capture and plugins
    /// are today.
    fn activity(&self, _type_id: &NodeTypeId) -> ActivityTraits {
        ActivityTraits::PLACED
    }
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
    /// The scene a held stream shows (ADR-0124 phase 2b), bound only while asked
    /// for with [`SceneBinder::sync_held`].
    Hold,
    /// The scene coming on air while a transition runs (ADR-0128 decision 5): the
    /// canvas's program scene drawn whole, so the program compositor can draw it as
    /// one layer over the outgoing scene. Bound only while asked for with
    /// [`SceneBinder::sync_transition`].
    Incoming,
}

/// The scene going off air on one canvas while a transition runs (ADR-0128 decision
/// 6). It names sources, not a scene: a held program transitioned to itself has the
/// same scene id going out and coming in (ADR-0063 decision 3).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingScene {
    /// The sources the outgoing picture draws: kept linked to the program compositor
    /// and running for the transition's length.
    pub drawn: BTreeSet<SourceId>,
    /// The sources the outgoing scene is heard with: kept in the program mixer.
    pub heard: BTreeSet<SourceId>,
    /// False when the transition draws without an incoming compositor (Move: the
    /// program compositor draws the incoming layers itself, decision 5).
    pub incoming: bool,
    /// The scene the incoming compositor draws while the program stays as it is (a
    /// manual transition, ADR-0135 decision 2); its heard sources join the program
    /// mixer. `None`: the canvas's program scene.
    pub incoming_scene: Option<SceneId>,
    /// Cue ids captured for the live outgoing program of a manual transition.
    /// Other roles continue to use their live cue set.
    pub program_cues: Option<BTreeSet<ItemId>>,
    /// The stinger's source (ADR-0140 decision 4): running, on air, linked to the
    /// program compositor and heard in the program mixer while the entry exists.
    pub stinger: Option<SourceId>,
}

impl Default for OutgoingScene {
    /// Nothing kept, with an incoming compositor.
    fn default() -> Self {
        Self::new(BTreeSet::new(), BTreeSet::new())
    }
}

impl OutgoingScene {
    /// The outgoing picture's `drawn` sources and its `heard` ones, with an incoming
    /// compositor.
    pub fn new(drawn: BTreeSet<SourceId>, heard: BTreeSet<SourceId>) -> Self {
        Self {
            drawn,
            heard,
            incoming: true,
            incoming_scene: None,
            program_cues: None,
            stinger: None,
        }
    }

    /// A manual transition towards `scene` (ADR-0135 decision 2): the incoming
    /// compositor draws `scene` and the program mixer also hears it; nothing of the
    /// program is kept beyond what the program scene binds.
    pub fn towards(scene: SceneId) -> Self {
        Self {
            drawn: BTreeSet::new(),
            heard: BTreeSet::new(),
            incoming: true,
            incoming_scene: Some(scene),
            program_cues: None,
            stinger: None,
        }
    }

    /// The same entry drawing the canvas's program scene again: what a manual
    /// transition keeps after the program change, with the outgoing sources.
    pub fn with_program(self, drawn: BTreeSet<SourceId>, heard: BTreeSet<SourceId>) -> Self {
        Self {
            drawn,
            heard,
            incoming: self.incoming,
            incoming_scene: None,
            program_cues: None,
            stinger: self.stinger,
        }
    }

    /// The outgoing picture's sources without an incoming compositor (Move).
    pub fn without_incoming(drawn: BTreeSet<SourceId>, heard: BTreeSet<SourceId>) -> Self {
        Self {
            drawn,
            heard,
            incoming: false,
            incoming_scene: None,
            program_cues: None,
            stinger: None,
        }
    }

    /// The same entry with `source` played as the stinger over it (ADR-0140).
    #[must_use]
    pub fn with_stinger(mut self, source: Option<SourceId>) -> Self {
        self.stinger = source;
        self
    }

    /// Freeze only the outgoing program occurrence's cue visibility.
    #[must_use]
    pub fn with_program_cues(mut self, cues: BTreeSet<ItemId>) -> Self {
        self.program_cues = Some(cues);
        self
    }
}

fn role_cues<'a>(
    canvas: CanvasId,
    role: Role,
    live: &'a BTreeSet<ItemId>,
    outgoing: &'a BTreeMap<CanvasId, &OutgoingScene>,
) -> &'a BTreeSet<ItemId> {
    if role == Role::Program {
        outgoing
            .get(&canvas)
            .and_then(|o| o.program_cues.as_ref())
            .unwrap_or(live)
    } else {
        live
    }
}

/// A hold scene asked for on one canvas (ADR-0124 phase 2b).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HoldScene {
    /// The scene drawn while held.
    pub scene: SceneId,
    /// Also bind a hold mixer that hears the scene's sources.
    pub sound: bool,
}

impl HoldScene {
    /// `scene`, with its sound when `sound`.
    pub fn new(scene: SceneId, sound: bool) -> Self {
        Self { scene, sound }
    }
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
    /// The scroll in canvas pixels, as in [`FlatLayer::scroll`](crate::FlatLayer::scroll)
    /// (ADR-0150).
    pub scroll: Option<Scroll>,
    /// Its run, as in [`FlatLayer::scroll_key`](crate::FlatLayer::scroll_key).
    pub scroll_key: Option<ScrollKey>,
    /// Its group, when that has group Scroll on this canvas (ADR-0193).
    pub group: Option<GroupMark>,
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
    /// The strip's producer (ADR-0037), in the mixer's partition.
    ///
    /// For an audio-only source this is the tail of its audio filter chain (the
    /// source node when no filter is enabled) (ADR-0082). For a picture-and-sound
    /// source it is the source's sound node (ADR-0106 decision 2); there is no
    /// audio-filter chain on that path.
    pub producer: NodeId,
}

/// The two nodes of a bound picture-and-sound source (ADR-0106 decision 2).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PictureSound {
    /// The source.
    pub source: SourceId,
    /// Its picture node, in the render partition (the node in `created_sources`).
    pub picture: NodeId,
    /// Its sound node, in the audio partition. The application gives it the sound
    /// half of the instance it made for the picture node.
    pub sound: NodeId,
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
    /// With audio bound, the source's node has both a video and an audio output but
    /// its type names no sound node (ADR-0106 decisions 2 and 7): its sound is not
    /// mixed. A camera microphone stays a companion source (ADR-0096).
    AudioOnVideoSource(SourceId),
    /// The source's saved activity policy uses a value its type does not allow
    /// (ADR-0152 decision 10): the type's default is in force and the saved value is
    /// kept. The host shows it as a source warning (`activity_unsupported`).
    ActivityUnsupported(SourceId),
    /// A group with group Scroll gets no span on a canvas (ADR-0193 decision 4): its
    /// members are drawn transparent there.
    GroupSpanRefused(CanvasId, SceneId, crate::model::GroupId, SpanRefusal),
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
    /// them (after the apply that removes them has been collected). A sound node is
    /// listed too; it always goes with its picture node.
    pub retired_sources: Vec<NodeId>,
    /// Sources whose settings changed: `(source, old node, new node)`. Both nodes are
    /// also in `retired_sources`, and a new picture or source node in
    /// `created_sources`. A picture-and-sound source has one entry for its picture
    /// node and one for its sound node (ADR-0106 decision 2). An exclusive device
    /// (most cameras) cannot open the new instance while the old one runs, so the
    /// application applies break-before-make: first without the new nodes, then with
    /// them (scene sources spec).
    pub replaced_sources: Vec<(SourceId, NodeId, NodeId)>,
    /// Sources whose settings changed and whose type takes settings in place
    /// ([`NodeTypes::live_settings`]): the node was kept, and the application sends
    /// the new settings to whatever feeds it (the text worker, ADR-0129 decision 5).
    /// Never in `retired_sources`, `created_sources` or `replaced_sources`.
    pub changed_sources: Vec<(SourceId, NodeId)>,
    /// Sources whose changed settings are all live keys of their type
    /// ([`NodeTypes::live_keys`]): `(source, picture node, changed keys)`, the keys
    /// in order. The nodes were kept, and the application forwards the new values to
    /// the source's control handle (ADR-0108 decision 12). Never in
    /// `retired_sources`, `created_sources` or `replaced_sources`.
    pub live_changed: Vec<(SourceId, NodeId, Vec<String>)>,
    /// Filter nodes this sync created.
    pub created_filters: Vec<(FilterId, NodeId)>,
    /// Filter nodes this sync removed, or that someone else removed.
    pub retired_filters: Vec<NodeId>,
    /// Bound filters whose node was kept but whose settings changed since the last
    /// sync (ADR-0062 decision 3): the application sends the new values to the
    /// running instance; the node is never rebuilt for it.
    pub changed_filters: Vec<(FilterId, NodeId)>,
    /// Video filters that joined their source's chain in this sync (turned on or added)
    /// while a shown layer drew the source both before and after it: the picture the
    /// filter receives was on screen up to now. Background removal keeps it until its
    /// first mask on this edge only (ADR-0199); a scene switch or restart is not listed.
    pub turned_on_filters: Vec<(FilterId, NodeId)>,
    /// The mixer of each canvas, when audio is enabled.
    pub mixers: BTreeMap<CanvasId, BoundMixer>,
    /// Mixer nodes this sync removed (their canvas is gone, or a hold mixer no
    /// longer asked for), with every link into and out of them.
    pub retired_mixers: Vec<NodeId>,
    /// The hold mixer of each canvas whose hold scene asked for its sound
    /// ([`SceneBinder::sync_held`]). It has no monitor output and no meters.
    pub hold_mixers: BTreeMap<CanvasId, BoundMixer>,
    /// Every bound picture-and-sound source with its two nodes, in source id order
    /// (ADR-0106). Listed on every sync, not only when created. A sound node is not
    /// in `created_sources`: the application adopts the sound half of the source's
    /// one instance into it.
    pub picture_sound: Vec<PictureSound>,
    /// The on-air set (ADR-0107 decision 6): the media and browser sources that a
    /// program scene (with its cued items) or a bound hold scene draws on some canvas,
    /// with `visible` true. A temporal source that is Active but not in this set is
    /// drawn only by a preview scene: a media source is then cued (ADR-0108 decision
    /// 10). Listed on every sync. The host writes a media source's on-air state from
    /// its level ([`Self::level_changes`], [`SceneBinder::level_of`]), which also
    /// counts a source whose `run` is `placed`.
    pub on_air_sources: BTreeSet<SourceId>,
    /// The sources whose level or effective `on_return` changed since the last sync
    /// (ADR-0152 decision 7): `(source, level, on_return)`, in source id order,
    /// `on_return` `None` for a type without one. A source that left every scene, or
    /// was deactivated, is listed once as [`Level::Unplaced`]; a deleted one is not.
    /// Empty when nothing changed. The host writes each to the source's control
    /// handle on the control path before it applies this sync, and writes a created
    /// or replaced node's current values when it adopts it
    /// ([`SceneBinder::level_of`]).
    pub level_changes: Vec<(SourceId, Level, Option<OnReturn>)>,
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
    /// The sound node of a picture-and-sound source, in the audio partition
    /// (ADR-0106). Lives and dies with `source`.
    sound: Option<BoundNode>,
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

    /// The node a compositor links. Video filters stay off while nothing draws the
    /// source above opacity 0, so the link is the source itself and a later fade
    /// still has a picture. An audio chain has no video output and always uses the tail.
    fn producer(&self, drawn: bool) -> &BoundNode {
        if self.source.pins.output.is_some() && !drawn {
            &self.source
        } else {
            self.tail()
        }
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
    /// The clock linger is counted on (ADR-0152 decision 8); none: no linger.
    clock: Option<Arc<dyn Clock>>,
    /// The partition of the mixer and of audio-only sources; `None`: audio is not
    /// bound.
    audio: Option<PartitionId>,
    /// Group span limits (ADR-0193 decision 4).
    spans: SpanLimits,
    state: State,
}

#[derive(Debug, Clone, Default)]
struct State {
    sources: BTreeMap<SourceId, BoundSource>,
    compositors: BTreeMap<(CanvasId, Role), BoundComp>,
    mixers: BTreeMap<CanvasId, BoundMix>,
    hold_mixers: BTreeMap<CanvasId, BoundMix>,
    /// Each source's last delivered level and effective `on_return` (ADR-0152
    /// decision 7); a source absent here is Unplaced or Off.
    levels: BTreeMap<SourceId, (Level, Option<OnReturn>)>,
    /// Lingering sources (ADR-0152 decision 3).
    linger: linger::Linger,
    /// The sources a shown layer drew after the last sync (for `turned_on_filters`).
    drawn: BTreeSet<SourceId>,
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
            clock: None,
            audio: None,
            spans: SpanLimits::default(),
            state: State::default(),
        }
    }

    /// Limits group span intermediates (ADR-0193 decision 4); the application passes
    /// the device's largest texture side in `limits.max_side`.
    pub fn with_span_limits(mut self, limits: SpanLimits) -> Self {
        self.spans = limits;
        self
    }

    /// Counts linger (ADR-0152 `linger_s`) on `clock`, read once per sync. Without a
    /// clock no source lingers.
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = Some(clock);
        self
    }

    /// When the soonest linger ends; `None` while no source lingers, so the host arms
    /// no wake (P13). The host syncs again at or after that time to end it.
    pub fn linger_end(&self) -> Option<Timestamp> {
        self.state.linger.next_end()
    }

    /// How long until the soonest linger ends, from `now`; zero when it is due.
    pub fn linger_wait(&self, now: Timestamp) -> Option<std::time::Duration> {
        self.linger_end().map(|end| end.saturating_since(now))
    }

    /// Whether `source` is Active only because it lingers.
    pub fn is_lingering(&self, source: SourceId) -> bool {
        self.state.linger.contains(source)
    }

    /// Each lingering source and when its linger ends, on this binder's clock: what a
    /// studio restart hands over (ADR-0152 decision 8).
    pub fn lingers(&self) -> BTreeMap<SourceId, Timestamp> {
        self.state.linger.ends().clone()
    }

    /// Takes over the lingers of the binder this one replaces, with their ends moved
    /// to this binder's clock; before the first sync, which keeps those whose source
    /// is not running again, not Off and whose time is not up.
    pub fn with_lingers(mut self, lingers: BTreeMap<SourceId, Timestamp>) -> Self {
        self.state.linger.adopt(lingers);
        self
    }

    /// Also binds audio (ADR-0039): one mixer node per canvas and the audio-only
    /// source nodes go into `partition`, an audio partition (ADR-0037; never
    /// `PartitionId::RENDER`, which ticks at frame rate). Each mixer gets a link from
    /// every source with audio that is visible in its canvas's program scene. A
    /// source that also has video stays in the render partition; when its type names
    /// a sound node ([`NodeTypes::sound_type`]) that node goes into `partition` and
    /// is the one linked (ADR-0106).
    pub fn with_audio(mut self, partition: PartitionId) -> Self {
        self.audio = Some(partition);
        self
    }

    /// The mixer node of `canvas`, when audio is bound. That canvas's audio encoder
    /// connects to its output.
    pub fn mixer(&self, canvas: CanvasId) -> Option<NodeId> {
        self.state.mixers.get(&canvas).map(|m| m.node.node)
    }

    /// The hold mixer of `canvas` while its hold scene is bound with its sound.
    pub fn hold_mixer(&self, canvas: CanvasId) -> Option<NodeId> {
        self.state.hold_mixers.get(&canvas).map(|m| m.node.node)
    }

    /// The node of a bound source (the picture node of a picture-and-sound source).
    pub fn source_node(&self, id: SourceId) -> Option<NodeId> {
        self.state.sources.get(&id).map(|b| b.source.node)
    }

    /// The sound node of a bound picture-and-sound source (ADR-0106).
    pub fn sound_node(&self, id: SourceId) -> Option<NodeId> {
        self.state
            .sources
            .get(&id)
            .and_then(|b| b.sound.as_ref())
            .map(|s| s.node)
    }

    /// The last node of a bound source's chain: its last enabled filter, or the source
    /// node itself when it has none, and whether that node is a filter. A filter
    /// publishes the canvas format (ADR-0062 decision 4); a source node may publish an
    /// 8-bit RGB picture. Separate video files read from here (ADR-0086 decision 3).
    pub fn chain_tail(&self, id: SourceId) -> Option<(NodeId, bool)> {
        self.state
            .sources
            .get(&id)
            .map(|b| (b.tail().node, !b.chain.is_empty()))
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
        self.sync_cued(collection, graph, &BTreeSet::new(), &BTreeSet::new())
    }

    /// Like [`Self::sync`]. `cued_items` are drawn by flatten and counted by
    /// `visible_sources` (program mixer included) even when `visible` is false.
    /// `cued_sources` is the temporal Active union (ADR-0105 decision 7): source ids
    /// of items a shown scene is drawing for a cue. Both default to empty from
    /// [`Self::sync`] until the host has a cue map. An empty set adds no branch in
    /// the item walk beyond the existing visibility test.
    pub fn sync_cued(
        &mut self,
        collection: &SceneCollection,
        graph: &mut GraphDescription,
        cued_items: &BTreeSet<ItemId>,
        cued_sources: &BTreeSet<SourceId>,
    ) -> Result<BindReport, BindError> {
        self.sync_held(
            collection,
            graph,
            cued_items,
            cued_sources,
            &BTreeMap::new(),
        )
    }

    /// Like [`Self::sync_held`], also binding a running transition per canvas in
    /// `outgoing` (ADR-0128 decision 6): a compositor for `(canvas, Role::Incoming)`
    /// drawing the canvas's program scene, linked into the program compositor (unless
    /// [`OutgoingScene::incoming`] is false, for Move); the outgoing scene's `drawn`
    /// sources linked to the program compositor and running; its `heard` sources in
    /// the program mixer. A canvas not in the collection, or without a program scene,
    /// is skipped. An empty map binds nothing of it and retires what an earlier call
    /// bound.
    pub fn sync_transition(
        &mut self,
        collection: &SceneCollection,
        graph: &mut GraphDescription,
        cued_items: &BTreeSet<ItemId>,
        cued_sources: &BTreeSet<SourceId>,
        hold: &BTreeMap<CanvasId, HoldScene>,
        outgoing: &BTreeMap<CanvasId, OutgoingScene>,
    ) -> Result<BindReport, BindError> {
        let mut draft_graph = graph.clone();
        let mut draft = self.state.clone();
        draft.linger.now = self.clock.as_ref().map(|clock| clock.now());
        let mut report = draft.sync(
            &*self.types,
            self.audio,
            collection,
            &mut draft_graph,
            (cued_items, cued_sources),
            (hold, outgoing),
        )?;
        group_spans::mark_groups(
            &mut report,
            collection,
            &draft,
            self.spans,
            (hold, outgoing),
        );
        *graph = draft_graph;
        self.state = draft;
        Ok(report)
    }

    /// Like [`Self::sync_cued`], also binding a hold scene per canvas in `hold`
    /// (ADR-0124 phase 2b): a compositor for `(canvas, Role::Hold)` drawing the
    /// scene's visible sources, the scene's temporal sources Active, and with
    /// `sound` a hold mixer. A canvas or scene not in the collection is skipped. An
    /// empty map binds nothing of it and retires what an earlier call bound.
    pub fn sync_held(
        &mut self,
        collection: &SceneCollection,
        graph: &mut GraphDescription,
        cued_items: &BTreeSet<ItemId>,
        cued_sources: &BTreeSet<SourceId>,
        hold: &BTreeMap<CanvasId, HoldScene>,
    ) -> Result<BindReport, BindError> {
        self.sync_transition(
            collection,
            graph,
            cued_items,
            cued_sources,
            hold,
            &BTreeMap::new(),
        )
    }

    /// A source's level and effective `on_return` as of the last successful sync
    /// (ADR-0152 decision 7): what the host writes to a node it adopts, so a new node
    /// never runs on a handle default. `(Unplaced, None)` for a source that no scene
    /// places, that is deactivated, or that the binder has not seen.
    pub fn level_of(&self, source: SourceId) -> (Level, Option<OnReturn>) {
        self.state
            .levels
            .get(&source)
            .copied()
            .unwrap_or((Level::Unplaced, None))
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
        // A source loses both nodes when someone removed either: it is bound again
        // from scratch, so its picture and sound nodes stay a pair (ADR-0106).
        let lost: Vec<SourceId> = self
            .sources
            .iter()
            .filter(|(_, b)| {
                graph.node(b.source.node).is_none()
                    || b.sound
                        .as_ref()
                        .is_some_and(|s| graph.node(s.node).is_none())
            })
            .map(|(id, _)| *id)
            .collect();
        for id in lost {
            if let Some(b) = self.sources.remove(&id) {
                retire(graph, b.source.node)?;
                report.retired_sources.push(b.source.node);
                if let Some(s) = &b.sound {
                    retire(graph, s.node)?;
                    report.retired_sources.push(s.node);
                }
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
        self.hold_mixers
            .retain(|_, m| graph.node(m.node.node).is_some());
        for m in self
            .mixers
            .values_mut()
            .chain(self.hold_mixers.values_mut())
        {
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
        (cued_items, cued_sources): (&BTreeSet<ItemId>, &BTreeSet<SourceId>),
        (hold, outgoing): (
            &BTreeMap<CanvasId, HoldScene>,
            &BTreeMap<CanvasId, OutgoingScene>,
        ),
    ) -> Result<BindReport, BindError> {
        let mut report = BindReport::default();
        let mut joined = Vec::new();
        // Transitions the collection can run: their canvas has a program scene.
        let outgoing: BTreeMap<CanvasId, &OutgoingScene> = outgoing
            .iter()
            .filter(|(c, _)| {
                collection
                    .canvases()
                    .any(|k| k.id == **c && k.program.is_some())
            })
            .map(|(c, o)| (*c, o))
            .collect();
        // Hold scenes the collection can draw: their canvas and scene exist.
        let hold: BTreeMap<CanvasId, HoldScene> = hold
            .iter()
            .filter(|(c, h)| {
                collection.canvases().any(|k| k.id == **c) && collection.scene(h.scene).is_some()
            })
            .map(|(c, h)| (*c, *h))
            .collect();
        self.heal(graph, &mut report)?;
        // Which sources are Active and which temporal ones are on air (ADR-0152
        // decisions 1 to 3). Computed first so the preview leaves out a source that
        // is Standby there (`run: on_air`).
        let (running, on_air, levels, lingering) = activity_states(
            types,
            collection,
            (cued_items, cued_sources),
            (&hold, &outgoing, &mut self.linger),
            &mut report,
        )?;
        report.level_changes = level_changes(&self.levels, &levels, collection);
        self.levels = levels;
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
            if types.live_settings(&source.type_id) {
                bound.settings = source.settings.clone();
                report.changed_sources.push((*id, bound.source.node));
                continue;
            }
            let live = types.live_keys(&source.type_id);
            if !live.is_empty() {
                let changed = changed_keys(&bound.settings, &source.settings);
                if changed.iter().all(|key| live.contains(&key.as_str())) {
                    bound.settings = source.settings.clone();
                    report.live_changed.push((*id, bound.source.node, changed));
                    continue;
                }
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
            if let Some(sound) = &mut bound.sound {
                let old = sound.node;
                let (descriptor, partition) = graph
                    .node(old)
                    .map(|n| (Arc::clone(&n.descriptor), n.partition))
                    .ok_or(GraphError::UnknownNode(old))?;
                retire(graph, old)?;
                let node = add_node(graph, descriptor)?;
                graph.set_partition(node, partition)?;
                sound.node = node;
                report.retired_sources.push(old);
                report.replaced_sources.push((*id, old, node));
            }
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
                if let Some(s) = &bound.sound {
                    retire(graph, s.node)?;
                    report.retired_sources.push(s.node);
                }
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
                // The sound node of a picture-and-sound type (ADR-0106 decision 2):
                // only with audio bound, and only for a type that names one. A named
                // sound type that is not installed refuses the whole source, so it is
                // never shown without its sound.
                let sound_desc = match (audio, pins.output.is_some()) {
                    (Some(_), true) => match types.sound_type(&source.type_id) {
                        Some(t) => {
                            let found = types
                                .descriptor(&t)
                                .map(|d| (video_pins(&d, false), d))
                                .filter(|(p, d)| {
                                    d.kind == NodeKind::Source
                                        && p.output.is_none()
                                        && p.audio.is_some()
                                });
                            if found.is_none() {
                                report
                                    .problems
                                    .push(BindProblem::UnknownSourceType(source.id, t));
                                continue;
                            }
                            found
                        }
                        None => None,
                    },
                    _ => None,
                };
                let node = add_node(graph, descriptor)?;
                if let (true, Some(partition)) = (audio_only, audio) {
                    graph.set_partition(node, partition)?;
                }
                report.created_sources.push((source.id, node));
                let sound = match (sound_desc, audio) {
                    (Some((pins, desc)), Some(partition)) => {
                        let node = add_node(graph, desc)?;
                        graph.set_partition(node, partition)?;
                        Some(BoundNode { node, pins })
                    }
                    _ => None,
                };
                slot.insert(BoundSource {
                    sound,
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
            note_joined(&mut joined, is_audio, source.id, &chain, bound);
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
            if let Some(h) = hold.get(&canvas.id) {
                needed.insert((canvas.id, Role::Hold), h.scene);
            }
            if let (Some(program), Some(o)) = (
                canvas.program,
                outgoing.get(&canvas.id).filter(|o| o.incoming),
            ) {
                needed.insert(
                    (canvas.id, Role::Incoming),
                    o.incoming_scene.unwrap_or(program),
                );
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
        // Drawn sources (flatten omits opacity 0); outgoing and stinger ones keep filters.
        let drawn_sources = drawn(collection, &needed, cued_items, &outgoing, &running)?;
        report.turned_on_filters = turned_on(&joined, &mut self.drawn, &drawn_sources);
        // An incoming compositor exists before the program compositor of its canvas
        // is linked, so the program compositor can draw it as a layer.
        let mut fresh: BTreeSet<(CanvasId, Role)> = BTreeSet::new();
        for &(canvas, role) in needed.keys().filter(|(_, r)| *r == Role::Incoming) {
            if let std::collections::btree_map::Entry::Vacant(e) =
                self.compositors.entry((canvas, role))
            {
                let node = add_node(graph, Arc::clone(&compositor_desc))?;
                e.insert(BoundComp {
                    node: BoundNode {
                        node,
                        pins: compositor_pins.clone(),
                    },
                    links: Vec::new(),
                });
                fresh.insert((canvas, role));
            }
        }
        let incoming: BTreeMap<CanvasId, BoundNode> = self
            .compositors
            .iter()
            .filter(|((_, r), _)| *r == Role::Incoming)
            .map(|((c, _), b)| (*c, b.node.clone()))
            .collect();
        for (&(canvas, role), &scene) in &needed {
            let cued_items = role_cues(canvas, role, cued_items, &outgoing);
            let mut created = fresh.contains(&(canvas, role));
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
                .flatten_cued(canvas, scene, cued_items)
                .map_err(BindError::Scene)?;
            let mut layers = Vec::with_capacity(flat.len());
            for layer in flat {
                // A source that is Standby at the preview's level (`run: on_air`) is
                // neither drawn nor linked there (ADR-0152 decision 9).
                if role == Role::Preview && !running.contains(&layer.source) {
                    continue;
                }
                if let Some(bound) = self
                    .sources
                    .get(&layer.source)
                    .filter(|b| b.source.pins.output.is_some())
                {
                    layers.push(BoundLayer {
                        producer: bound.producer(true).node,
                        source: layer.source,
                        path: layer.path,
                        transform: layer.transform,
                        crop: layer.crop,
                        opacity: layer.opacity,
                        style: layer.style,
                        box_size: layer.box_size,
                        scroll: layer.scroll,
                        scroll_key: layer.scroll_key,
                        group: None,
                    });
                }
            }
            // Producers keep warm sources on this canvas scheduled (linked), including
            // hidden and off-scene ones, so devices stay open. Drawn layers still come
            // only from flatten. A source the scene draws at opacity 0 stays linked.
            let mut producers: Vec<BoundNode> = Vec::new();
            // Program keeps every warm source linked, including hidden and off the
            // current scene. Temporal sources are omitted from `warm_sources`
            // (ADR-0107). The program scene still links one it draws, including at
            // opacity 0, so a fade keeps it scheduled. A hidden temporal source is
            // in neither set unless a timed-show cue draws it (ADR-0105).
            let warm = match role {
                Role::Program => {
                    let mut warm = collection.warm_sources_with(canvas, types, &lingering);
                    warm.extend(
                        collection
                            .visible_sources_cued(canvas, scene, cued_items)
                            .map_err(BindError::Scene)?,
                    );
                    // The outgoing picture's sources stay linked for the transition.
                    if let Some(o) = outgoing.get(&canvas) {
                        warm.extend(o.drawn.iter().copied());
                        warm.extend(o.stinger);
                    }
                    warm
                }
                Role::Preview => collection
                    .visible_sources_cued(canvas, scene, cued_items)
                    .map_err(BindError::Scene)?
                    .into_iter()
                    .filter(|id| running.contains(id))
                    .collect(),
                Role::Hold | Role::Incoming => collection
                    .visible_sources_cued(canvas, scene, cued_items)
                    .map_err(BindError::Scene)?,
            };
            let ordered = layers.iter().map(|l| l.source).chain(warm.iter().copied());
            for source in ordered {
                if let Some(bound) = self
                    .sources
                    .get(&source)
                    .filter(|b| b.source.pins.output.is_some())
                {
                    let tail = bound.producer(drawn_sources.contains(&source));
                    if !producers.iter().any(|p| p.node == tail.node) {
                        producers.push(tail.clone());
                    }
                }
            }
            // The incoming scene's picture is one more producer of the program
            // compositor while a transition runs.
            if role == Role::Program
                && let Some(node) = incoming.get(&canvas)
            {
                producers.push(node.clone());
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

        // One mixer per canvas. Its inputs are the sources with audio visible in the
        // canvas's program scene. Audio-only sources are linked from their chain
        // tail, picture-and-sound sources from their sound node (ADR-0106).
        if let (Some((desc, pins)), Some(partition)) = (mixer_desc, audio) {
            for (id, bound) in &self.sources {
                if let Some(s) = &bound.sound {
                    report.picture_sound.push(PictureSound {
                        source: *id,
                        picture: bound.source.node,
                        sound: s.node,
                    });
                } else if bound.source.pins.output.is_some() && bound.source.pins.audio.is_some() {
                    report.problems.push(BindProblem::AudioOnVideoSource(*id));
                }
            }
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
                let mut heard: BTreeSet<SourceId> = match canvas.program {
                    Some(scene) => cue_levels::program_sources(
                        collection, canvas.id, scene, cued_items, &outgoing,
                    )?,
                    None => BTreeSet::new(),
                };
                // The outgoing scene is heard until its transition ends; a manual
                // transition's incoming scene is heard from its start (ADR-0135).
                if let Some(o) = outgoing.get(&canvas.id) {
                    heard.extend(o.heard.iter().copied());
                    heard.extend(o.stinger);
                    if let Some(scene) = o.incoming_scene {
                        heard.extend(
                            collection
                                .visible_sources_cued(canvas.id, scene, cued_items)
                                .map_err(BindError::Scene)?,
                        );
                    }
                }
                let inputs = link_mixer(&self.sources, graph, mix, &heard)?;
                report.mixers.insert(
                    canvas.id,
                    BoundMixer {
                        node: mix.node.node,
                        inputs,
                        created,
                    },
                );
            }
            // Hold mixers (ADR-0124 phase 2b): one per canvas whose hold scene asked
            // for its sound, hearing that scene's visible sources as a program mixer
            // hears its scene's.
            let unneeded: Vec<CanvasId> = self
                .hold_mixers
                .keys()
                .copied()
                .filter(|c| !hold.get(c).is_some_and(|h| h.sound))
                .collect();
            for c in unneeded {
                if let Some(m) = self.hold_mixers.remove(&c) {
                    retire(graph, m.node.node)?;
                    report.retired_mixers.push(m.node.node);
                }
            }
            for (&canvas, h) in hold.iter().filter(|(_, h)| h.sound) {
                let mut created = false;
                let mix = match self.hold_mixers.entry(canvas) {
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
                let heard: BTreeSet<SourceId> = collection
                    .visible_sources_cued(canvas, h.scene, cued_items)
                    .map_err(BindError::Scene)?
                    .into_iter()
                    .collect();
                let inputs = link_mixer(&self.sources, graph, mix, &heard)?;
                report.hold_mixers.insert(
                    canvas,
                    BoundMixer {
                        node: mix.node.node,
                        inputs,
                        created,
                    },
                );
            }
        }

        // Wanted states (ADR-0152): a source is Active when its level reaches its
        // effective `run`; the outgoing scene, the stinger and a hold scene keep
        // theirs running whatever their policy.
        report.on_air_sources = on_air;
        for (id, bound) in &self.sources {
            let state = if running.contains(id) {
                Lifecycle::Active
            } else {
                Lifecycle::Standby
            };
            set_state(graph, bound.source.node, state)?;
            // The sound node is Active exactly when the picture node is (ADR-0106
            // decision 9).
            if let Some(s) = &bound.sound {
                set_state(graph, s.node, state)?;
            }
            // Video filters run only while a shown layer draws the source. An audio
            // chain follows the source: it is the heard path (scene model spec).
            let filters_on = bound.source.pins.output.is_none() || drawn_sources.contains(id);
            for (f, b) in &bound.filters {
                let s = if bound.chain.contains(f) && filters_on {
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
        for m in self.mixers.values().chain(self.hold_mixers.values()) {
            set_state(graph, m.node.node, Lifecycle::Active)?;
        }
        Ok(report)
    }
}

/// The sources the binder sets Active and the temporal ones on air (ADR-0152
/// decisions 1 to 3, ADR-0107 decision 6). Levels come from the collection
/// (placed, preview, program with cues), raised to On air by a bound hold scene and
/// a running transition's outgoing `drawn`, `heard` and stinger, which also stay
/// Active whatever their policy. A cued source (ADR-0105) is at least Shown. A
/// policy the type does not allow is reported and the type's default used.
fn activity_states(
    types: &dyn NodeTypes,
    collection: &SceneCollection,
    (cued_items, cued_sources): (&BTreeSet<ItemId>, &BTreeSet<SourceId>),
    (hold, outgoing, linger): (
        &BTreeMap<CanvasId, HoldScene>,
        &BTreeMap<CanvasId, &OutgoingScene>,
        &mut linger::Linger,
    ),
    report: &mut BindReport,
) -> Result<ActivityStates, BindError> {
    let mut levels = cue_levels::source_levels(collection, cued_items, outgoing)?;
    for id in cued_sources {
        if collection.source(*id).is_some_and(|s| !s.standby()) {
            let level = levels.entry(*id).or_insert(Level::Shown);
            *level = (*level).max(Level::Shown);
        }
    }
    let mut forced = BTreeSet::new();
    for o in outgoing.values() {
        forced.extend(o.drawn.iter().chain(&o.heard).chain(&o.stinger).copied());
    }
    for (&canvas, h) in hold {
        let drawn = collection
            .visible_sources_cued(canvas, h.scene, cued_items)
            .map_err(BindError::Scene)?;
        forced.extend(drawn);
    }
    for id in &forced {
        levels.insert(*id, Level::OnAir);
    }
    let mut running = BTreeSet::new();
    let mut on_air = BTreeSet::new();
    let mut delivered = BTreeMap::new();
    for (id, level) in levels {
        let Some(source) = collection.source(id) else {
            continue;
        };
        let traits = types.activity(&source.type_id);
        if !source.activity.is_default() && !traits.allows(&source.activity) {
            report.problems.push(BindProblem::ActivityUnsupported(id));
        }
        let policy = traits.effective(Some(&source.activity));
        delivered.insert(id, (level, policy.on_return));
        if forced.contains(&id) || policy.active_at(level) {
            running.insert(id);
            // Only an Active temporal source has a membership that matters.
            if level == Level::OnAir && policy.is_temporal() {
                on_air.insert(id);
            }
        }
    }
    // A source that dropped below its `run` stays Active for its linger, as a warm
    // source of its type (decision 4); an unplaced one is Active but linked nowhere.
    let lingering = linger.apply(types, collection, &mut running);
    Ok((running, on_air, delivered, lingering))
}

/// The running set (lingering sources included), the on-air set, each placed
/// source's level with its effective `on_return`, and the lingering sources, from
/// [`activity_states`].
type ActivityStates = (
    BTreeSet<SourceId>,
    BTreeSet<SourceId>,
    BTreeMap<SourceId, (Level, Option<OnReturn>)>,
    BTreeSet<SourceId>,
);

/// The sources whose level or effective `on_return` differs from the last sync
/// (ADR-0152 decision 7), in source id order. A source that left every level still
/// exists and is reported as [`Level::Unplaced`] with its effective `on_return`; a
/// deleted source is not reported.
fn level_changes(
    old: &BTreeMap<SourceId, (Level, Option<OnReturn>)>,
    new: &BTreeMap<SourceId, (Level, Option<OnReturn>)>,
    collection: &SceneCollection,
) -> Vec<(SourceId, Level, Option<OnReturn>)> {
    let mut changes: Vec<(SourceId, Level, Option<OnReturn>)> = new
        .iter()
        .filter(|(id, now)| old.get(id) != Some(now))
        .map(|(id, (level, on_return))| (*id, *level, *on_return))
        .collect();
    for (id, (_, on_return)) in old {
        if !new.contains_key(id) && collection.source(*id).is_some() {
            changes.push((*id, Level::Unplaced, *on_return));
        }
    }
    changes.sort_by_key(|c| c.0);
    changes
}

/// Links `mix` to the sources in `heard` that have sound (ADR-0039, ADR-0106):
/// audio-only sources from their chain tail, picture-and-sound sources from their
/// sound node. Unlinks what it no longer hears. Returns its inputs, in source id
/// order.
fn link_mixer(
    sources: &BTreeMap<SourceId, BoundSource>,
    graph: &mut GraphDescription,
    mix: &mut BoundMix,
    heard: &BTreeSet<SourceId>,
) -> Result<Vec<BoundAudio>, GraphError> {
    let mut inputs = Vec::new();
    let mut outs: BTreeMap<NodeId, String> = BTreeMap::new();
    for id in heard {
        let Some(bound) = sources.get(id) else {
            continue;
        };
        // The mixer hears the tail of an audio chain (ADR-0082), or the sound node
        // of a picture-and-sound source (ADR-0106). A picture node is never linked.
        let producer = if bound.source.pins.output.is_none() {
            Some(bound.tail())
        } else {
            bound.sound.as_ref()
        };
        if let Some(p) = producer {
            if let Some(out) = &p.pins.audio {
                inputs.push(BoundAudio {
                    source: *id,
                    producer: p.node,
                });
                outs.insert(p.node, out.clone());
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
    Ok(inputs)
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

/// The keys whose values differ between `old` and `new`, present in either, in order.
fn changed_keys(old: &crate::model::Settings, new: &crate::model::Settings) -> Vec<String> {
    let mut keys: Vec<String> = old
        .iter()
        .filter(|(key, value)| new.get(*key) != Some(value))
        .map(|(key, _)| key.clone())
        .collect();
    keys.extend(new.keys().filter(|key| !old.contains_key(*key)).cloned());
    keys.sort();
    keys
}
