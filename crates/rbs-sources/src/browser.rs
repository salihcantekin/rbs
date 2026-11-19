//! Browser source type (ADR-0109). The studio does not link or load CEF.
//!
//! A real CEF build replaces the helper executable and is a process tree. This module
//! validates settings, classifies every URL as network-capable, and grants at most two
//! running helpers (program before preview, lower source id inside a class). The
//! composition root spawns the executable from `rbs-browser-runner` (or the CEF tree
//! root) by path. `rbs-sources` does not spawn processes.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use rbs_audio_io::edge::EdgeClock;
use rbs_browser_ipc::control::Control;
use rbs_browser_ipc::{Configure, PageOptions, restart_delay};
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
/// Control-API reason for a page that has no address yet: it starts no process and
/// holds no slot (Add source › Browser adds one, and the Page tab asks for it).
pub const BROWSER_NO_ADDRESS: &str = "browser_no_address";
/// Control-API reason when a source does not get a slot.
pub const BROWSER_CAP: &str = "browser_cap";
/// Reason when the browser engine add-on is not installed, or the runner could not
/// load it (ADR-0145, add-ons spec "Browser engine consumer"). No slot, no back-off.
pub const BROWSER_RUNTIME_MISSING: &str = "browser_runtime_missing";
/// Reason when the installed browser engine is another version than the runner's.
pub const BROWSER_RUNTIME_MISMATCH: &str = "browser_runtime_mismatch";
/// Reason when the composition root set no runner, or found none.
pub const HELPER_MISSING: &str = "helper_missing";
/// Reason after any other runner exit (counts toward back-off).
pub const HELPER_EXIT: &str = "helper_exit";

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
    /// Placed but drawn by no shown scene, with `run: placed` (ADR-0149 decision 5,
    /// ADR-0152): a background page. It never takes a slot from a shown page.
    Background,
}

/// One source competing for a helper slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrowserClaim {
    /// Source id. Lower wins inside a class.
    pub source: SourceId,
    /// Program, preview or background.
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

/// Largest `css` in bytes (ADR-0132 decision 4).
pub const MAX_CSS_BYTES: usize = 64 * 1024;
/// Largest `script` in bytes (ADR-0149 decision 1).
pub const MAX_SCRIPT_BYTES: usize = 32 * 1024;
/// Longest reload or script interval, a day in seconds (ADR-0149 decision 1).
pub const MAX_INTERVAL_S: u32 = 86_400;
/// Shortest periodic reload in seconds (ADR-0149 decision 1).
pub const MIN_RELOAD_EVERY_S: u32 = 10;
/// Page zoom bounds in percent and its default (ADR-0149 decision 1).
pub const ZOOM_RANGE: std::ops::RangeInclusive<u32> = 25..=500;
/// Default page zoom in percent.
pub const DEFAULT_ZOOM: u32 = 100;

/// The settings keys of a browser source, in the order the Page tab shows them.
pub const BROWSER_KEYS: [&str; 16] = [
    keys::URL,
    keys::WIDTH,
    keys::HEIGHT,
    keys::FPS,
    keys::TRANSPARENT,
    keys::CSS,
    keys::RELOAD_ON_FILE_CHANGE,
    keys::KEEP_SIGNED_IN,
    keys::PROFILE_ID,
    keys::RELOAD_EVERY_S,
    keys::SCRIPT,
    keys::SCRIPT_ON_LOAD,
    keys::SCRIPT_EVERY_S,
    keys::ZOOM_PERCENT,
    keys::KEEP_RUNNING,
    keys::RETRY_ON_ERROR,
];

