//! Browser source type (ADR-0109). The studio does not link or load CEF.
//!
//! A real CEF build replaces the helper executable and is a process tree. This module
//! validates settings, classifies every URL as network-capable, and grants at most two
//! running helpers (program before preview, lower source id inside a class). The
//! composition root spawns the executable from `rbs-browser-runner` (or the CEF tree
//! root) by path. `rbs-sources` does not spawn processes.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use rbs_browser_ipc::{Configure, restart_delay};
use rbs_core::media::{
    ColorSpace, PixelFormat, PlaneLayout, Size, SystemFrame, VideoFormat, VideoFrame,
};
use rbs_core::time::Timestamp;
use rbs_gpu::node::GpuRuntime;
use rbs_gpu::upload::SourceUpload;
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::{Node, NodeError, PrepareCtx, ProcessCtx, WarmCtx};
use rbs_scene::model::{SettingValue, Settings, SourceId};

use crate::types::keys;

/// Node type id. Video only: a runner that cannot emit samples has no audio pin.
pub const BROWSER_NODE_TYPE: &str = "org.rbs.browser";
/// At most two helper trees run.
pub const MAX_HELPERS: usize = 2;
/// Control-API reason when a source does not get a slot.
pub const BROWSER_CAP: &str = "browser_cap";
/// Wire kind an agent sees without the network permission (code -32011 when the
/// control API grows that kind).
pub const NOT_PERMITTED: &str = "not_permitted";
/// `data.reason` for that refusal.
pub const NETWORK: &str = "network";

/// Default picture size.
pub const DEFAULT_SIZE: (u32, u32) = (1920, 1080);
/// Default page rate.
pub const DEFAULT_FPS: u32 = 30;
/// Largest side.
pub const MAX_SIDE: u32 = 8192;

/// The one descriptor for `org.rbs.source.browser`. No audio pin.
pub fn browser_descriptor() -> NodeDescriptor {
    NodeDescriptor::new(
        NodeTypeId::new_static(BROWSER_NODE_TYPE),
        NodeKind::Source,
        Vec::new(),
        vec![Pin::one("video", LinkKind::Video)],
    )
}

/// Where a shown browser sits. Program takes a slot before preview.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ShowClass {
    /// On the program scene.
    Program,
    /// Preview only.
    Preview,
}

/// One source competing for a helper slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrowserClaim {
    /// Source id. Lower wins inside a class.
    pub source: SourceId,
    /// Program or preview.
    pub class: ShowClass,
}

/// The grant for one claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrowserGrant {
    /// Source.
    pub source: SourceId,
    /// True when this source may start a helper.
    pub slot: bool,
    /// `browser_cap` when `slot` is false.
    pub reason: Option<&'static str>,
}

/// Shared by the composition root. A dead helper must be [`BrowserBudget::release`]d
/// so it does not keep a slot.
#[derive(Debug, Default)]
pub struct BrowserBudget {
    held: Mutex<Vec<SourceId>>,
}

impl BrowserBudget {
    /// A budget the composition root can share.
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Grants up to [`MAX_HELPERS`] slots. Program first, then lower source id.
    pub fn grant(&self, claims: &[BrowserClaim]) -> Vec<BrowserGrant> {
        let mut best: Vec<BrowserClaim> = Vec::new();
        for claim in claims {
            if let Some(existing) = best.iter_mut().find(|item| item.source == claim.source) {
                if claim.class < existing.class {
                    existing.class = claim.class;
                }
            } else {
                best.push(*claim);
            }
        }
        best.sort_by_key(|claim| (claim.class, claim.source));
        let winners: Vec<SourceId> = best
            .iter()
            .take(MAX_HELPERS)
            .map(|claim| claim.source)
            .collect();
        if let Ok(mut held) = self.held.lock() {
            *held = winners.clone();
        }
        best.into_iter()
            .map(|claim| {
                let slot = winners.contains(&claim.source);
                BrowserGrant {
                    source: claim.source,
                    slot,
                    reason: if slot { None } else { Some(BROWSER_CAP) },
                }
            })
            .collect()
    }

    /// Drops a slot. A crashed helper must call this.
    pub fn release(&self, source: SourceId) {
        if let Ok(mut held) = self.held.lock() {
            held.retain(|id| *id != source);
        }
    }

    /// Whether `source` currently holds a slot.
    pub fn holds(&self, source: SourceId) -> bool {
        self.held
            .lock()
            .map(|held| held.contains(&source))
            .unwrap_or(false)
    }

