# ADR-0025: Partitions joined by queue endpoint nodes

- Status: Accepted
- Date: 2026-11-03
- Deciders: Salih (owner)

## Context

ADR-0020 runs one schedule per executor and, until now, everything in the render tick: encoders and outputs too, with the rule that their `process` may only hand work to their own workers without blocking. The media graph spec says encoders and outputs run on their own workers and consume through bounded queues, and ADR-0020 (Decision 8) sketched how: cut the plan at partition boundaries, and put a queue-writer endpoint on the producer side and a queue-reader endpoint on the consumer side, so `Inputs` and `Outputs` stay as they are.

The encoders and outputs thread stated what it needs:

- **Render to encoder:** keep the newest frame and drop the oldest, counted, so the encoder can keep timestamps honest and ask for a keyframe.
- **Encoder to output:** a plain bounded hand-off that never drops silently, because dropping encoded packets breaks references. The transport already has its own drop policy (output ports spec). If the hand-off fills up, the output fails rather than sending a broken stream.
- **One queue per output:** for P7, a stalled output must not hold back the encoder or the other outputs.
- **Output workers wake on packets,** plus a low-rate timer for `poll_events`, and do nothing while Installed.
- **Encoder workers** may run at frame cadence.

Forces:

- **P6:** the render thread must not wait, allocate or lock. A frame dropped on the render thread releases its lease, which lease owners already do without locks (media types spec).
- **P1:** `rbs-graph` must not name a GPU or codec crate. Every partition can have its own `Runtime`.
- **ADR-0020, Decision 5:** the description's lifecycle states are what the scene wants. Whether a node runs must stay a function of the whole graph's plan, even when that graph is split.
- **Tests** must not sleep or depend on thread timing.

## Decision

1. **Every node has a partition.** `NodeEntry::partition: PartitionId`, set with `GraphDescription::set_partition`, defaults to `PartitionId::RENDER`. The application assigns encoders and outputs, for example one partition per encoder and one per output. A partition is one executor with its own `Controller`, `Executor`, `Runtime` type and thread.
2. **One logical description, lowered per partition.** The scene and the application keep editing one `GraphDescription`. `rbs_graph::partition::Lowering::lower(&logical)` returns one description per partition (`Lowered::parts`). For every media link whose two ends are in different partitions, it puts endpoint nodes in their place:
   - a **writer** node in the producer's partition, of kind `Output`, so it counts as a sink and keeps its upstream scheduled;
   - a **reader** node in the consumer's partition, of kind `Source`.
   - Their ids come from a reserved range (`NodeId::RESERVED_START` = `0xF000_0000`; `add_node` never issues these) and stay the same for as long as the link and both partitions do, so endpoints keep running across swaps.
