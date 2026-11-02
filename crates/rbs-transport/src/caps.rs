//! Well-known capability keys of transport adapters. See `docs/specs/output-ports.md`.

use rbs_core::capability::CapabilityKey;

/// Set: URL schemes handled (`rtmp`, `rtmps`, `srt`, `file`, ...).
pub const SCHEMES: CapabilityKey = CapabilityKey::new_static("transport.schemes");
/// Set: accepted input, [`INPUT_PACKETS`] and/or `container.<name>` values from
/// `ContainerId::transport_input`.
pub const INPUT: CapabilityKey = CapabilityKey::new_static("transport.input");
/// Set: codecs accepted with packet input (`CodecId::as_str` names).
pub const CODECS: CapabilityKey = CapabilityKey::new_static("transport.codecs");
/// Flag: reconnects by itself according to a `ReconnectPolicy`.
pub const RECONNECT: CapabilityKey = CapabilityKey::new_static("transport.reconnect");
/// Flag: reports a bandwidth estimate in `TransportStats`.
pub const BANDWIDTH_ESTIMATE: CapabilityKey =
    CapabilityKey::new_static("transport.bandwidth_estimate");
/// Flag: applies patch chunks (the destination is seekable).
pub const SEEKABLE: CapabilityKey = CapabilityKey::new_static("transport.seekable");

/// Value of [`INPUT`] for transports that take encoded packets and frame them.
pub const INPUT_PACKETS: &str = "packets";