    /// How many slots are held.
    pub fn running(&self) -> usize {
        self.held.lock().map(|held| held.len()).unwrap_or(0)
    }
}

/// Checked browser settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserSettings {
    /// URL.
    pub url: String,
    /// Even width.
    pub width: u32,
    /// Even height.
    pub height: u32,
    /// 1..=60.
    pub fps: u32,
    /// Default false.
    pub transparent: bool,
}

impl BrowserSettings {
    /// The wire configure block. No click forwarding and no CSS.
    pub fn configure(&self) -> Configure {
        Configure {
            url: self.url.clone(),
            width: self.width,
            height: self.height,
            fps: self.fps,
            transparent: self.transparent,
        }
    }
}

/// Scheme of an accepted URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserScheme {
    /// `https`.
    Https,
    /// `http`.
    Http,
    /// `file`. Still network-capable.
    File,
}

/// Parses the allow list. Anything else is refused.
pub fn browser_scheme(url: &str) -> Result<BrowserScheme, &'static str> {
    let text = url.trim();
    let lower = text.to_ascii_lowercase();
    if lower.starts_with("https://") && text.len() > "https://".len() {
        Ok(BrowserScheme::Https)
    } else if lower.starts_with("http://") && text.len() > "http://".len() {
        Ok(BrowserScheme::Http)
    } else if lower.starts_with("file://") && text.len() > "file://".len() {
        Ok(BrowserScheme::File)
    } else {
        Err("an http, https, or file URL")
    }
}

/// Every browser URL is network-capable, including `file`.
pub fn browser_url_is_network(url: &str) -> bool {
    let _ = url;
    true
}

/// Directory a `file` URL may read, when the URL is a file URL.
pub fn file_directory_scope(url: &str) -> Option<String> {
    let text = url.trim();
    let rest = text
        .get("file://".len()..)
        .filter(|_| browser_scheme(text).ok() == Some(BrowserScheme::File))?;
    let path = rest.trim_start_matches('/');
    let slash = path.rfind(['/', '\\'])?;
    Some(path[..=slash].to_owned())
}

/// An agent without the network permission cannot set any browser URL.
pub fn agent_may_set_browser_url(url: &str, network_permission: bool) -> Result<(), AgentDenial> {
    if network_permission || !browser_url_is_network(url) {
        Ok(())
    } else {
        Err(AgentDenial {
            kind: NOT_PERMITTED,
            reason: NETWORK,
        })
    }
}

/// What the control API should surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentDenial {
    /// `not_permitted`.
    pub kind: &'static str,
    /// `network`.
    pub reason: &'static str,
}

/// Validates stored settings. Missing size and fps take the defaults at instantiate.
pub fn validate_browser(settings: &Settings) -> Result<(), (&'static str, &'static str)> {
    for key in settings.keys() {
        if ![
            keys::URL,
            keys::WIDTH,
            keys::HEIGHT,
            keys::FPS,
            keys::TRANSPARENT,
        ]
        .contains(&key.as_str())
        {
            return Err(("settings", "only keys this source type uses"));
        }
    }
    match settings.get(keys::URL) {
        None => return Err((keys::URL, "an http, https, or file URL")),
        Some(SettingValue::Text(url)) => {
            browser_scheme(url).map_err(|expected| (keys::URL, expected))?;
        }
        Some(_) => return Err((keys::URL, "text")),
    }
    let width = int_setting(settings, keys::WIDTH)?;
    let height = int_setting(settings, keys::HEIGHT)?;
    if width.is_some() != height.is_some() {
        return Err((keys::WIDTH, "set together with height"));
    }
    if let (Some(width), Some(height)) = (width, height) {
        check_side(width, height)?;
    }
    if let Some(fps) = int_setting(settings, keys::FPS)? {
        if !(1..=60).contains(&fps) {
            return Err((keys::FPS, "from 1 to 60 frames per second"));
        }
    }
    match settings.get(keys::TRANSPARENT) {
        None | Some(SettingValue::Bool(_)) => {}
        Some(_) => return Err((keys::TRANSPARENT, "a bool")),
    }
    Ok(())
}