3. **Liveness matches the whole graph.** An endpoint pair is Active exactly when its logical link is in `logical.plan()`; other nodes keep their states. Writers are sinks and readers are sources, so each partition's own plan then schedules the same nodes as the whole graph's plan. Lowering does not write lifecycle states into the logical description.
4. **Queues.** No new dependency and no `unsafe`.
   - **Video: a one-frame mailbox, newest wins.** The writer offers each new frame once, recognised by (timestamp, sequence), as a `VideoFrame::share` copy. It replaces a frame the reader has not taken yet, so after a stall the encoder gets the newest frame, never the one from when the stall began. The mailbox is an `AtomicBool` plus a `Mutex<Option<VideoFrame>>` that both ends only `try_lock`, to move an `Option`, like parameter channels (ADR-0023). A writer that meets the reader there keeps its frame and offers it on its next tick (a newer frame replaces it then); a reader that meets the writer takes the frame on its next tick. A replaced frame is dropped outside the lock, which releases its lease without locks (media types spec). Counted in `QueueStats`:
     - `skipped`: frames replaced before the reader took them, or held by an endpoint when it was released;
     - `unshareable`: native surfaces, which the render partition must import first;
     - `video_drops()` is the sum, for the encoder to compare between frames.

     Video writers never wake the consumer; encoder workers tick at frame cadence.
   - **Packets: the consumer never gets a broken stream.** Endpoints share a `std::sync::mpsc::sync_channel`, bounded and preallocated; `try_send` and `try_recv` never allocate and never wait for the other side. (In std's array flavour they may spin briefly, and rarely yield, when both ends touch the same slot at once. Packet endpoints never run on the render thread.) When the queue is full the packet is lost and `QueueStats::overflows` grows; the writer then drops packets up to the next keyframe (`resync_drops`). Endpoints that start or restart also wait for a keyframe. The output sees a gap in time but a decodable stream, so no node has to watch a counter for correctness; an encoder may watch `overflows` to make a keyframe early. The transport keeps its own drop policy (output ports spec).
     - Each crossing link has its own queue, so one output per queue.
     - Default capacity: 256 packets (`Lowering::set_packet_capacity`).
     - The reader passes on at most `PER_TICK_EVENTS` per tick and keeps the rest queued, in order.
     - The writer rings the consumer partition's `Doorbell`.
   - Bytes, data and audio links cannot cross yet (`LowerProblem::UnsupportedKind`); audio has its own executor.
5. **Doorbells.** One per consumer partition (`Lowering::doorbell(p)`). `ring` is one atomic swap, plus an unpark of the registered worker when the flag was clear. `wait` registers the calling thread (again if the worker moved to a new thread). The worker's loop calls `Doorbell::wait(timeout)` between ticks, with its low-rate timer as the timeout. Only non-real-time producers ring; the render thread never does, because video writers do not ring.
6. **What the application does with a `Lowered`.** It is plain control-thread data:
   - adopt each `created` queue's writer into `crossing.from`'s controller and its reader into `crossing.to`'s, both under the reported ids (`QueueEnds::writer::<X>()` and `QueueReaderEnd::into_node::<Y>()` box them for each partition's runtime);
   - apply each part to its controller. With `moved` nodes, in two phases: first the parts of the partitions they leave, then take each idle instance from the old controller and adopt it into the new one, then apply the other parts (applying the new part first fails with `JoinError::NoInstance`);
   - after the next swap, take back and drop the endpoints of `retired` queues;
   - give each `Crossing::stats` to the encoder that consumes it, if it wants drop counts (again after a move re-creates the queue);
   - treat the parts as incomplete while `problems` is not empty: those links are missing.

   Reserved ids are taken with checked arithmetic; when they run out, the link is reported as `LowerProblem::IdsExhausted`.
7. **Endpoints are ordinary nodes.** `VideoWriter`, `VideoReader`, `PacketWriter` and `PacketReader` implement `Node<X>` for every `Runtime`. The executor, `Inputs` and `Outputs` do not change. Partitions swap independently, so for a moment one side of a queue may run while the other has not joined yet. Nothing waits on it: the writer drops (video) or counts an overflow only when the queue is actually full, and the reader sees nothing.
8. **Shutdown order.** Drain the controllers in pipeline order: render, then encoders, then outputs. Each drain stops a partition's inputs before the next one flushes. Output nodes finish their transports with a deadline in `cool` and `release`.
9. **Not in this change.** The worker loop itself (thread, cadence, doorbell wait, restart checks) belongs to the engine in `rbs-app`. So do per-link capacities, and partitions for audio. The `Inputs` and `Outputs` contract allows a later executor that swaps several partitions together, if a node ever needs that.

## Consequences

