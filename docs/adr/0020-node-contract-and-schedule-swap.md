# ADR-0020: Node contract and lock-free schedule swap

- Status: Accepted
- Date: 2026-11-02
- Deciders: Salih (owner)

## Context

`rbs-graph` can describe and plan a graph ([media graph spec](../specs/media-graph.md)), but nothing runs it. The GPU compositor (`rbs-gpu`), capture sources, encoders and outputs are being written in parallel and all need one contract for "a node" and one answer to how a live edit reaches the real-time threads.

Forces:

- Real time (P6, hot paths): a tick must not allocate, block or take a contended lock, including the tick on which a live edit takes effect.
- Live editing without stutter (media graph spec): nodes that join are warmed up before they run; nodes that leave are torn down off the real-time thread.
- Zero cost when unused (P13, ADR-0013): a node the plan skips must not hold resources, but the description must still remember that the scene wants it, so it can come back. The previous spec draft had the engine rewrite skipped nodes to Standby in the description, which loses that intent (open question in the spec).
- Layers (P1): `rbs-graph` must not name `wgpu` or an audio API, yet compositor nodes need the GPU context in `prepare` and the render thread's texture pool in `process`. `rbs-gpu` reports that pool acquisition of a new size allocates GPU memory and must happen on the render thread before the node's first tick, and that `GpuContext::poll` and native-surface retirement run once per tick.
- No panics across boundaries (rule 8): a failing or panicking node must not take down the canvas.

## Decision

1. **Node trait.** `rbs_graph::runtime::Node<X: Runtime>` has `descriptor`, `prepare`, `warm`, `process`, `cool`, `release`:
   - `prepare` / `release` run on the control thread and may allocate, block and do I/O (open devices, build pipelines).
   - `warm` / `cool` run on the executor thread between ticks, may allocate but must not block (pool textures of a new size, `Compositor::reserve_layers`).
   - `process` runs every tick under real-time rules and returns `Result<(), NodeError>`.
2. **Runtime parameter.** `trait Runtime { type Shared: Send + Sync; type Local: Send; fn begin_tick; fn end_tick }`. The composition root picks the types (for the render executor: `GpuContext` as `Shared`, texture pool and importer as `Local`, `poll` and retirement in `begin_tick`). `rbs-graph` stays free of GPU and audio crates, and tests use `()`.
3. **Ownership instead of locks.** A `Controller` (control thread) and an `Executor` (real-time thread) are created as a pair. Node instances are owned by exactly one side at a time: idle (Standby, no resources) in the controller, or scheduled (Active) in the executor's schedule. They move inside schedules through two bounded channels of capacity 1; no node is ever shared.
4. **Swap protocol.**
   - `Controller::apply(description)` plans, calls `prepare` on joining nodes, lays out value slots and sends the schedule. One schedule may be in flight; a second `apply` returns `Busy` until `collect`.
   - After a tick, the executor takes the pending schedule: it moves staying nodes, their health counters and their last video frames into it, calls `cool` on leaving nodes and `warm` on joining ones, and swaps. The next tick runs the new graph.
   - The old schedule goes back to the controller, which calls `release` on the nodes that left and keeps them idle. Old slots, and the frames in them, are dropped there.
   - The executor's own tick and swap work allocates nothing (enforced by a test). What `warm` and `cool` do is their own cost and lands in the slack after the swapping tick; nodes keep it small by doing slow work in `prepare` (see Consequences).
   - Shutdown: `Controller::drain` sends an empty schedule; after the next tick every node is cooled, and `collect` releases them. Dropping an executor without draining is a bug; as a last resort its `Drop` still cools warmed nodes, but `release` never runs.
5. **Wanted versus effective state.** The lifecycle state in the `GraphDescription` is what the scene wants and is never rewritten by the engine. A node runs exactly when it is in the applied plan. A wanted-Active node that the plan skips, or whose `prepare` failed, is idle with its resources released. It joins again, through `prepare` and `warm`, as soon as a later `apply` schedules it.
6. **Values between nodes.**
   - Slots are laid out in schedule order, so a node reads its producers' slots and writes its own through one `split_at_mut`.
   - Video is state: a slot keeps the latest frame until replaced, so a slower or failing producer shows its last frame.
   - Packets and bytes are per-tick events in buffers of fixed capacity (`PER_TICK_EVENTS`); a push beyond it returns the value.
   - Links into a many-input are presented in connection order.
   - Audio links are refused by this executor (`UnsupportedLink`); the audio executor uses the same `Node` trait with pre-allocated planar buffers and is specified with `rbs-audio`. Data links are event-driven and not carried by the tick.
7. **Failures.**
   - `process` is called inside `catch_unwind`. An error counts as a failed tick; after `failure_limit` consecutive failures (default 8), or on any panic, the node is bypassed: it is no longer called, its video outputs keep the last frame and its event outputs stay empty.
   - A failed `warm` bypasses the node and calls `cool` once; the node is not cooled again when it leaves. A failed or panicking `prepare` calls `release` and leaves the node idle, and the plan is recomputed without it (`JoinError` in the report).
   - `begin_tick` and `end_tick` also run inside `catch_unwind`. The panic path itself is not real-time (the panic hook may allocate and lock), and `Local` must stay usable after a node panicked while using it (for the texture pool: every operation leaves it consistent before it can panic). Builds use `panic = "unwind"`; with `abort` a node panic ends the process.
   - A bypassed node stays prepared until it leaves the plan. Every `collect` returns the counters of all nodes of the swapped-out schedule, so the control side sees bypassed nodes. Restart with back-off, using those counters, and the event stream report follow in the next change; until then a failed `prepare` is retried on every `apply`, so a `prepare` that blocks makes edits slow, and restart policy must remember failures.
