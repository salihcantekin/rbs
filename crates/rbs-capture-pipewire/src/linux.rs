//! The Linux backend: node enumeration, format negotiation and the stream thread.
//!
//! Threads: `open`, `start`, `stop` and `enumerate` run on a control thread;
//! `latest_frame` on the render thread; the PipeWire main loop of an Active session on
//! its own thread (`rbs-pipewire`), which owns every PipeWire object of the session.
//! The render thread only touches the lock-free ring.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::os::fd::{FromRawFd, OwnedFd};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Once, PoisonError, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use bytes::BytesMut;
use pipewire as pw;
use pw::spa;
use pw::spa::pod::{ChoiceValue, Object, Pod, PropertyFlags, Value, serialize::PodSerializer};
use pw::spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Fraction, Id, Rectangle, SpaTypes};

use rbs_capture::handoff::{FrameMeta, Grave, Graveyard};
use rbs_capture::{
    CaptureChange, CaptureError, CaptureFactory, CaptureRequest, CaptureSession, CaptureStats,
    CaptureTarget, ErrorDomain, GpuDeviceId, Permission, SyncKind, TargetId, TargetKind, caps,
};
use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::media::{
    ColorSpace, DmaBufPlane, DmaBufSurface, MemoryKind, PixelFormat, Size, VideoFormat, VideoFrame,
};
use rbs_core::registry::{AdapterFactory, AdapterId, HARDWARE, ProbeError};
use rbs_core::time::{Clock, Timestamp};
use rbs_core::timing::{DriftEstimator, FrameRate};

use crate::portal::{self, Cast, CastRequest};
use crate::{ADAPTER_ID, Consumer, FRAMES_IN_FLIGHT, Picture, RING_SLOTS, STREAM_BUFFERS, Shared};

/// Highest frame rate offered in the stream negotiation.
const MAX_RATE: u32 = 240;

/// Longest wait for the PipeWire daemon in `open` and `start`.
const DAEMON_TIMEOUT: Duration = Duration::from_millis(500);

/// Longest wait for the PipeWire daemon in `enumerate` (the port's 100 ms budget).
const ENUMERATE_TIMEOUT: Duration = Duration::from_millis(100);

const NODE_PREFIX: &str = "node:";

/// Ids of portal targets: `portal:display:<key>` or `portal:window:<key>`, where the key
/// is the restore token the portal handed out at `pick`, or [`LIVE_PREFIX`] and a
/// number for a pick the portal gave no token for.
const PORTAL_PREFIX: &str = "portal:";

/// Keys of picks without a restore token, whose cast the factory keeps until a session
/// starts it.
const LIVE_PREFIX: &str = "live-";

/// Picks without a restore token kept at once; the oldest is closed first.
const MAX_LIVE: usize = 4;

/// Longest wait for the portal to restore a choice at `start`. A valid token restores
/// without a dialog in well under this; a portal that shows its dialog instead (the
/// token is no longer valid) is answered with `TargetNotFound` and the dialog closed.
const RESTORE_TIMEOUT: Duration = Duration::from_secs(2);

/// Largest accepted age, in nanoseconds, of a buffer's header timestamp at arrival.
const MAX_PTS_AGE: u64 = 1_000_000_000;

/// `DRM_FORMAT_MOD_LINEAR`, offered when the importer names no modifiers.
const MOD_LINEAR: u64 = 0;

/// DRM fourcc codes of the formats the importer accepts (gpu-compositor.md, Native
/// import).
const DRM_AR24: u32 = u32::from_le_bytes(*b"AR24");
const DRM_XR24: u32 = u32::from_le_bytes(*b"XR24");
const DRM_AB24: u32 = u32::from_le_bytes(*b"AB24");
const DRM_XB24: u32 = u32::from_le_bytes(*b"XB24");

static INIT: Once = Once::new();

/// Rings of stopped sessions that frames still refer to. Its janitor thread drops them
/// once idle, so the last lease never closes fds or frees memory on the render thread.
/// Importers duplicate DMA-BUF fds, so a short grace is enough.
static GRAVEYARD: Graveyard = Graveyard::new(
    "rbs-pipewire-janitor",
    Duration::from_millis(100),
    Duration::from_millis(100),
);

/// The pictures of a stopped session, kept while frames refer to them.
struct RingGrave {
    shared: Arc<Shared>,
    bytes: u64,
}

impl Grave for RingGrave {
    fn is_idle(&self) -> bool {
        self.shared.ring.is_idle()
    }
    fn bytes(&self) -> u64 {
        self.bytes
    }
}

fn init() {
    INIT.call_once(pw::init);
}

fn backend(code: i64) -> CaptureError {
    CaptureError::Backend {
        domain: ErrorDomain::Errno,
        code,
        recoverable: true,
    }
}

fn pw_error(_e: pw::Error) -> CaptureError {
    backend(i64::from(libc::ECONNREFUSED))
}

/// A PipeWire video format the adapter can deliver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Wire {
    spa: spa::param::video::VideoFormat,
    pixel_format: PixelFormat,
    fourcc: u32,
    /// The fourth byte is undefined (BGRx, RGBx).
    opaque: bool,
}

const WIRES: [Wire; 4] = [
    Wire {
        spa: spa::param::video::VideoFormat::BGRA,
        pixel_format: PixelFormat::Bgra8,
        fourcc: DRM_AR24,
        opaque: false,
    },
    Wire {
        spa: spa::param::video::VideoFormat::BGRx,
        pixel_format: PixelFormat::Bgra8,
        fourcc: DRM_XR24,
        opaque: true,
    },
    Wire {
        spa: spa::param::video::VideoFormat::RGBA,
        pixel_format: PixelFormat::Rgba8,
        fourcc: DRM_AB24,
        opaque: false,
    },
    Wire {
        spa: spa::param::video::VideoFormat::RGBx,
        pixel_format: PixelFormat::Rgba8,
        fourcc: DRM_XB24,
        opaque: true,
    },
];

fn wire_of(format: spa::param::video::VideoFormat) -> Option<Wire> {
    WIRES.iter().copied().find(|w| w.spa == format)
}

/// The path of the PipeWire socket the default connection uses.
pub(crate) fn socket_present() -> bool {
    if let Some(remote) = std::env::var_os("PIPEWIRE_REMOTE") {
        let path = std::path::PathBuf::from(remote);
        if path.is_absolute() {
            return path.exists();
        }
    }
    let Some(dir) =
        std::env::var_os("PIPEWIRE_RUNTIME_DIR").or_else(|| std::env::var_os("XDG_RUNTIME_DIR"))
    else {
        return false;
    };
    let name = std::env::var_os("PIPEWIRE_REMOTE").unwrap_or_else(|| "pipewire-0".into());
    std::path::Path::new(&dir).join(name).exists()
}

/// A video source node.
struct Node {
    name: String,
    description: String,
}

/// Lists video source nodes with one round trip to the daemon (control thread only).
fn video_sources() -> Result<Vec<Node>, CaptureError> {
    Ok(query(None, ENUMERATE_TIMEOUT)?.0)
}

/// A format a node offers: a deliverable wire format in shared memory or as DMA-BUF.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NodeFormat {
    wire: Wire,
    memory: MemoryKind,
}

