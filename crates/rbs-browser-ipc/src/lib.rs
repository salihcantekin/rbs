//! Wire format between the studio and the browser helper executable (ADR-0109).
//!
//! A real CEF build replaces that executable and is a process tree (browser, GPU,
//! renderer, utilities). This crate is the message layout only. The studio process
//! does not link or load CEF.
//!
//! Control messages are little-endian and length-prefixed, on the helper's stdin:
//!
//! ```text
//! u32 length | payload
//! payload: u8 kind | body
//! kind 1 configure: u16 url_len | url utf-8 | u32 width | u32 height | u32 fps | u8 transparent
//!                   | u16 frames_len | frames section name utf-8
//!                   | u16 sound_len | sound section name utf-8 (empty: no sound)
//!                   | u32 sample_rate (the rate the studio asks the engine for)
//! ```
//!
//! Frames do not cross the pipe. They cross the studio-allocated shared memory
//! section named in `configure` (`rbs_browser_shm`), which also carries the heartbeat;
//! page sound crosses a second section. The studio
//! allocates that section before it spawns the helper. Dropping it does not join the
//! child. There is no JS binding and no device-permission field on the wire.

use std::time::Duration;

pub mod automation;
pub mod control;
pub mod input;
pub mod page;
pub mod runtime;

/// Exit codes of the CEF runner (`rbs-browser-cef`). Any exit makes the source
/// Unavailable; these two name why (add-ons spec "Browser engine consumer").
pub mod exit {
    /// The `configure` message did not decode.
    pub const BAD_CONFIGURE: i32 = 2;
    /// The studio's frame section did not open.
    pub const NO_SECTION: i32 = 3;
    /// The engine did not initialise.
    pub const INIT_FAILED: i32 = 4;
    /// The engine made no browser.
    pub const NO_BROWSER: i32 = 5;
    /// The page's renderer process went away.
    pub const RENDERER_GONE: i32 = 6;
    /// The downloaded engine is missing or did not load: `browser_runtime_missing`.
    pub const RUNTIME_MISSING: i32 = 7;
    /// The downloaded engine is another CEF version than the runner was built
    /// for: `browser_runtime_mismatch`.
    pub const RUNTIME_MISMATCH: i32 = 8;
    /// The GPU path did not work in this tree (another adapter, the studio's
    /// textures did not open, or copies kept failing): the studio starts the page
    /// again on shared memory (ADR-0157 decision 9).
    pub const GPU_FALLBACK: i32 = 9;
    /// What the OS loader returns when a load-time library is missing (Linux
    /// `libcef.so`): the process ends before `main`.
    pub const LOADER_MISSING: i32 = 127;

    /// Why a runner exited, as far as the engine is concerned.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Cause {
        /// The engine is not there or did not load.
        RuntimeMissing,
        /// The engine is another version.
        RuntimeMismatch,
        /// The GPU path failed; the page runs again on shared memory, with no
        /// back-off (ADR-0157 decision 9).
        GpuFallback,
        /// Anything else: a page, a crash, a bad section.
        Other,
    }

    /// Maps an exit to its cause. `code` is the exit status (`None`: ended by a
    /// signal or killed by the studio); `beaten` is true once the runner beat its
    /// heartbeat, its first hello. An exit before that hello with a code the runner
    /// does not use itself (a loader failure, a crash while loading the engine)
    /// is a missing runtime.
    pub fn cause(code: Option<i32>, beaten: bool) -> Cause {
        match code {
            Some(RUNTIME_MISSING | LOADER_MISSING) => Cause::RuntimeMissing,
            Some(RUNTIME_MISMATCH) => Cause::RuntimeMismatch,
            Some(GPU_FALLBACK) => Cause::GpuFallback,
            Some(BAD_CONFIGURE | NO_SECTION | INIT_FAILED | NO_BROWSER | RENDERER_GONE) => {
                Cause::Other
            }
            Some(_) if !beaten => Cause::RuntimeMissing,
            _ => Cause::Other,
        }
    }
}

