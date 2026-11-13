//! The source type registry: adapters in, node descriptors and node instances out.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use rbs_audio::{AudioGraphConfig, InputNode, InputParams, InputStats};
use rbs_audio_io::caps as audio_caps;
use rbs_audio_io::{
    AudioBackend, AudioIoError, AudioOutput, EndpointId, EndpointKind, InputRequest, OutputRequest,
};
use rbs_capture::caps as capture_caps;
use rbs_capture::{CaptureError, CaptureFactory, CaptureRequest, TargetId, TargetKind};
use rbs_core::capability::{Constraint, Requirement};
use rbs_core::media::Size;
use rbs_core::registry::{AdapterFactory, AdapterId, DuplicateAdapter, Registry};
use rbs_core::time::Clock;
use rbs_core::timing::FrameRate;
use rbs_graph::node::{NodeDescriptor, NodeTypeId};
use rbs_graph::runtime::param::ParamSender;
use rbs_nodes::capture::CaptureNodeStats;
use rbs_nodes::{CaptureSourceNode, StillSourceNode};
use rbs_scene::binder::{COMPOSITOR_TYPE, MIXER_TYPE, NodeTypes};
use rbs_scene::model::{SettingValue, Settings, Source};

use crate::types::{self, SourceClass, class_of, keys};

/// Largest sync offset an audio source may ask for, unless the context says otherwise.
pub const DEFAULT_MAX_AUDIO_OFFSET: Duration = Duration::from_millis(200);

/// Why a source could not be instantiated or listed.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceError {
    /// The type is not a built-in source type.
    UnknownType(NodeTypeId),
    /// No probed adapter can capture this kind.
    NoAdapter(NodeTypeId),
    /// The `adapter` setting names an adapter that is not registered, not probed, or
    /// cannot capture this kind.
    AdapterUnavailable(AdapterId),
    /// No `target` setting, and no default target exists (windows have no default;
    /// no display, camera or endpoint is present).
    NoTarget(NodeTypeId),
    /// A setting has the wrong type or an impossible value.
    InvalidSetting {
        /// The key.
        key: &'static str,
        /// What it must be.
        expected: &'static str,
    },
    /// The capture adapter refused.
    Capture(CaptureError),
    /// The audio adapter refused.
    Audio(AudioIoError),
    /// A still source's picture could not be read or made (ADR-0074).
    Image(String),
    /// A media file could not be opened or decoded (ADR-0108). Same role as `Image`.
    Media(String),
}

impl SourceError {
    /// The cause this error stands for, for the typed unavailable state (ADR-0077):
    /// a capture target or audio endpoint that is gone, refused or busy, else `Other`.
    pub fn fault(&self) -> rbs_core::fault::SourceFault {
        use rbs_core::fault::SourceFault;
        match self {
            SourceError::Capture(e) => rbs_nodes::capture_fault(e),
            SourceError::Audio(AudioIoError::EndpointNotFound | AudioIoError::EndpointLost) => {
                SourceFault::TargetGone
            }
            SourceError::Audio(
                AudioIoError::PermissionDenied | AudioIoError::PermissionNotDetermined,
            ) => SourceFault::PermissionDenied,
            SourceError::Audio(AudioIoError::DeviceBusy) => SourceFault::Busy,
            _ => SourceFault::Other,
        }
    }
}

impl fmt::Display for SourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SourceError::UnknownType(t) => write!(f, "unknown source type {t}"),
            SourceError::NoAdapter(t) => write!(f, "no adapter can capture {t}"),
            SourceError::AdapterUnavailable(a) => write!(f, "adapter {a} is not available"),
            SourceError::NoTarget(t) => write!(f, "no target chosen and no default for {t}"),
            SourceError::InvalidSetting { key, expected } => {
                write!(f, "setting {key:?} must be {expected}")
            }
            SourceError::Capture(e) => write!(f, "capture: {e}"),
            SourceError::Audio(e) => write!(f, "audio: {e}"),
            SourceError::Image(e) => write!(f, "picture: {e}"),
            SourceError::Media(e) => write!(f, "media: {e}"),
        }
    }
}

impl std::error::Error for SourceError {}

/// A target a source of some type can capture, for pickers and for headless defaults.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceTarget {
    /// The adapter that listed it; store it as the `adapter` setting.
    pub adapter: AdapterId,
    /// The target or endpoint id; store it as the `target` setting.
    pub target: String,
    /// Human readable name from the OS.
    pub name: String,
    /// The OS default of its kind (audio endpoints only; capture has no default).
    pub is_default: bool,
    /// Native size in pixels when the adapter knows it (displays and windows), so a
    /// picker can place the source with its aspect ratio. A display's size is the
    /// framebuffer in physical pixels.
    pub size: Option<(u32, u32)>,
    /// OS pixels per logical point of a display, when the adapter read it (ADR-0102).
    /// The canvas uses [`Self::size`] and is not multiplied by this.
    pub scale: Option<rbs_capture::DisplayScale>,
    /// The application that owns a window, when the OS tells it (grouping only).
    pub app: Option<String>,
    /// What a camera offers, empty for everything else: the modes the Source settings
    /// window lists.
    pub modes: Vec<SourceMode>,
    /// The opaque id of the audio input that belongs with a camera (its microphone),
    /// when the capture adapter knows it; the audio adapter resolves it (ADR-0096).
    pub related_audio: Option<String>,
    /// A display's work area in the same pixels as [`Self::size`] (ADR-0104).
    /// `None` when the adapter did not report one.
    pub work_rect: Option<rbs_capture::WorkRect>,
}

/// A playback device a monitor can use ([`SourceTypes::monitor_devices`]).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorDevice {
    /// The adapter that listed it.
    pub adapter: AdapterId,
    /// The endpoint id; store it as the monitor device.
    pub target: String,
    /// Human readable name from the OS.
    pub name: String,
    /// The OS default output.
    pub is_default: bool,
    /// The physical device, shared with a loopback endpoint of the same device.
    pub device_key: Option<String>,
}

/// One mode of a camera: a picture size and the frame rates it runs at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceMode {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Lowest whole frame rate.
    pub min_fps: u32,
    /// Highest whole frame rate.
    pub max_fps: u32,
}