/// The `Video/Source` nodes, and the formats of the node named `formats_of` (`None`
/// when no such node exists). One round trip, plus one more to read the formats.
fn query(
    formats_of: Option<&str>,
    timeout: Duration,
) -> Result<(Vec<Node>, Option<Vec<NodeFormat>>), CaptureError> {
    init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(pw_error)?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(pw_error)?;
    let core = context.connect_rc(None).map_err(pw_error)?;
    let registry = core.get_registry_rc().map_err(pw_error)?;

    let found: Rc<RefCell<Vec<Node>>> = Rc::new(RefCell::new(Vec::new()));
    let formats: Rc<RefCell<Option<Vec<NodeFormat>>>> = Rc::new(RefCell::new(None));
    // The proxy and listener of the bound node; they must outlive the loop.
    let bound: Rc<RefCell<Option<(pw::node::Node, pw::node::NodeListener)>>> =
        Rc::new(RefCell::new(None));
    // Sequence number that ends the loop: the first sync, or the one sent after
    // asking the bound node for its formats.
    let last = Rc::new(Cell::new(core.sync(0).map_err(pw_error)?));
    let done = Rc::new(Cell::new(false));
    let _core_listener = core
        .add_listener_local()
        .done({
            let done = Rc::clone(&done);
            let last = Rc::clone(&last);
            let mainloop = mainloop.clone();
            move |id, seq| {
                if id == pw::core::PW_ID_CORE && seq == last.get() {
                    done.set(true);
                    mainloop.quit();
                }
            }
        })
        .register();
    let wanted = formats_of.map(str::to_owned);
    let _registry_listener = registry
        .add_listener_local()
        .global({
            let found = Rc::clone(&found);
            let formats = Rc::clone(&formats);
            let bound = Rc::clone(&bound);
            let registry = registry.clone();
            let core = core.clone();
            let last = Rc::clone(&last);
            move |global| {
                if global.type_ != pw::types::ObjectType::Node {
                    return;
                }
                let Some(props) = global.props else {
                    return;
                };
                if props.get("media.class") != Some("Video/Source") {
                    return;
                }
                let Some(name) = props.get("node.name") else {
                    return;
                };
                let description = props
                    .get("node.description")
                    .or_else(|| props.get("node.nick"))
                    .unwrap_or(name);
                found.borrow_mut().push(Node {
                    name: name.to_owned(),
                    description: description.to_owned(),
                });
                if wanted.as_deref() != Some(name) || bound.borrow().is_some() {
                    return;
                }
                let Ok(node) = registry.bind::<pw::node::Node, _>(global) else {
                    return;
                };
                *formats.borrow_mut() = Some(Vec::new());
                let listener = node
                    .add_listener_local()
                    .param({
                        let formats = Rc::clone(&formats);
                        move |_, _, _, _, param| {
                            if let (Some(param), Some(list)) =
                                (param, formats.borrow_mut().as_mut())
                            {
                                node_formats(param, list);
                            }
                        }
                    })
                    .register();
                node.enum_params(0, Some(spa::param::ParamType::EnumFormat), 0, u32::MAX);
                if let Ok(seq) = core.sync(0) {
                    last.set(seq);
                }
                *bound.borrow_mut() = Some((node, listener));
            }
        })
        .register();
    let timer = mainloop.loop_().add_timer({
        let mainloop = mainloop.clone();
        move |_| mainloop.quit()
    });
    let _ = timer.update_timer(Some(timeout), None);
    mainloop.run();
    if !done.get() {
        return Err(backend(i64::from(libc::ETIMEDOUT)));
    }
    let nodes = std::mem::take(&mut *found.borrow_mut());
    let formats = formats.borrow_mut().take();
    Ok((nodes, formats))
}

