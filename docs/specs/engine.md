# Spec: Engine loop

- Status: Draft
- Owner crate(s): `rbs-engine`
- Related ADRs: [0013](../adr/0013-zero-cost-when-unused.md), [0020](../adr/0020-node-contract-and-schedule-swap.md), [0024](../adr/0024-node-restart-with-back-off.md), [0026](../adr/0026-partitions-and-queues.md), [0027](../adr/0027-engine-loop.md)

## Purpose

Runs a logical media graph split into partitions, each partition's executor on its own worker thread, with the control loop that drives restarts and a shutdown in pipeline order. The composition roots (`rbs-app`, `rbs-cli`) supply runtimes and node instances; the engine names no adapter, GPU or UI crate.

## Scope

In scope: worker threads, pacing, applying lowered descriptions to partitions (including node moves), the restart control loop, shutdown, a non-blocking handle for UI threads.

Out of scope (later): thread priorities and real-time classes, high-resolution timers, CPU affinity, audio partitions, the control API (`rbs-api`).

## Contract

```rust
pub struct Engine;                  // control-thread API
impl Engine {
    pub fn new() -> Self;           // MonotonicClock, EngineConfig::default()
    pub fn with_clock(clock: Arc<dyn Clock>, config: EngineConfig) -> Self;
    pub fn add_partition<X: Runtime>(&mut self, id: PartitionId, shared: Arc<X::Shared>,
        local: X::Local, pacing: Pacing) -> Result<(), EngineError>;
    pub fn adopt<X: Runtime>(&mut self, p: PartitionId, id: NodeId, node: Box<dyn Node<X>>)
        -> Result<(), EngineError>;
    pub fn take<X: Runtime>(&mut self, p: PartitionId, id: NodeId)
        -> Result<Option<Box<dyn Node<X>>>, EngineError>;
    pub fn apply(&mut self, logical: &GraphDescription) -> Result<Applied, EngineError>;
    pub fn service(&mut self) -> Serviced;     // one round of the control loop
    pub fn shutdown(&mut self) -> Shutdown;    // idempotent; also on drop
    pub fn pipeline_order(&self) -> Vec<PartitionId>;
    pub fn crossings(&self) -> &[Crossing];
    pub fn retry(&mut self, id: NodeId) -> bool;
    pub fn fault(&self, id: NodeId) -> Option<FaultStatus>;
    pub fn set_restart_policy(&mut self, p: PartitionId, policy: RestartPolicy) -> Result<(), EngineError>;
    pub fn worker_stats(&self, p: PartitionId) -> Option<Arc<WorkerStats>>;
    pub fn has_worker(&self, p: PartitionId) -> bool;
    pub fn workers(&self) -> usize;
    pub fn wake(&self, p: PartitionId);
    pub fn set_packet_capacity(&mut self, packets: usize);
}

pub enum Pacing {
    Cadence { cadence: Cadence, offset: Duration },  // render, encoders
    Doorbell { timer: Duration },                    // outputs
}

pub struct EngineHandle;            // non-blocking, owns the engine on "rbs-control"
impl EngineHandle {
    pub fn new(engine: Engine, events: EventSink) -> Self;
    pub fn apply(&self, logical: GraphDescription) -> Result<(), EngineError>;
    pub fn run(&self, job: impl FnOnce(&mut Engine) + Send + 'static) -> Result<(), EngineError>;
    pub fn shutdown(&self) -> Result<(), EngineError>;
}
pub type EventSink = Box<dyn FnMut(EngineEvent) + Send>;
pub enum EngineEvent { Applied(Applied), ApplyFailed(EngineError),
    Runtime(PartitionId, RuntimeEvent), Error(EngineError), ShutDown(Shutdown) }
```

Invariants (each has a test in `crates/rbs-engine/tests/engine.rs` unless noted):

1. **Zero cost when unused.** A partition has a worker thread exactly while its running schedule has nodes. An engine whose partitions run nothing has no thread, and `service().wait` is `None` when nothing runs and no restart is due. An `EngineHandle` starts its control thread on the first command.
2. **Cadence.** Tick `i` of a `Cadence` worker runs no earlier than `cadence.time_of(i) + offset` on the engine clock and carries `Tick::of(&cadence, i)`. Tick indices never repeat, also across worker restarts. A late worker runs only the newest due tick; `WorkerStats::late` grows by the number skipped.
3. **Doorbell.** A `Doorbell` worker ticks after each ring and after `timer` without one. With a long timer, every tick follows a ring (at most two wake-ups per ring).
4. **Swaps.** After `Engine::apply` returns without `SwapTimeout`, each touched partition runs its new schedule. A cadence worker swaps on wake-up between ticks (`Executor::swap_pending`) and never runs an extra tick to do so (`rbs-graph` test `swap_pending_swaps_between_ticks_without_running_a_tick`).
5. **Moves** follow ADR-0026 Decision 6 in two phases; the moved instance runs in its new partition, and a move to another runtime keeps it in the old one (`MoveWrongRuntime`).
6. **Retired endpoints** are dropped once idle, on the control thread.
7. **Restarts.** A node bypassed on a worker is reported (`RuntimeEvent::Failed`) by a later `service` and, after its back-off, prepared again (`Restarted`) without any edit.
8. **Shutdown order.** Partitions drain in pipeline order (topological along queues, ties and cycles by id): every node of a producer partition is released before any node of the partitions it feeds. All workers are joined afterwards; later calls return `EngineError::ShutDown`.
9. **Runtime types** are checked: typed calls on a partition of another runtime return `WrongRuntime`.

Thread rules: every `Engine` method runs on the control thread and may block up to `EngineConfig::swap_timeout` per partition. `EngineHandle` methods never block (they queue a command); its `EventSink` runs on the control thread and must not block. Dropping an `EngineHandle` shuts down and joins.

## Errors and failure behaviour

- `EngineError` reports per-partition problems inside `Applied::errors`, `Serviced::errors` and `Shutdown::errors`; one partition's failure does not stop the others. `Engine::apply` returns `Err` only for `UnknownPartition` (nothing applied) and `ShutDown`.
- A swap that does not complete within `swap_timeout` is reported (`SwapTimeout`) and collected by a later `service`. A worker thread that cannot start is reported (`Spawn`) and tried again on the next apply.
- Node failures are contained by the executor and controller (ADR-0020, ADR-0024); the worker loop itself never panics on node errors.

## Performance requirements

- The worker loop (hot path) does per iteration: one atomic load, clock reads, a `park_timeout` or `Doorbell::wait`, `Executor::swap_pending` (one `try_recv` when nothing is pending) and `run_tick`. No allocation, lock or I/O.
- Measured with `cargo bench -p rbs-engine --bench cross_thread` (numbers in ADR-0027): packet hand-off encoder to output across threads p50 36 us; render to output at 60 fps with a quarter-frame encoder offset mean 4.4 ms.

## Conformance tests

`crates/rbs-engine/tests/engine.rs`; the lowering property `crates/rbs-graph/tests/partition_plan.rs` (the parts' plans together equal the logical plan over random graphs and edits).

## Open questions

- Thread priorities, real-time classes and high-resolution timers per platform.
- Whether output drains should pass every queued packet before release (today: one tick's worth).
- A non-blocking `apply` state machine if a control-thread caller ever cannot wait.
