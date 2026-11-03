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