/// Adds the deliverable formats of one `EnumFormat` param to `list`.
fn node_formats(param: &Pod, list: &mut Vec<NodeFormat>) {
    use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    use spa::pod::deserialize::PodDeserializer;

    let Ok((_, Value::Object(object))) = PodDeserializer::deserialize_any_from(param.as_bytes())
    else {
        return;
    };
    let id_of = |key: FormatProperties| {
        object
            .properties
            .iter()
            .find(|p| p.key == key.as_raw())
            .map(|p| &p.value)
    };
    let is = |key, want: u32| matches!(id_of(key), Some(Value::Id(Id(v))) if *v == want);
    if !is(FormatProperties::MediaType, MediaType::Video.as_raw())
        || !is(FormatProperties::MediaSubtype, MediaSubtype::Raw.as_raw())
    {
        return;
    }
    let memory = if id_of(FormatProperties::VideoModifier).is_some() {
        MemoryKind::DmaBuf
    } else {
        MemoryKind::System
    };
    let ids: Vec<u32> = match id_of(FormatProperties::VideoFormat) {
        Some(Value::Id(Id(v))) => vec![*v],
        Some(Value::Choice(ChoiceValue::Id(Choice(_, choice)))) => match choice {
            ChoiceEnum::None(Id(v)) => vec![*v],
            ChoiceEnum::Enum {
                default,
                alternatives,
            } => std::iter::once(default)
                .chain(alternatives)
                .map(|Id(v)| *v)
                .collect(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    };
    for id in ids {
        if let Some(wire) = wire_of(spa::param::video::VideoFormat::from_raw(id)) {
            let format = NodeFormat { wire, memory };
            if !list.contains(&format) {
                list.push(format);
            }
        }
    }
}

/// The offer narrowed to the node's formats: the first requested pixel format the node
/// offers in a requested memory kind, then the first such memory kind, so the
/// negotiation can only land on what the request order prefers. `None` if nothing
/// matches.
fn narrow(offer: &Offer, available: &[NodeFormat]) -> Option<Offer> {
    let pairs = offer.wires.iter().flat_map(|wire| {
        offer.memory.iter().map(move |memory| NodeFormat {
            wire: *wire,
            memory: *memory,
        })
    });
    let chosen = pairs.into_iter().find(|pair| available.contains(pair))?;
    // Every wire of the chosen pixel format in the chosen memory kind (BGRA and BGRx
    // both deliver Bgra8).
    let wires = offer
        .wires
        .iter()
        .copied()
        .filter(|w| {
            w.pixel_format == chosen.wire.pixel_format
                && available.contains(&NodeFormat {
                    wire: *w,
                    memory: chosen.memory,
                })
        })
        .collect();
    Some(Offer {
        wires,
        memory: vec![chosen.memory],
        modifiers: offer.modifiers.clone(),
        frame_rate: offer.frame_rate,
    })
}

/// What a target id names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target<'a> {
    /// A video source node, by `node.name`.
    Node(&'a str),
    /// A portal screen cast, by restore token or live pick key.
    Portal { kind: TargetKind, key: &'a str },
}

fn parse(id: &TargetId) -> Option<Target<'_>> {
    let id = id.as_str();
    if let Some(name) = id.strip_prefix(NODE_PREFIX) {
        return (!name.is_empty()).then_some(Target::Node(name));
    }
    let (kind, key) = id.strip_prefix(PORTAL_PREFIX)?.split_once(':')?;
    let kind = match kind {
        "display" => TargetKind::Display,
        "window" => TargetKind::Window,
        _ => return None,
    };
    (!key.is_empty()).then_some(Target::Portal { kind, key })
}

fn portal_id(kind: TargetKind, key: &str) -> TargetId {
    TargetId::new(format!("{PORTAL_PREFIX}{}:{key}", kind.as_str()))
}

/// The portal choices of one factory, shared with its sessions.
#[derive(Debug, Default)]
struct Picks {
    /// Every key and restore token seen, by choice (an index into `newest`). Restore
    /// tokens are single use, so all ids that ever named a choice map to it.
    choices: HashMap<String, usize>,
    /// The newest restore token per choice; `None` once a start returned no new token,
    /// so the choice cannot be restored any more.
    newest: Vec<Option<String>>,
    /// Casts picked without a restore token, until a session starts them.
    live: Vec<(String, Cast)>,
    next_live: u64,
}

fn lock(picks: &Mutex<Picks>) -> MutexGuard<'_, Picks> {
    picks.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Picks {
    /// The choice `key` names, added with `key` as its token if unknown (an id stored by
    /// an earlier run).
    fn choice(&mut self, key: &str) -> usize {
        if let Some(choice) = self.choices.get(key) {
            return *choice;
        }
        self.newest.push(Some(key.to_owned()));
        let choice = self.newest.len() - 1;
        self.choices.insert(key.to_owned(), choice);
        choice
    }

    /// The newest token of the choice `key` names: `None` if it cannot be restored.
    fn newest(&mut self, key: &str) -> Option<String> {
        let choice = self.choice(key);
        self.newest.get(choice).cloned().flatten()
    }

    /// Records `token` as the newest of the choice `key` names (`None`: no token left).
    fn advance(&mut self, key: &str, token: Option<String>) {
        let choice = self.choice(key);
        if let Some(token) = &token {
            self.choices.insert(token.clone(), choice);
        }
        if let Some(newest) = self.newest.get_mut(choice) {
            *newest = token;
        }
    }
}

impl Drop for Picks {
    fn drop(&mut self) {
        for (_, cast) in &mut self.live {
            cast.close();
        }
    }
}

/// What the stream offers, from the request.
#[derive(Debug, Clone)]
struct Offer {
    /// Formats in request order.
    wires: Vec<Wire>,
    /// Memory kinds in request order: `DmaBuf` and/or `System`.
    memory: Vec<MemoryKind>,
    /// Importer modifiers per fourcc (empty: linear only).
    modifiers: Vec<(u32, u64)>,
    frame_rate: Option<FrameRate>,
}

impl Offer {
    fn modifiers_for(&self, fourcc: u32) -> Vec<i64> {
        let listed: Vec<i64> = self
            .modifiers
            .iter()
            .filter(|(f, _)| *f == fourcc)
            .map(|(_, m)| *m as i64)
            .collect();
        if listed.is_empty() {
            vec![MOD_LINEAR as i64]
        } else {
            listed
        }
    }
}

/// Checks the request against what the adapter can deliver (capture-port.md,
/// zero-copy negotiation).
fn accept(request: &CaptureRequest) -> Result<Offer, CaptureError> {
    let wires: Vec<Wire> = request
        .pixel_formats
        .iter()
        .flat_map(|pf| WIRES.iter().copied().filter(move |w| w.pixel_format == *pf))
        .collect();
    let mut memory = Vec::new();
    let mut modifiers = Vec::new();
    for kind in &request.memory {
        match kind {
            MemoryKind::System => memory.push(MemoryKind::System),
            MemoryKind::DmaBuf => {
                let Some(gpu) = request.gpu.as_ref() else {
                    continue;
                };
                if !matches!(gpu.device, GpuDeviceId::DrmRenderNode { .. })
                    || !gpu.sync.contains(&SyncKind::Ready)
                {
                    continue;
                }
                modifiers = gpu.dmabuf_modifiers.clone();
                memory.push(MemoryKind::DmaBuf);
            }
            _ => {}
        }
    }
    if wires.is_empty() || memory.is_empty() {
        return Err(CaptureError::FormatUnsupported);
    }
    Ok(Offer {
        wires,
        memory,
        modifiers,
        frame_rate: request.frame_rate,
    })
}

/// Factory of the PipeWire adapter.
#[derive(Debug, Default)]
pub struct PipeWireCaptureFactory {
    picks: Arc<Mutex<Picks>>,
}

impl PipeWireCaptureFactory {
    /// Creates the factory. Does nothing until used.
    pub fn new() -> Self {
        Self::default()
    }
}

impl AdapterFactory for PipeWireCaptureFactory {
    fn id(&self) -> AdapterId {
        AdapterId::new(ADAPTER_ID)
    }

    fn display_name(&self) -> &str {
        "PipeWire"
    }

    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        // Cheap (capture-port.md: within 50 ms): only checks that the daemon's socket
        // exists; no connection.
        if !socket_present() {
            return Err(ProbeError::new("no PipeWire daemon socket for this user"));
        }
        let set =
            |items: &[&str]| CapabilityValue::Set(items.iter().map(|s| (*s).to_owned()).collect());
        // Screens and windows come through the desktop portal on the session bus;
        // whether the portal runs is only known when `pick` asks it.
        let portal = portal::bus_present();
        let kinds: &[&str] = if portal {
            &["display", "window", "camera"]
        } else {
            &["camera"]
        };
        let picker: &[&str] = if portal { &["display", "window"] } else { &[] };
        Ok(CapabilitySet::new()
            .with(caps::TARGET_KINDS, set(kinds))
            .with(caps::TARGET_ENUMERATE, set(&["camera"]))
            .with(caps::TARGET_PICKER, set(picker))
            .with(
                caps::PIXEL_FORMATS,
                set(&[PixelFormat::Bgra8.as_str(), PixelFormat::Rgba8.as_str()]),
            )
            .with(
                caps::MEMORY,
                set(&[MemoryKind::DmaBuf.as_str(), MemoryKind::System.as_str()]),
            )
            .with(caps::CURSOR, CapabilityValue::Flag(portal))
            .with(
                caps::FRAME_RATE_MAX,
                CapabilityValue::Int(i64::from(MAX_RATE)),
            )
            .with(
                caps::FRAMES_IN_FLIGHT,
                CapabilityValue::Int(FRAMES_IN_FLIGHT as i64),
            )
            .with(caps::HDR, CapabilityValue::Flag(false))
            .with(HARDWARE, CapabilityValue::Flag(true)))
    }
}

impl CaptureFactory for PipeWireCaptureFactory {
    fn enumerate(&self, kind: TargetKind) -> Result<Vec<CaptureTarget>, CaptureError> {
        if kind != TargetKind::Camera {
            return Err(CaptureError::Unsupported(
                "screens and windows are chosen with pick (desktop portal)",
            ));
        }
        Ok(video_sources()?
            .into_iter()
            .map(|node| {
                CaptureTarget::new(
                    TargetId::new(format!("{NODE_PREFIX}{}", node.name)),
                    TargetKind::Camera,
                    node.description,
                )
            })
            .collect())
    }

    fn open(&self, request: &CaptureRequest) -> Result<Box<dyn CaptureSession>, CaptureError> {
        let target = parse(&request.target).ok_or(CaptureError::TargetNotFound)?;
        let offer = accept(request)?;
        let (source, offer) = match target {
            Target::Node(name) => {
                let Some(available) = query(Some(name), DAEMON_TIMEOUT)?.1 else {
                    return Err(CaptureError::TargetNotFound);
                };
                let offer = narrow(&offer, &available).ok_or(CaptureError::FormatUnsupported)?;
                (Source::Node(name.to_owned()), offer)
            }
            Target::Portal { kind, key } => {
                let mut picks = lock(&self.picks);
                let known = if key.starts_with(LIVE_PREFIX) {
                    picks.live.iter().any(|(k, _)| k == key)
                } else {
                    picks.newest(key).is_some()
                };
                drop(picks);
                if !known {
                    return Err(CaptureError::TargetNotFound);
                }
                let offer = portal_offer(offer).ok_or(CaptureError::FormatUnsupported)?;
                let source = Source::Portal(PortalSource {
                    kind,
                    key: key.to_owned(),
                    token: None,
                    cursor: request.cursor,
                    picks: Arc::clone(&self.picks),
                });
                (source, offer)
            }
        };
        let pixel_format = offer
            .wires
            .first()
            .map_or(PixelFormat::Bgra8, |w| w.pixel_format);
        Ok(Box::new(PipeWireSession {
            source,
            offer,
            clock: Arc::clone(&request.clock),
            format: VideoFormat::new(Size::new(0, 0), pixel_format, ColorSpace::SRGB),
            active: None,
            next_sequence: 0,
            last_timestamp: None,
            base: CaptureStats::default(),
            retired: None,
        }))
    }

    fn pick(&self, kind: TargetKind) -> Result<CaptureTarget, CaptureError> {
        if !matches!(kind, TargetKind::Display | TargetKind::Window) {
            return Err(CaptureError::Unsupported("cameras are listed by enumerate"));
        }
        if !portal::bus_present() {
            return Err(CaptureError::Unsupported(
                "no session bus for the desktop portal",
            ));
        }
        // Waits for the user; the cursor is embedded so a live pick (below) shows it.
        let mut cast = Cast::start(&CastRequest {
            kind,
            cursor: true,
            restore_token: None,
            timeout: None,
        })?;
        let size = cast.stream.size;
        let mut picks = lock(&self.picks);
        let key = match cast.stream.restore_token.clone() {
            Some(token) => {
                // A session restores the choice with the token at `start`; nothing is
                // kept meanwhile (Standby, P13).
                cast.close();
                drop(cast);
                picks.advance(&token, Some(token.clone()));
                token
            }
            None => {
                // The portal cannot restore this choice (no persistence, or the user
                // declined it): keep the started cast for the session that opens it.
                picks.next_live += 1;
                let key = format!("{LIVE_PREFIX}{}", picks.next_live);
                picks.live.push((key.clone(), cast));
                if picks.live.len() > MAX_LIVE {
                    let (_, mut oldest) = picks.live.remove(0);
                    oldest.close();
                }
                key
            }
        };
        drop(picks);
        let name = match kind {
            TargetKind::Display => "Picked display",
            _ => "Picked window",
        };
        let target = CaptureTarget::new(portal_id(kind, &key), kind, name);
        Ok(match size {
            Some((w, h)) => target.with_size(w, h),
            None => target,
        })
    }

    fn permission(&self, _kind: TargetKind) -> Permission {
        // Outside a sandbox the daemon grants access to nodes; inside one the camera
        // portal decides, which `start` reports as `PermissionDenied`. Screens and
        // windows are granted per choice in the portal's dialog (`pick`), so there is
        // no separate permission to ask for.
        Permission::Granted
    }
}

/// The offer for a portal screen cast. Compositors deliver screens and windows in
/// BGRA or BGRx (mutter, KWin, wlroots); RGB orders are not offered, so `format()` is
/// exact before `start`. Memory kinds stay in request order: whether the compositor
/// can share DMA-BUFs with the importer's modifiers is only known once linked.
fn portal_offer(offer: Offer) -> Option<Offer> {
    if offer.wires.first()?.pixel_format != PixelFormat::Bgra8 {
        // The first requested format the adapter supports must win (invariant 4);
        // when that is RGBA, a portal target cannot deliver it.
        return None;
    }
    let wires: Vec<Wire> = offer
        .wires
        .into_iter()
        .filter(|w| w.pixel_format == PixelFormat::Bgra8)
        .collect();
    Some(Offer { wires, ..offer })
}

fn serialize(value: Value) -> Option<Vec<u8>> {
    PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &value)
        .ok()
        .map(|(cursor, _)| cursor.into_inner())
}