/// Configure kind.
pub const KIND_CONFIGURE: u8 = 1;
/// Straight-alpha sRGB `Rgba8`.
pub const PIXEL_FORMAT: &str = "rgba8";
/// Colour space of helper frames.
pub const COLOR_SPACE: &str = "srgb";
/// Largest control payload accepted, so a bad length cannot allocate without bound.
/// A configure carries up to 64 KiB of CSS and 32 KiB of script (ADR-0149 decision 8).
pub const MAX_PAYLOAD: usize = 128 * 1024;
/// The helper beats at least this often.
pub const HEARTBEAT_PERIOD: Duration = Duration::from_millis(500);
/// No heartbeat for this long is a hang (ADR-0109 decision 8).
pub const HEARTBEAT_LIMIT: Duration = Duration::from_secs(2);
/// Before its first heartbeat a helper may take this long: a cold engine start
/// (first launch after install, a slow disk) blocks before the helper can beat.
pub const STARTUP_LIMIT: Duration = Duration::from_secs(15);

/// What the page is not allowed to do. A later CEF runner inherits this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageBoundary {
    /// No JS binding into RBS.
    pub js_bindings: bool,
    /// No camera, microphone, geolocation, download, popup, or persistent grant.
    pub device_prompts: bool,
}

impl PageBoundary {
    /// The contract for every runner, including the test double.
    pub const DENIED: Self = Self {
        js_bindings: false,
        device_prompts: false,
    };
}

/// Launch settings carried on the wire. `transparent` is included (decision 9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Configure {
    /// `https`, `http`, or `file` URL.
    pub url: String,
    /// Even width.
    pub width: u32,
    /// Even height.
    pub height: u32,
    /// Frames per second, 1..=60.
    pub fps: u32,
    /// When false, alpha is forced opaque before publish.
    pub transparent: bool,
    /// Name of the studio-allocated frame section (`rbs_browser_shm::FrameRing`).
    pub frames: String,
    /// Name of the studio-allocated sound section (`rbs_browser_shm::SoundRing`).
    /// Empty: the studio takes no page sound, and the runner turns sound off.
    pub sound: String,
    /// Sample rate the runner asks its engine for, so the studio rarely resamples.
    /// 0: the engine's own.
    pub sample_rate: u32,
    /// CSS, zoom, the user's script, timers and retry (ADR-0132, ADR-0149).
    pub page: PageOptions,
    /// Shared textures, when this tree paints on the GPU. `None`: shared memory.
    pub gpu: Option<GpuShare>,
}

/// What the runner does to the page besides showing it (ADR-0132 decisions 4, 5 and 10,
/// ADR-0149). The default is a plain page: no CSS, no script, no timers, zoom 100,
/// retry on, a fresh profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageOptions {
    /// Custom CSS; empty for none.
    pub css: String,
    /// Page zoom in percent (25..=500).
    pub zoom_percent: u16,
    /// The user's script; empty for none.
    pub script: String,
    /// Run the script after each load.
    pub script_on_load: bool,
    /// Seconds between script runs; 0 for none.
    pub script_every_s: u32,
    /// Seconds between reloads; 0 for none.
    pub reload_every_s: u32,
    /// Keep error pages off air and retry.
    pub retry_on_error: bool,
    /// Reload a `file` page when its directory changes.
    pub reload_on_file_change: bool,
    /// The kept profile's directory; empty for a fresh temporary profile.
    pub profile_dir: String,
}

/// Most textures a [`GpuShare`] names (the frame section's `MAX_GPU_SLOTS`).
pub const MAX_GPU_TEXTURES: usize = 8;

/// Version of the `Configure` GPU block (ADR-0157 decision 10). A block of another
/// version is ignored: the page paints in software.
pub const GPU_BLOCK_VERSION: u8 = 2;

/// Studio-owned shared textures the helper copies into (ADR-0109 decision 8,
/// ADR-0157). Handle values are valid in the helper process. Absent on the
/// shared-memory path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpuShare {
    /// Render adapter the textures were created on; the helper runs CEF on it.
    pub adapter_luid: u64,
    /// Shared fence handle in the helper process.
    pub fence: u64,
    /// Texture handles in the helper process, in slot order; the first `count` hold.
    pub textures: [u64; MAX_GPU_TEXTURES],
    /// How many textures there are (the frame section's GPU slot count).
    pub count: u32,
}

impl GpuShare {
    /// The texture handles that hold.
    pub fn handles(&self) -> &[u64] {
        &self.textures[..(self.count as usize).min(MAX_GPU_TEXTURES)]
    }
}

impl Default for PageOptions {
    fn default() -> Self {
        Self {
            css: String::new(),
            zoom_percent: 100,
            script: String::new(),
            script_on_load: false,
            script_every_s: 0,
            reload_every_s: 0,
            retry_on_error: true,
            reload_on_file_change: false,
            profile_dir: String::new(),
        }
    }
}

