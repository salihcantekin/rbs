//! Shared session logic for hardware video encoder adapters (NVENC, QSV, AMF,
//! VA-API): everything that is the same for every vendor and needs no driver.
//!
//! - [`device`]: what a probed device encodes ([`DeviceCaps`](device::DeviceCaps)) and
//!   the capability set an adapter declares from it.
//! - [`plan`]: validates a [`VideoEncoderConfig`](rbs_codec::VideoEncoderConfig)
//!   against a device and maps it to vendor-neutral settings that each adapter turns
//!   into its own parameters.
//! - [`session`]: the encoder port contract (ordering, keyframes, `Busy`, refusals,
//!   errors) over a small [`Engine`](session::Engine) trait that each adapter
//!   implements with its driver calls.
//! - [`copy`]: the row-by-row copy of a system-memory NV12 or P010 frame into a
//!   driver-owned buffer.
//!
//! Adapters keep only their loader, bindings and engine. Because the session is plain
//! Rust, the port conformance suite runs here against a fake engine on every CI
//! runner, with no GPU (see `docs/specs/output-ports.md`, "Hardware encoder sessions").
//!
//! This crate depends on the encoder port and format helpers only, never on an
//! adapter or vendor library.

pub mod copy;
pub mod device;
pub mod plan;
pub mod session;

/// Counters of one session, for metrics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct SessionStats {
    /// Frames handed to the encoder.
    pub submitted: u64,
    /// Packets moved out by `drain` or `flush`.
    pub packets: u64,
    /// System-memory frames copied into an encoder input buffer.
    pub copied: u64,
}