fn source_modes(modes: &[rbs_capture::CameraMode]) -> Vec<SourceMode> {
    let whole = |r: rbs_core::timing::FrameRate| {
        let r = r.per_second();
        u32::try_from((u64::from(r.num()) + u64::from(r.den()) / 2) / u64::from(r.den().max(1)))
            .unwrap_or(u32::MAX)
    };
    modes
        .iter()
        .map(|m| SourceMode {
            width: m.width,
            height: m.height,
            min_fps: whole(m.min_frame_rate),
            max_fps: whole(m.max_frame_rate),
        })
        .collect()
}

/// One adapter's OS picker for a capture kind, from [`SourceTypes::pickers`]. It holds
/// the adapter's factory only (no resources), so a host can move it to a thread of its
/// own while the dialog is up.
#[derive(Clone)]
pub struct SourcePicker {
    factory: Arc<dyn CaptureFactory>,
    kind: TargetKind,
}

impl fmt::Debug for SourcePicker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SourcePicker")
            .field("adapter", &self.factory.id())
            .field("kind", &self.kind)
            .finish()
    }
}

impl SourcePicker {
    /// The adapter that shows the picker.
    pub fn adapter(&self) -> AdapterId {
        self.factory.id()
    }

    /// Its display name, for a "choose with ..." entry.
    pub fn adapter_name(&self) -> &str {
        self.factory.display_name()
    }

    /// Shows the OS picker and blocks until the user chooses or closes it (capture port
    /// `pick`): never call it on a real-time thread, and expect it to block for as long
    /// as the dialog is up. A closed picker is `Capture(CaptureError::Cancelled)`, not a
    /// denial. The returned target is stored like a listed one (`adapter` and `target`
    /// settings); its id carries the OS restore token. Not a default target
    /// (`is_default` is false).
    pub fn pick(&self) -> Result<SourceTarget, SourceError> {
        let t = self.factory.pick(self.kind).map_err(SourceError::Capture)?;
        Ok(SourceTarget {
            adapter: self.factory.id(),
            target: t.id.as_str().to_owned(),
            name: t.name,
            is_default: false,
            size: t.size,
            scale: t.scale,
            app: t.app,
            modes: Vec::new(),
            related_audio: None,
            work_rect: t.work_rect,
        })
    }
}

/// What [`SourceTypes::instantiate`] needs besides the source.
#[non_exhaustive]
#[derive(Clone)]
pub struct InstanceContext {
    /// The master clock (ADR-0004); capture frames and audio samples are stamped
    /// with it.
    pub clock: Arc<dyn Clock>,
    /// The audio partition's format, block size and latency.
    pub audio: AudioGraphConfig,
    /// Largest sync offset an audio source may ask for (sizes its FIFO).
    pub max_audio_offset: Duration,
    /// Shared cap of two browser helpers (ADR-0109). The composition root passes one
    /// budget into every instantiate.
    pub browser_budget: Arc<crate::browser::BrowserBudget>,
    /// Helper tree the composition root wires. `None` until it sets a path.
    pub browser_tree: Option<Arc<dyn crate::browser::BrowserTree>>,
}

impl InstanceContext {
    /// A context with the default maximum audio offset.
    pub fn new(clock: Arc<dyn Clock>, audio: AudioGraphConfig) -> Self {
        Self {
            clock,
            audio,
            max_audio_offset: DEFAULT_MAX_AUDIO_OFFSET,
            browser_budget: crate::browser::BrowserBudget::shared(),
            browser_tree: None,
        }
    }

    /// Sets the largest sync offset an audio source may ask for.
    pub fn with_max_audio_offset(mut self, offset: Duration) -> Self {
        self.max_audio_offset = offset;
        self
    }

    /// Shares the browser cap and the helper tree across sources.
    pub fn with_browser(
        mut self,
        budget: Arc<crate::browser::BrowserBudget>,
        tree: Option<Arc<dyn crate::browser::BrowserTree>>,
    ) -> Self {
        self.browser_budget = budget;
        self.browser_tree = tree;
        self
    }
}

/// A capture source node for the render partition (`Node<GpuRuntime>`).
#[non_exhaustive]
pub struct VideoSource {
    /// The node; it opens the session in `prepare`.
    pub node: CaptureSourceNode,
    /// Its counters.
    pub stats: Arc<CaptureNodeStats>,
    /// The adapter it captures with.
    pub adapter: AdapterId,
    /// The target it captures (the default one if the settings named none).
    pub target: TargetId,
}

/// An audio input node for the audio partition (`Node<X>` for any runtime).
#[non_exhaustive]
pub struct AudioSource {
    /// The node, holding an opened stream in Standby (no device activity until
    /// `prepare`, audio input port contract).
    pub node: InputNode,
    /// The sync offset channel (ADR-0023).
    pub offset: ParamSender<InputParams>,
    /// Its counters.
    pub stats: Arc<InputStats>,
    /// The adapter it reads with.
    pub adapter: AdapterId,
    /// The endpoint it reads (the default one if the settings named none).
    pub endpoint: EndpointId,
}

/// A still source node for the render partition (ADR-0074).
#[non_exhaustive]
pub struct StillSource {
    /// The node; it uploads its picture in `warm` and costs nothing per frame.
    pub node: StillSourceNode,
    /// Its counters (`picture_size` is known from the start).
    pub stats: Arc<CaptureNodeStats>,
}

/// A media-file node (ADR-0108). Video and audio pins are both always present.
#[non_exhaustive]
pub struct MediaSource {
    /// The node. Standby holds no decoder and no queues.
    pub node: rbs_nodes::MediaSourceNode,
    /// Upload, drop, repeat, and underrun counters.
    pub stats: Arc<rbs_nodes::MediaStats>,
    /// The decoder factory chosen for this node.
    pub adapter: AdapterId,
}

/// A browser node (ADR-0109). No helper until it is activated with a slot.
#[non_exhaustive]
pub struct BrowserSource {
    /// The node.
    pub node: crate::browser::BrowserSourceNode,
}