8. **Partitions.** One executor runs one partition. The render partition is one executor per canvas tick; encoders and outputs get their own executors; cross-partition links become bounded queues with the per-output drop policy of the spec. Splitting a plan into partitions and the queues are the next change. Sketch: the planner cuts the plan at partition boundaries; the producing side gets a queue-writer endpoint that counts as a sink for `plan()` pruning, and the consuming executor's schedule starts with a queue-reader endpoint whose slot is filled before its first node runs, so `Inputs` and `Outputs` stay as they are. Until then encoders and outputs run in the render tick, so their `process` may only hand work to their own workers through queues that never block.
9. **Thread affinity.** `prepare` runs on the control thread, `warm` and `process` on the executor thread. Resources bound to the thread that uses them (some capture and device APIs) are created in `warm`, which must not block, or on a worker owned by the node.
10. **Semver.** All types are new public API. Struct and enum types that will grow are `#[non_exhaustive]`; new `Node` and `Runtime` methods are added with default bodies, so existing nodes keep compiling. `Collected` and `ApplyReport` are structs so fields can be added.

## Consequences

- Capture, compositor, encoder and output threads can implement `Node` now; `rbs-gpu` wires its context and pool through `Runtime` without `rbs-graph` depending on it.
- Live edits cost one `apply` on the control thread (about 56 µs for a 32-source canvas, see the spec) and nothing extra on the tick.
- Measured tick overhead is about 25 ns per scheduled node on the reference VM; the bench lives in `crates/rbs-graph/benches/executor.rs`.
- The control thread must call `collect` after each swap; until it does, edits wait. The engine's control loop does this on a short timer.
- `Box<dyn Node>` means one virtual call per node per tick, which the numbers above include.
- Nodes must cope with `warm` being called on the render thread: long warm-ups belong in `prepare`. Many nodes joining at once (a scene switch adding ten GPU layers) all warm in one tick's slack; `rbs-gpu` measures a realistic `warm` and, if it does not fit, the executor limits warm-ups per tick (a joining node stays out of `process` until warmed). This is tracked as a follow-up.
- A joining node has no output on its first tick, so a consumer sees an empty input for one tick; a readiness signal ("first frame ready before the swap") is a follow-up.
- Dropping a `VideoFrame` in `process` calls `LeaseOwner::release` on the executor thread, so lease owners must release without locks (already required by the media types spec).
- A video frame written during a failed tick is kept (it is a valid frame); events from that tick are discarded.

## Alternatives considered

- **Nodes behind `Arc<Mutex<_>>` shared by both sides.** Simple, but takes a lock per node per tick and lets the control thread stall the render thread.
- **Recreate node instances on every edit.** No ownership transfer needed, but loses node state (decoder position, capture session) and makes every edit a stutter.
- **Rewrite skipped nodes to Standby in the description** (previous spec draft). Loses what the scene wants, so a node demoted for lack of a sink could not be found again.
- **A generic GPU handle type in `rbs-core`.** Would put a GPU abstraction into the foundation layer for one consumer; an associated type on `Runtime` keeps it at the composition root.
- **A slab of nodes owned only by the executor, driven by commands, with an `ArcSwap` or triple-buffered schedule.** Needs no `collect` step and has no `Busy` state, but either the executor applies commands (allocation and unbounded work on the real-time thread) or nodes are shared between threads again. Moving whole schedules keeps every allocation and every `prepare`/`release` on the control thread; `Busy` lasts at most one tick.
- **Clearing video slots every tick.** Simpler, but a source at a lower frame rate than the canvas, or a failing one, would blink.

## Review

Independent AI session per ADR-0014, 2026-11-02 (got only the proposal, specs and code). Verdict: changes requested; all concerns addressed in the same change:

1. Blocking: a node was cooled twice after a failed `warm`. Fixed with a per-node `warmed` flag; test `a_node_that_failed_warm_is_not_cooled_again_when_it_leaves`.
2. Blocking: no shutdown protocol. Added `Controller::drain` and a last-resort `cool` in `Executor::drop`; test `drain_cools_and_releases_every_node_before_shutdown`.
3. The swap was claimed allocation-free although `warm`/`cool` may allocate. Wording changed (Decision 4); per-tick warm-up limit recorded as a follow-up under Consequences.
4. Bypassed nodes were invisible to the controller. `Collected::health` now reports every node at each swap (test `collect_reports_health_of_nodes_that_stay`); that bypassed nodes keep `prepare` resources is stated in Decision 7.
5. Inconsistent panic handling. `prepare` errors and panics now both call `release`; `begin_tick`/`end_tick` run under `catch_unwind`; `Local` tolerance, the non-real-time panic path and `panic = "unwind"` are stated.
6. A failed `prepare` is retried on every apply. Recorded as a requirement for the restart policy (Decision 7).
7. Fit of the contract: encoders/outputs inside the render tick, thread-bound resources and first-frame readiness are now stated (Decisions 8 and 9, Consequences), with a partition-boundary sketch.
8. Public API: `collect` returns a `#[non_exhaustive]` `Collected`; `Retired` is crate-private; controller-only errors moved to `JoinError`; semver stated (Decision 10).
9. Edge cases: lease release on the executor thread and the failed-tick video asymmetry documented; `build` now indexes links per pin, O(nodes + links).
10. Slab-plus-commands alternative added with the reason it was rejected.

Owner approval: Salih, 2026-11-02 (early-phase merge policy: merge once CI is green; large reviews later).

- Record, 2026-11-03: audio links refused by this executor and a separate audio executor are superseded by [ADR-0037](0037-audio-graph.md): audio runs on this executor in block-cadence partitions, with audio value slots.
