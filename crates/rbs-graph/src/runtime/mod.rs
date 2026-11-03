//! Running a graph: the [`Node`] contract, the real-time [`Executor`] and the
//! off-thread [`Controller`] that builds schedules and owns idle nodes (ADR-0020).
//!
//! - The control thread edits a [`GraphDescription`](crate::description::GraphDescription)
//!   and calls [`Controller::apply`]. That plans the graph, calls [`Node::prepare`] on
//!   nodes joining the schedule (warm-up off the real-time thread), lays out the value
//!   slots and sends the new schedule to the executor.
//! - The executor runs [`Executor::run_tick`] on its thread. After each tick it picks
//!   up a pending schedule, calls [`Node::warm`] on joining nodes, moves the nodes that
//!   stay (with their last video frames) into it and swaps, so the next tick runs the
//!   new graph. The old schedule goes back to the controller, which calls
//!   [`Node::release`] on nodes that left, off the real-time thread.
//! - Node instances move between the two sides; there is no lock and no shared mutable
//!   state, and the steady state and the swap do not allocate on the executor thread.
//!
//! Generic over a [`Runtime`], which supplies the resources nodes need, so this crate
//! never names `wgpu` or an audio API. Spec: `docs/specs/media-graph.md`.

mod controller;
mod executor;
mod restart;
mod schedule;

pub mod param;
pub mod queue;

use std::fmt;

use rbs_core::time::Timestamp;
use rbs_core::timing::Cadence;

pub use controller::{
    ApplyError, ApplyReport, Collected, ControlError, Controller, JoinError, channel,
};
pub use executor::{DEFAULT_FAILURE_LIMIT, Executor, NodeHealth};
pub use restart::{FaultCause, FaultStatus, RestartPolicy, RuntimeEvent};
pub use schedule::{
    AudioOut, Inputs, MAX_BLOCK_FRAMES, OutputError, Outputs, PER_TICK_EVENTS, Rejected,
};

use crate::description::NodeId;
use crate::node::NodeDescriptor;

/// The resources a family of nodes runs with, supplied by the composition root.
///
/// For the render executor `Shared` is the GPU context (used by `prepare` on the
/// control thread) and `Local` holds what the render thread owns, such as the texture
/// pool and the native-surface importer. Tests use `()` for both.
pub trait Runtime: 'static {
    /// Shared with every thread; read-only through `&`.
    type Shared: Send + Sync + 'static;
    /// Owned by the executor thread; lent as `&mut` to `warm` and `process`.
    type Local: Send + 'static;

    /// Called on a thread that is about to run this executor, before its first tick or
    /// swap there: each time a worker thread starts, and on the control thread around a
    /// swap made without a worker. A runtime with thread-bound resources (a COM
    /// apartment, a current CUDA or GL context) binds them here, since workers restart
    /// on new OS threads (ADR-0013, ADR-0030). Not real-time: may allocate and make
    /// system calls. A panic is caught and the executor runs on. On the control thread a
    /// context may already be current there; `attach` saves it and `detach` restores it.
    /// When the engine drops a partition it retires the executor (`Executor::retire`):
    /// attach, cool, detach, then `Local` is dropped. Release thread-bound state in
    /// `detach`; `Local`'s own `Drop` runs unattached (also when a worker thread dies
    /// from a panic outside the executor's catch) and must not assume a bound context.
    fn attach(shared: &Self::Shared, local: &mut Self::Local) {
        let _ = (shared, local);
    }

    /// Called on the same thread after its last tick or swap there, before `Local`
    /// moves to another thread: unbinds what `attach` bound. Not real-time.
    fn detach(shared: &Self::Shared, local: &mut Self::Local) {
        let _ = (shared, local);
    }

    /// Called once per tick before any node runs (for example GPU fence polling and
    /// retiring imported surfaces). Real-time rules apply.
    fn begin_tick(shared: &Self::Shared, local: &mut Self::Local, tick: Tick) {
        let _ = (shared, local, tick);
    }

    /// Called once per tick after every node ran (for example queue submission).
    /// Real-time rules apply.
    fn end_tick(shared: &Self::Shared, local: &mut Self::Local, tick: Tick) {
        let _ = (shared, local, tick);
    }
}

/// A runtime with no resources, for tests and CPU-only graphs.
impl Runtime for () {
    type Shared = ();
    type Local = ();
}

/// One tick of an executor: its index on the cadence and its master-clock time.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tick {
    /// Index on the executor's cadence.
    pub index: u64,
    /// Master-clock time of the tick.
    pub time: Timestamp,
}

impl Tick {
    /// A tick at an explicit time.
    pub const fn new(index: u64, time: Timestamp) -> Self {
        Self { index, time }
    }

    /// Tick `index` of `cadence`; the time is computed from the index, so it never
    /// drifts.
    pub fn of(cadence: &Cadence, index: u64) -> Self {
        Self {
            index,
            time: cadence.time_of(index),
        }
    }
}

/// Why a node call failed. Variants without heap data can be built on the real-time
/// thread without allocating.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeError {
    /// An input the node cannot work with (wrong format, missing frame it requires).
    InvalidInput(&'static str),
    /// A backend call failed (device lost, encoder error).
    Backend(&'static str),
    /// Writing an output failed.
    Output(OutputError),
    /// The node is not configured or its resources are gone.
    NotReady,
    /// A failure with a message; only for `prepare` and `release`, which may allocate.
    Failed(String),
    /// A failure that retrying will not fix (permission denied, a device that is gone
    /// for good). From `prepare` the node is not restarted automatically; from
    /// `process` it is bypassed at once. The user retries it (ADR-0024).
    Permanent(&'static str),
}

impl fmt::Display for NodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NodeError::InvalidInput(m) => write!(f, "invalid input: {m}"),
            NodeError::Backend(m) => write!(f, "backend error: {m}"),
            NodeError::Output(e) => write!(f, "output error: {e}"),
            NodeError::NotReady => f.write_str("node not ready"),
            NodeError::Failed(m) => f.write_str(m),
            NodeError::Permanent(m) => write!(f, "permanent failure: {m}"),
        }
    }
}

