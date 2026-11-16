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
//! ```
//!
//! Frames do not cross the pipe. They cross the studio-allocated shared memory
//! section named in `configure` ([`shm`]), which also carries the heartbeat. The studio
//! allocates that section before it spawns the helper. Dropping it does not join the
//! child. There is no JS binding and no device-permission field on the wire.

use std::time::Duration;

pub mod page;
pub mod shm;

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
    /// Name of the studio-allocated frame section ([`shm::FrameRing`]).
    pub frames: String,
}

/// Encodes a configure payload, including the length prefix.
pub fn encode_configure(config: &Configure) -> Vec<u8> {
    let url = config.url.as_bytes();
    let url_len = u16::try_from(url.len()).unwrap_or(u16::MAX);
    let url = &url[..usize::from(url_len)];
    let frames = config.frames.as_bytes();
    let frames_len = u16::try_from(frames.len()).unwrap_or(u16::MAX);
    let frames = &frames[..usize::from(frames_len)];
    let mut body = Vec::with_capacity(1 + 2 + url.len() + 4 + 4 + 4 + 1 + 2 + frames.len());
    body.push(KIND_CONFIGURE);
    body.extend_from_slice(&url_len.to_le_bytes());
    body.extend_from_slice(url);
    body.extend_from_slice(&config.width.to_le_bytes());
    body.extend_from_slice(&config.height.to_le_bytes());
    body.extend_from_slice(&config.fps.to_le_bytes());
    body.push(u8::from(config.transparent));
    body.extend_from_slice(&frames_len.to_le_bytes());
    body.extend_from_slice(frames);
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
    Some(Configure {
        url,
        width,
        height,
        fps,
        transparent,
        frames,
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
