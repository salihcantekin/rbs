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
/// Flag: deprecated alias of [`PERMISSION_INPUT`] (ADR-0141); adapters set both to
/// the same value until the next major version.
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

/// Flag: the OS has a user permission for capturing `Input` endpoints (ADR-0141).
pub const PERMISSION_INPUT: CapabilityKey = CapabilityKey::new_static("audio.permission.input");
/// Flag: the OS has a user permission for capturing `OutputLoopback` endpoints.
pub const PERMISSION_LOOPBACK: CapabilityKey =
    CapabilityKey::new_static("audio.permission.loopback");
/// Flag: the OS has a user permission for capturing `Application` endpoints.
pub const PERMISSION_APPLICATION: CapabilityKey =
    CapabilityKey::new_static("audio.permission.application");
/// Flag: `request_permission(Input)` can show the OS prompt in this process now.
pub const PERMISSION_INPUT_PROMPT: CapabilityKey =
    CapabilityKey::new_static("audio.permission.input.prompt");
/// Flag: `request_permission(OutputLoopback)` can show the OS prompt in this process now.
pub const PERMISSION_LOOPBACK_PROMPT: CapabilityKey =
    CapabilityKey::new_static("audio.permission.loopback.prompt");
/// Flag: `request_permission(Application)` can show the OS prompt in this process now.
pub const PERMISSION_APPLICATION_PROMPT: CapabilityKey =
    CapabilityKey::new_static("audio.permission.application.prompt");

/// The permission keys of a capture kind: (has a permission, can prompt now). `None`
/// for `Output`, which is played to, not captured.
pub fn permission_keys(kind: crate::EndpointKind) -> Option<(CapabilityKey, CapabilityKey)> {
    match kind {
        crate::EndpointKind::Input => Some((PERMISSION_INPUT, PERMISSION_INPUT_PROMPT)),
        crate::EndpointKind::OutputLoopback => {
            Some((PERMISSION_LOOPBACK, PERMISSION_LOOPBACK_PROMPT))
        }
        crate::EndpointKind::Application => {
            Some((PERMISSION_APPLICATION, PERMISSION_APPLICATION_PROMPT))
        }
        crate::EndpointKind::Output => None,
    }
}
