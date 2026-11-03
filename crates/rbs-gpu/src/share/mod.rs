//! Cross-process preview (spec `docs/specs/gpu-compositor.md`, "Preview export
//! (cross-process)", ADR-0028).
//!
//! The engine's `PreviewPublisher` scales canvases into a fixed ring of RGBA IOSurfaces
//! and shares them with one UI process over a Mach service. The UI's
//! `PreviewSubscriber` looks the surfaces up once, aliases them as textures on its own
//! device (or shows them as a Core Animation layer, `PreviewLayer`, ADR-0034) and then
//! receives one small message per frame. No pixels cross the process
//! boundary and nothing is copied: both processes use the same IOSurface memory.
//!
//! Only macOS has a backend so far; the endpoint and descriptor types exist everywhere.
//!
//! This is a marked low-level module (principle P11): every `unsafe` block states its
//! invariant.
//!
//! Hot path: `PreviewPublisher::publish` and `PreviewPublisher::tick` do not allocate in
//! RBS code and never wait: every Mach call on the engine side uses a zero timeout.

#[cfg(target_os = "macos")]
mod layer;
#[cfg(target_vendor = "apple")]
mod mach;
#[cfg(target_vendor = "apple")]
mod publisher;
#[cfg(target_vendor = "apple")]
mod subscriber;

use std::fmt;
use std::str::FromStr;

#[cfg(target_os = "macos")]
pub use layer::{LayerRect, PreviewLayer};
#[cfg(target_vendor = "apple")]
pub use publisher::PreviewPublisher;
#[cfg(target_vendor = "apple")]
pub use subscriber::{
    PREVIEW_TEXTURE_FORMAT, PREVIEW_TEXTURE_USAGE, PreviewFrame, PreviewSubscriber,
};

/// Most surfaces one publisher shares.
pub const MAX_PREVIEW_SURFACES: usize = 8;

/// `kCVPixelFormatType_32RGBA` ('RGBA').
#[cfg(target_vendor = "apple")]
const FOURCC_RGBA: u32 = u32::from_be_bytes(*b"RGBA");

/// Size and ring of a preview.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreviewDesc {
    /// Width in pixels; the canvas is scaled to it.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Surfaces in the ring, 2 to [`MAX_PREVIEW_SURFACES`]. Three is enough for a UI that
    /// shows the latest frame: one shown, one in flight, one being written.
    pub surfaces: usize,
}

/// What a UI needs to connect to a publisher: the Mach service name and the token.
///
/// Written as `service#token` (token in hex) on command lines and in the control API.
#[derive(Clone, PartialEq, Eq)]
pub struct PreviewEndpoint {
    /// Mach service name, `org.rbs.preview.<pid>.<n>`.
    pub service: String,
    /// Random token the subscriber must present.
    pub token: u64,
}

impl fmt::Debug for PreviewEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The token is a credential: keep it out of logs.
        f.debug_struct("PreviewEndpoint")
            .field("service", &self.service)
            .finish_non_exhaustive()
    }
}

impl fmt::Display for PreviewEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}#{:016x}", self.service, self.token)
    }
}

impl FromStr for PreviewEndpoint {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (service, token) = s.rsplit_once('#').ok_or("expected service#token")?;
        if service.is_empty() {
            return Err("empty service name");
        }
        let token = u64::from_str_radix(token, 16).map_err(|_| "token is not hex")?;
        Ok(Self {
            service: service.to_owned(),
            token,
        })
    }
}

/// Counters of one publisher.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PreviewStats {
    /// Canvases converted into a surface.
    pub converted: u64,
    /// Frames sent to the UI.
    pub sent: u64,
    /// `publish` calls while nobody was subscribed (no GPU work done).
    pub skipped: u64,
    /// `publish` calls refused because every surface was pending or held.
    pub busy: u64,
    /// Converted frames not delivered (UI queue full, UI gone, or unsubscribed).
    pub dropped: u64,
    /// Accepted subscriptions.
    pub subscribes: u64,
    /// Ignored messages (wrong token or version, unknown messages).
    pub rejected: u64,
}

#[cfg(test)]
mod tests {
    use super::PreviewEndpoint;

    #[test]
    fn endpoint_round_trips_through_text() {
        let e = PreviewEndpoint {
            service: "org.rbs.preview.42.0".into(),
            token: 0x0123_4567_89ab_cdef,
        };
        let s = e.to_string();
        assert_eq!(s, "org.rbs.preview.42.0#0123456789abcdef");
        assert_eq!(s.parse::<PreviewEndpoint>(), Ok(e.clone()));
        assert!(!format!("{e:?}").contains("0123456789abcdef"));
        assert!("no-token".parse::<PreviewEndpoint>().is_err());
        assert!("#12".parse::<PreviewEndpoint>().is_err());
        assert!("svc#xyz".parse::<PreviewEndpoint>().is_err());
    }
}