impl NodeError {
    /// True if retrying will not help ([`NodeError::Permanent`]).
    pub fn is_permanent(&self) -> bool {
        matches!(self, NodeError::Permanent(_))
    }

    /// A short static description, for counters that must not allocate.
    pub fn summary(&self) -> &'static str {
        match self {
            NodeError::InvalidInput(m) | NodeError::Backend(m) | NodeError::Permanent(m) => m,
            NodeError::Output(_) => "output error",
            NodeError::NotReady => "not ready",
            NodeError::Failed(_) => "failed",
        }
    }
}

impl std::error::Error for NodeError {}

impl From<OutputError> for NodeError {
    fn from(e: OutputError) -> Self {
        NodeError::Output(e)
    }
}

/// Context for [`Node::prepare`] and [`Node::release`], on the control thread.
#[non_exhaustive]
pub struct PrepareCtx<'a, X: Runtime> {
    /// The node's id in the graph.
    pub node: NodeId,
    /// Shared resources.
    pub shared: &'a X::Shared,
}

/// Context for [`Node::warm`] and [`Node::cool`], on the executor thread between ticks.
#[non_exhaustive]
pub struct WarmCtx<'a, X: Runtime> {
    /// The node's id in the graph.
    pub node: NodeId,
    /// Shared resources.
    pub shared: &'a X::Shared,
    /// Resources owned by the executor thread.
    pub local: &'a mut X::Local,
}

/// Context for [`Node::process`]: the tick, resources, the values on the node's inputs
/// and its outputs.
pub struct ProcessCtx<'a, X: Runtime> {
    pub(crate) tick: Tick,
    pub(crate) node: NodeId,
    pub(crate) shared: &'a X::Shared,
    pub(crate) local: &'a mut X::Local,
    pub(crate) inputs: Inputs<'a>,
    pub(crate) outputs: Outputs<'a>,
}

impl<'a, X: Runtime> ProcessCtx<'a, X> {
    /// The tick being processed.
    pub fn tick(&self) -> Tick {
        self.tick
    }

    /// The node's id in the graph.
    pub fn node(&self) -> NodeId {
        self.node
    }

    /// Shared resources.
    pub fn shared(&self) -> &'a X::Shared {
        self.shared
    }

    /// The values on the inputs. `Inputs` is `Copy` and borrows only the producers'
    /// slots, so it can be held while writing outputs.
    pub fn inputs(&self) -> Inputs<'a> {
        self.inputs
    }

    /// The outputs.
    pub fn outputs(&mut self) -> &mut Outputs<'a> {
        &mut self.outputs
    }

    /// Resources owned by the executor thread.
    pub fn local(&mut self) -> &mut X::Local {
        self.local
    }

    /// Outputs and local resources at once (for example a compositor writing into a
    /// pool texture and then publishing it).
    pub fn parts(&mut self) -> (Inputs<'a>, &mut Outputs<'a>, &mut X::Local) {
        (self.inputs, &mut self.outputs, self.local)
    }
}

/// A node instance: a source, filter, compositor, encoder and so on.
///
/// Calls and the thread they run on:
///
/// | Call | Thread | Rules |
/// | --- | --- | --- |
/// | `prepare` | control | May allocate, block and do I/O. Opens devices, builds pipelines. Called when the node joins the schedule, before the swap. |
/// | `warm` | executor, between ticks | May allocate (pool textures), must not block. Called once after `prepare`, before the first `process`. |
/// | `process` | executor, every tick | Real time: no allocation, blocking, I/O or contended locks (P6). |
/// | `cool` | executor, between ticks | Returns what `warm` took (pool textures). Called once when the node leaves the schedule. Must not block. |
/// | `release` | control | Frees what `prepare` took; the node is Standby again and may be prepared again later. |
///
/// Errors are returned, never panicked; a panic in `process` is caught and the node is
/// bypassed (ADR-0020).
pub trait Node<X: Runtime>: Send {
    /// The node type's descriptor; must equal the one the node has in the graph.
    fn descriptor(&self) -> &NodeDescriptor;

    /// Acquires resources and gets ready to produce (warm-up).
    fn prepare(&mut self, ctx: &mut PrepareCtx<'_, X>) -> Result<(), NodeError>;

    /// Acquires executor-thread resources, such as pool textures of a new size, so
    /// that `process` never allocates.
    fn warm(&mut self, ctx: &mut WarmCtx<'_, X>) -> Result<(), NodeError> {
        let _ = ctx;
        Ok(())
    }

    /// Does one tick of work: reads inputs, writes outputs.
    fn process(&mut self, ctx: &mut ProcessCtx<'_, X>) -> Result<(), NodeError>;

    /// Returns executor-thread resources taken in `warm`. Also called after a failed
    /// `warm`, so it must cope with partial warm-up.
    fn cool(&mut self, ctx: &mut WarmCtx<'_, X>) {
        let _ = ctx;
    }

    /// Releases resources; the instance stays usable for a later `prepare`.
    fn release(&mut self, ctx: &mut PrepareCtx<'_, X>);
}
