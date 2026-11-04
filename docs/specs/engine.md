# Spec: Engine loop

- Status: Draft
- Owner crate(s): `rbs-engine`
- Related ADRs: [0037](../adr/0037-audio-graph.md), [0013](../adr/0013-zero-cost-when-unused.md), [0020](../adr/0020-node-contract-and-schedule-swap.md), [0024](../adr/0024-node-restart-with-back-off.md), [0026](../adr/0026-partitions-and-queues.md), [0027](../adr/0027-engine-loop.md), [0029](../adr/0029-final-ticks-on-drain.md), [0030](../adr/0030-worker-thread-setup.md)

## Purpose

Runs a logical media graph split into partitions, each partition's executor on its own worker thread, with the control loop that drives restarts and a shutdown in pipeline order. The composition roots (`rbs-app`, `rbs-cli`) supply runtimes and node instances; the engine names no adapter, GPU or UI crate.

## Scope

In scope: worker threads, pacing, applying lowered descriptions to partitions (including node moves), the restart control loop, shutdown, a non-blocking handle for UI threads.

Out of scope (later): CPU affinity, waitable high-resolution timers, the control API (`rbs-api`). Audio partitions are ordinary `Cadence` partitions at the block rate with `catch_up` (ADR-0037, [audio graph spec](audio-graph.md)).

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
    pub fn apply_draining(&mut self, logical: &GraphDescription) -> Result<Applied, EngineError>; // ADR-0041
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
    pub fn set_thread_policy(&mut self, policy: Option<Arc<dyn ThreadPolicy>>); // ADR-0030
}

pub enum Pacing {
    Cadence { cadence: Cadence, offset: Duration, catch_up: u32 },  // render, encoders (catch_up 0); audio (Pacing::stream)
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

Invariants (tested in `crates/rbs-engine/tests/engine.rs` unless noted):

1. **Zero cost when unused.** A partition has a worker thread exactly while its running schedule has nodes. An engine whose partitions run nothing has no thread, and `service().wait` is `None` when nothing runs and no restart is due. An `EngineHandle` starts its control thread on the first command.
2. **Cadence.** Tick `i` of a `Cadence` worker runs no earlier than `cadence.time_of(i) + offset` on the engine clock and carries `Tick::of(&cadence, i)`. Tick indices never repeat, also across worker restarts. A late worker at most `catch_up` ticks behind runs the next tick, so it runs every due tick in order and counts each one run behind time in `WorkerStats::caught_up` (audio partitions, where a skipped tick is a lost block; `Pacing::stream(cadence, catch_up)`); further behind, or with `catch_up` 0 (video: `Pacing::cadence`, `cadence_after`), it runs only the newest due tick and `WorkerStats::late` grows by the number skipped (`worker.rs` tests `a_stream_worker_*`).
3. **Doorbell.** A `Doorbell` worker ticks after each ring and after `timer` without one (also when nothing ever rings). With a long timer, every tick follows a ring (at most two wake-ups per ring).
4. **Swaps.** After `Engine::apply` returns without `SwapTimeout`, each touched partition runs its new schedule. A cadence worker swaps on wake-up between ticks (`Executor::swap_pending`) and never runs an extra tick to do so (`rbs-graph` test `swap_pending_swaps_between_ticks_without_running_a_tick`).
5. **Moves** follow ADR-0026 Decision 6 in two phases; a partition that gains a moved instance is sent its part again after the move, also when it lost a node in the same edit. The moved instance runs in its new partition; a move to another runtime keeps it in the old one (`MoveWrongRuntime`). A move whose old partition has not swapped in time waits and is finished by `service`.
6. **Retired endpoints** are dropped once idle, on the control thread, also when their swap timed out (then by `service`).
6a. **Unsent parts.** A part not sent because a swap was still in flight is sent by a later `service`; `service().wait` stays bounded while such work is pending.
7. **Restarts.** A node bypassed on a worker is reported (`RuntimeEvent::Failed`) by a later `service` and, after its back-off, prepared again (`Restarted`) without any edit.
8. **Shutdown order.** Partitions drain in pipeline order (topological along queues, ties and cycles by id): every node of a producer partition is released before any node of the partitions it feeds. All workers are joined afterwards; later calls return `EngineError::ShutDown`.
8a. **Final ticks.** Before its empty schedule is swapped in, each running partition runs its final tick(s) on its own worker, on the old schedule: a `Cadence` worker one tick at once (the next free index, which may be up to one period ahead of its time), so an encoder encodes the last frame render wrote; a `Doorbell` worker one tick, then one more for each ring a `PacketReader` left because packets were still queued (at most `MAX_DRAIN_TICKS` = 4096), so every packet in the queue when the output drains is passed on (best effort in a cycle of partitions); an audio queue reader rings after every block it passes on a final tick, so its partition's last final tick passes none (ADR-0045). Final ticks carry `Tick::draining = true` (ADR-0043), and only they do: it tells nodes that this is the end of their streams, so an encoder flushes what it holds back. It is set when the tick is built in the drain branch; the run path is unchanged. Afterwards the worker runs no tick; the empty schedule is swapped (and nodes cooled) on the same worker thread. If the final ticks miss `swap_timeout`, the worker is joined instead and the partition drained on the control thread.
8c. **Draining apply** (ADR-0041). `apply_draining` is `apply`, except that each partition that has a worker and whose new part is empty runs its final tick(s) first, exactly as in 8a, and then gets the empty part (its worker then stops, invariant 1). Parts are sent in pipeline order over the union of the queues before and after the edit, so a partition drains after the partitions that fed it, even though the edit retires the queues between them. A part left unsent by a `SwapTimeout` (an earlier swap of that partition still in flight) keeps its drain: `service` runs the final ticks right before it sends the part, unless a later `apply` gave the partition nodes again (test `a_drain_held_up_by_a_swap_timeout_runs_when_the_control_loop_sends_the_part`). A drain that misses `swap_timeout` is `DrainTimeout`; the worker is joined and the part swapped on the control thread. Partitions that keep nodes are not drained and do not stop. A drained partition takes a schedule again like any idle one (a new worker; tick indices continue, so the index its final tick used ahead of time is skipped). `apply` never runs a final tick (test `apply_without_draining_runs_no_final_tick`; `a_branch_removed_with_apply_draining_drains_while_render_keeps_running`).
8b. **Thread setup** (ADR-0030). With a `ThreadPolicy` set, each worker calls `enter` on its own thread before its first tick (`Periodic { period }` for `Cadence`, `Responsive` for `Doorbell`) and drops the guard on that thread after its last tick; an error or panic leaves the worker at the default class (`WorkerStats::policy_errors`, `thread_grant() == None`). Without a policy nothing is called. Every thread that ticks or swaps an executor calls `Executor::attach` first and `Executor::detach` before the executor leaves it (workers, and the control thread around a swap without a worker); a dropped partition retires its parked executor on the control thread (`Executor::retire`: attach, cool, detach, drop).
9. **Runtime types** are checked: typed calls on a partition of another runtime return `WrongRuntime`.
10. **Observable stops and tick indices.** `WorkerStats::stops` counts the times the control thread asked a worker of the partition to stop, counted before it joins the worker. `WorkerStats::next_index` is the index of the next tick the partition's worker runs: every lower index ran, was skipped late, or passed while no worker ran (a drained worker's final tick taken ahead of its time counts as run). It is `ticks + late` plus a base the worker stores once when it starts, so the tick loop does no extra work. Tests use these (with `starts`, `ticks` and the tick indices a runtime sees in `begin_tick`/`end_tick`) to react to engine steps instead of timing them (ADR-0032).