/// Keys a running page takes in place (ADR-0132 decision 4, ADR-0149 decision 1): a
/// change of only these keeps the node and its process, and reaches the runner over
/// IPC ([`apply_browser_live`]). Every other key opens the page again.
pub const BROWSER_LIVE_KEYS: [&str; 8] = [
    keys::CSS,
    keys::RELOAD_ON_FILE_CHANGE,
    keys::RELOAD_EVERY_S,
    keys::SCRIPT,
    keys::SCRIPT_ON_LOAD,
    keys::SCRIPT_EVERY_S,
    keys::ZOOM_PERCENT,
    keys::RETRY_ON_ERROR,
];

/// Hands a browser source's new live settings to its control handle: a running page
/// gets them at once, and the next start of this node uses them. `settings` passed
/// `validate_settings` before the document write, so a refusal here changes nothing.
/// Control thread.
pub fn apply_browser_live(
    control: &BrowserControlSidecar,
    settings: &Settings,
) -> Result<(), (&'static str, &'static str)> {
    let page = browser_settings(settings)?.configure().page;
    control.set_page(page);
    Ok(())
}

/// Keys whose value can make the page reach the network on its own, so a session
/// without the network permission may not write them (ADR-0052, ADR-0132 decision 4,
/// ADR-0149 decisions 3 and 5). `keep_running` counts only when set to true.
pub const NETWORK_KEYS: [&str; 6] = [
    keys::URL,
    keys::CSS,
    keys::SCRIPT,
    keys::SCRIPT_ON_LOAD,
    keys::SCRIPT_EVERY_S,
    keys::KEEP_RUNNING,
];

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
    /// Custom CSS; empty for none.
    pub css: String,
    /// Reload a `file` page when its directory changes.
    pub reload_on_file_change: bool,
    /// The kept profile, when `keep_signed_in` is on and the server gave one.
    pub profile_id: Option<String>,
    /// Seconds between reloads; 0 for none.
    pub reload_every_s: u32,
    /// The user's script; empty for none.
    pub script: String,
    /// Run the script after each load.
    pub script_on_load: bool,
    /// Seconds between script runs; 0 for none.
    pub script_every_s: u32,
    /// Page zoom in percent.
    pub zoom_percent: u32,
    /// Active while placed anywhere (ADR-0149 decision 5).
    pub keep_running: bool,
    /// Keep error pages off air and retry (default true).
    pub retry_on_error: bool,
}

impl BrowserSettings {
    /// A page at `url` with every other setting at its default.
    pub fn page(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            width: DEFAULT_SIZE.0,
            height: DEFAULT_SIZE.1,
            fps: DEFAULT_FPS,
            transparent: false,
            css: String::new(),
            reload_on_file_change: false,
            profile_id: None,
            reload_every_s: 0,
            script: String::new(),
            script_on_load: false,
            script_every_s: 0,
            zoom_percent: DEFAULT_ZOOM,
            keep_running: false,
            retry_on_error: true,
        }
    }

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
            page: PageOptions {
                css: self.css.clone(),
                zoom_percent: u16::try_from(self.zoom_percent).unwrap_or(u16::MAX),
                script: self.script.clone(),
                script_on_load: self.script_on_load,
                script_every_s: self.script_every_s,
                reload_every_s: self.reload_every_s,
                retry_on_error: self.retry_on_error,
                reload_on_file_change: self.reload_on_file_change,
                profile_dir: String::new(),
            },
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
        if !BROWSER_KEYS.contains(&key.as_str()) {
            return Err(("settings", "only keys this source type uses"));
        }
    }
    match settings.get(keys::URL) {
        // No address yet: the page starts nothing (`BROWSER_NO_ADDRESS`).
        None => {}
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
    for key in [
        keys::TRANSPARENT,
        keys::RELOAD_ON_FILE_CHANGE,
        keys::KEEP_SIGNED_IN,
        keys::SCRIPT_ON_LOAD,
        keys::KEEP_RUNNING,
        keys::RETRY_ON_ERROR,
    ] {
        bool_setting(settings, key)?;
    }
    if text_setting(settings, keys::CSS)?.is_some_and(|css| css.len() > MAX_CSS_BYTES) {
        return Err((keys::CSS, "at most 64 KiB"));
    }
    if text_setting(settings, keys::SCRIPT)?.is_some_and(|s| s.len() > MAX_SCRIPT_BYTES) {
        return Err((keys::SCRIPT, "at most 32 768 bytes"));
    }
    if let Some(id) = text_setting(settings, keys::PROFILE_ID)? {
        if !is_profile_id(id) {
            return Err((keys::PROFILE_ID, "32 lowercase hex digits"));
        }
    }
    if let Some(s) = count_setting(settings, keys::RELOAD_EVERY_S)? {
        if s != 0 && !(MIN_RELOAD_EVERY_S..=MAX_INTERVAL_S).contains(&s) {
            return Err((keys::RELOAD_EVERY_S, "0, or from 10 to 86400 seconds"));
        }
    }
    if let Some(s) = count_setting(settings, keys::SCRIPT_EVERY_S)? {
        if s > MAX_INTERVAL_S {
            return Err((keys::SCRIPT_EVERY_S, "from 0 to 86400 seconds"));
        }
    }
    if let Some(z) = count_setting(settings, keys::ZOOM_PERCENT)? {
        if !ZOOM_RANGE.contains(&z) {
            return Err((keys::ZOOM_PERCENT, "from 25 to 500 percent"));
        }
    }
    Ok(())
}

