//! Muxer port: the contract every container muxer adapter implements.
//!
//! A muxer turns encoded packets of one or more streams into [`MuxChunk`]s of a
//! container byte stream (fragmented MP4, Matroska, MPEG-TS, FLV, ...). Transports
//! deliver the chunks (`rbs-transport`). Adapters implement [`MuxerFactory`], declare
//! the keys in [`caps`], and run `conformance::muxer` (feature `conformance`) in their tests.
//!
//! Contract: `docs/specs/output-ports.md`. Decision: ADR-0019. Depends only on
//! `rbs-core` (principle P1).
//!
//! [`MuxChunk`]: rbs_core::media::MuxChunk

pub mod caps;
#[cfg(feature = "conformance")]
pub mod conformance;
pub mod error;
pub mod muxer;

pub use error::{CreateError, MuxError};
pub use muxer::{Muxer, MuxerConfig, MuxerFactory};