- Encoders and outputs can run on their own threads now, using the same `Node` trait. A stalled output only overflows its own queue, and the encoder and the other outputs keep running (test `a_stalled_output_overflows_its_own_queue_only`).
- **Cost.** Measured on the reference VM with `cargo bench -p rbs-graph --bench executor -- queue_crossing`:
  - source, encoder and output in one partition: 61 ns per frame;
  - the same pipeline over three partitions: 271 ns per frame. That covers three executor ticks, four endpoint nodes and two hand-offs (mailbox and channel). The bench runs all three executors on one thread, so it measures the work, not cross-core cache traffic; the engine loop in `rbs-app` will measure that end to end.
  - Ticks with queues allocate nothing (test `queue_endpoints_do_not_allocate_per_tick`).
- **Latency.** At most one frame of extra latency from render to encoder, plus the encoder's own cadence. Packets reach the output on the doorbell wake-up.
- **Idle cost.** Endpoints of a link that is not live are adopted but not scheduled; a packet queue keeps its preallocated buffer (256 packet handles) until the link is removed. No threads, polling or GPU resources (P13).
- **More moving parts for the application.** It has to adopt endpoint instances, move instances between controllers, and keep one controller per partition. That is the price of not changing the executor; a helper in `rbs-app` will hide it.
- `add_node` now stops at `RESERVED_START`, which leaves about four billion ids.
- `NodeEntry` gains a field. It is `#[non_exhaustive]`, so this is additive.
- Restarts (ADR-0024) work per partition: a failing output restarts on its own executor without touching the encoder. A failing encoder leaves its output starved, not stopped.

## Alternatives considered

- **Endpoint slots inside the executor.** The executor would read cross-partition inputs into slots before the first node runs, without extra nodes. It saves two virtual calls per crossing, but it changes the hot executor loop and `Schedule` layout, and it needs queue logic in the executor. Endpoint nodes keep the executor untouched; ADR-0020 sketched them.
- **Insert endpoints into the logical description.** No lowering step, but the scene binder and the application would see nodes they did not create, and moving a node between partitions would rewrite their links.
- **A bounded channel for video too** (drop the new frame when full, skip to the newest on read). Rejected in review: after a stall the encoder got the frame from when the stall began.
- **A lock-free ring crate (`rtrb`) or an own SPSC ring with `unsafe`.** A new dependency or `unsafe` in a core crate, for what a one-frame mailbox already gives.
- **Outputs fail on a packet overflow** (they watch `overflows` and are restarted). That needs a contract to hand queue stats to plugin outputs, and a forgotten check sends a broken stream. Resyncing at the writer keeps the stream valid without any contract.
- **Drop packets by GOP in the graph queue.** That duplicates the transport's own policy (output ports spec) and hides broken streams from the output.
- **Wake encoder workers from the render thread.** That costs an unpark system call per frame on the render thread. Encoders run at frame cadence instead.

## Review

- 2026-11-03, independent AI session (fresh context, proposal, specs and code only), per ADR-0014: approve with changes. Blocking findings and how they were resolved:
  1. Video was drop-newest-when-full, so a stalled encoder got a stale frame. Replaced with a one-frame mailbox; test `after_a_stall_the_consumer_gets_the_newest_frame_not_a_stale_one`.
  2. "Never dropped silently" relied on an unspecified way to give outputs the queue stats. Replaced with keyframe resync in the endpoints; tests `after_a_packet_overflow_the_output_resumes_at_a_keyframe`, `packets_carried_over_to_the_next_tick_keep_their_order`.
  3. The documented order for moving a node failed with `NoInstance`. Two-phase order documented; test `an_active_node_moves_between_partitions_in_two_phases`.
  4. The cost wording claimed three threads; corrected. Hot-path checklist filled in the PR.

  Non-blocking findings taken: checked reserved-id arithmetic (`LowerProblem::IdsExhausted`), doorbell re-registers a new worker thread and has a single-ring test, parts documented as incomplete while `problems` is not empty, stats hand-over after a move, idle cost stated. Deferred: a property test over random graphs that the union of the parts' plans equals the logical plan (to come with the engine loop in `rbs-app`).
- Owner approval: Salih, 2026-11-03 (early-phase policy: threads may accept ADRs and merge on green CI).