/// A node instance for a bound source.
#[non_exhaustive]
pub enum SourceInstance {
    /// Adopt into the render partition.
    Still(StillSource),
    /// Adopt into the render partition.
    Video(VideoSource),
    /// Adopt into the render partition. Its audio pin is the ADR-0106 producer.
    /// Boxed: the node with its queues is far larger than the other variants.
    Media(Box<MediaSource>),
    /// Adopt into the render partition. No audio pin.
    Browser(BrowserSource),
    /// Adopt into the audio partition.
    Audio(AudioSource),
}

impl fmt::Debug for SourceInstance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SourceInstance::Still(s) => f
                .debug_struct("Still")
                .field("size", &s.node.size())
                .finish_non_exhaustive(),
            SourceInstance::Video(v) => f
                .debug_struct("Video")
                .field("adapter", &v.adapter)
                .field("target", &v.target)
                .finish_non_exhaustive(),
            SourceInstance::Media(m) => f
                .debug_struct("Media")
                .field("adapter", &m.adapter)
                .finish_non_exhaustive(),
            SourceInstance::Browser(b) => f
                .debug_struct("Browser")
                .field("running", &b.node.helper_running())
                .finish_non_exhaustive(),
            SourceInstance::Audio(a) => f
                .debug_struct("Audio")
                .field("adapter", &a.adapter)
                .field("endpoint", &a.endpoint)
                .finish_non_exhaustive(),
        }
    }
}

/// Node descriptors by type id, for the scene binder. A snapshot: build a new one with
/// [`SourceTypes::node_types`] after registering or probing adapters.
#[derive(Debug, Clone, Default)]
pub struct NodeTypeTable {
    types: BTreeMap<NodeTypeId, Arc<NodeDescriptor>>,
}

impl NodeTypeTable {
    /// Adds or replaces a type (for example a plugin's node type).
    pub fn insert(&mut self, type_id: NodeTypeId, descriptor: NodeDescriptor) {
        self.types.insert(type_id, Arc::new(descriptor));
    }

    /// The type ids in the table.
    pub fn ids(&self) -> impl Iterator<Item = &NodeTypeId> {
        self.types.keys()
    }
}

impl NodeTypes for NodeTypeTable {
    fn descriptor(&self, type_id: &NodeTypeId) -> Option<Arc<NodeDescriptor>> {
        self.types.get(type_id).cloned()
    }
}

/// The capture and audio input adapters, and the source types they provide.
///
/// Control thread only. Registering an adapter costs nothing: adapters hold no
/// resources until a session or stream starts (P13).
pub struct SourceTypes {
    capture: Registry<dyn CaptureFactory>,
    audio: Registry<dyn AudioBackend>,
    media: Registry<dyn rbs_media_decode::MediaDecoderFactory>,
}

impl Default for SourceTypes {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for SourceTypes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SourceTypes")
            .field("capture", &self.capture.ids().collect::<Vec<_>>())
            .field("audio", &self.audio.ids().collect::<Vec<_>>())
            .finish()
    }
}

fn capture_requirement(kind: TargetKind) -> Requirement {
    Requirement::new().with(
        capture_caps::TARGET_KINDS,
        Constraint::Includes(kind.as_str().to_owned()),
    )
}

fn audio_requirement(kind: EndpointKind) -> Requirement {
    Requirement::new().with(
        audio_caps::INPUT_KINDS,
        Constraint::Includes(kind.as_str().to_owned()),
    )
}

/// The keys a source type uses; others are refused when settings are set through the
/// API or the Source settings window (a stored document may still carry more, so a file
/// from a newer version loads).
fn known_keys(type_id: &NodeTypeId) -> Option<&'static [&'static str]> {
    Some(match class_of(type_id)? {
        SourceClass::Capture(TargetKind::Camera) => &[
            keys::ADAPTER,
            keys::TARGET,
            keys::WIDTH,
            keys::HEIGHT,
            keys::FPS,
        ],
        SourceClass::Capture(_) => &[
            keys::ADAPTER,
            keys::TARGET,
            keys::CURSOR,
            keys::WIDTH,
            keys::HEIGHT,
            keys::FPS,
        ],
        SourceClass::Audio(EndpointKind::OutputLoopback) => {
            &[keys::ADAPTER, keys::TARGET, keys::EXCLUDE_OWN_PROCESS]
        }
        SourceClass::Audio(_) => &[keys::ADAPTER, keys::TARGET],
        SourceClass::Still => &[
            keys::PATH,
            keys::SAMPLE,
            keys::COLOR,
            keys::COLOR2,
            keys::ANGLE,
            keys::FILLED,
            keys::WIDTH,
            keys::HEIGHT,
        ],
        SourceClass::Media => &[
            keys::ADAPTER,
            keys::PATH,
            keys::LOOP,
            keys::RESTART_WHEN_SHOWN,
        ],
        SourceClass::Browser => &[
            keys::URL,
            keys::WIDTH,
            keys::HEIGHT,
            keys::FPS,
            keys::TRANSPARENT,
        ],
    })
}