/// Applies defaults. `validate_browser` must have succeeded.
pub fn browser_settings(
    settings: &Settings,
) -> Result<BrowserSettings, (&'static str, &'static str)> {
    validate_browser(settings)?;
    let url = match settings.get(keys::URL) {
        Some(SettingValue::Text(url)) => url.clone(),
        _ => return Err((keys::URL, "an http, https, or file URL")),
    };
    let (width, height) = match (
        int_setting(settings, keys::WIDTH)?,
        int_setting(settings, keys::HEIGHT)?,
    ) {
        (Some(width), Some(height)) => (width, height),
        (None, None) => DEFAULT_SIZE,
        _ => return Err((keys::WIDTH, "set together with height")),
    };
    let fps = int_setting(settings, keys::FPS)?.unwrap_or(DEFAULT_FPS);
    let transparent = match settings.get(keys::TRANSPARENT) {
        Some(SettingValue::Bool(on)) => *on,
        _ => false,
    };
    Ok(BrowserSettings {
        url,
        width,
        height,
        fps,
        transparent,
    })
}

fn int_setting(
    settings: &Settings,
    key: &'static str,
) -> Result<Option<u32>, (&'static str, &'static str)> {
    match settings.get(key) {
        None => Ok(None),
        Some(SettingValue::Int(value)) => match u32::try_from(*value) {
            Ok(value) if value > 0 => Ok(Some(value)),
            _ => Err((key, "a positive integer")),
        },
        Some(_) => Err((key, "an integer")),
    }
}

fn check_side(width: u32, height: u32) -> Result<(), (&'static str, &'static str)> {
    if width > MAX_SIDE || height > MAX_SIDE {
        return Err((keys::WIDTH, "at most 8192 pixels on a side"));
    }
    if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return Err((keys::WIDTH, "an even width and height"));
    }
    Ok(())
}

/// Output pin index on browser source nodes.
pub const VIDEO_PIN: usize = 0;

/// Studio-owned RGBA the render thread uploads. The helper writes on the control thread.
#[derive(Debug)]
pub struct BrowserFrameSink {
    pixels: Mutex<Vec<u8>>,
    width: AtomicU32,
    height: AtomicU32,
    generation: AtomicU32,
}

impl BrowserFrameSink {
    /// Empty buffer.
    pub fn new() -> Self {
        Self {
            pixels: Mutex::new(Vec::new()),
            width: AtomicU32::new(0),
            height: AtomicU32::new(0),
            generation: AtomicU32::new(0),
        }
    }

    /// Publishes straight-alpha `Rgba8` bytes. Runs on the control thread.
    pub fn publish(&self, rgba: &[u8], width: u32, height: u32) {
        if let Ok(mut pixels) = self.pixels.lock() {
            if pixels.len() != rgba.len() {
                *pixels = rgba.to_owned();
            } else {
                pixels.copy_from_slice(rgba);
            }
        }
        self.width.store(width, Ordering::Release);
        self.height.store(height, Ordering::Release);
        self.generation.fetch_add(1, Ordering::Release);
    }

    /// Generation the render path last uploaded.
    pub fn generation(&self) -> &AtomicU32 {
        &self.generation
    }

    /// Copies the latest pixels when `generation > 0`.
    pub fn snapshot(&self) -> Option<(Bytes, u32, u32)> {
        let generation = self.generation.load(Ordering::Acquire);
        if generation == 0 {
            return None;
        }
        let pixels = self.pixels.lock().ok()?;
        let width = self.width.load(Ordering::Acquire);
        let height = self.height.load(Ordering::Acquire);
        if pixels.is_empty() || width == 0 || height == 0 {
            return None;
        }
        Some((Bytes::copy_from_slice(&pixels), width, height))
    }
}

impl Default for BrowserFrameSink {
    fn default() -> Self {
        Self::new()
    }
}

/// Control-thread side of a browser source: helper I/O and the frame sink.
pub struct BrowserControlSidecar {
    session: Mutex<Option<Box<dyn BrowserSession>>>,
    sink: Arc<BrowserFrameSink>,
    budget: Arc<BrowserBudget>,
    source: SourceId,
    exited: AtomicBool,
}

impl BrowserControlSidecar {
    /// Shared state for one browser source.
    pub fn new(source: SourceId, budget: Arc<BrowserBudget>) -> Self {
        Self {
            session: Mutex::new(None),
            sink: Arc::new(BrowserFrameSink::new()),
            budget,
            source,
            exited: AtomicBool::new(false),
        }
    }

    /// The sink helpers fill and the render path reads.
    pub fn sink(&self) -> Arc<BrowserFrameSink> {
        Arc::clone(&self.sink)
    }

