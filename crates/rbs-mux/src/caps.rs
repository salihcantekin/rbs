//! Well-known capability keys of muxer adapters. See `docs/specs/output-ports.md`.

use rbs_core::capability::CapabilityKey;

/// Set: containers written, as `ContainerId::as_str` names (`mp4.fragmented`,
/// `matroska`, `mpegts`, `flv`).
pub const CONTAINER: CapabilityKey = CapabilityKey::new_static("mux.container");
/// Set: codecs the muxer can carry, as `CodecId::as_str` names.
pub const CODECS: CapabilityKey = CapabilityKey::new_static("mux.codecs");
/// Flag: the header and media chunks alone are playable (no trailer needed), so a
/// recording survives a crash and the output can be streamed while it grows.
pub const LIVE: CapabilityKey = CapabilityKey::new_static("mux.live");
/// Flag: `finish` may emit patch chunks for seekable transports.
pub const PATCHES: CapabilityKey = CapabilityKey::new_static("mux.patches");
