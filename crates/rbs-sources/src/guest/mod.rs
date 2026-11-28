//! The guest source's worker logic (spec `guest-source.md`, ADR-0139 decision 11):
//! the incoming rate limit, the checks of frame times, the sender-to-master clock map
//! and the jitter buffer.
//!
//! All of it runs on the guest source's own worker thread, never on the render or
//! audio thread, and takes time as an argument so tests drive it without a clock.

pub mod bucket;
pub mod checks;
pub mod clock_map;
pub mod handoff;
pub mod jitter;
pub mod node;
pub mod timing;
pub mod worker;
#[cfg(test)]
mod worker_tests;

pub use bucket::TokenBucket;
pub use clock_map::ClockMap;
pub use handoff::{GuestPictures, NodeSink};
pub use jitter::{JitterBuffer, Pushed, Queued};
pub use node::{
    GUEST_SOUND_NODE_TYPE, GUEST_SOURCE_TYPE, GuestControl, GuestDecoders, GuestSource,
    GuestSourceNode, guest_descriptor, guest_source,
};
pub use timing::{Timed, TrackTiming};
pub use worker::{GuestCounts, GuestEnd, GuestSink, GuestWorker, MakeVideoDecoder, WorkerSettings};