/// Checks `settings` for a source of `type_id` without touching any device: only keys the
/// type uses, each of the right kind, a size given as both sides, a sane size and rate.
/// What the OS offers (does the target exist, does the camera have this mode) is only
/// known when the source opens and shows in its status then.
pub fn validate_settings(type_id: &NodeTypeId, settings: &Settings) -> Result<(), SourceError> {
    if class_of(type_id) == Some(SourceClass::Media) {
        return validate_media(settings);
    }
    if class_of(type_id) == Some(SourceClass::Browser) {
        return crate::browser::validate_browser(settings)
            .map_err(|(key, expected)| SourceError::InvalidSetting { key, expected });
    }
    /// The largest picture side and frame rate accepted.
    const MAX_SIDE: u32 = 16_384;
    const MAX_FPS: u32 = 240;
    let known = known_keys(type_id).ok_or_else(|| SourceError::UnknownType(type_id.clone()))?;
    if settings.keys().any(|k| !known.contains(&k.as_str())) {
        return Err(SourceError::InvalidSetting {
            key: "settings",
            expected: "only keys this source type uses",
        });
    }
    text(settings, keys::ADAPTER)?;
    text(settings, keys::TARGET)?;
    flag(settings, keys::CURSOR)?;
    flag(settings, keys::EXCLUDE_OWN_PROCESS)?;
    flag(settings, keys::FILLED)?;
    let (w, h) = (
        positive(settings, keys::WIDTH)?,
        positive(settings, keys::HEIGHT)?,
    );
    if w.is_some() != h.is_some() {
        return Err(SourceError::InvalidSetting {
            key: keys::WIDTH,
            expected: "set together with height",
        });
    }
    if w.into_iter().chain(h).any(|v| v > MAX_SIDE) {
        return Err(SourceError::InvalidSetting {
            key: keys::WIDTH,
            expected: "at most 16384 pixels",
        });
    }
    if positive(settings, keys::FPS)?.is_some_and(|n| n > MAX_FPS) {
        return Err(SourceError::InvalidSetting {
            key: keys::FPS,
            expected: "at most 240 frames per second",
        });
    }
    text(settings, keys::PATH)?;
    text(settings, keys::SAMPLE)?;
    text(settings, keys::COLOR)?;
    text(settings, keys::COLOR2)?;
    if settings
        .get(keys::ANGLE)
        .is_some_and(|v| !matches!(v, SettingValue::Int(_)))
    {
        return Err(SourceError::InvalidSetting {
            key: keys::ANGLE,
            expected: "an integer",
        });
    }
    Ok(())
}

fn text<'a>(settings: &'a Settings, key: &'static str) -> Result<Option<&'a str>, SourceError> {
    match settings.get(key) {
        None => Ok(None),
        Some(SettingValue::Text(t)) => Ok(Some(t.as_str())),
        Some(_) => Err(SourceError::InvalidSetting {
            key,
            expected: "text",
        }),
    }
}

