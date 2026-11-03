//! The RBS engine loop (ADR-0027): runs a logical media graph split into partitions
//! (ADR-0026), each partition's executor on its own worker thread.
//!
//! - [`Engine`]: control-thread API. Add partitions with their runtime and
//!   [`Pacing`], adopt node instances, [`Engine::apply`] the logical graph, call
//!   [`Engine::service`] on the control loop's timer for restarts (ADR-0024), and
//!   [`Engine::shutdown`] to drain in pipeline order.
//! - [`EngineHandle`]: the same engine on its own control thread, for UI threads that
//!   must never block; events arrive through a callback.
//! - Workers: render and encoder partitions tick at frame cadence, output partitions
//!   on their doorbell plus a low-rate timer. A partition has a thread only while its
//!   schedule has nodes (ADR-0013).
//!
//! This crate names no adapter, GPU or UI crate; the composition roots (`rbs-app`,
//! `rbs-cli`) supply the runtimes and node instances. Spec: `docs/specs/engine.md`.

mod engine;
mod handle;
mod part;
mod worker;

pub use engine::{Applied, Engine, EngineConfig, EngineError, Serviced, Shutdown};
pub use handle::{EngineEvent, EngineHandle, EventSink};
pub use worker::{DEFAULT_DOORBELL_TIMER, Pacing, WorkerStats};
