# ADR-0025: Partitions joined by queue endpoint nodes

- Status: Proposed
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
4. **Queues.** Endpoints share a `std::sync::mpsc::sync_channel`. It is bounded and preallocated, and its `try_send` and `try_recv` never allocate or wait (the array flavour is lock-free). No new dependency and no `unsafe`.
   - **Video, newest wins.** The writer sends each new frame once, recognised by (timestamp, sequence), as a `VideoFrame::share` copy. When the queue is full it drops that frame. The reader takes everything waiting and passes on only the newest. Drops are counted in `QueueStats`:
     - `dropped_full`;
     - `skipped`;
     - `unshareable`, for native surfaces, which the render partition must import first;
     - `video_drops()` is the sum, for the encoder to compare between frames.

     Default capacity: 2 frames. Video writers never wake the consumer; encoder workers tick at frame cadence.
   - **Packets, never dropped silently.** The writer sends every packet. When the queue is full the packet is lost and `QueueStats::overflows` grows. The consumer output must treat any increase as a broken stream and fail (`NodeError`), so that ADR-0024 restarts it cleanly from the next keyframe.
     - Each crossing link has its own queue, so one output per queue.
     - Default capacity: 256 packets.
     - The reader passes on at most `PER_TICK_EVENTS` per tick and keeps the rest queued.
     - The writer rings the consumer partition's `Doorbell`.
   - Bytes, data and audio links cannot cross yet (`LowerProblem::UnsupportedKind`); audio has its own executor.
5. **Doorbells.** One per consumer partition (`Lowering::doorbell(p)`). `ring` is one atomic swap, plus an unpark only while the worker is parked. The worker's loop calls `Doorbell::wait(timeout)` between ticks, with its low-rate timer as the timeout. Only non-real-time producers ring; the render thread never does, because video writers do not ring.
6. **What the application does with a `Lowered`.** It is plain control-thread data:
   - adopt each `created` queue's writer into `crossing.from`'s controller and its reader into `crossing.to`'s, both under the reported ids (`QueueEnds::writer::<X>()` and `QueueReaderEnd::into_node::<Y>()` box them for each partition's runtime);
   - apply each part to its controller;
   - after the next swap, take back and drop the endpoints of `retired` queues;
   - for `moved` nodes, take the idle instance from the old partition's controller and adopt it into the new one;
   - give each `Crossing::stats` to the consumer node: the encoder or the output.
7. **Endpoints are ordinary nodes.** `VideoWriter`, `VideoReader`, `PacketWriter` and `PacketReader` implement `Node<X>` for every `Runtime`. The executor, `Inputs` and `Outputs` do not change. Partitions swap independently, so for a moment one side of a queue may run while the other has not joined yet. Nothing waits on it: the writer drops (video) or counts an overflow only when the queue is actually full, and the reader sees nothing.
8. **Shutdown order.** Drain the controllers in pipeline order: render, then encoders, then outputs. Each drain stops a partition's inputs before the next one flushes. Output nodes finish their transports with a deadline in `cool` and `release`.
9. **Not in this change.** The worker loop itself (thread, cadence, doorbell wait, restart checks) belongs to the engine in `rbs-app`. So do per-link capacities, and partitions for audio. The `Inputs` and `Outputs` contract allows a later executor that swaps several partitions together, if a node ever needs that.

## Consequences

- Encoders and outputs can run on their own threads now, using the same `Node` trait. A stalled output only overflows its own queue, and the encoder and the other outputs keep running (test `a_stalled_output_overflows_its_own_queue_only`).
- **Cost.** Measured on the reference VM with `cargo bench -p rbs-graph --bench executor -- queue_crossing`:
  - source, encoder and output in one partition: 65 ns per frame;
  - the same pipeline over three partitions: 320 ns per frame. That covers three executor ticks, four endpoint nodes and two channel hand-offs, spread over three threads.
  - Ticks with queues allocate nothing (test `queue_endpoints_do_not_allocate_per_tick`).
- **Latency.** At most one frame of extra latency from render to encoder, plus the encoder's own cadence. Packets reach the output on the doorbell wake-up.
- **More moving parts for the application.** It has to adopt endpoint instances, move instances between controllers, and keep one controller per partition. That is the price of not changing the executor; a helper in `rbs-app` will hide it.
- `add_node` now stops at `RESERVED_START`, which leaves about four billion ids.
- `NodeEntry` gains a field. It is `#[non_exhaustive]`, so this is additive.
- Restarts (ADR-0024) work per partition: a failing output restarts on its own executor without touching the encoder. A failing encoder leaves its output starved, not stopped.

## Alternatives considered

- **Endpoint slots inside the executor.** The executor would read cross-partition inputs into slots before the first node runs, without extra nodes. It saves two virtual calls per crossing, but it changes the hot executor loop and `Schedule` layout, and it needs queue logic in the executor. Endpoint nodes keep the executor untouched; ADR-0020 sketched them.
- **Insert endpoints into the logical description.** No lowering step, but the scene binder and the application would see nodes they did not create, and moving a node between partitions would rewrite their links.
- **A lock-free ring crate (`rtrb`) or an own SPSC ring with `unsafe`.** These would allow a true drop-oldest on the producer side. But it means a new dependency or `unsafe` in a core crate, and drop-newest-when-full plus skip-to-newest on read gives the encoder the same newest frame.
- **Drop packets by GOP in the graph queue.** That duplicates the transport's own policy (output ports spec) and hides broken streams from the output.
- **Wake encoder workers from the render thread.** That costs an unpark system call per frame on the render thread. Encoders run at frame cadence instead.

## Review

Pending: independent review per ADR-0014.
