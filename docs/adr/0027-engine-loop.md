# ADR-0027: Engine loop: a worker thread per partition in a headless rbs-engine crate

- Status: Proposed
- Date: 2026-11-03
- Deciders: Salih (owner)
- Amends: [ADR-0026](0026-partitions-and-queues.md) Decision 9 and [ADR-0024](0024-node-restart-with-back-off.md) Consequences, which place the engine loop in `rbs-app`; it lives in `rbs-engine` instead.

## Context

ADR-0026 split the graph into partitions joined by queues and left the worker loop to "the engine in `rbs-app`" (Decision 9): threads, cadence, doorbell waits, restart checks, adopting the lowered endpoints and the shutdown order. ADR-0024 (Decision 9) likewise leaves its control loop (`collect`, `restart_due`, `apply`, sleep for `next_due`) to the engine.

Forces:

- **`rbs-app` is the Slint desktop shell** (PR #38, `rust-version` 1.92 for Slint). `rbs-cli` and tests need the same loop without a window, and ADR-0008 makes UIs clients of the engine.
- **The UI thread must never block** (U-rules, agreed with the UI thread): the shell needs calls that return at once and a way to hear about state changes.
- **P13 / ADR-0013:** an engine with nothing running must start no thread. The idle shell measures 0 % CPU and 0 context switches.
- **P6 / P15:** the loop around `Executor::run_tick` runs every frame.
- **Partitions may use different `Runtime`s** (a GPU runtime for render, a CPU one for outputs). Node instances can only move between partitions of the same runtime.
- **Tests must not sleep for timing** and use `TestClock`.

## Decision

1. **A new crate `rbs-engine`** (Engine layer) holds the loop. It depends on `rbs-core` and `rbs-graph` only and is generic over `Runtime`, so it names no GPU, codec, adapter or UI crate. `rbs-app` and `rbs-cli` stay the composition roots: they add partitions with their runtimes, adopt node instances and call the engine.
2. **One worker thread per partition, only while it runs something.** `Engine::add_partition::<X>(id, shared, local, pacing)` creates the controller and executor; no thread starts. A worker starts when a schedule with nodes is applied and stops (joined, executor kept) once the partition's running schedule is empty. An empty schedule sent to a partition without a worker is swapped on the control thread with `Executor::swap_pending`: no tick, no `begin_tick`/`end_tick`, no tick index used. Because the executor and `X::Local` move to a new thread when a worker restarts, `Local` and node instances must not be thread-affine (no attach hook yet).
3. **Pacing.**
   - `Pacing::Cadence { cadence, offset }`: tick `i` runs at `cadence.time_of(i) + offset` and carries `cadence.time_of(i)`. A late worker runs the newest due tick only and counts the skipped ones (`WorkerStats::late`). The render executor uses offset zero; an encoder on the render cadence uses an offset (a quarter frame is the default advice), so it reads the frame the render tick just wrote. Between ticks the worker parks for the time left.
   - `Pacing::Doorbell { timer }`: output workers wait on their partition's `Doorbell` with `timer` as the low-rate heartbeat (default 100 ms) and tick after each wait.
   - The worker loop reads a stop flag and the clock per iteration and never allocates, locks or does I/O.
4. **Prompt swaps without extra ticks.** `Executor::swap_pending()` (new in `rbs-graph`) applies a pending schedule between ticks without running one, the same swap `run_tick` does after its nodes. A cadence worker calls it whenever it wakes before its next tick, and the control thread wakes it (unpark) after an `apply`. A doorbell worker is rung and ticks. So an `apply` never waits for the next frame and never adds an off-cadence tick.
5. **Applying a logical graph** (`Engine::apply`), on the control thread, following ADR-0026 Decision 6:
   - every node's partition must have been added, else nothing is applied (`UnknownPartition`);
   - lower; adopt each new queue's writer and reader under the reported ids;
   - phase 1: apply and settle the parts of the partitions that moved nodes leave; move each idle instance (`take`, `adopt`; a move to a partition of another runtime keeps the instance where it was and reports `MoveWrongRuntime`); phase 2: apply and settle, in pipeline order, every part not sent yet, including the part of a phase-1 partition that gained a moved instance;
   - work a timed-out swap holds up is kept, not dropped: a part that could not be sent stays pending, a move whose instance is still in the old schedule waits, and a retired endpoint still inside the executor stays listed. `service` finishes all three once the swap is collected;
   - take back and drop the endpoints of retired queues once idle;
   - report per-partition `ApplyReport`s, runtime events, new queues (their stats for encoders), moves, lowering problems and errors. One partition's error does not stop the others.
   - "Settle" waits up to `EngineConfig::swap_timeout` (default 1 s) for the swap; a timeout is reported and collected later.
6. **Control loop.** `Engine::service()` collects every partition, finishes pending moves, sends each part that is pending or whose `restart_due()` is true, and returns `wait`: the smallest `next_due()`, capped by `EngineConfig::control_interval` (default 250 ms) while any worker runs or any fault, pending part, move or retired endpoint remains, and `None` when nothing runs and nothing is due (sleep until the next edit). Bypasses are seen at the latest one interval later.
7. **Shutdown in pipeline order** (ADR-0026 Decision 8). The pipeline order is a topological order of partitions along queue direction (ties and cycles by id), so render, then encoders, then outputs, whatever the ids. `Engine::shutdown()` drains each partition in that order (empty schedule, settle), stops its worker, finishes a drain that timed out on the control thread (cool, release), then drops all endpoints. Drains swap between ticks, so a cadence partition's last frame is not encoded and an output passes at most one tick's packets before release. It is idempotent and runs on drop; a node hung in `process` blocks it.
8. **`EngineHandle` for UI threads.** It owns the engine on an `rbs-control` thread that starts with the first command (an unused handle has no thread; once started, the thread stays parked on its command channel while nothing runs, an accepted cost). A panicking job is reported and the engine keeps running. `apply(description)`, `run(|engine| ...)` and `shutdown()` only queue a command and return at once. An `EventSink` callback (`FnMut(EngineEvent) + Send`) receives `Applied`, `ApplyFailed`, `Runtime(partition, event)`, `Error` and `ShutDown`; a UI forwards them to its event loop. The control thread sleeps on its command channel for `service().wait` (forever when `None`). Dropping the handle shuts down and joins.
9. **Packet readers ring their own doorbell when they leave packets queued** (`rbs-graph`, queue endpoints): at most `PER_TICK_EVENTS` pass per tick, and without the ring the rest would wait for the writer's next ring or the heartbeat.
10. **Not in this change:** thread priorities and OS real-time classes (MMCSS, `THREAD_TIME_CONSTRAINT_POLICY`, rtkit), high-resolution timers, CPU affinity, partitions for audio, and per-link capacities. They are additive to `Pacing` and the worker.

## Consequences

- `rbs-app` and `rbs-cli` share one loop; the shell calls `EngineHandle` from the UI thread and never blocks. ADR-0026's "helper in `rbs-app`" is `rbs_engine::Engine`.
- An engine with nothing scheduled has no thread and the control loop has no timer (tests `an_engine_that_runs_nothing_has_no_threads`, and the encoder partition's worker stops after its node moves away in `an_encoder_moves_to_the_output_partition_in_two_phases`).
- Output workers wake on packets only (test `frames_and_packets_cross_three_worker_threads` uses an hour-long heartbeat).
- Restarts happen without edits (test `a_bypassed_node_restarts_through_the_control_loop`).
- **Measured** (`cargo bench -p rbs-engine --bench cross_thread`, reference VM: 4 vCPU Intel Xeon 2.1 GHz, Linux, release):
  - encoder `process` to output `process` through the packet queue and doorbell, 1 kHz: mean 51 us, p50 36 us, p99 180 us;
  - render tick time to output `process` at 60 fps: with no encoder offset the latency is bimodal (p50 0.3 ms, p99 17.0 ms, mean 7.1 ms) because render and encoder wake at the same instant in either order; with a quarter-frame offset it is steady (mean 4.4 ms, p99 4.9 ms, max 5.8 ms). Hence the offset advice in Decision 3.
  - `rbs-graph` executor and queue micro-benchmarks, main (separate target directory) against this change (rbs-graph rebuilt): `executor_tick` 6/20/68 nodes 110/424/1607 ns against 104/408/1524 ns, `queue_crossing` one/three partitions 59/281 ns against 54/251 ns. No regression; the differences are within this VM's run-to-run noise (main against itself moved up to +10 %). The tick loop code is unchanged.
- The control thread may block up to `swap_timeout` per partition in `Engine::apply` and `shutdown`. Only `EngineHandle` is safe on a UI thread.
- `park_timeout` precision depends on the OS timer (tens of microseconds to about a millisecond). A late tick is counted, not hidden. High-resolution timers come with thread priorities.
- Packets still queued when an output partition is drained beyond one tick's `PER_TICK_EVENTS` are dropped with its reader. Outputs finish their transports in `cool` and `release` (ADR-0026 Decision 8).

## Alternatives considered

- **The loop inside `rbs-app`** (as ADR-0026 Decision 9 worded it). `rbs-cli` would pull in Slint and its MSRV, and tests would need the window crate. Rejected; agreed with the UI thread.
- **Long-lived worker threads per added partition.** Simpler, but every Installed partition would keep a parked thread, against ADR-0013.
- **A blocking `Engine` API only.** The UI thread would block for up to a frame per partition on each edit.
- **An `apply` state machine that never blocks the control thread.** It avoids `swap_timeout`, but makes the two-phase move order and retired endpoints asynchronous across calls for little gain: the control thread is not real time, and `EngineHandle` already keeps the UI free.
- **Waiting for the next cadence tick to swap** (no `swap_pending`). Every edit would wait up to a frame per partition, and tests with `TestClock` would need a thread driving time.
- **Run an extra tick on wake to swap.** That adds an off-cadence tick with a repeated or skipped timestamp.
- **Encoder woken by the render thread.** Rejected in ADR-0026 (an unpark per frame on the render thread); the cadence offset gives the same latency without it.

## Review

Independent review per ADR-0014: pending.
