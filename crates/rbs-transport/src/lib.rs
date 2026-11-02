//! Transport port: the contract every output delivery adapter implements (files,
//! RTMP/RTMPS, SRT, WHIP, ...).
//!
//! A transport takes either encoded packets (protocols with their own framing, such
//! as RTMP) or the chunks of a muxer (SRT with MPEG-TS, files), as declared in the
//! `transport.input` capability (ADR-0019). Sending never blocks: adapters queue into
//! a bounded queue they own and run their network work on threads or runtimes they
//! start in [`Transport::start`] and stop in [`Transport::finish`] or `Drop`.
//!
//! Contract: `docs/specs/output-ports.md`. Depends only on `rbs-core` (principle P1).

pub mod caps;
pub mod config;
#[cfg(feature = "conformance")]
pub mod conformance;
pub mod error;
pub mod transport;

pub use config::{QueueLimits, ReconnectPolicy, Secret, TransportConfig, TransportInput};
pub use error::{CreateError, TransportError};
pub use transport::{
    DropReason, SendOutcome, Transport, TransportEvent, TransportFactory, TransportItem,
    TransportStats,
};