fn flag(settings: &Settings, key: &'static str) -> Result<Option<bool>, SourceError> {
    match settings.get(key) {
        None => Ok(None),
        Some(SettingValue::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(SourceError::InvalidSetting {
            key,
            expected: "a bool",
        }),
    }
}

fn positive(settings: &Settings, key: &'static str) -> Result<Option<u32>, SourceError> {
    match settings.get(key) {
        None => Ok(None),
        Some(SettingValue::Int(n)) => match u32::try_from(*n) {
            Ok(v) if v > 0 => Ok(Some(v)),
            _ => Err(SourceError::InvalidSetting {
                key,
                expected: "a positive integer",
            }),
        },
        Some(_) => Err(SourceError::InvalidSetting {
            key,
            expected: "an integer",
        }),
    }
}

/// Picks the adapter: the named one if it is a candidate, else the best candidate.
fn choose<F: ?Sized + AdapterFactory>(
    candidates: &[Arc<F>],
    named: Option<&str>,
    type_id: &NodeTypeId,
) -> Result<Vec<Arc<F>>, SourceError> {
    match named {
        Some(name) => candidates
            .iter()
            .find(|c| c.id().as_str() == name)
            .map(|c| vec![Arc::clone(c)])
            .ok_or_else(|| SourceError::AdapterUnavailable(AdapterId::new(name))),
        None if candidates.is_empty() => Err(SourceError::NoAdapter(type_id.clone())),
        None => Ok(candidates.to_vec()),
    }
}

impl SourceTypes {
    /// An empty registry: only the compositor and mixer types are installed.
    pub fn new() -> Self {
        Self {
            capture: Registry::new(),
            audio: Registry::new(),
            media: Registry::new(),
        }
    }

    /// Registers a capture adapter. Call [`probe`](Self::probe) afterwards.
    pub fn register_capture(
        &mut self,
        factory: Arc<dyn CaptureFactory>,
    ) -> Result<(), DuplicateAdapter> {
        self.capture.register(factory)
    }

    /// Registers an audio input adapter. Call [`probe`](Self::probe) afterwards.
    pub fn register_audio(
        &mut self,
        backend: Arc<dyn AudioBackend>,
    ) -> Result<(), DuplicateAdapter> {
        self.audio.register(backend)
    }

    /// Registers a media-file decoder. Call [`probe`](Self::probe) afterwards.
    /// The scene type id stays `org.rbs.source.media`; this id is only the adapter.
    pub fn register_media(
        &mut self,
        factory: Arc<dyn rbs_media_decode::MediaDecoderFactory>,
    ) -> Result<(), DuplicateAdapter> {
        self.media.register(factory)
    }

    /// The capture adapters, to set a preference order or the user's choice.
    pub fn capture_adapters(&mut self) -> &mut Registry<dyn CaptureFactory> {
        &mut self.capture
    }

    /// The audio input adapters, to set a preference order or the user's choice.
    pub fn audio_adapters(&mut self) -> &mut Registry<dyn AudioBackend> {
        &mut self.audio
    }

    /// Probes every adapter (each under 50 ms, adapter registry contract).
    pub fn probe(&mut self) {
        self.capture.probe_all();
        self.audio.probe_all();
        self.media.probe_all();
    }

    /// The capture adapter `adapter` of the capture type `type_id`, if it is one of
    /// its candidates. Hosts use it to grab a thumbnail on a worker thread
    /// (`CaptureFactory::thumbnail`, ADR-0077) without holding the registry. `None`
    /// for audio and still types, and for an adapter that is not a candidate.
    pub fn capture_factory(
        &self,
        type_id: &NodeTypeId,
        adapter: &AdapterId,
    ) -> Option<Arc<dyn CaptureFactory>> {
        let SourceClass::Capture(kind) = class_of(type_id)? else {
            return None;
        };
        self.capture_candidates(kind)
            .into_iter()
            .find(|f| &f.id() == adapter)
    }

    fn capture_candidates(&self, kind: TargetKind) -> Vec<Arc<dyn CaptureFactory>> {
        self.capture.select(&capture_requirement(kind)).candidates
    }

    fn audio_candidates(&self, kind: EndpointKind) -> Vec<Arc<dyn AudioBackend>> {
        self.audio.select(&audio_requirement(kind)).candidates
    }

    /// Probed audio adapters that open outputs (`audio.output`), best first.
    fn output_candidates(&self) -> Vec<Arc<dyn AudioBackend>> {
        let requirement = Requirement::new().with(audio_caps::OUTPUT, Constraint::Flag(true));
        self.audio.select(&requirement).candidates
    }

    /// True when some audio adapter can play to a device (monitoring, ADR-0097).
    pub fn can_monitor(&self) -> bool {
        !self.output_candidates().is_empty()
    }

    /// The playback devices a monitor can use, best adapter first, for the Settings
    /// picker (ADR-0097 decision 4). Enumerates: call it only while the picker is open.
    /// `target` is what [`SourceTypes::open_monitor`] takes; `device_key` lets a host
    /// warn when a loopback source captures the same device (decision 8).
    pub fn monitor_devices(&self) -> Vec<MonitorDevice> {
        let mut out = Vec::new();
        for b in self.output_candidates() {
            for e in b.enumerate(EndpointKind::Output).unwrap_or_default() {
                out.push(MonitorDevice {
                    adapter: b.id(),
                    target: e.id.as_str().to_owned(),
                    name: e.name,
                    is_default: e.is_default,
                    device_key: e.device_key,
                });
            }
        }
        out
    }

    /// Opens a monitor output (in Standby) on the control thread: on `target`, a
    /// playback endpoint id from [`SourceTypes::monitor_devices`], from the first
    /// adapter, best first, that lists it; `None` (System default) takes the OS default
    /// output of the best adapter that lists one. `NoAdapter` when no adapter plays
    /// (`type_id` names the request in the error), `NoTarget` when no device is present.
    pub fn open_monitor(
        &self,
        target: Option<&str>,
    ) -> Result<(Box<dyn AudioOutput>, AdapterId, EndpointId), SourceError> {
        let type_id = NodeTypeId::new_static(rbs_audio::OUTPUT_NODE_TYPE);
        let candidates = self.output_candidates();
        if candidates.is_empty() {
            return Err(SourceError::NoAdapter(type_id));
        }
        let (backend, endpoint) = match target {
            Some(t) => candidates
                .iter()
                .find(|b| {
                    b.enumerate(EndpointKind::Output)
                        .is_ok_and(|l| l.iter().any(|x| x.id.as_str() == t))
                })
                .map(|b| (Arc::clone(b), EndpointId::new(t)))
                .ok_or(SourceError::Audio(AudioIoError::EndpointNotFound))?,
            None => default_endpoint(&candidates, EndpointKind::Output)
                .ok_or(SourceError::NoTarget(type_id))?,
        };
        let output = backend
            .open_output(&OutputRequest::new(endpoint.clone()))
            .map_err(SourceError::Audio)?;
        Ok((output, backend.id(), endpoint))
    }

    /// True when some audio adapter can leave RBS's own sound out of a system loopback
    /// capture (`audio.loopback.exclude_self`, ADR-0096). Without it a display sound
    /// companion would echo, so hosts do not offer one.
    pub fn can_exclude_own_sound(&self) -> bool {
        let requirement = audio_requirement(EndpointKind::OutputLoopback)
            .with(audio_caps::LOOPBACK_EXCLUDE_SELF, Constraint::Flag(true));
        !self.audio.select(&requirement).candidates.is_empty()
    }

    /// The OS pickers for `type_id`, best adapter first: probed capture adapters that
    /// declare its kind in `capture.target.picker`. Empty for audio types and kinds no
    /// adapter picks. Some systems offer displays and windows only this way (the
    /// PipeWire screen-cast portal); hosts show a "choose" entry for each picker next
    /// to the listed targets. Never prompts.
    pub fn pickers(&self, type_id: &NodeTypeId) -> Result<Vec<SourcePicker>, SourceError> {
        match class_of(type_id).ok_or_else(|| SourceError::UnknownType(type_id.clone()))? {
            SourceClass::Capture(kind) => {
                let requirement = capture_requirement(kind).with(
                    capture_caps::TARGET_PICKER,
                    Constraint::Includes(kind.as_str().to_owned()),
                );
                Ok(self
                    .capture
                    .select(&requirement)
                    .candidates
                    .into_iter()
                    .map(|factory| SourcePicker { factory, kind })
                    .collect())
            }
            SourceClass::Audio(_)
            | SourceClass::Still
            | SourceClass::Media
            | SourceClass::Browser => Ok(Vec::new()),
        }
    }

    /// Whether some probed adapter provides `type_id`.
    pub fn is_installed(&self, type_id: &NodeTypeId) -> bool {
        match class_of(type_id) {
            Some(SourceClass::Capture(kind)) => !self.capture_candidates(kind).is_empty(),
            Some(SourceClass::Audio(kind)) => !self.audio_candidates(kind).is_empty(),
            Some(SourceClass::Still) | Some(SourceClass::Browser) => true,
            Some(SourceClass::Media) => !self.media_candidates(None).is_empty(),
            _ => false,
        }
    }

    /// The installed built-in source types, in [`types::ALL`] order.
    pub fn installed(&self) -> Vec<NodeTypeId> {
        types::ALL
            .iter()
            .map(|t| NodeTypeId::new_static(t))
            .filter(|t| self.is_installed(t))
            .collect()
    }

    /// The descriptors the scene binder needs: every installed source type (capture
    /// types share [`rbs_nodes::capture_descriptor`], audio types
    /// [`rbs_audio::input_descriptor`]), the compositor and the mixer. A source type
    /// with no adapter is missing, so the binder reports it as unknown.
    pub fn node_types(&self) -> NodeTypeTable {
        let mut table = NodeTypeTable::default();
        table.insert(
            NodeTypeId::new_static(COMPOSITOR_TYPE),
            rbs_gpu::node::compositor_descriptor(),
        );
        table.insert(
            NodeTypeId::new_static(MIXER_TYPE),
            rbs_audio::mixer_descriptor(),
        );
        for ty in rbs_filters::catalogue() {
            table.insert(ty.type_id(), rbs_filters::descriptor(*ty));
        }
        for ty in rbs_audio_filters::catalogue() {
            table.insert(ty.type_id(), rbs_audio_filters::descriptor(*ty));
        }
        for t in self.installed() {
            let descriptor = match class_of(&t) {
                Some(SourceClass::Capture(_)) => rbs_nodes::capture_descriptor(),
                Some(SourceClass::Still) => rbs_nodes::still_descriptor(),
                Some(SourceClass::Media) => rbs_nodes::media_descriptor(),
                Some(SourceClass::Browser) => crate::browser::browser_descriptor(),
                _ => rbs_audio::input_descriptor(),
            };
            table.insert(t, descriptor);
        }
        table
    }

    /// Lists what a source of `type_id` can capture, from every adapter that provides
    /// it, best adapter first. Not real-time (up to 100 ms per adapter); never
    /// prompts. Adapters that cannot list (picker-only, no permission) are skipped.
    pub fn targets(&self, type_id: &NodeTypeId) -> Result<Vec<SourceTarget>, SourceError> {
        let mut out = Vec::new();
        match class_of(type_id).ok_or_else(|| SourceError::UnknownType(type_id.clone()))? {
            SourceClass::Capture(kind) => {
                for f in self.capture_candidates(kind) {
                    for t in f.enumerate(kind).unwrap_or_default() {
                        out.push(SourceTarget {
                            adapter: f.id(),
                            target: t.id.as_str().to_owned(),
                            name: t.name,
                            is_default: false,
                            size: t.size,
                            scale: t.scale,
                            modes: source_modes(&t.modes),
                            related_audio: t.related.and_then(|r| {
                                (r.kind == rbs_capture::RelatedKind::Audio).then_some(r.id)
                            }),
                            app: t.app,
                            work_rect: t.work_rect,
                        });
                    }
                }
            }
            SourceClass::Audio(kind) => {
                for b in self.audio_candidates(kind) {
                    for e in b.enumerate(kind).unwrap_or_default() {
                        out.push(SourceTarget {
                            adapter: b.id(),
                            target: e.id.as_str().to_owned(),
                            name: e.name,
                            is_default: e.is_default,
                            size: None,
                            scale: None,
                            app: None,
                            modes: Vec::new(),
                            related_audio: None,
                            work_rect: None,
                        });
                    }
                }
            }
            SourceClass::Still | SourceClass::Media | SourceClass::Browser => {}
        }
        Ok(out)
    }

    /// Builds the node instance for `source` (control thread; may enumerate targets,
    /// up to 100 ms per adapter).
    ///
    /// - Adapter: the `adapter` setting, else the best probed adapter for the kind.
    /// - Target: the `target` setting (without an `adapter` setting, from the first
    ///   adapter, best first, that lists it, else the best adapter); else the first display or camera the adapter
    ///   lists, or the OS default audio endpoint (the first one if none is marked).
    ///   Windows have no default ([`SourceError::NoTarget`]). Without an `adapter`
    ///   setting, adapters are tried best first until one lists a default.
    /// - Capture nodes open their session in `prepare`; audio nodes open the stream
    ///   here, in Standby (no device activity, audio input port contract).
    pub fn instantiate(
        &self,
        source: &Source,
        ctx: &InstanceContext,
    ) -> Result<SourceInstance, SourceError> {
        let type_id = &source.type_id;
        let settings = &source.settings;
        let named = text(settings, keys::ADAPTER)?;
        let target = text(settings, keys::TARGET)?;
        match class_of(type_id).ok_or_else(|| SourceError::UnknownType(type_id.clone()))? {
            SourceClass::Capture(kind) => {
                let cursor = flag(settings, keys::CURSOR)?.unwrap_or(true);
                let size = match (
                    positive(settings, keys::WIDTH)?,
                    positive(settings, keys::HEIGHT)?,
                ) {
                    (Some(w), Some(h)) => Some(Size::new(w, h)),
                    (None, None) => None,
                    _ => {
                        return Err(SourceError::InvalidSetting {
                            key: keys::WIDTH,
                            expected: "set together with height",
                        });
                    }
                };
                let fps = match positive(settings, keys::FPS)? {
                    Some(n) => Some(FrameRate::new(n, 1).ok_or(SourceError::InvalidSetting {
                        key: keys::FPS,
                        expected: "a frame rate",
                    })?),
                    None => None,
                };
                let candidates = choose(&self.capture_candidates(kind), named, type_id)?;
                let (factory, target) = match target {
                    Some(t) => {
                        // Ids are adapter-defined: the adapter that lists the target,
                        // else the best one (picker-only targets are never listed).
                        let owner = candidates
                            .iter()
                            .find(|f| {
                                f.enumerate(kind)
                                    .is_ok_and(|l| l.iter().any(|x| x.id.as_str() == t))
                            })
                            .unwrap_or(&candidates[0]);
                        (Arc::clone(owner), TargetId::new(t))
                    }
                    None => default_capture_target(&candidates, kind)
                        .ok_or_else(|| SourceError::NoTarget(type_id.clone()))?,
                };
                let mut request =
                    CaptureRequest::new(target.clone(), Arc::clone(&ctx.clock)).with_cursor(cursor);
                if let Some(size) = size {
                    request = request.with_size(size);
                }
                if let Some(fps) = fps {
                    request = request.with_frame_rate(fps);
                }
                let adapter = factory.id();
                let node = CaptureSourceNode::new(factory, request);
                let stats = node.stats();
                Ok(SourceInstance::Video(VideoSource {
                    node,
                    stats,
                    adapter,
                    target,
                }))
            }
            SourceClass::Still => {
                let (w, h, rgba) = still_pixels(type_id.as_str(), settings)?;
                let node = StillSourceNode::new(w, h, rgba)
                    .map_err(|e| SourceError::Image(e.to_string()))?;
                let stats = node.stats();
                Ok(SourceInstance::Still(StillSource { node, stats }))
            }
            SourceClass::Media => self.instantiate_media(source, ctx),
            SourceClass::Browser => self.instantiate_browser(source, ctx),
            SourceClass::Audio(kind) => {
                let candidates = choose(&self.audio_candidates(kind), named, type_id)?;
                let (backend, endpoint) = match target {
                    Some(t) => {
                        let owner = candidates
                            .iter()
                            .find(|b| {
                                b.enumerate(kind)
                                    .is_ok_and(|l| l.iter().any(|x| x.id.as_str() == t))
                            })
                            .unwrap_or(&candidates[0]);
                        (Arc::clone(owner), EndpointId::new(t))
                    }
                    None => default_endpoint(&candidates, kind)
                        .ok_or_else(|| SourceError::NoTarget(type_id.clone()))?,
                };
                let exclude = flag(settings, keys::EXCLUDE_OWN_PROCESS)?.unwrap_or(false);
                let request = InputRequest::new(endpoint.clone(), Arc::clone(&ctx.clock))
                    .with_sample_rates(&[ctx.audio.format.sample_rate])
                    .with_exclude_own_process(exclude);
                let stream = backend.open_input(&request).map_err(SourceError::Audio)?;
                let (node, offset, stats) = InputNode::new(stream, ctx.audio, ctx.max_audio_offset);
                Ok(SourceInstance::Audio(AudioSource {
                    node,
                    offset,
                    stats,
                    adapter: backend.id(),
                    endpoint,
                }))
            }
        }
    }

    fn media_candidates(
        &self,
        named: Option<&str>,
    ) -> Vec<Arc<dyn rbs_media_decode::MediaDecoderFactory>> {
        let mut all = self.media.select(&Requirement::new()).candidates;
        let order: Vec<_> = self.media.ids().cloned().collect();
        all.sort_by_key(|factory| {
            let id = factory.id();
            let rank = self.media.capabilities(&id).map(pixel_rank).unwrap_or(2);
            let index = order
                .iter()
                .position(|item| item == &id)
                .unwrap_or(usize::MAX);
            (rank, index)
        });
        if let Some(name) = named {
            all.retain(|factory| factory.id().as_str() == name);
        }
        all
    }

    fn instantiate_media(
        &self,
        source: &Source,
        ctx: &InstanceContext,
    ) -> Result<SourceInstance, SourceError> {
        validate_media(&source.settings)?;
        let path_text = text(&source.settings, keys::PATH)?.ok_or(SourceError::InvalidSetting {
            key: keys::PATH,
            expected: "a local absolute path",
        })?;
        let path =
            rbs_media_decode::accept_local_media_path(path_text).map_err(|error| match error {
                rbs_media_decode::PathError::Invalid(expected) => SourceError::InvalidSetting {
                    key: keys::PATH,
                    expected,
                },
                rbs_media_decode::PathError::Media(text) => SourceError::Media(text),
            })?;
        let named = text(&source.settings, keys::ADAPTER)?;
        let candidates = self.media_candidates(named);
        if candidates.is_empty() {
            return Err(match named {
                Some(name) => SourceError::AdapterUnavailable(AdapterId::new(name)),
                None => SourceError::NoAdapter(source.type_id.clone()),
            });
        }
        let factory = Arc::clone(&candidates[0]);
        // Probe without keeping the decoder (ADR-0108 decision 3).
        {
            let mut decoder = factory
                .open(&path)
                .map_err(|error| SourceError::Media(error.to_string()))?;
            let sample = decoder
                .pull()
                .map_err(|error| SourceError::Media(error.to_string()))?;
            let picture =
                sample
                    .and_then(|sample| sample.picture)
                    .ok_or(SourceError::InvalidSetting {
                        key: keys::PATH,
                        expected: "a file with a picture stream",
                    })?;
            if let Some(why) =
                rbs_media_decode::refuse_picture(picture.width, picture.height, picture.bytes.len())
            {
                return Err(SourceError::InvalidSetting {
                    key: keys::WIDTH,
                    expected: why.expected(),
                });
            }
        }
        let loop_playback = flag(&source.settings, keys::LOOP)?.unwrap_or(true);
        let restart = flag(&source.settings, keys::RESTART_WHEN_SHOWN)?.unwrap_or(true);
        let node = rbs_nodes::MediaSourceNode::new(
            factory.clone(),
            path,
            loop_playback,
            restart,
            Arc::clone(&ctx.clock),
            ctx.audio.format.sample_rate.hz(),
            u16::try_from(ctx.audio.format.channels.channels()).unwrap_or(2),
            ctx.audio.block_frames,
        );
        let stats = node.stats();
        Ok(SourceInstance::Media(Box::new(MediaSource {
            node,
            stats,
            adapter: factory.id(),
        })))
    }

    fn instantiate_browser(
        &self,
        source: &Source,
        ctx: &InstanceContext,
    ) -> Result<SourceInstance, SourceError> {
        let settings = crate::browser::browser_settings(&source.settings)
            .map_err(|(key, expected)| SourceError::InvalidSetting { key, expected })?;
        let node = crate::browser::BrowserSourceNode::new(
            settings,
            Arc::clone(&ctx.browser_budget),
            source.id,
            ctx.browser_tree.clone(),
        );
        Ok(SourceInstance::Browser(BrowserSource { node }))
    }
}

fn pixel_rank(caps: &rbs_core::capability::CapabilitySet) -> u8 {
    match caps.get(&rbs_media_decode::caps::PIXEL) {
        Some(rbs_core::capability::CapabilityValue::Set(values))
            if values
                .iter()
                .any(|value| value == rbs_media_decode::pixel::ZERO_COPY) =>
        {
            0
        }
        Some(rbs_core::capability::CapabilityValue::Set(values))
            if values
                .iter()
                .any(|value| value == rbs_media_decode::pixel::CPU_UPLOAD) =>
        {
            1
        }
        _ => 2,
    }
}

fn validate_media(settings: &Settings) -> Result<(), SourceError> {
    for key in settings.keys() {
        if ![
            keys::ADAPTER,
            keys::PATH,
            keys::LOOP,
            keys::RESTART_WHEN_SHOWN,
        ]
        .contains(&key.as_str())
        {
            return Err(SourceError::InvalidSetting {
                key: "settings",
                expected: "only keys this source type uses",
            });
        }
    }
    match settings.get(keys::PATH) {
        Some(SettingValue::Text(path)) => {
            // Shape only here. `instantiate` canonicalizes before any decoder open.
            if rbs_media_decode::accept_local_media_path(path).is_err() && raw_path_is_remote(path)
            {
                return Err(SourceError::InvalidSetting {
                    key: keys::PATH,
                    expected: "a local absolute path, not a URL, UNC path, or device path",
                });
            }
        }
        Some(_) => {
            return Err(SourceError::InvalidSetting {
                key: keys::PATH,
                expected: "text",
            });
        }
        None => {
            return Err(SourceError::InvalidSetting {
                key: keys::PATH,
                expected: "a local absolute path",
            });
        }
    }
    flag(settings, keys::LOOP)?;
    flag(settings, keys::RESTART_WHEN_SHOWN)?;
    text(settings, keys::ADAPTER)?;
    Ok(())
}

fn raw_path_is_remote(path: &str) -> bool {
    let lower = path.trim().to_ascii_lowercase();
    lower.contains("://")
        || path.trim().starts_with(r"\\")
        || path.trim().starts_with("//")
        || lower.starts_with("/dev/")
        || lower.starts_with("/dev\\")
}

/// The pixels of an image or colour source from its settings (ADR-0074).
fn still_pixels(type_id: &str, settings: &Settings) -> Result<crate::still::Picture, SourceError> {
    use crate::still;
    if type_id == types::IMAGE_SOURCE {
        if let Some(path) = text(settings, keys::PATH)? {
            return still::decode_png(std::path::Path::new(path)).map_err(SourceError::Image);
        }
        let id = text(settings, keys::SAMPLE)?.ok_or(SourceError::InvalidSetting {
            key: keys::PATH,
            expected: "set (or choose a sample)",
        })?;
        let (w, h) = still::DEFAULT_SIZE;
        let rgba = still::sample_picture(id, w, h).ok_or(SourceError::InvalidSetting {
            key: keys::SAMPLE,
            expected: "the id of a built-in sample",
        })?;
        return Ok((w, h, rgba));
    }
    let colour = |key: &'static str| -> Result<Option<[u8; 4]>, SourceError> {
        match text(settings, key)? {
            None => Ok(None),
            Some(t) => still::parse_color(t)
                .map(Some)
                .ok_or(SourceError::InvalidSetting {
                    key,
                    expected: "a colour like #rrggbb or #rrggbbaa",
                }),
        }
    };
    let mut first = colour(keys::COLOR)?.unwrap_or([0, 0, 0, 255]);
    let mut second = colour(keys::COLOR2)?;
    // Absent means filled, so an older document stays a solid colour.
    if flag(settings, keys::FILLED)?.is_some_and(|on| !on) {
        first[3] = 0;
        if let Some(end) = second.as_mut() {
            end[3] = 0;
        }
    }
    let angle = match settings.get(keys::ANGLE) {
        None => 0.0,
        Some(SettingValue::Int(n)) => *n as f32,
        Some(_) => {
            return Err(SourceError::InvalidSetting {
                key: keys::ANGLE,
                expected: "an integer",
            });
        }
    };
    let (w, h) = match (
        positive(settings, keys::WIDTH)?,
        positive(settings, keys::HEIGHT)?,
    ) {
        (Some(w), Some(h)) => (w, h),
        (None, None) => still::DEFAULT_SIZE,
        _ => {
            return Err(SourceError::InvalidSetting {
                key: keys::WIDTH,
                expected: "set together with height",
            });
        }
    };
    if w > still::MAX_SIDE || h > still::MAX_SIDE {
        return Err(SourceError::InvalidSetting {
            key: keys::WIDTH,
            expected: "at most 8192 pixels on a side",
        });
    }
    Ok((w, h, still::color_picture(w, h, first, second, angle)))
}