fn prop(key: u32, value: Value) -> spa::pod::Property {
    spa::pod::Property::new(key, value)
}

/// One `EnumFormat` object: `wire` in shared memory, or as DMA-BUF with `modifiers`
/// (fixed when it holds one modifier).
fn enum_format(wire: Wire, modifiers: Option<&[i64]>, rate: Option<FrameRate>) -> Option<Vec<u8>> {
    let max_rate = rate.map_or(MAX_RATE, |r| {
        r.per_second().num().div_ceil(r.per_second().den().max(1))
    });
    let mut properties = vec![
        prop(
            spa::param::format::FormatProperties::MediaType.as_raw(),
            Value::Id(Id(spa::param::format::MediaType::Video.as_raw())),
        ),
        prop(
            spa::param::format::FormatProperties::MediaSubtype.as_raw(),
            Value::Id(Id(spa::param::format::MediaSubtype::Raw.as_raw())),
        ),
        prop(
            spa::param::format::FormatProperties::VideoFormat.as_raw(),
            Value::Id(Id(wire.spa.as_raw())),
        ),
        prop(
            spa::param::format::FormatProperties::VideoSize.as_raw(),
            Value::Choice(ChoiceValue::Rectangle(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Range {
                    default: Rectangle {
                        width: 1920,
                        height: 1080,
                    },
                    min: Rectangle {
                        width: 1,
                        height: 1,
                    },
                    max: Rectangle {
                        width: 16384,
                        height: 16384,
                    },
                },
            ))),
        ),
        prop(
            spa::param::format::FormatProperties::VideoFramerate.as_raw(),
            Value::Choice(ChoiceValue::Fraction(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Range {
                    default: Fraction {
                        num: max_rate.min(60),
                        denom: 1,
                    },
                    min: Fraction { num: 0, denom: 1 },
                    max: Fraction {
                        num: max_rate,
                        denom: 1,
                    },
                },
            ))),
        ),
    ];
    if let Some(modifiers) = modifiers {
        let (value, flags) = match modifiers {
            [only] => (Value::Long(*only), PropertyFlags::MANDATORY),
            [first, ..] => (
                Value::Choice(ChoiceValue::Long(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Enum {
                        default: *first,
                        alternatives: modifiers.to_vec(),
                    },
                ))),
                PropertyFlags::MANDATORY | PropertyFlags::DONT_FIXATE,
            ),
            [] => return None,
        };
        properties.push(spa::pod::Property {
            key: spa::param::format::FormatProperties::VideoModifier.as_raw(),
            flags,
            value,
        });
    }
    serialize(Value::Object(Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties,
    }))
}

/// The `EnumFormat` params of an offer, in preference order. DMA-BUF entries carry the
/// importer's modifiers; with more than one, the source fixes the modifier.
fn enum_formats(offer: &Offer) -> Vec<Vec<u8>> {
    let mut params = Vec::new();
    for memory in &offer.memory {
        for wire in &offer.wires {
            let pod = match memory {
                MemoryKind::DmaBuf => enum_format(
                    *wire,
                    Some(&offer.modifiers_for(wire.fourcc)),
                    offer.frame_rate,
                ),
                _ => enum_format(*wire, None, offer.frame_rate),
            };
            params.extend(pod);
        }
    }
    params
}

