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
use rbs_audio_io::edge::EdgeClock;
use rbs_browser_ipc::{Configure, restart_delay};
use rbs_core::media::{
    ColorSpace, PixelFormat, PlaneLayout, Size, SystemFrame, VideoFormat, VideoFrame,
    next_generation,
};
use rbs_core::time::{Clock, Timestamp};
use rbs_gpu::node::GpuRuntime;
use rbs_gpu::upload::SourceUpload;
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::{Node, NodeError, PrepareCtx, ProcessCtx, WarmCtx};
use rbs_scene::model::{SettingValue, Settings, SourceId};

use crate::types::keys;

/// Node type id of the picture node. Its sound is the second node,
/// [`crate::BROWSER_SOUND_NODE_TYPE`] (ADR-0106).
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

/// The descriptor of the picture node. No audio pin: the sound node has it.
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
    /// The wire configure block. The composition root fills `frames` with the name of
    /// the frame section it allocates before spawning the helper.
    pub fn configure(&self) -> Configure {
        Configure {
            url: self.url.clone(),
            width: self.width,
            height: self.height,
            fps: self.fps,
            transparent: self.transparent,
            frames: String::new(),
            sound: String::new(),
            sample_rate: 0,
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

/// The newest page picture, waiting for the render thread.
struct PendingPicture {
    frame: VideoFrame,
    /// Master time of the paint, or `None` when unknown: due at once.
    stamp: Option<Timestamp>,
}

/// Studio-owned RGBA the render thread uploads (ADR-0109 decision 8). The supervisor
/// thread publishes each page picture with its master-time stamp; the render thread
/// takes the newest one whose stamp is due, so a picture is not shown before the sound
/// painted with it is heard (ADR-0106 decision 4).
///
/// The supervisor makes the frame (copy, allocation) and keeps one reference to its
/// bytes, so the render thread only uploads and drops a reference: it never allocates,
/// frees or waits here.
pub struct BrowserFrameSink {
    pending: Mutex<Option<PendingPicture>>,
    /// Supervisor only: the last picture's bytes, reused when the render thread let go.
    kept: Mutex<Kept>,
    width: AtomicU32,
    height: AtomicU32,
    generation: AtomicU32,
}

#[derive(Default)]
struct Kept {
    last: Option<Bytes>,
    /// Bytes the render thread still held when a newer picture came; dropped here once
    /// it lets go, so the last reference never goes on the render thread.
    retired: Option<Bytes>,
}

impl BrowserFrameSink {
    /// Empty buffer.
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(None),
            kept: Mutex::new(Kept::default()),
            width: AtomicU32::new(0),
            height: AtomicU32::new(0),
            generation: AtomicU32::new(0),
        }
    }

    /// Publishes straight-alpha `Rgba8` bytes, due at once. Supervisor or control thread.
    pub fn publish(&self, rgba: &[u8], width: u32, height: u32) {
        self.publish_at(rgba, width, height, None);
    }

    /// Publishes straight-alpha `Rgba8` bytes painted at master time `stamp` (see
    /// [`BrowserPictureEdge`]). A picture the render thread has not taken yet is
    /// replaced. Supervisor or control thread, never the render thread.
    pub fn publish_at(&self, rgba: &[u8], width: u32, height: u32, stamp: Option<Timestamp>) {
        let expected = (width as usize)
            .saturating_mul(height as usize)
            .saturating_mul(4);
        if width == 0 || height == 0 || rgba.len() != expected {
            return;
        }
        // The replaced picture is freed here, on this thread.
        let replaced = self.pending.lock().ok().and_then(|mut slot| slot.take());
        drop(replaced);
        let Ok(mut kept) = self.kept.lock() else {
            return;
        };
        if kept.retired.as_ref().is_some_and(Bytes::is_unique) {
            kept.retired = None;
        }
        let bytes = match kept.last.take().map(Bytes::try_into_mut) {
            Some(Ok(mut reuse)) if reuse.len() == rgba.len() => {
                reuse.copy_from_slice(rgba);
                reuse.freeze()
            }
            Some(Err(held)) => {
                // The render thread still reads it; at most one is kept back.
                kept.retired = Some(held);
                Bytes::copy_from_slice(rgba)
            }
            _ => Bytes::copy_from_slice(rgba),
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
        let Ok(system) = SystemFrame::new(format, bytes.clone(), &planes) else {
            return;
        };
        kept.last = Some(bytes);
        drop(kept);
        // A new key per picture, also when it has no stamp (ADR-0117 decision 4).
        let frame = VideoFrame::system(stamp.unwrap_or(Timestamp::ZERO), system, None)
            .with_sequence(next_generation());
        if let Ok(mut slot) = self.pending.lock() {
            *slot = Some(PendingPicture { frame, stamp });
        }
        self.width.store(width, Ordering::Release);
        self.height.store(height, Ordering::Release);
        self.generation.fetch_add(1, Ordering::Release);
    }

    /// Forgets a picture the render thread has not taken (the helper stopped).
    pub(crate) fn clear(&self) {
        let stale = self.pending.lock().ok().and_then(|mut slot| slot.take());
        drop(stale);
    }

    /// Pictures published so far.
    pub fn generation(&self) -> &AtomicU32 {
        &self.generation
    }

    /// Render thread: the newest picture whose stamp is at or before `now`, once.
    /// `None` when there is none, it is not due yet, or the supervisor holds the
    /// slot this instant (taken on a later tick). Never waits, never allocates.
    pub fn take_due(&self, now: Timestamp) -> Option<VideoFrame> {
        let mut slot = self.pending.try_lock().ok()?;
        if slot.as_ref()?.stamp.is_some_and(|stamp| stamp > now) {
            return None;
        }
        slot.take().map(|pending| pending.frame)
    }
}

/// Maps a helper's paint time to master time on the supervisor thread (ADR-0109
/// decision 8), by the rule page sound follows (decision 6): an [`EdgeClock`] fed with
/// OS and master times read back to back. Never places a picture after now.
pub struct BrowserPictureEdge {
    edge: EdgeClock,
    clock: Arc<dyn Clock>,
}

impl BrowserPictureEdge {
    /// An edge onto the master `clock`.
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            edge: EdgeClock::default(),
            clock,
        }
    }

    /// The master time of a picture painted at OS time `painted`, given that the OS
    /// clock reads `os_now` now. `painted` 0 (unknown) or later than `os_now` (an
    /// untrusted helper) counts as now.
    pub fn stamp(&mut self, painted: u64, os_now: u64) -> Timestamp {
        let now = self.clock.now();
        self.edge.observe(os_now, now);
        let painted = if painted == 0 || painted > os_now {
            os_now
        } else {
            painted
        };
        self.edge.to_master(painted, Duration::ZERO, now)
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

    fn frames(&self) -> &BrowserFrameSink {
        &self.sink
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
    /// Starts the tree root. Called only when Active and a slot is held. With
    /// `sound`, `config.sample_rate` is set and the tree hands the page's sound to a
    /// [`crate::BrowserSoundFeed`] over it (ADR-0109 decision 6).
    fn start(
        &self,
        config: &Configure,
        sink: Arc<BrowserFrameSink>,
        sound: Option<Arc<crate::BrowserSound>>,
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
    sound: Option<Arc<crate::BrowserSound>>,
}

impl BrowserSourceNode {
    /// Hands the page's sound to the sound node over `sound` (ADR-0106).
    pub fn with_sound(mut self, sound: Arc<crate::BrowserSound>) -> Self {
        self.sound = Some(sound);
        self
    }

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
            sound: None,
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
        let mut configure = self.settings.configure();
        if let Some(sound) = self.sound.as_ref() {
            configure.sample_rate = sound.sample_rate();
        }
        match tree.start(&configure, self.control.sink(), self.sound.clone()) {
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
        self.control.frames().clear();
        self.budget.release(self.source);
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
        // Textures for the page size, so the first picture does not create them.
        let format = VideoFormat::new(
            Size::new(self.settings.width, self.settings.height),
            PixelFormat::Rgba8,
            ColorSpace::SRGB,
        );
        self.upload
            .prepare_for(ctx.shared, &mut ctx.local.pool, &format)
            .map_err(|_| NodeError::Backend("browser picture upload failed"))
    }

    fn process(&mut self, ctx: &mut ProcessCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        // The newest picture whose stamp is due (ADR-0109 decision 8). The output keeps
        // it until the next one, as a still's does.
        let Some(system) = self.control.frames().take_due(ctx.tick().time) else {
            return Ok(());
        };
        let texture = self
            .upload
            .upload(ctx.shared(), &mut ctx.local().pool, system)
            .map_err(|_| NodeError::Backend("browser picture upload failed"))?;
        drop(ctx.outputs().set_video(VIDEO_PIN, Some(texture))?);
        Ok(())
    }

    fn cool(&mut self, ctx: &mut WarmCtx<'_, GpuRuntime>) {
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
    use super::*;

    const RGBA: [u8; 16] = [7; 16];

    #[test]
    fn the_render_thread_takes_the_newest_due_picture_once() {
        let sink = BrowserFrameSink::new();
        let at = |ms: u64| Timestamp::from_nanos(ms * 1_000_000);
        assert!(sink.take_due(at(100)).is_none(), "nothing yet");
        sink.publish_at(&RGBA, 2, 2, Some(at(50)));
        assert!(sink.take_due(at(49)).is_none(), "not before its time");
        sink.publish_at(&[9; 16], 2, 2, Some(at(60)));
        assert!(sink.take_due(at(55)).is_none(), "the newer one is not due");
        let frame = sink.take_due(at(60)).unwrap();
        assert_eq!(frame.timestamp(), at(60));
        assert!(sink.take_due(at(70)).is_none(), "taken once");
        // Without a stamp a picture is due at once.
        sink.publish(&RGBA, 2, 2);
        assert!(sink.take_due(Timestamp::ZERO).is_some());
        assert_eq!(sink.generation().load(Ordering::Relaxed), 3);
    }

    #[test]
    fn a_picture_of_the_wrong_size_is_not_published() {
        let sink = BrowserFrameSink::new();
        sink.publish(&RGBA, 3, 2);
        sink.publish(&RGBA, 0, 4);
        assert!(sink.take_due(Timestamp::ZERO).is_none());
    }

    #[test]
    fn taking_and_dropping_a_picture_does_not_allocate_or_free() {
        let sink = BrowserFrameSink::new();
        sink.publish_at(&RGBA, 2, 2, Some(Timestamp::ZERO));
        let info = allocation_counter::measure(|| {
            let frame = sink.take_due(Timestamp::ZERO);
            assert!(frame.is_some());
            drop(frame);
        });
        assert_eq!(info.count_total, 0, "{info:?}");
    }

    #[test]
    fn the_supervisor_reuses_the_bytes_the_render_thread_let_go() {
        let sink = BrowserFrameSink::new();
        sink.publish(&RGBA, 2, 2);
        drop(sink.take_due(Timestamp::ZERO));
        let info = allocation_counter::measure(|| sink.publish(&[1; 16], 2, 2));
        // The frame's own bookkeeping may allocate; the pixels are not copied anew.
        let fresh = allocation_counter::measure(|| sink.publish(&[2; 16], 2, 2));
        assert!(info.bytes_total <= fresh.bytes_total, "{info:?} {fresh:?}");
    }

    #[test]
    fn a_picture_from_the_future_is_placed_now() {
        use rbs_core::time::TestClock;
        let clock = TestClock::new();
        clock.advance(Duration::from_secs(1));
        let mut edge = BrowserPictureEdge::new(Arc::new(clock.clone()));
        let os_now = 5_000_000_000;
        assert_eq!(edge.stamp(os_now + 1_000_000, os_now), clock.now());
        clock.advance(Duration::from_millis(10));
        let unknown = edge.stamp(0, os_now + 10_000_000);
        assert!(unknown <= clock.now());
    }
}