fn default_capture_target(
    candidates: &[Arc<dyn CaptureFactory>],
    kind: TargetKind,
) -> Option<(Arc<dyn CaptureFactory>, TargetId)> {
    if kind == TargetKind::Window {
        return None;
    }
    candidates.iter().find_map(|f| {
        let first = f.enumerate(kind).ok()?.into_iter().next()?;
        Some((Arc::clone(f), first.id))
    })
}

fn default_endpoint(
    candidates: &[Arc<dyn AudioBackend>],
    kind: EndpointKind,
) -> Option<(Arc<dyn AudioBackend>, EndpointId)> {
    candidates.iter().find_map(|b| {
        let endpoints = b.enumerate(kind).ok()?;
        let chosen = endpoints
            .iter()
            .find(|e| e.is_default)
            .or_else(|| endpoints.first())?;
        Some((Arc::clone(b), chosen.id.clone()))
    })
}

#[cfg(test)]
mod fill_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
    use super::*;

    fn colour_settings(filled: Option<bool>) -> Settings {
        let mut settings = Settings::new();
        settings.insert(keys::COLOR.into(), SettingValue::Text("#ff8000".into()));
        settings.insert(keys::WIDTH.into(), SettingValue::Int(2));
        settings.insert(keys::HEIGHT.into(), SettingValue::Int(1));
        if let Some(on) = filled {
            settings.insert(keys::FILLED.into(), SettingValue::Bool(on));
        }
        settings
    }

    #[test]
    fn a_hollow_colour_keeps_its_rgb_and_clears_the_alpha() {
        let (_, _, px) = still_pixels(types::COLOR_SOURCE, &colour_settings(Some(false))).unwrap();
        assert_eq!(px, [255, 128, 0, 0, 255, 128, 0, 0]);
    }

    #[test]
    fn a_missing_filled_flag_stays_opaque() {
        let (_, _, px) = still_pixels(types::COLOR_SOURCE, &colour_settings(None)).unwrap();
        assert_eq!(px[3], 255);
        assert_eq!(px[7], 255);
    }

    #[test]
    fn filled_must_be_a_bool() {
        let id = NodeTypeId::new(types::COLOR_SOURCE);
        let mut bad = colour_settings(None);
        bad.insert(keys::FILLED.into(), SettingValue::Text("no".into()));
        assert!(validate_settings(&id, &bad).is_err());
        assert!(validate_settings(&id, &colour_settings(Some(false))).is_ok());
    }
}