/// `Buffers` and `Meta` params for the negotiated format.
fn buffer_params(dmabuf: bool) -> Vec<Vec<u8>> {
    let data_types = if dmabuf {
        1 << spa::sys::SPA_DATA_DmaBuf
    } else {
        (1 << spa::sys::SPA_DATA_MemFd) | (1 << spa::sys::SPA_DATA_MemPtr)
    };
    let buffers = serialize(Value::Object(Object {
        type_: SpaTypes::ObjectParamBuffers.as_raw(),
        id: spa::param::ParamType::Buffers.as_raw(),
        properties: vec![
            prop(
                spa::sys::SPA_PARAM_BUFFERS_buffers,
                Value::Choice(ChoiceValue::Int(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Range {
                        default: STREAM_BUFFERS as i32,
                        // DMA-BUF slots keep their buffers dequeued, and freed ones are
                        // returned when the next buffer arrives: the source must keep at
                        // least one buffer while every slot holds one.
                        min: if dmabuf { RING_SLOTS as i32 + 1 } else { 2 },
                        max: 16,
                    },
                ))),
            ),
            prop(
                spa::sys::SPA_PARAM_BUFFERS_dataType,
                Value::Choice(ChoiceValue::Int(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Flags {
                        default: data_types,
                        flags: vec![data_types],
                    },
                ))),
            ),
        ],
    }));
    let meta = serialize(Value::Object(Object {
        type_: SpaTypes::ObjectParamMeta.as_raw(),
        id: spa::param::ParamType::Meta.as_raw(),
        properties: vec![
            prop(
                spa::sys::SPA_PARAM_META_type,
                Value::Id(Id(spa::sys::SPA_META_Header)),
            ),
            prop(
                spa::sys::SPA_PARAM_META_size,
                Value::Int(std::mem::size_of::<spa::sys::spa_meta_header>() as i32),
            ),
        ],
    }));
    buffers.into_iter().chain(meta).collect()
}

fn pods(bytes: &[Vec<u8>]) -> Vec<&Pod> {
    bytes.iter().filter_map(|b| Pod::from_bytes(b)).collect()
}

/// The format both sides agreed on.
#[derive(Debug, Clone, Copy)]
struct Negotiated {
    wire: Wire,
    width: u32,
    height: u32,
    dmabuf: bool,
    modifier: u64,
}

/// Nanoseconds on `CLOCK_MONOTONIC`, the clock of PipeWire's buffer timestamps.
fn monotonic_nanos() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid out pointer.
    let _ = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    let secs = u64::try_from(ts.tv_sec).unwrap_or(0);
    let nanos = u64::try_from(ts.tv_nsec).unwrap_or(0);
    secs.saturating_mul(1_000_000_000).saturating_add(nanos)
}

/// Counters the stream thread shares with the session.
#[derive(Default)]
struct Status {
    lost: AtomicBool,
    copied: AtomicU64,
}

/// State of the stream thread. Lives on that thread only.
struct Producer {
    shared: Arc<Shared>,
    status: Arc<Status>,
    offer: Offer,
    negotiated: Option<Negotiated>,
    /// PipeWire buffers kept dequeued for DMA-BUF slots, by slot.
    held: [*mut pw::sys::pw_buffer; RING_SLOTS],
    sequence: u64,
    clock: Arc<dyn Clock>,
    drift: DriftEstimator,
    last_timestamp: Option<Timestamp>,
    started: Option<mpsc::SyncSender<Result<(), CaptureError>>>,
}

impl Producer {
    fn report_start(&mut self, result: Result<(), CaptureError>) {
        if let Some(tx) = self.started.take() {
            let _ = tx.try_send(result);
        }
    }

    /// Gives free DMA-BUF slots' buffers back to the source.
    fn return_free(&mut self, stream: &pw::stream::Stream) {
        for (slot, held) in self.held.iter_mut().enumerate() {
            if !held.is_null() && self.shared.ring.is_free(slot) {
                // SAFETY: a buffer this stream dequeued and has not queued since; the
                // slot is free, so no frame refers to it.
                unsafe { stream.queue_raw_buffer(*held) };
                *held = std::ptr::null_mut();
            }
        }
    }

    /// PipeWire frees `buffer` (renegotiation or disconnect). A slot that kept it
    /// dequeued forgets it, so it is never queued again; frames still referring to the
    /// slot keep the memory alive through the slot's duplicated fd.
    fn on_remove_buffer(&mut self, buffer: *mut pw::sys::pw_buffer) {
        for held in &mut self.held {
            if *held == buffer {
                *held = std::ptr::null_mut();
            }
        }
    }

    fn on_param(&mut self, stream: &pw::stream::Stream, id: u32, param: Option<&Pod>) {
        let Some(param) = param else {
            return;
        };
        if id != spa::param::ParamType::Format.as_raw() {
            return;
        }
        let mut info = spa::param::video::VideoInfoRaw::new();
        if info.parse(param).is_err() {
            return;
        }
        let Some(wire) = wire_of(info.format()) else {
            return;
        };
        let modifier_prop = param.as_object().ok().and_then(|o| {
            o.find_prop(Id(
                spa::param::format::FormatProperties::VideoModifier.as_raw()
            ))
        });
        let dmabuf = modifier_prop.is_some();
        if let Some(prop) = modifier_prop {
            if prop.flags().contains(spa::pod::PodPropFlags::DONT_FIXATE) {
                // The modifier is still open: the source, which allocates, fixes it
                // and sends the final format (PipeWire DMA-BUF negotiation). Wait.
                return;
            }
        }
        let size = info.size();
        self.negotiated = Some(Negotiated {
            wire,
            width: size.width,
            height: size.height,
            dmabuf,
            modifier: info.modifier(),
        });
        let params = buffer_params(dmabuf);
        let mut params = pods(&params);
        let _ = stream.update_params(&mut params);
    }

    fn on_state(&mut self, new: &pw::stream::StreamState) {
        match new {
            pw::stream::StreamState::Paused | pw::stream::StreamState::Streaming => {
                self.report_start(Ok(()));
            }
            pw::stream::StreamState::Error(_) => {
                self.report_start(Err(backend(i64::from(libc::EIO))));
                self.status.lost.store(true, Ordering::Release);
            }
            pw::stream::StreamState::Unconnected => {
                self.status.lost.store(true, Ordering::Release);
            }
            pw::stream::StreamState::Connecting => {}
        }
    }

    fn on_process(&mut self, stream: &pw::stream::Stream) {
        self.return_free(stream);
        // Only the newest buffer matters; older queued ones were replaced.
        let mut newest: *mut pw::sys::pw_buffer = std::ptr::null_mut();
        loop {
            // SAFETY: dequeuing on the stream's own thread.
            let next = unsafe { stream.dequeue_raw_buffer() };
            if next.is_null() {
                break;
            }
            if !newest.is_null() {
                // SAFETY: dequeued above and not used further.
                unsafe { stream.queue_raw_buffer(newest) };
                self.shared.ring.count_drop();
                self.sequence += 1;
            }
            newest = next;
        }
        if newest.is_null() {
            return;
        }
        let keep = self.deliver(stream, newest);
        if !keep {
            // SAFETY: dequeued above; not kept by a slot.
            unsafe { stream.queue_raw_buffer(newest) };
        }
    }