/// True for a kept-profile id: 32 lowercase hex digits (ADR-0132 decision 10).
pub fn is_profile_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn bool_setting(
    settings: &Settings,
    key: &'static str,
) -> Result<Option<bool>, (&'static str, &'static str)> {
    match settings.get(key) {
        None => Ok(None),
        Some(SettingValue::Bool(on)) => Ok(Some(*on)),
        Some(_) => Err((key, "a bool")),
    }
}

fn text_setting<'a>(
    settings: &'a Settings,
    key: &'static str,
) -> Result<Option<&'a str>, (&'static str, &'static str)> {
    match settings.get(key) {
        None => Ok(None),
        Some(SettingValue::Text(text)) => Ok(Some(text.as_str())),
        Some(_) => Err((key, "text")),
    }
}

/// An integer that may be 0.
fn count_setting(
    settings: &Settings,
    key: &'static str,
) -> Result<Option<u32>, (&'static str, &'static str)> {
    match settings.get(key) {
        None => Ok(None),
        Some(SettingValue::Int(value)) => u32::try_from(*value)
            .map(Some)
            .map_err(|_| (key, "a whole number, 0 or more")),
        Some(_) => Err((key, "an integer")),
    }
}

/// Applies defaults. `validate_browser` must have succeeded.
pub fn browser_settings(
    settings: &Settings,
) -> Result<BrowserSettings, (&'static str, &'static str)> {
    validate_browser(settings)?;
    let url = match settings.get(keys::URL) {
        Some(SettingValue::Text(url)) => url.clone(),
        None => String::new(),
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
    let flag = |key| bool_setting(settings, key).map(|on| on.unwrap_or(false));
    let text = |key| text_setting(settings, key).map(|t| t.unwrap_or_default().to_owned());
    let count = |key| count_setting(settings, key).map(|n| n.unwrap_or(0));
    let keep_signed_in = flag(keys::KEEP_SIGNED_IN)?;
    Ok(BrowserSettings {
        url,
        width,
        height,
        fps,
        transparent: flag(keys::TRANSPARENT)?,
        css: text(keys::CSS)?,
        reload_on_file_change: flag(keys::RELOAD_ON_FILE_CHANGE)?,
        profile_id: text_setting(settings, keys::PROFILE_ID)?
            .filter(|_| keep_signed_in)
            .map(str::to_owned),
        reload_every_s: count(keys::RELOAD_EVERY_S)?,
        script: text(keys::SCRIPT)?,
        script_on_load: flag(keys::SCRIPT_ON_LOAD)?,
        script_every_s: count(keys::SCRIPT_EVERY_S)?,
        zoom_percent: count_setting(settings, keys::ZOOM_PERCENT)?.unwrap_or(DEFAULT_ZOOM),
        keep_running: flag(keys::KEEP_RUNNING)?,
        retry_on_error: bool_setting(settings, keys::RETRY_ON_ERROR)?.unwrap_or(true),
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
    /// Set when `pending` holds a picture and cleared when that picture is taken.
    /// The render thread's idle path is one atomic load (the video queue's mailbox).
    fresh: AtomicBool,
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
    /// Master time of `last`, so a page shown again republishes it without the pixels.
    stamp: Option<Timestamp>,
}

impl BrowserFrameSink {
    /// Empty buffer.
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(None),
            fresh: AtomicBool::new(false),
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
        let replaced = self.take_pending();
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
        self.store_picture(kept, bytes, width, height, stamp);
    }

    /// Publishes a buffer the supervisor owns. When the previous picture is still
    /// held, the buffer becomes the frame's bytes and `rgba` is left at the same
    /// length for the next section copy, which overwrites it. When the render thread
    /// has let the previous bytes go, those bytes are reused and `rgba` is unchanged.
    pub fn publish_frame(
        &self,
        rgba: &mut Vec<u8>,
        width: u32,
        height: u32,
        stamp: Option<Timestamp>,
    ) {
        let expected = (width as usize)
            .saturating_mul(height as usize)
            .saturating_mul(4);
        if width == 0 || height == 0 || rgba.len() != expected {
            return;
        }
        let replaced = self.take_pending();
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
            held => {
                if let Some(Err(held)) = held {
                    kept.retired = Some(held);
                }
                let painted = std::mem::take(rgba);
                // The next section copy overwrites every byte before anything reads it.
                // A page shown again republishes `kept.last` and does not read `rgba`.
                rgba.reserve(expected);
                // SAFETY: `expected` equals `painted.len()`, which was one full frame.
                // `take_latest` writes that many bytes over this buffer before a read.
                unsafe { rgba.set_len(expected) };
                Bytes::from(painted)
            }
        };
        self.store_picture(kept, bytes, width, height, stamp);
    }

    /// Puts the last picture back for a page that was shown again and has not painted.
    /// Clones the bytes (a reference count). No pixel copy.
    pub fn republish(&self) {
        let Ok(kept) = self.kept.lock() else {
            return;
        };
        let Some(bytes) = kept.last.clone() else {
            return;
        };
        let stamp = kept.stamp;
        let width = self.width.load(Ordering::Acquire);
        let height = self.height.load(Ordering::Acquire);
        drop(kept);
        if width == 0 || height == 0 {
            return;
        }
        let format = VideoFormat::new(
            Size::new(width, height),
            PixelFormat::Rgba8,
            ColorSpace::SRGB,
        );
        let planes = [PlaneLayout {
            offset: 0,
            stride: width as usize * 4,
        }];
        let Ok(system) = SystemFrame::new(format, bytes, &planes) else {
            return;
        };
        let frame = VideoFrame::system(stamp.unwrap_or(Timestamp::ZERO), system, None)
            .with_sequence(next_generation());
        if let Ok(mut slot) = self.pending.lock() {
            *slot = Some(PendingPicture { frame, stamp });
            self.fresh.store(true, Ordering::Release);
        }
        self.generation.fetch_add(1, Ordering::Release);
    }

    fn store_picture(
        &self,
        mut kept: std::sync::MutexGuard<'_, Kept>,
        bytes: Bytes,
        width: u32,
        height: u32,
        stamp: Option<Timestamp>,
    ) {
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
        kept.stamp = stamp;
        drop(kept);
        let frame = VideoFrame::system(stamp.unwrap_or(Timestamp::ZERO), system, None)
            .with_sequence(next_generation());
        if let Ok(mut slot) = self.pending.lock() {
            *slot = Some(PendingPicture { frame, stamp });
            self.fresh.store(true, Ordering::Release);
        }
        self.width.store(width, Ordering::Release);
        self.height.store(height, Ordering::Release);
        self.generation.fetch_add(1, Ordering::Release);
    }

    /// Drops a picture still waiting, and clears the idle flag with it.
    fn take_pending(&self) -> Option<PendingPicture> {
        self.pending.lock().ok().and_then(|mut slot| {
            self.fresh.store(false, Ordering::Release);
            slot.take()
        })
    }

    /// Forgets a picture the render thread has not taken (the helper stopped).
    pub(crate) fn clear(&self) {
        let _stale = self.take_pending();
    }

    /// Pictures published so far.
    pub fn generation(&self) -> &AtomicU32 {
        &self.generation
    }

    /// Render thread: the newest picture whose stamp is at or before `now`, once.
    /// `None` when there is none, it is not due yet, or the supervisor holds the
    /// slot this instant (taken on a later tick). Nothing published since the last
    /// take is one atomic load and does not lock. Never waits, never allocates.
    pub fn take_due(&self, now: Timestamp) -> Option<VideoFrame> {
        if !self.fresh.load(Ordering::Acquire) {
            return None;
        }
        let mut slot = self.pending.try_lock().ok()?;
        if slot.as_ref()?.stamp.is_some_and(|stamp| stamp > now) {
            return None;
        }
        self.fresh.store(false, Ordering::Release);
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
    /// The engine reason of the last exit or refused start (0 none, 1 missing, 2
    /// mismatch), read by the studio to word the source and to start it again after
    /// an install.
    engine: AtomicU8,
    /// Page options changed in place since this node was made (live keys): the next
    /// start uses them instead of the node's own.
    page: Mutex<Option<PageOptions>>,
    /// What a start needs: the node's settings, tree and sound, so the studio can
    /// start a background page whose picture node is not scheduled.
    starter: Mutex<Option<Starter>>,
    /// False while no shown scene draws the page (ADR-0149 decision 5).
    shown: AtomicBool,
    /// The page is a background page: a release of the picture node keeps its helper.
    background: AtomicBool,
    /// A background page that lost its slot to a shown page (`browser_cap`).
    capped: AtomicBool,
}

/// The parts of a browser node a start needs (see [`BrowserControlSidecar::start`]).
struct Starter {
    settings: BrowserSettings,
    tree: Option<Arc<dyn BrowserTree>>,
    sound: Option<Arc<crate::BrowserSound>>,
}

/// Why [`BrowserControlSidecar::start`] started no helper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartFailure {
    /// The error the node's prepare reports.
    pub error: BrowserStartError,
    /// The source reason (`browser_cap`, `browser_no_address`, ...).
    pub reason: &'static str,
    /// The helper exited or could not be started: counts toward back-off.
    pub exit: bool,
}

fn encode_engine(reason: Option<&'static str>) -> u8 {
    match reason {
        Some(BROWSER_RUNTIME_MISSING) => 1,
        Some(BROWSER_RUNTIME_MISMATCH) => 2,
        _ => 0,
    }
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
            engine: AtomicU8::new(0),
            page: Mutex::new(None),
            starter: Mutex::new(None),
            shown: AtomicBool::new(true),
            background: AtomicBool::new(false),
            capped: AtomicBool::new(false),
        }
    }

    fn set_starter(&self, starter: Starter) {
        if let Ok(mut slot) = self.starter.lock() {
            *slot = Some(starter);
        }
    }

    fn set_sound(&self, sound: Arc<crate::BrowserSound>) {
        if let Ok(mut slot) = self.starter.lock()
            && let Some(starter) = slot.as_mut()
        {
            starter.sound = Some(sound);
        }
    }

    /// Starts the helper when this source holds a slot (the node's prepare, or the
    /// studio for a background page). `Ok(true)` when it started one now, `Ok(false)`
    /// when one already runs. Control path; never waits on the page.
    pub fn start(&self) -> Result<bool, StartFailure> {
        if self.helper_running() {
            return Ok(false);
        }
        let fail = |error, reason, exit| StartFailure {
            error,
            reason,
            exit,
        };
        let Ok(guard) = self.starter.lock() else {
            return Err(fail(
                BrowserStartError::Failed("the browser node is gone".into()),
                HELPER_MISSING,
                false,
            ));
        };
        let Some(starter) = guard.as_ref() else {
            return Err(fail(
                BrowserStartError::Failed("the browser node is gone".into()),
                HELPER_MISSING,
                false,
            ));
        };
        if starter.settings.url.is_empty() {
            self.budget.release(self.source);
            return Err(fail(
                BrowserStartError::Unavailable(BROWSER_NO_ADDRESS),
                BROWSER_NO_ADDRESS,
                false,
            ));
        }
        let Some(tree) = starter.tree.clone() else {
            return Err(fail(
                BrowserStartError::Failed(
                    "the composition root has not set a browser helper".into(),
                ),
                HELPER_MISSING,
                false,
            ));
        };
        // Before the slot: a source that cannot start holds none and does not back
        // off (add-ons spec "Browser engine consumer").
        if let Some(reason) = tree.unavailable() {
            self.budget.release(self.source);
            self.set_engine_reason(Some(reason));
            return Err(fail(BrowserStartError::Unavailable(reason), reason, false));
        }
        if !self.budget.holds(self.source) {
            return Err(fail(BrowserStartError::Cap, BROWSER_CAP, false));
        }
        let mut configure = starter.settings.configure();
        if let Some(page) = self.page() {
            configure.page = page;
        }
        if let Some(id) = starter.settings.profile_id.as_deref() {
            configure.page.profile_dir = tree.profile_dir(id).unwrap_or_default();
        }
        if let Some(sound) = starter.sound.as_ref() {
            configure.sample_rate = sound.sample_rate();
        }
        let sound = starter.sound.clone();
        drop(guard);
        match tree.start(&configure, self.sink(), sound) {
            Ok(mut session) => {
                session.refresh_sink(&self.sink);
                if session.poll().is_some() {
                    let engine = session.exit_reason();
                    self.budget.release(self.source);
                    self.set_engine_reason(engine);
                    return Err(fail(
                        BrowserStartError::Exited,
                        engine.unwrap_or(HELPER_EXIT),
                        true,
                    ));
                }
                if !self.shown.load(Ordering::Acquire) {
                    session.set_shown(false);
                }
                self.set_session(session);
                self.set_engine_reason(None);
                self.capped.store(false, Ordering::Release);
                Ok(true)
            }
            Err(error) => {
                self.budget.release(self.source);
                Err(fail(BrowserStartError::Failed(error), HELPER_EXIT, true))
            }
        }
    }

    /// Whether a shown scene draws the page (ADR-0149 decision 5): hidden, a running
    /// helper paints once per second and its sound is dropped; the next start begins
    /// hidden. Control path.
    pub fn set_shown(&self, shown: bool) {
        if self.shown.swap(shown, Ordering::AcqRel) == shown {
            return;
        }
        if let Ok(mut guard) = self.session.lock()
            && let Some(session) = guard.as_mut()
        {
            session.set_shown(shown);
        }
    }

    /// False while no shown scene draws the page.
    pub fn shown(&self) -> bool {
        self.shown.load(Ordering::Acquire)
    }

    /// Marks the page as a background page (ADR-0149 decision 5, ADR-0152 `run:
    /// placed`): a release of its picture node then keeps the helper. Control path.
    pub fn set_background(&self, background: bool) {
        self.background.store(background, Ordering::Release);
    }

    /// True while the page is a background page.
    pub fn background(&self) -> bool {
        self.background.load(Ordering::Acquire)
    }

    /// Stops a background page that lost its slot to a shown page, or that is no
    /// longer wanted (`capped` true reports `browser_cap`). Control path.
    pub fn stop_background(&self, capped: bool) {
        self.clear_session();
        self.frames().clear();
        self.budget.release(self.source);
        self.capped.store(capped, Ordering::Release);
    }

    /// True while a background page is stopped because a shown page took its slot.
    pub fn capped(&self) -> bool {
        self.capped.load(Ordering::Acquire)
    }

    /// New page options (live keys): kept for the next start and sent to a running
    /// page. True when a running page took them.
    pub fn set_page(&self, page: PageOptions) -> bool {
        let sent = self.send(&Control::Page(Box::new(page.clone())));
        if let Ok(mut slot) = self.page.lock() {
            *slot = Some(page);
        }
        sent
    }

    /// The page options set in place since this node was made, if any.
    fn page(&self) -> Option<PageOptions> {
        self.page.lock().ok().and_then(|slot| slot.clone())
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
            self.engine
                .store(encode_engine(session.exit_reason()), Ordering::Release);
            session.stop();
            *guard = None;
            self.budget.release(self.source);
            self.exited.store(true, Ordering::Release);
            return;
        }
        session.refresh_sink(&self.sink);
    }

    /// The engine reason of the last exit or refused start
    /// ([`BROWSER_RUNTIME_MISSING`] or [`BROWSER_RUNTIME_MISMATCH`]); `None` once a
    /// helper runs. Never waits.
    pub fn engine_reason(&self) -> Option<&'static str> {
        match self.engine.load(Ordering::Acquire) {
            1 => Some(BROWSER_RUNTIME_MISSING),
            2 => Some(BROWSER_RUNTIME_MISMATCH),
            _ => None,
        }
    }

    fn set_engine_reason(&self, reason: Option<&'static str>) {
        self.engine.store(encode_engine(reason), Ordering::Release);
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

    /// Sends a control message to the running page (`browser.reload`,
    /// `browser.run_script`); false when no helper runs. Control path.
    pub fn send(&self, message: &Control) -> bool {
        let Ok(mut guard) = self.session.lock() else {
            return false;
        };
        guard.as_mut().is_some_and(|session| session.send(message))
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

    /// Why no tree can start now ([`BROWSER_RUNTIME_MISSING`], [`HELPER_MISSING`]),
    /// checked at each start before a slot is used. `None`: [`BrowserTree::start`]
    /// may run. Never waits on the network; may read the disk.
    fn unavailable(&self) -> Option<&'static str> {
        None
    }

    /// The directory of the kept profile `profile_id` (ADR-0132 decision 10), or
    /// `None` when this tree keeps no profiles: the page then gets a fresh one.
    fn profile_dir(&self, profile_id: &str) -> Option<String> {
        let _ = profile_id;
        None
    }
}

