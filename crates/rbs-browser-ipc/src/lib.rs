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
pub const MAX_PAYLOAD: usize = 64 * 1024;
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
    Some(Configure {
        url,
        width,
        height,
        fps,
        transparent,
        frames,
        sound,
        sample_rate,
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
        };
        let encoded = encode_configure(&config);
        assert_eq!(decode_configure(&encoded), Some(config));
        assert_eq!(decode_configure(&encoded[..encoded.len() - 1]), None);
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
