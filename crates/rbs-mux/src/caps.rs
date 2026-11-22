//! Well-known capability keys of muxer adapters. See `docs/specs/output-ports.md`.

use rbs_core::capability::CapabilityKey;

/// Set: containers written, as `ContainerId::as_str` names (`mp4.fragmented`,
/// `matroska`, `mpegts`, `flv`).
pub const CONTAINER: CapabilityKey = CapabilityKey::new_static("mux.container");
/// Set: `<container>.<codec>` pairs the muxer can carry (ADR-0085). `container` is
/// [`ContainerId::as_str`](rbs_core::media::ContainerId::as_str) and `codec` is
/// [`CodecId::as_str`](rbs_core::media::CodecId::as_str), for example `matroska.h264`
/// or `wav.pcm`. A bare codec name is still accepted by readers.
pub const CODECS: CapabilityKey = CapabilityKey::new_static("mux.codecs");
/// Flag: the header and media chunks alone are playable (no trailer needed), so a
/// recording survives a crash and the output can be streamed while it grows.
pub const LIVE: CapabilityKey = CapabilityKey::new_static("mux.live");
/// Flag: `finish` may emit patch chunks for seekable transports.
pub const PATCHES: CapabilityKey = CapabilityKey::new_static("mux.patches");
/// Int: audio streams one file of this muxer carries (ADR-0170 decision 7). A muxer
/// that does not declare it takes one: read it with [`audio_streams`].
pub const AUDIO_STREAMS: CapabilityKey = CapabilityKey::new_static("mux.audio_streams");

/// Audio streams one file of a muxer with `caps` carries: its [`AUDIO_STREAMS`], or 1
/// when it declares none (or a value below 1).
pub fn audio_streams(caps: &rbs_core::capability::CapabilitySet) -> usize {
    match caps.get(&AUDIO_STREAMS) {
        Some(rbs_core::capability::CapabilityValue::Int(n)) => {
            usize::try_from(*n).unwrap_or(0).max(1)
        }
        _ => 1,
    }
}