    /// Publishes `buffer` into a ring slot. Returns whether the slot keeps it dequeued.
    fn deliver(&mut self, stream: &pw::stream::Stream, buffer: *mut pw::sys::pw_buffer) -> bool {
        let arrived = self.clock.now();
        let host = monotonic_nanos();
        let Some(negotiated) = self.negotiated else {
            return false;
        };
        // SAFETY: a buffer dequeued from the stream on this thread; PipeWire keeps
        // `buffer`, its `spa_buffer` and their arrays valid until it is queued again.
        let (data, pts) = unsafe {
            let spa_buffer = (*buffer).buffer;
            if spa_buffer.is_null() || (*spa_buffer).n_datas < 1 || (*spa_buffer).datas.is_null() {
                return false;
            }
            let mut pts = None;
            for index in 0..(*spa_buffer).n_metas as usize {
                let meta = &*(*spa_buffer).metas.add(index);
                if meta.type_ == spa::sys::SPA_META_Header
                    && meta.size as usize >= std::mem::size_of::<spa::sys::spa_meta_header>()
                    && !meta.data.is_null()
                {
                    let header = &*(meta.data as *const spa::sys::spa_meta_header);
                    if header.pts > 0 {
                        pts = Some(header.pts as u64);
                    }
                }
            }
            (&*(*spa_buffer).datas, pts)
        };
        // SAFETY: as above; the chunk pointer is set by PipeWire for every data.
        let chunk = unsafe { data.chunk.as_ref() };
        let Some(chunk) = chunk else {
            return false;
        };
        if chunk.flags & spa::sys::SPA_CHUNK_FLAG_CORRUPTED as i32 != 0 {
            self.shared.ring.count_drop();
            self.sequence += 1;
            return false;
        }
        let (width, height) = (negotiated.width, negotiated.height);
        let min_stride = width as usize * 4;
        let stride = usize::try_from(chunk.stride)
            .ok()
            .filter(|s| *s >= min_stride)
            .unwrap_or(min_stride);

        let Some(slot) = self.shared.ring.claim() else {
            // The consumer holds every slot: drop rather than wait (invariant 9).
            self.shared.ring.count_drop();
            self.sequence += 1;
            return false;
        };
        // The slot may have come free after `return_free` ran: its old buffer is no
        // longer referenced (the claim proves it), so give it back before reusing the
        // slot.
        if let Some(held) = self.held.get_mut(slot) {
            if !held.is_null() {
                // SAFETY: a buffer this stream dequeued and has not queued since; the
                // claim shows no frame refers to it.
                unsafe { stream.queue_raw_buffer(*held) };
                *held = std::ptr::null_mut();
            }
        }
        // SAFETY: `slot` was just claimed by this thread and is not published.
        let Some(contents) = (unsafe { self.shared.slot_mut(slot) }) else {
            self.shared.ring.abandon(slot);
            return false;
        };
        let keep = if negotiated.dmabuf && data.type_ == spa::sys::SPA_DATA_DmaBuf {
            let Ok(raw) = i32::try_from(data.fd) else {
                self.shared.ring.abandon(slot);
                return false;
            };
            // SAFETY: duplicating an fd PipeWire owns for this buffer; the copy is
            // owned by the slot.
            let dup = unsafe { libc::fcntl(raw, libc::F_DUPFD_CLOEXEC, 0) };
            if dup < 0 {
                self.shared.ring.abandon(slot);
                return false;
            }
            // SAFETY: `dup` is a new fd nobody else owns.
            let fd = unsafe { OwnedFd::from_raw_fd(dup) };
            let plane = DmaBufPlane {
                fd: dup,
                offset: chunk.offset,
                stride: stride as u32,
            };
            let Ok(surface) =
                DmaBufSurface::new(negotiated.wire.fourcc, negotiated.modifier, &[plane])
            else {
                self.shared.ring.abandon(slot);
                return false;
            };
            contents.picture = Picture::DmaBuf {
                surface,
                fd: Some(fd),
            };
            true
        } else if !data.data.is_null() {
            let len = stride * height as usize;
            let offset = chunk.offset as usize;
            if (data.maxsize as usize) < offset + len {
                self.shared.ring.abandon(slot);
                return false;
            }
            // SAFETY: PipeWire mapped the buffer (MAP_BUFFERS); the chunk lies inside
            // `maxsize` bytes from `data`, checked above.
            let source =
                unsafe { std::slice::from_raw_parts((data.data as *const u8).add(offset), len) };
            let out_stride = min_stride;
            let previous = std::mem::replace(&mut contents.picture, Picture::Empty);
            let mut out = match previous {
                Picture::System { data, .. } => data.try_into_mut().unwrap_or_default(),
                _ => BytesMut::new(),
            };
            out.clear();
            out.resize(out_stride * height as usize, 0);
            for (row, dst) in out.chunks_exact_mut(out_stride).enumerate() {
                let start = row * stride;
                dst.copy_from_slice(&source[start..start + out_stride]);
                if negotiated.wire.opaque {
                    for pixel in dst.chunks_exact_mut(4) {
                        pixel[3] = 0xff;
                    }
                }
            }
            let data = out.freeze();
            // Promote to shared storage here, so the render thread's clone only bumps a
            // reference count.
            drop(data.clone());
            contents.picture = Picture::System {
                data,
                stride: out_stride,
            };
            self.status.copied.fetch_add(1, Ordering::Relaxed);
            false
        } else {
            self.shared.ring.abandon(slot);
            return false;
        };
        contents.pixel_format = negotiated.wire.pixel_format;
        if keep {
            if let Some(held) = self.held.get_mut(slot) {
                *held = buffer;
            }
        }

        // Clock edge (ADR-0004): pair the monotonic clock with the master clock now,
        // then convert the buffer's timestamp.
        // Header timestamps are CLOCK_MONOTONIC for cameras and screen casts; one that
        // maps to more than `MAX_PTS_AGE` before arrival is on another clock and is
        // ignored.
        let mapped = match pts {
            Some(source) => {
                let _ = self.drift.observe(host, arrived);
                self.drift
                    .mapping()
                    .map(|m| m.to_master(source))
                    .filter(|m| arrived.as_nanos().saturating_sub(m.as_nanos()) <= MAX_PTS_AGE)
                    .unwrap_or(arrived)
            }
            None => arrived,
        };
        let floor = self
            .last_timestamp
            .map_or(Timestamp::ZERO, |t| t + Duration::from_nanos(1));
        let timestamp = mapped.min(arrived).max(floor);
        self.last_timestamp = Some(timestamp);
        self.shared.ring.publish(
            slot,
            FrameMeta::new(
                timestamp.as_nanos(),
                self.sequence,
                pts.unwrap_or(0),
                width,
                height,
            ),
        );
        self.sequence += 1;
        keep
    }
}

/// What the stream thread hands back when it ends.
struct Ended {
    sequence: u64,
    last_timestamp: Option<Timestamp>,
}

/// How the stream reaches its source.
enum Link {
    /// A node of the default daemon connection, by `node.name`.
    Node(String),
    /// The node of a portal cast, on the connection the portal opened for it.
    Portal { fd: OwnedFd, node: u32 },
}