    /// Polls the helper and refreshes the sink. Never runs in `process` (P6).
    pub fn control_tick(&self) {
        let Ok(mut guard) = self.session.lock() else {
            return;
        };
        let Some(session) = guard.as_mut() else {
            return;
        };
        if session.poll().is_some() {
            session.stop();
            *guard = None;
            self.budget.release(self.source);
            self.exited.store(true, Ordering::Release);
            return;
        }
        session.refresh_sink(&self.sink);
    }

    /// True once when the helper exited between ticks.
    pub fn take_exited(&self) -> bool {
        self.exited.swap(false, Ordering::AcqRel)
    }

    /// Stores a running session.
    pub(crate) fn set_session(&self, session: Box<dyn BrowserSession>) {
        if let Ok(mut guard) = self.session.lock() {
            *guard = Some(session);
        }
    }

    /// Kills a running helper.
    pub(crate) fn clear_session(&self) {
        if let Ok(mut guard) = self.session.lock() {
            if let Some(mut session) = guard.take() {
                session.stop();
            }
        }
    }

    /// True when a helper process is still attached.
    pub fn helper_running(&self) -> bool {
        self.session
            .lock()
            .ok()
            .is_some_and(|guard| guard.is_some())
    }
}

/// A helper the composition root starts. `rbs-sources` does not spawn it.
pub trait BrowserTree: Send + Sync {
    /// Starts the tree root. Called only when Active and a slot is held.
    fn start(
        &self,
        config: &Configure,
        sink: Arc<BrowserFrameSink>,
    ) -> Result<Box<dyn BrowserSession>, String>;
}

/// One running tree. `poll` and `stop` run on the control thread, never in `process`.
pub trait BrowserSession: Send {
    /// `None` while the process is alive.
    fn poll(&mut self) -> Option<i32>;
    /// Kills the tree. Standby.
    fn stop(&mut self);
    /// Copies the latest helper frame into `sink`.
    fn refresh_sink(&mut self, sink: &BrowserFrameSink) {
        let _ = sink;
    }
}

/// The browser node. Standby holds no helper.
pub struct BrowserSourceNode {
    descriptor: NodeDescriptor,
    settings: BrowserSettings,
    budget: Arc<BrowserBudget>,
    source: SourceId,
    tree: Option<Arc<dyn BrowserTree>>,
    control: Arc<BrowserControlSidecar>,
    attempts: u32,
    reason: Option<&'static str>,
    retry_after: Option<Duration>,
    starts: u32,
    upload: SourceUpload,
    uploaded_generation: u32,
    ready: Option<VideoFrame>,
}

impl BrowserSourceNode {
    /// Standby. Does not start a helper.
    pub fn new(
        settings: BrowserSettings,
        budget: Arc<BrowserBudget>,
        source: SourceId,
        tree: Option<Arc<dyn BrowserTree>>,
    ) -> Self {
        Self {
            descriptor: browser_descriptor(),
            settings,
            budget: Arc::clone(&budget),
            source,
            tree,
            control: Arc::new(BrowserControlSidecar::new(source, budget)),
            attempts: 0,
            reason: None,
            retry_after: None,
            starts: 0,
            upload: SourceUpload::new(),
            uploaded_generation: 0,
            ready: None,
        }
    }

    /// Control-thread handle for polling helpers (ADR-0109).
    pub fn control(&self) -> Arc<BrowserControlSidecar> {
        Arc::clone(&self.control)
    }

    /// True while the helper process exists.
    pub fn helper_running(&self) -> bool {
        self.control.helper_running()
    }

    /// Width and height from settings. Known before a helper starts.
    pub fn picture_size(&self) -> (u32, u32) {
        (self.settings.width, self.settings.height)
    }

    /// How many times a helper was started.
    pub fn starts(&self) -> u32 {
        self.starts
    }