/// One running tree. `poll` and `stop` run on the control thread, never in `process`.
pub trait BrowserSession: Send {
    /// `None` while the process is alive.
    fn poll(&mut self) -> Option<i32>;
    /// Kills the tree. Standby.
    fn stop(&mut self);
    /// After [`BrowserSession::poll`] saw an exit: the source reason it maps to
    /// ([`BROWSER_RUNTIME_MISSING`], [`BROWSER_RUNTIME_MISMATCH`]); `None` for any
    /// other exit ([`HELPER_EXIT`]).
    fn exit_reason(&self) -> Option<&'static str> {
        None
    }
    /// Copies the latest helper frame into `sink`.
    fn refresh_sink(&mut self, sink: &BrowserFrameSink) {
        let _ = sink;
    }
    /// Sends a control message to the running page (ADR-0132 decision 8); false when
    /// the tree cannot take one. Control path, never `process`.
    fn send(&mut self, message: &Control) -> bool {
        let _ = message;
        false
    }
    /// Hides (false) or shows (true) a background page (ADR-0149 decision 5): hidden,
    /// it paints once per second and its sound is dropped. Control path.
    fn set_shown(&mut self, shown: bool) {
        let _ = self.send(&Control::Shown(shown));
    }
}

/// The browser node. Standby holds no helper.
pub struct BrowserSourceNode {
    descriptor: NodeDescriptor,
    settings: BrowserSettings,
    budget: Arc<BrowserBudget>,
    source: SourceId,
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
        self.control.set_sound(Arc::clone(&sound));
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
        let control = Arc::new(BrowserControlSidecar::new(source, Arc::clone(&budget)));
        control.set_starter(Starter {
            settings: settings.clone(),
            tree,
            sound: None,
        });
        Self {
            descriptor: browser_descriptor(),
            settings,
            budget,
            source,
            control,
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

    /// `browser_cap`, `helper_exit`, `helper_missing`, `browser_runtime_missing` or
    /// `browser_runtime_mismatch` when the source is Unavailable.
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
        match self.control.start() {
            Ok(started) => {
                if started {
                    self.starts = self.starts.saturating_add(1);
                }
                self.reason = None;
                Ok(())
            }
            Err(failure) => {
                if failure.exit {
                    if failure.error == BrowserStartError::Exited {
                        self.starts = self.starts.saturating_add(1);
                    }
                    self.note_exit();
                }
                self.reason = Some(failure.reason);
                Err(failure.error)
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

    /// Standby. Kills the helper, so no process remains, unless the page is a
    /// background page (ADR-0149 decision 5): its picture node leaves the schedule
    /// and its helper keeps running for the studio.
    pub fn standby(&mut self) {
        if self.control.background() {
            return;
        }
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
        self.reason = Some(self.control.engine_reason().unwrap_or(HELPER_EXIT));
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
    /// No tree can start now (the browser engine is missing); no slot is held and
    /// the node is not restarted until the studio asks.
    Unavailable(&'static str),
}

impl std::fmt::Display for BrowserStartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BrowserStartError::Cap => f.write_str(BROWSER_CAP),
            BrowserStartError::Exited => f.write_str("browser helper exited"),
            BrowserStartError::Failed(text) => f.write_str(text),
            BrowserStartError::Unavailable(reason) => f.write_str(reason),
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
        self.activate().map_err(|error| match error {
            // Retrying will not help until the engine is installed; the studio starts
            // the node again then (ADR-0024 Permanent).
            BrowserStartError::Unavailable(reason) => NodeError::Permanent(reason),
            other => NodeError::Failed(other.to_string()),
        })
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
    fn a_held_picture_keeps_the_supervisors_buffer() {
        let sink = BrowserFrameSink::new();
        let mut buf = vec![1u8; 16];
        let ptr = buf.as_ptr();
        sink.publish_frame(&mut buf, 2, 2, None);
        let held = sink.take_due(Timestamp::ZERO).unwrap();
        match held.payload() {
            rbs_core::media::FramePayload::System(system) => {
                assert_eq!(system.data().as_ptr(), ptr);
            }
            _ => panic!("system frame"),
        }
        assert_eq!(buf.len(), 16);
        let ptr2 = buf.as_ptr();
        buf.fill(2);
        sink.publish_frame(&mut buf, 2, 2, None);
        let next = sink.take_due(Timestamp::ZERO).unwrap();
        match next.payload() {
            rbs_core::media::FramePayload::System(system) => {
                assert_eq!(system.data().as_ptr(), ptr2);
                assert_eq!(system.data()[0], 2);
            }
            _ => panic!("system frame"),
        }
        sink.republish();
        let again = sink.take_due(Timestamp::ZERO).unwrap();
        match again.payload() {
            rbs_core::media::FramePayload::System(system) => {
                assert_eq!(system.data().as_ptr(), ptr2);
            }
            _ => panic!("system frame"),
        }
        drop(held);
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