fn put_text(body: &mut Vec<u8>, text: &str) {
    let bytes = text.as_bytes();
    let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
    body.extend_from_slice(&len.to_le_bytes());
    body.extend_from_slice(&bytes[..usize::try_from(len).unwrap_or(bytes.len())]);
}

fn take_text(rest: &mut &[u8]) -> Option<String> {
    let len = take_u32(rest)? as usize;
    let text = std::str::from_utf8(rest.get(..len)?).ok()?.to_owned();
    *rest = rest.get(len..)?;
    Some(text)
}

fn take_u32(rest: &mut &[u8]) -> Option<u32> {
    let v = u32::from_le_bytes(rest.get(..4)?.try_into().ok()?);
    *rest = rest.get(4..)?;
    Some(v)
}

fn take_u8(rest: &mut &[u8]) -> Option<u8> {
    let v = *rest.first()?;
    *rest = rest.get(1..)?;
    Some(v)
}

impl PageOptions {
    pub(crate) fn encode(&self, body: &mut Vec<u8>) {
        put_text(body, &self.css);
        body.extend_from_slice(&u32::from(self.zoom_percent).to_le_bytes());
        put_text(body, &self.script);
        body.push(u8::from(self.script_on_load));
        body.extend_from_slice(&self.script_every_s.to_le_bytes());
        body.extend_from_slice(&self.reload_every_s.to_le_bytes());
        body.push(u8::from(self.retry_on_error));
        body.push(u8::from(self.reload_on_file_change));
        put_text(body, &self.profile_dir);
    }

    fn encode_gpu(body: &mut Vec<u8>, gpu: &GpuShare) {
        body.extend_from_slice(b"RBSg");
        body.push(GPU_BLOCK_VERSION);
        body.extend_from_slice(&gpu.adapter_luid.to_le_bytes());
        body.extend_from_slice(&gpu.fence.to_le_bytes());
        let handles = gpu.handles();
        body.push(handles.len() as u8);
        for handle in handles {
            body.extend_from_slice(&handle.to_le_bytes());
        }
    }

    /// `Some(None)` without a block or with a block of another version; `None` for
    /// a malformed block of this version.
    fn decode_gpu(rest: &[u8]) -> Option<Option<GpuShare>> {
        let Some(tail) = rest.strip_prefix(b"RBSg") else {
            return Some(None);
        };
        let (&version, mut tail) = tail.split_first()?;
        if version != GPU_BLOCK_VERSION {
            return Some(None);
        }
        let take = |tail: &mut &[u8]| -> Option<u64> {
            let v = u64::from_le_bytes(tail.get(..8)?.try_into().ok()?);
            *tail = tail.get(8..)?;
            Some(v)
        };
        let adapter_luid = take(&mut tail)?;
        let fence = take(&mut tail)?;
        let (&count, mut tail) = tail.split_first()?;
        if count == 0 || usize::from(count) > MAX_GPU_TEXTURES {
            return None;
        }
        let mut textures = [0; MAX_GPU_TEXTURES];
        for handle in textures.iter_mut().take(usize::from(count)) {
            *handle = take(&mut tail)?;
        }
        Some(Some(GpuShare {
            adapter_luid,
            fence,
            textures,
            count: u32::from(count),
        }))
    }

    pub(crate) fn decode(rest: &mut &[u8]) -> Option<Self> {
        Some(Self {
            css: take_text(rest)?,
            zoom_percent: u16::try_from(take_u32(rest)?).ok()?,
            script: take_text(rest)?,
            script_on_load: take_u8(rest)? != 0,
            script_every_s: take_u32(rest)?,
            reload_every_s: take_u32(rest)?,
            retry_on_error: take_u8(rest)? != 0,
            reload_on_file_change: take_u8(rest)? != 0,
            profile_dir: take_text(rest)?,
        })
    }
}