    /// `browser_cap` or `helper_exit` when the source is Unavailable.
    pub fn reason(&self) -> Option<&'static str> {
        self.reason
    }

    /// Back-off attempts after an exit.
    pub fn attempts(&self) -> u32 {
        self.attempts
    }

    /// Delay after the last exit.
    pub fn retry_after(&self) -> Option<Duration> {
        self.retry_after
    }

    /// Starts the helper when this source holds a slot. Otherwise `browser_cap` and
    /// no process.
    pub fn activate(&mut self) -> Result<(), BrowserStartError> {
        if self.control.helper_running() {
            return Ok(());
        }
        if !self.budget.holds(self.source) {
            self.reason = Some(BROWSER_CAP);
            return Err(BrowserStartError::Cap);
        }
        let Some(tree) = self.tree.clone() else {
            self.reason = Some("helper_missing");
            return Err(BrowserStartError::Failed(
                "the composition root has not set a browser helper".into(),
            ));
        };
        match tree.start(&self.settings.configure(), self.control.sink()) {
            Ok(mut session) => {
                self.starts = self.starts.saturating_add(1);
                session.refresh_sink(self.control.sink().as_ref());
                if session.poll().is_some() {
                    self.note_exit();
                    return Err(BrowserStartError::Exited);
                }
                self.control.set_session(session);
                self.reason = None;
                Ok(())
            }
            Err(error) => {
                self.budget.release(self.source);
                self.note_exit();
                Err(BrowserStartError::Failed(error))
            }
        }
    }

    /// Control thread. An exit releases the slot, keeps the host running, and counts
    /// toward back-off.
    pub fn poll(&mut self) {
        self.control.control_tick();
        if self.control.take_exited() {
            self.mark_unavailable_after_exit();
        }
    }

    /// Standby. Kills the helper. No process remains.
    pub fn standby(&mut self) {
        self.control.clear_session();
        self.budget.release(self.source);
        self.ready = None;
        self.uploaded_generation = 0;
    }

    fn note_exit(&mut self) {
        self.budget.release(self.source);
        self.mark_unavailable_after_exit();
        self.control.clear_session();
    }

    fn mark_unavailable_after_exit(&mut self) {
        self.attempts = self.attempts.saturating_add(1);
        self.retry_after = Some(restart_delay(self.attempts));
        self.reason = Some("helper_exit");
    }
}

impl Drop for BrowserSourceNode {
    fn drop(&mut self) {
        self.control.clear_session();
    }
}

/// Why activate did not start a helper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserStartError {
    /// The global cap refused a slot. No process was started.
    Cap,
    /// The helper exited. The host keeps running.
    Exited,
    /// The tree could not be started.
    Failed(String),
}

impl std::fmt::Display for BrowserStartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BrowserStartError::Cap => f.write_str(BROWSER_CAP),
            BrowserStartError::Exited => f.write_str("browser helper exited"),
            BrowserStartError::Failed(text) => f.write_str(text),
        }
    }
}

impl Node<GpuRuntime> for BrowserSourceNode {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.descriptor
    }

    fn prepare(&mut self, _ctx: &mut PrepareCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        if self.control.take_exited() {
            self.mark_unavailable_after_exit();
        }
        self.activate()
            .map_err(|error| NodeError::Failed(error.to_string()))
    }

    fn warm(&mut self, ctx: &mut WarmCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        let generation = self.control.sink().generation().load(Ordering::Acquire);
        if generation == 0 || generation == self.uploaded_generation {
            return Ok(());
        }
        let Some((pixels, width, height)) = self.control.sink().snapshot() else {
            return Ok(());
        };
        let format = VideoFormat::new(
            Size::new(width, height),
            PixelFormat::Rgba8,
            ColorSpace::SRGB,
        );
        let planes = [PlaneLayout {
            offset: 0,
            stride: width as usize * 4,
        }];
        let system = SystemFrame::new(format, pixels, &planes)
            .map_err(|_| NodeError::Permanent("browser picture layout invalid"))?;
        let frame = VideoFrame::system(Timestamp::ZERO, system, None);
        let uploaded = self
            .upload
            .upload(ctx.shared, &mut ctx.local.pool, frame)
            .map_err(|_| NodeError::Backend("browser picture upload failed"))?;
        self.uploaded_generation = generation;
        self.ready = Some(uploaded);
        Ok(())
    }

    fn process(&mut self, ctx: &mut ProcessCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        let Some(frame) = self.ready.take() else {
            return Ok(());
        };
        drop(ctx.outputs().set_video(VIDEO_PIN, Some(frame))?);
        Ok(())
    }

    fn cool(&mut self, ctx: &mut WarmCtx<'_, GpuRuntime>) {
        self.ready = None;
        self.upload.release(&mut ctx.local.pool);
    }

    fn release(&mut self, _ctx: &mut PrepareCtx<'_, GpuRuntime>) {
        self.standby();
    }
}

/// Source reach for the agent permission check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceReach {
    /// Opens local files only. `locations` are the setting keys.
    LocalOnly {
        /// Location fields.
        locations: &'static [&'static str],
    },
    /// May open a network connection. Browser sources are always this, including `file`.
    Network,
}