/// Runs one session's PipeWire main loop until told to stop.
fn run_stream(link: Link, producer: Producer, stop: pw::channel::Receiver<()>) -> Ended {
    let fallback = Ended {
        sequence: producer.sequence,
        last_timestamp: producer.last_timestamp,
    };
    let producer = Rc::new(RefCell::new(producer));
    let fail = |producer: &Rc<RefCell<Producer>>| {
        producer
            .borrow_mut()
            .report_start(Err(backend(i64::from(libc::ECONNREFUSED))));
    };
    let Ok(mainloop) = pw::main_loop::MainLoopRc::new(None) else {
        fail(&producer);
        return fallback;
    };
    let Ok(context) = pw::context::ContextRc::new(&mainloop, None) else {
        fail(&producer);
        return fallback;
    };
    let (connected, node, portal_node) = match link {
        Link::Node(name) => (context.connect_rc(None), name, None),
        Link::Portal { fd, node } => (context.connect_fd_rc(fd, None), String::new(), Some(node)),
    };
    let Ok(core) = connected else {
        fail(&producer);
        return fallback;
    };
    let _stop = stop.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |()| mainloop.quit()
    });

    // Target loss: the node disappears from the registry.
    let target_id = Rc::new(Cell::new(None::<u32>));
    let Ok(registry) = core.get_registry_rc() else {
        fail(&producer);
        return fallback;
    };
    let _registry_listener = registry
        .add_listener_local()
        .global({
            let target_id = Rc::clone(&target_id);
            let node = node.clone();
            move |global| {
                if global.type_ != pw::types::ObjectType::Node {
                    return;
                }
                let found = match portal_node {
                    Some(id) => global.id == id,
                    None => global.props.and_then(|p| p.get("node.name")) == Some(node.as_str()),
                };
                if found {
                    target_id.set(Some(global.id));
                }
            }
        })
        .global_remove({
            let target_id = Rc::clone(&target_id);
            let producer = Rc::clone(&producer);
            move |id| {
                if target_id.get() == Some(id) {
                    producer.borrow().status.lost.store(true, Ordering::Release);
                }
            }
        })
        .register();
    // The node may have gone between `open` and `start`: if the first registry round
    // trip did not list it, the target is lost (invariant 7) rather than silent.
    let listed = core.sync(0).ok();
    let _core_listener = core
        .add_listener_local()
        .done({
            let target_id = Rc::clone(&target_id);
            let producer = Rc::clone(&producer);
            move |id, seq| {
                if id == pw::core::PW_ID_CORE && Some(seq) == listed && target_id.get().is_none() {
                    producer.borrow().status.lost.store(true, Ordering::Release);
                }
            }
        })
        .register();

    let mut props = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Video",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::NODE_NAME => "rbs-capture",
        // Never link to another source (a different camera) when the target is gone.
        "node.dont-fallback" => "true",
    };
    match portal_node {
        Some(_) => props.insert(*pw::keys::MEDIA_ROLE, "Screen"),
        None => {
            props.insert(*pw::keys::MEDIA_ROLE, "Camera");
            props.insert("target.object", node.as_str());
        }
    }
    let Ok(stream) = pw::stream::StreamRc::new(core.clone(), "rbs-capture", props) else {
        fail(&producer);
        return fallback;
    };
    let listener = stream
        .add_local_listener_with_user_data(Rc::clone(&producer))
        .state_changed(|_, producer, _, new| producer.borrow_mut().on_state(&new))
        .param_changed(|stream, producer, id, param| {
            producer.borrow_mut().on_param(stream, id, param);
        })
        .process(|stream, producer| producer.borrow_mut().on_process(stream))
        .remove_buffer(|_, producer, buffer| producer.borrow_mut().on_remove_buffer(buffer))
        .register();
    let Ok(_listener) = listener else {
        fail(&producer);
        return fallback;
    };
    let params = enum_formats(&producer.borrow().offer);
    let mut params = pods(&params);
    if stream
        .connect(
            spa::utils::Direction::Input,
            // Portal nodes are named by id; the portal's connection only shows them.
            portal_node,
            pw::stream::StreamFlags::AUTOCONNECT
                | pw::stream::StreamFlags::MAP_BUFFERS
                | pw::stream::StreamFlags::DONT_RECONNECT,
            &mut params,
        )
        .is_err()
    {
        fail(&producer);
        return fallback;
    }
    mainloop.run();
    let _ = stream.disconnect();
    let producer = producer.borrow();
    Ended {
        sequence: producer.sequence,
        last_timestamp: producer.last_timestamp,
    }
}

struct Active {
    stop: pw::channel::Sender<()>,
    thread: Option<JoinHandle<Ended>>,
    consumer: Consumer,
    status: Arc<Status>,
    /// The portal cast feeding the stream; closed after the stream thread ends.
    cast: Option<Cast>,
}

/// What a session captures.
enum Source {
    /// A video source node, by `node.name`, on the default daemon connection.
    Node(String),
    /// A portal screen cast.
    Portal(PortalSource),
}

struct PortalSource {
    kind: TargetKind,
    /// The key of the id the session was opened with.
    key: String,
    /// The newest restore token this session knows of.
    token: Option<String>,
    cursor: bool,
    picks: Arc<Mutex<Picks>>,
}

impl PortalSource {
    /// Starts the cast: the live pick if this key has one, otherwise restores the
    /// choice with the newest token, without a dialog.
    fn start(&mut self) -> Result<Cast, CaptureError> {
        let token = {
            let mut picks = lock(&self.picks);
            if let Some(index) = picks.live.iter().position(|(k, _)| *k == self.key) {
                let (_, cast) = picks.live.remove(index);
                // Started once; it cannot be restored after it stops.
                picks.advance(&self.key, None);
                self.token = None;
                return Ok(cast);
            }
            if self.key.starts_with(LIVE_PREFIX) && !picks.choices.contains_key(&self.key) {
                return Err(CaptureError::TargetNotFound);
            }
            // An unknown key is a token stored in a scene by an earlier run.
            picks
                .newest(&self.key)
                .ok_or(CaptureError::TargetNotFound)?
        };
        let result = Cast::start(&CastRequest {
            kind: self.kind,
            cursor: self.cursor,
            restore_token: Some(&token),
            timeout: Some(RESTORE_TIMEOUT),
        });
        // Once the portal tried to restore, the token is used up: the new one (if any)
        // replaces it. A failure before that (no bus, a failed call) keeps it.
        let used = match &result {
            Ok(_) | Err(CaptureError::Cancelled | CaptureError::PermissionDenied) => true,
            Err(CaptureError::Backend {
                domain: ErrorDomain::Library,
                code,
                ..
            }) => *code == portal::code::TIMEOUT || *code == portal::code::NO_STREAM,
            Err(_) => false,
        };
        if used {
            let next = result
                .as_ref()
                .ok()
                .and_then(|cast| cast.stream.restore_token.clone());
            lock(&self.picks).advance(&self.key, next.clone());
            self.token = next;
        }
        match result {
            Err(CaptureError::Backend {
                domain: ErrorDomain::Library,
                code: portal::code::TIMEOUT,
                ..
            }) => Err(CaptureError::TargetNotFound),
            other => other,
        }
    }
}

struct PipeWireSession {
    source: Source,
    offer: Offer,
    clock: Arc<dyn Clock>,
    format: VideoFormat,
    active: Option<Active>,
    next_sequence: u64,
    last_timestamp: Option<Timestamp>,
    /// Counters of earlier Active periods.
    base: CaptureStats,
    /// The ring of the last Active period while frames still referred to it at
    /// `stop`; see [`PipeWireSession::retire`].
    retired: Option<Arc<Shared>>,
}

/// Lets a `Producer` cross to the stream thread before any PipeWire object exists.
struct SendProducer(Producer);

// SAFETY: the only non-`Send` field is `held`, raw pointers that are all null until the
// stream thread dequeues buffers; the producer is moved to that thread before it starts
// and never leaves it.
unsafe impl Send for SendProducer {}

impl PipeWireSession {
    /// Lets go of the retired ring on the control thread: dropped here when no frame
    /// refers to it any more, otherwise buried, so the last lease never closes fds or
    /// frees memory on the render thread.
    fn retire(&mut self) {
        let Some(shared) = self.retired.take() else {
            return;
        };
        if !shared.ring.is_idle() {
            // SAFETY: the stream thread has ended, so no slot is being written.
            let bytes = unsafe { shared.held_bytes() };
            GRAVEYARD.bury(Box::new(RingGrave { shared, bytes }));
        }
    }