/// Encodes a configure payload, including the length prefix.
pub fn encode_configure(config: &Configure) -> Vec<u8> {
    let url = config.url.as_bytes();
    let url_len = u16::try_from(url.len()).unwrap_or(u16::MAX);
    let url = &url[..usize::from(url_len)];
    let frames = config.frames.as_bytes();
    let frames_len = u16::try_from(frames.len()).unwrap_or(u16::MAX);
    let frames = &frames[..usize::from(frames_len)];
    let sound = config.sound.as_bytes();
    let sound_len = u16::try_from(sound.len()).unwrap_or(u16::MAX);
    let sound = &sound[..usize::from(sound_len)];
    let mut body = Vec::with_capacity(
        1 + 2 + url.len() + 4 + 4 + 4 + 1 + 2 + frames.len() + 2 + sound.len() + 4,
    );
    body.push(KIND_CONFIGURE);
    body.extend_from_slice(&url_len.to_le_bytes());
    body.extend_from_slice(url);
    body.extend_from_slice(&config.width.to_le_bytes());
    body.extend_from_slice(&config.height.to_le_bytes());
    body.extend_from_slice(&config.fps.to_le_bytes());
    body.push(u8::from(config.transparent));
    body.extend_from_slice(&frames_len.to_le_bytes());
    body.extend_from_slice(frames);
    body.extend_from_slice(&sound_len.to_le_bytes());
    body.extend_from_slice(sound);
    body.extend_from_slice(&config.sample_rate.to_le_bytes());
    config.page.encode(&mut body);
    if let Some(gpu) = &config.gpu {
        PageOptions::encode_gpu(&mut body, gpu);
    }
    prefix(body)
}

/// Decodes a configure message (with or without the length prefix).
pub fn decode_configure(bytes: &[u8]) -> Option<Configure> {
    let body = strip_prefix(bytes)?;
    if body.first().copied() != Some(KIND_CONFIGURE) || body.len() < 1 + 2 + 4 + 4 + 4 + 1 {
        return None;
    }
    let url_len = u16::from_le_bytes([body[1], body[2]]) as usize;
    let rest = body.get(3..)?;
    if rest.len() < url_len + 13 {
        return None;
    }
    let url = std::str::from_utf8(rest.get(..url_len)?).ok()?.to_owned();
    let n = url_len;
    let width = u32::from_le_bytes(rest[n..n + 4].try_into().ok()?);
    let height = u32::from_le_bytes(rest[n + 4..n + 8].try_into().ok()?);
    let fps = u32::from_le_bytes(rest[n + 8..n + 12].try_into().ok()?);
    let transparent = rest[n + 12] != 0;
    let tail = rest.get(n + 13..)?;
    let frames_len = u16::from_le_bytes([*tail.first()?, *tail.get(1)?]) as usize;
    let frames = std::str::from_utf8(tail.get(2..2 + frames_len)?)
        .ok()?
        .to_owned();
    let tail = tail.get(2 + frames_len..)?;
    let sound_len = u16::from_le_bytes([*tail.first()?, *tail.get(1)?]) as usize;
    let sound = std::str::from_utf8(tail.get(2..2 + sound_len)?)
        .ok()?
        .to_owned();
    let rate = tail.get(2 + sound_len..2 + sound_len + 4)?;
    let sample_rate = u32::from_le_bytes(rate.try_into().ok()?);
    // An older studio sends no page block: a plain page.
    let mut rest = tail.get(2 + sound_len + 4..)?;
    let page = if rest.is_empty() {
        PageOptions::default()
    } else {
        PageOptions::decode(&mut rest)?
    };
    let gpu = PageOptions::decode_gpu(rest)?;
    Some(Configure {
        url,
        width,
        height,
        fps,
        transparent,
        frames,
        sound,
        sample_rate,
        page,
        gpu,
    })
}

/// Forces alpha to opaque when `transparent` is false. Runs off the render thread.
pub fn seal_alpha(rgba: &mut [u8], transparent: bool) {
    if transparent {
        return;
    }
    for pixel in rgba.chunks_exact_mut(4) {
        pixel[3] = 255;
    }
}

/// ADR-0024's default curve: 500 ms, doubling, capped at 30 s. `attempt` is 1-based.
pub fn restart_delay(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(16);
    Duration::from_millis(500u64.saturating_mul(1_u64 << shift)).min(Duration::from_secs(30))
}

/// A hang is [`HEARTBEAT_LIMIT`] without a heartbeat (supervised off the render
/// thread). A page that does not change paints nothing; that is not a hang. Callers
/// pass the time since the counter last moved, from their own clock.
pub fn heartbeat_lost(silent_for: Duration) -> bool {
    silent_for >= HEARTBEAT_LIMIT
}

/// A helper that has not beaten once is hung after [`STARTUP_LIMIT`] from spawn.
pub fn startup_lost(since_spawn: Duration) -> bool {
    since_spawn >= STARTUP_LIMIT
}

fn prefix(body: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + body.len());
    let len = u32::try_from(body.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&body[..usize::try_from(len).unwrap_or(body.len())]);
    out
}

