//! Well-known capability keys of the audio I/O port.
//!
//! See the table in `docs/specs/audio-io-port.md`. Kinds in sets use
//! [`EndpointKind::as_str`](crate::EndpointKind::as_str).

use rbs_core::capability::CapabilityKey;

/// Set of endpoint kinds `open_input` accepts.
pub const INPUT_KINDS: CapabilityKey = CapabilityKey::new_static("audio.input.kinds");
/// Set of endpoint kinds `enumerate` can list.
pub const INPUT_ENUMERATE: CapabilityKey = CapabilityKey::new_static("audio.input.enumerate");
/// Int: the most channels a stream can deliver.
pub const INPUT_CHANNELS_MAX: CapabilityKey = CapabilityKey::new_static("audio.input.channels.max");
/// Flag: the adapter needs a user permission for some kinds (microphone privacy).
pub const INPUT_PERMISSION: CapabilityKey = CapabilityKey::new_static("audio.input.permission");
/// Flag: `OutputLoopback` streams can leave the sound of this process out of the
/// capture (`InputRequest::exclude_own_process`, ADR-0096). Without it a user
/// interface must not offer a display-sound companion, because it would echo.
pub const LOOPBACK_EXCLUDE_SELF: CapabilityKey =
    CapabilityKey::new_static("audio.loopback.exclude_self");

/// Flag: the adapter implements `open_output` (monitoring, ADR-0097).
pub const OUTPUT: CapabilityKey = CapabilityKey::new_static("audio.output");
/// Flag: `enumerate(EndpointKind::Output)` lists playback devices.
pub const OUTPUT_ENUMERATE: CapabilityKey = CapabilityKey::new_static("audio.output.enumerate");
/// Int: the adapter's best estimate of the output latency in nanoseconds (device
/// buffer plus OS path), for a user interface to show.
pub const OUTPUT_LATENCY_NS: CapabilityKey = CapabilityKey::new_static("audio.output.latency_ns");
