//! The audio input port of RBS: microphones and line inputs, output loopback (what an
//! output device plays) and per-application capture.
//!
//! Every OS audio API is an adapter implementing [`AudioBackend`] and
//! [`AudioInputStream`]. Adapters declare what they can do with the keys in [`caps`]
//! and prove it with the suite in `conformance` (feature `conformance`). Contract:
//! `docs/specs/audio-io-port.md`. Decision: `docs/adr/0033-audio-input-port.md`.
//!
//! [`ring`] and [`edge`] are adapter-support modules: the lock-free sample ring that
//! carries samples from the OS callback to the engine's audio thread, and the clock
//! edge that stamps them in master time. Every adapter uses them, so the real-time
//! rules are implemented and tested once.
//!
//! This crate is a port: it depends only on `rbs-core` and never on an OS API or an
//! adapter (principle P1).

pub mod caps;
#[cfg(feature = "conformance")]
pub mod conformance;
pub mod edge;
mod endpoint;
mod error;
pub mod output;
pub mod ring;
mod stream;

pub use endpoint::{AudioEndpoint, EndpointId, EndpointKind, Permission};
pub use error::{AudioIoError, ErrorDomain};
pub use output::{AudioOutput, OutputConsumer, OutputProducer, OutputRequest, OutputStats};
pub use stream::{
    AudioBackend, AudioInputStream, AudioRead, DEFAULT_BUFFER, InputRequest, InputStats,
    MAX_BUFFER, MIN_BUFFER,
};