    fn activate(&mut self) -> Result<Active, CaptureError> {
        init();
        let shared = Arc::new(Shared::new());
        let status = Arc::new(Status::default());
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let producer = SendProducer(Producer {
            shared: Arc::clone(&shared),
            status: Arc::clone(&status),
            offer: self.offer.clone(),
            negotiated: None,
            held: [std::ptr::null_mut(); RING_SLOTS],
            sequence: self.next_sequence,
            clock: Arc::clone(&self.clock),
            drift: DriftEstimator::new(Duration::from_millis(100)),
            last_timestamp: self.last_timestamp,
            started: Some(started_tx),
        });
        let (link, mut cast) = match &mut self.source {
            Source::Node(name) => (Link::Node(name.clone()), None),
            Source::Portal(source) => {
                let mut cast = source.start()?;
                match cast.open_remote() {
                    Ok(fd) => (
                        Link::Portal {
                            fd,
                            node: cast.stream.node,
                        },
                        Some(cast),
                    ),
                    Err(e) => {
                        cast.close();
                        return Err(e);
                    }
                }
            }
        };
        let close = |cast: &mut Option<Cast>| {
            if let Some(cast) = cast.as_mut() {
                cast.close();
            }
        };
        let (stop_tx, stop_rx) = pw::channel::channel();
        let spawned = std::thread::Builder::new()
            .name("rbs-pipewire".into())
            .spawn(move || {
                let producer = producer;
                run_stream(link, producer.0, stop_rx)
            });
        let thread = match spawned {
            Ok(thread) => thread,
            Err(e) => {
                close(&mut cast);
                return Err(backend(i64::from(e.raw_os_error().unwrap_or(libc::EAGAIN))));
            }
        };
        let started = started_rx
            .recv_timeout(DAEMON_TIMEOUT)
            .unwrap_or_else(|_| Err(backend(i64::from(libc::ETIMEDOUT))));
        if let Err(e) = started {
            let _ = stop_tx.send(());
            let _ = thread.join();
            close(&mut cast);
            return Err(e);
        }
        // SAFETY: the stream thread only publishes complete single-plane DMA-BUFs of
        // the negotiated size and format, each kept dequeued (so the source does not
        // write it) and its fd duplicated while the slot is referenced.
        let consumer = unsafe { Consumer::new(shared, self.format) };
        Ok(Active {
            stop: stop_tx,
            thread: Some(thread),
            consumer,
            status,
            cast,
        })
    }
}

impl CaptureSession for PipeWireSession {
    fn format(&self) -> VideoFormat {
        self.active
            .as_ref()
            .map_or(self.format, |a| a.consumer.format())
    }

    fn start(&mut self) -> Result<(), CaptureError> {
        if self.active.is_some() {
            return Ok(());
        }
        self.retire();
        let active = self.activate()?;
        self.active = Some(active);
        Ok(())
    }

    fn stop(&mut self) {
        let Some(mut active) = self.active.take() else {
            return;
        };
        let _ = active.stop.send(());
        if let Some(ended) = active.thread.take().and_then(|t| t.join().ok()) {
            self.next_sequence = ended.sequence;
            self.last_timestamp = ended.last_timestamp;
        }
        if let Some(cast) = active.cast.as_mut() {
            cast.close();
        }
        let shared = Arc::clone(active.consumer.shared());
        let waiting = shared.ring.clear();
        self.base.delivered += active.consumer.delivered();
        self.base.dropped += shared.ring.dropped() + u64::from(waiting);
        self.base.cpu_copied += active.status.copied.load(Ordering::Relaxed);
        self.format = active.consumer.format();
        drop(active);
        if !shared.ring.is_idle() {
            self.retired = Some(shared);
        }
    }

    fn latest_frame(&mut self) -> Result<Option<VideoFrame>, CaptureError> {
        let Some(active) = self.active.as_mut() else {
            return Ok(None);
        };
        if active.status.lost.load(Ordering::Acquire) {
            return Err(CaptureError::TargetLost);
        }
        Ok(active.consumer.next())
    }

    fn stats(&self) -> CaptureStats {
        let mut stats = self.base;
        if let Some(active) = self.active.as_ref() {
            stats.delivered += active.consumer.delivered();
            stats.dropped += active.consumer.shared().ring.dropped();
            stats.cpu_copied += active.status.copied.load(Ordering::Relaxed);
        }
        stats
    }

    fn reconfigure(&mut self, change: CaptureChange) -> Result<(), CaptureError> {
        match change {
            CaptureChange::FrameRate(rate) => {
                // Applies from the next start; a running stream keeps its rate.
                self.offer.frame_rate = Some(rate);
                Ok(())
            }
            CaptureChange::Cursor(cursor) => match &mut self.source {
                // Applies from the next start, like the frame rate.
                Source::Portal(source) => {
                    source.cursor = cursor;
                    Ok(())
                }
                Source::Node(_) => Err(CaptureError::Unsupported("cameras have no cursor")),
            },
            _ => Err(CaptureError::Unsupported("change not supported")),
        }
    }

    fn current_target(&self) -> Option<TargetId> {
        match &self.source {
            Source::Portal(source) => source
                .token
                .as_deref()
                .filter(|token| *token != source.key)
                .map(|token| portal_id(source.kind, token)),
            Source::Node(_) => None,
        }
    }
}

impl Drop for PipeWireSession {
    fn drop(&mut self) {
        self.stop();
        self.retire();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_name_nodes() {
        assert_eq!(
            parse(&TargetId::new("node:v4l2_input.usb")),
            Some(Target::Node("v4l2_input.usb"))
        );
        assert_eq!(parse(&TargetId::new("node:")), None);
        assert_eq!(parse(&TargetId::new("display:1")), None);
    }

    #[test]
    fn ids_name_portal_choices() {
        let id = portal_id(TargetKind::Window, "0d1e-55");
        assert_eq!(id.as_str(), "portal:window:0d1e-55");
        assert_eq!(
            parse(&id),
            Some(Target::Portal {
                kind: TargetKind::Window,
                key: "0d1e-55"
            })
        );
        assert_eq!(
            parse(&TargetId::new("portal:display:live-3")),
            Some(Target::Portal {
                kind: TargetKind::Display,
                key: "live-3"
            })
        );
        assert_eq!(parse(&TargetId::new("portal:display:")), None);
        assert_eq!(parse(&TargetId::new("portal:camera:abc")), None);
        assert_eq!(parse(&TargetId::new("portal:abc")), None);
    }

    #[test]
    fn portal_offers_only_bgra() {
        let offer = |wires: Vec<Wire>| Offer {
            wires,
            memory: vec![MemoryKind::DmaBuf, MemoryKind::System],
            modifiers: Vec::new(),
            frame_rate: None,
        };
        let bgra_first = portal_offer(offer(WIRES.to_vec())).map(|o| o.wires);
        assert_eq!(bgra_first, Some(vec![WIRES[0], WIRES[1]]));
        assert!(portal_offer(offer(vec![WIRES[2], WIRES[0]])).is_none());
        let kept = portal_offer(offer(vec![WIRES[1]])).map(|o| o.memory);
        assert_eq!(kept, Some(vec![MemoryKind::DmaBuf, MemoryKind::System]));
    }

    #[test]
    fn fourccs_match_the_importer() {
        assert_eq!(DRM_AR24, 0x3432_5241);
        assert_eq!(DRM_XR24, 0x3432_5258);
        assert_eq!(DRM_AB24, 0x3432_4241);
        assert_eq!(DRM_XB24, 0x3432_4258);
    }

    #[test]
    fn offers_serialize_in_request_order() {
        let offer = Offer {
            wires: vec![WIRES[0], WIRES[1]],
            memory: vec![MemoryKind::DmaBuf, MemoryKind::System],
            modifiers: vec![(DRM_AR24, 7), (DRM_AR24, 9)],
            frame_rate: None,
        };
        let params = enum_formats(&offer);
        assert_eq!(params.len(), 4);
        assert!(params.iter().all(|p| Pod::from_bytes(p).is_some()));
        assert_eq!(offer.modifiers_for(DRM_AR24), vec![7, 9]);
        assert_eq!(offer.modifiers_for(DRM_XR24), vec![0]);
        assert_eq!(buffer_params(true).len(), 2);
    }
}