fn strip_prefix(bytes: &[u8]) -> Option<&[u8]> {
    if bytes.len() >= 4 {
        let len = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
        let kind = bytes.get(4).copied();
        if bytes.len() == 4 + len && kind == Some(KIND_CONFIGURE) {
            return bytes.get(4..);
        }
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
    use super::*;

    #[test]
    fn configure_round_trips_with_the_frame_section() {
        let config = Configure {
            url: "https://example.com/overlay".into(),
            width: 2,
            height: 2,
            fps: 30,
            transparent: false,
            frames: "/rbsb.1.0".into(),
            sound: "/rbsb.1.1".into(),
            sample_rate: 48_000,
            page: PageOptions::default(),
            gpu: None,
        };
        let encoded = encode_configure(&config);
        assert_eq!(decode_configure(&encoded), Some(config.clone()));
        assert_eq!(decode_configure(&encoded[..encoded.len() - 1]), None);
        let page = Configure {
            page: PageOptions {
                css: "body { background: none }".into(),
                zoom_percent: 150,
                script: "document.title = 'x'".into(),
                script_on_load: true,
                script_every_s: 30,
                reload_every_s: 600,
                retry_on_error: false,
                reload_on_file_change: true,
                profile_dir: "/data/browser-profiles/0123".into(),
            },
            ..config
        };
        let encoded = encode_configure(&page);
        assert_eq!(decode_configure(&encoded), Some(page.clone()));
        let gpu = Configure {
            gpu: Some(GpuShare {
                adapter_luid: 7,
                fence: 11,
                textures: [1, 2, 3, 4, 0, 0, 0, 0],
                count: 4,
            }),
            ..page
        };
        let encoded = encode_configure(&gpu);
        assert_eq!(decode_configure(&encoded), Some(gpu.clone()));
        // A block of another version is ignored: shared memory.
        let tag = encoded.windows(4).position(|w| w == b"RBSg").unwrap();
        let mut other = encoded.clone();
        other[tag + 4] = GPU_BLOCK_VERSION + 1;
        assert_eq!(
            decode_configure(&other).map(|c| c.gpu),
            Some(None),
            "unknown GPU block version"
        );
        // A malformed block of this version is refused.
        let mut bad = encoded;
        bad[tag + 4 + 1 + 16] = 0;
        assert_eq!(decode_configure(&bad), None, "zero textures");
    }

    #[test]
    fn opaque_alpha_is_forced_when_transparency_is_off() {
        let mut rgba = vec![1, 2, 3, 10, 4, 5, 6, 20];
        seal_alpha(&mut rgba, false);
        assert_eq!(&rgba[3..4], &[255]);
        assert_eq!(&rgba[7..8], &[255]);
        seal_alpha(&mut rgba, true);
        assert_eq!(rgba[3], 255);
    }

    #[test]
    fn the_page_boundary_denies_script_and_devices() {
        const {
            assert!(!PageBoundary::DENIED.js_bindings);
            assert!(!PageBoundary::DENIED.device_prompts);
        }
    }

    #[test]
    fn runner_exits_map_to_engine_causes() {
        use exit::{Cause, cause};
        assert_eq!(
            cause(Some(exit::RUNTIME_MISSING), true),
            Cause::RuntimeMissing
        );
        assert_eq!(cause(Some(127), false), Cause::RuntimeMissing);
        assert_eq!(
            cause(Some(exit::RUNTIME_MISMATCH), false),
            Cause::RuntimeMismatch
        );
        // Windows: a crash while loading (0xC0000135, DLL not found) before the hello.
        assert_eq!(cause(Some(-1_073_741_515), false), Cause::RuntimeMissing);
        assert_eq!(cause(Some(-1_073_741_515), true), Cause::Other);
        assert_eq!(cause(Some(exit::NO_SECTION), false), Cause::Other);
        assert_eq!(cause(Some(0), true), Cause::Other);
        assert_eq!(cause(None, false), Cause::Other, "killed by the studio");
    }

    #[test]
    fn a_hang_is_two_seconds_without_a_heartbeat() {
        assert!(!heartbeat_lost(Duration::from_millis(1999)));
        assert!(heartbeat_lost(HEARTBEAT_LIMIT));
        assert!(HEARTBEAT_PERIOD * 4 <= HEARTBEAT_LIMIT);
        assert!(!startup_lost(HEARTBEAT_LIMIT));
        assert!(startup_lost(STARTUP_LIMIT));
        assert_eq!(restart_delay(1), Duration::from_millis(500));
        assert_eq!(restart_delay(2), Duration::from_millis(1000));
    }
}