Thread rules: every `Engine` method runs on the control thread and may block up to `EngineConfig::swap_timeout` per wait for a worker (`shutdown` waits up to twice per partition: final ticks, then the swap), plus as long as a node hung in `process` takes to return. `EngineHandle` methods never block (they queue a command); its `EventSink` runs on the control thread and must not block. Dropping an `EngineHandle` shuts down and joins.

## Errors and failure behaviour

- `EngineError` reports per-partition problems inside `Applied::errors`, `Serviced::errors` and `Shutdown::errors`; one partition's failure does not stop the others. `Engine::apply` returns `Err` only for `UnknownPartition` (nothing applied) and `ShutDown`.
- Final ticks that do not finish within `swap_timeout` are reported (`DrainTimeout`); the worker is joined (waiting for a node still in `process`) and the partition is drained on the control thread.
- A swap that does not complete within `swap_timeout` is reported (`SwapTimeout`); `service` collects it and finishes what waited for it (unsent parts, moves, retired endpoints). A worker thread that cannot start is reported (`Spawn`) and tried again on the next apply. A drain that times out at shutdown is finished after the worker is joined: the nodes are cooled on the control thread and released.
- `EngineHandle`: a panicking `run` job is reported (`EngineError::JobPanicked`) and the engine keeps running; a panicking `EventSink` ends the control thread. A node hung in `process` blocks shutdown (and so `EngineHandle`'s drop) until it returns.
- `X::Local` and node instances move to a new OS thread whenever a partition's worker is stopped and started again (ADR-0013). A runtime that needs a thread-bound context (COM apartment, CUDA current context) binds it in `Runtime::attach` and unbinds it in `Runtime::detach`, which run on each thread before it ticks or swaps and before the executor leaves it (ADR-0030). Node instances themselves must not be thread-affine.
- Node failures are contained by the executor and controller (ADR-0020, ADR-0024); the worker loop itself never panics on node errors.

## Performance requirements

- The worker loop (hot path) does per iteration: one atomic load of the worker's mode (run, drain, held, stop; two on a doorbell worker, before and after its wait), clock reads, a `park_timeout` or `Doorbell::wait`, `Executor::swap_pending` (one `try_recv` when nothing is pending) and `run_tick`. No allocation or I/O. Cadence workers take no lock; `Doorbell::wait` takes the doorbell's registration lock, contended at most by a ringer's `try_lock` around one `unpark` (output workers only).
- Measured with `cargo bench -p rbs-engine --bench cross_thread` (numbers in ADR-0027): packet hand-off encoder to output across threads p50 36 us; render to output at 60 fps with a quarter-frame encoder offset mean 4.4 ms.

## Conformance tests

`crates/rbs-engine/tests/engine.rs`; the allocation guard for the worker loop (both pacings, run on the test thread) in `crates/rbs-engine/src/worker.rs`; the lowering property `crates/rbs-graph/tests/partition_plan.rs` (the parts' plans together equal the logical plan over random graphs and edits).

## Open questions

- A move overtaken by a second move of the same node before its first finished (A to B pending after a timeout, then B to C) is dropped and the second reports `MoveNotIdle`; the node is applied again on the next edit that touches it. Tracking where each instance actually is would close this.
- A non-blocking `apply` state machine if a control-thread caller ever cannot wait.
