# Spec: Media graph

- Status: Draft
- Owner crate(s): `rbs-graph`, `rbs-core`
- Related ADRs: [0037](../adr/0037-audio-graph.md), [0002](../adr/0002-headless-engine-and-media-graph.md), [0004](../adr/0004-single-master-clock.md), [0005](../adr/0005-gpu-compositing-with-wgpu.md), [0013](../adr/0013-zero-cost-when-unused.md), [0020](../adr/0020-node-contract-and-schedule-swap.md), [0027](../adr/0027-engine-loop.md)

## Purpose

The runtime model of the engine: nodes, typed links, scheduling and live editing.

## Node kinds

| Kind | Inputs | Outputs | Examples |
| --- | --- | --- | --- |
| Source | no media (data allowed) | at least one output | screen, window, camera, media file, browser, remote guest; data-only producers (timers, chat events) |
| VideoFilter | video | video | colour correction, chroma key, LUT, blur |
| AudioFilter | audio | audio | gain, noise suppression, compressor, CLAP plugin |
| Compositor | many video | video | one per canvas, plus one per nested scene in use (its output feeds the parent compositor like a source) |
| Transition | two or more video (stinger: third), optional audio | video, optional audio | cut, fade, stinger, shader transition |
| AudioMixer | many audio | audio | buses and mixes |
| Encoder | video or audio | packets | via codec port |
| Muxer | packets | packets or bytes | via mux port |
| Output | any media | no media | stream and recording (packets or bytes, via transport port); virtual camera, NDI or SDI out, monitoring device, preview (video or audio) |

The built-in nodes that wrap the ports (capture sources, video export, encoders, outputs) are specified in [io-nodes.md](io-nodes.md).

## Links

Links are typed: `Video`, `Audio`, `Packets`, `Bytes` (muxed container output), `Data`. The graph is a DAG for every link type: feedback such as congestion events driving an encoder's bitrate goes through the control plane, not a graph link. A link carries a format descriptor (for video: size, pixel format, colour space, memory kind). The value types and formats are defined in the [media types spec](media-types.md). Video links carry only pool-texture or system-memory frames: native capture surfaces are imported by `rbs-gpu` in the source node wrapper and never travel on a link. Format negotiation happens when the link is created; a converter node is inserted automatically when needed and is visible in the graph.

## Lifecycle

Every node follows the Installed / Standby / Active states of [ADR-0013](../adr/0013-zero-cost-when-unused.md):

| State | In scheduled graph | Resources |
| --- | --- | --- |
| Installed | No | None; node type known from metadata only |
| Standby | No | Released, or kept warm when the scene or user policy says "keep warm" (shown in diagnostics) |
| Active | Yes, unless `plan()` skips it (below) | Allocated in `prepare` |

The state stored in the graph description is what the scene **wants**; the engine never rewrites it. A node is **effectively** Active exactly when it is in the applied plan. A wanted-Active node that `plan()` skips, or whose `prepare` failed, is kept idle with its resources released (ADR-0013 rule 5) and joins again, through `prepare` and `warm`, as soon as a later edit makes it schedulable (ADR-0020).

- A source becomes Active when it is visible in a scene that is on air, in preview, or feeding any output (including audio-only use). A source that is only in a scene that is not on air and not in preview is Standby.
- Transitions: Installed -> Standby (create, configure, no resources unless keep-warm), Standby -> Active (`prepare`, then included in the next graph swap), Active -> Standby (removed at the next swap; resources released on a background thread unless keep-warm), Standby -> Installed (destroyed).
- Hidden layers inside an Active scene are not pulled by the compositor; their sources are Standby unless another active scene or output uses them. Showing such a layer moves its source to Active, so it appears after warm-up; sources that must appear instantly use keep-warm.

## Scheduling

- Each canvas has a render tick on the master clock at its frame rate; tick times come from `rbs_core::timing::Cadence`, so they never drift. On each tick the compositor pulls the latest frame from each Active source at or before the tick time.
- The audio graph runs in audio partitions: `Cadence` partitions at the block rate (10 ms) that run every due tick in order (`catch_up`). Input nodes pull from the audio input port's lock-free rings each block; every audio link carries one block of the graph format per tick ([audio graph spec](audio-graph.md), ADR-0037).
- Encoders and outputs run on their own workers and consume through bounded queues. A queue never blocks the render tick; what it drops, and how drops are counted, is below.

- One executor runs one partition: the render executor per canvas, the audio executor, and one per encoder and output worker ([ADR-0026](../adr/0026-partitions-and-queues.md)). Every node has a `partition` (`GraphDescription::set_partition`, default `PartitionId::RENDER`). `partition::Lowering::lower(&logical)` returns one description per partition: a media link between partitions becomes a queue writer node (kind `Output`) in the producer's partition and a queue reader node (kind `Source`) in the consumer's, with ids from the reserved range (`NodeId::RESERVED_START`) that stay stable across edits. An endpoint pair is Active exactly when its logical link is in `logical.plan()`, so each part schedules what the whole graph's plan schedules. The application (`rbs_engine::Engine::apply`, [engine spec](engine.md)) adopts `Lowered::created` endpoints into the two controllers, applies each part, and takes back `retired` endpoints after the next swap. For `moved` nodes it applies in two phases: first the parts of the partitions they leave, then it moves each instance (`Controller::take`, `adopt`), then it applies the other parts; the consumer of a re-created queue gets its new `Crossing::stats`. While `Lowered::problems` is not empty the parts are not a faithful split.
- Queues (`runtime::queue`) never allocate per value and never wait. Video: a one-frame mailbox, newest wins. The writer replaces a frame the reader has not taken, so after a stall the encoder gets the newest frame; both ends only `try_lock` it to move an `Option` (a writer that meets the reader keeps its frame for its next tick). Replaced and unshareable frames are counted in `QueueStats` (`video_drops()` for the encoder). Packets: a bounded `sync_channel`, one per crossing link, default 256 packets, at most `PER_TICK_EVENTS` passed on per tick; a reader that leaves packets queued rings its own partition's doorbell so the rest pass on the next tick at once. The consumer never gets a broken stream: a packet lost to a full queue counts `QueueStats::overflows`, and the writer then drops packets up to the next keyframe (`resync_drops`); endpoints that start or restart also begin at a keyframe. The output sees a gap in time, not a corrupt stream; an encoder may watch `overflows` to make a keyframe early. Packet writers ring the consumer partition's `Doorbell` (`Lowering::doorbell`); video writers never wake anyone, since encoders tick at frame cadence. Audio: a fixed set of pre-allocated block buffers passed between a free and a full `sync_channel` (`queue::audio`, default `AUDIO_QUEUE_BLOCKS` = 50); a block with no free buffer is lost and counted (`overflows`), and the reader fills lost blocks with silence at their times (`filled`, up to `AUDIO_FILL_LIMIT` = 1 s), so the consumer's timeline has no gap. Audio writers ring the consumer partition's doorbell; the reader passes one block per tick and rings its own doorbell while more are queued (`Lowering::set_audio_queue`). Bytes and data links cannot cross yet (`LowerProblem`).
- Shutdown drains controllers in pipeline order: render, encoders, outputs.

## Live editing

- The control thread edits the graph description and applies it. The render and audio threads swap to the new schedule at the next tick/block boundary.
- Nodes that are added are prepared and warmed before they run, so scene switches do not stutter. A joining node has no output on its first tick; a "first frame ready" signal before the swap is not implemented yet.
- Removed nodes are torn down on a background thread.

## Runtime (implemented in `rbs_graph::runtime`)

Contract decided in [ADR-0020](../adr/0020-node-contract-and-schedule-swap.md).

| Type | Contract |
| --- | --- |
| `Runtime` | Supplies `Shared: Send + Sync` (read by every thread; GPU context for the render executor) and `Local: Send` (owned by the executor thread; texture pool and importer), plus `begin_tick` / `end_tick` hooks run once per tick (GPU polling, surface retirement) and `attach` / `detach` hooks run on each thread that takes over or gives up the executor (`Executor::attach` / `Executor::detach`; thread-bound contexts, ADR-0030). `()` is a runtime with no resources |
| `Node<X>` | `descriptor`, `prepare`, `warm`, `process`, `cool`, `release`; threads and rules in the table below |
| `Tick` | Index, master-clock time and `draining` (a final tick of a draining partition: no new input upstream; one tick on a cadence partition, possibly several on a doorbell partition, ADR-0043); `Tick::of(cadence, index)` computes the time from the index, `.draining()` marks it |
| `ProcessCtx` | Tick, node id, `shared`, `local`, `inputs()` (`Inputs`, `Copy`), `outputs()` (`Outputs`), `parts()` for all at once |
| `Inputs` | `video(pin, link)`, `videos(pin)`, `audio(pin, link)` and `audios(pin)` (an `AudioBlock` committed this tick, or `None`), `packets(pin)`, `bytes(pin)`, `link_count(pin)`; links into a many-input in connection order. `source(pin, link)` names the producing node and `find(pin, node)` finds its link, so parameters can refer to producers by id ([ADR-0023](../adr/0023-node-parameter-channels.md)) |
| `Outputs` | `set_video(pin, frame)` returns the previous frame; `audio(pin)` lends the pin's block buffer (`AudioOut`: `channel_mut`, `channels_mut`, `commit(format, timestamp, frames)`, `write(block)`; `AudioTooLarge` beyond its capacity, `UnsupportedSamples` for a layout it cannot copy); `push_packet` / `push_bytes` return the value in `Rejected` when the pin is full (`PER_TICK_EVENTS` per tick) or of another kind |
| `Controller` | Control thread. `adopt` (hand over an idle instance), `take` (idle only), `apply(description)` returns an `ApplyReport` (plan sent, nodes that could not join with a `JoinError`, events), `collect` / `collect_timeout` return `Collected` (nodes that left, now released, the counters of every node at the swap, events), `drain` (empty schedule, before shutdown). Restarts: `set_restart_policy`, `set_clock`, `restart_due`, `next_due`, `retry(id)`, `fault(id)`, `faulted()` |
| `Executor` | Real-time thread. `run_tick(tick)`, `swap_pending()` (the swap `run_tick` does after its nodes, between ticks without running one; for cadence workers woken after an `apply`, ADR-0027), `health(id)`, `scheduled()`, `set_failure_limit` |
| `NodeError` | Returned by node calls; variants without heap data can be built on the real-time thread. `JoinError` (no instance, descriptor mismatch, prepare error or panic) is the controller's own |

| Call | Thread | Rules |
| --- | --- | --- |
| `prepare` | control | May allocate, block and do I/O. Called when the node joins, before the schedule is sent |
| `warm` | executor, between ticks | May allocate (pool textures of a new size, more compositor layers); must not block. Once before the first `process` |
| `process` | executor, every tick | Real time: no allocation, blocking, I/O or contended locks |
| `cool` | executor, between ticks | Returns what `warm` took. When the node leaves, and after a failed `warm` |
| `release` | control | Returns what `prepare` took; `prepare` after `release` must work as on a fresh instance (restarts reuse the instance) |

Swap protocol:

1. `apply` plans the description. (Audio links are carried since ADR-0037; `ApplyError::UnsupportedLink` was removed, a breaking change of the pre-1.0 crate.)
2. It calls `prepare` on nodes that join. A failed `prepare` holds the node in the plan without an instance (ADR-0024). A node with no instance or a different descriptor is treated as Standby, and the plan is recomputed; nodes prepared in an earlier round that are no longer scheduled are released again.
3. It lays out one value slot per output pin, in schedule order (an audio pin's slot holds a planar buffer of `MAX_CHANNELS x` `set_audio_block_frames` frames, allocated here), and sends the schedule. Only one schedule is in flight; `apply` returns `Busy` until `collect` has taken back the previous one.
4. After its next tick the executor moves staying nodes, their counters and their video slot contents into the new schedule, calls `cool` on leaving nodes that were warmed and `warm` on joining ones, and swaps. The executor's own swap work does not allocate; `warm` and `cool` run in the slack after the tick and are the node's cost.
5. `collect` receives the old schedule, calls `release` on the nodes that left and keeps them idle. Frames still in old slots are dropped there.
6. Shutdown: `drain`, one tick, `collect`. Dropping an executor that still runs nodes cools them but never releases them.

Values: a video slot keeps its frame until the producer replaces or clears it, so a source slower than the canvas, or a failing one, shows its last frame. Audio, packet and byte slots are per-tick events, emptied before the producer runs (and after a failed tick); a consumer treats an audio input with no block as silence. Data links are not carried by the tick.

Thread affinity: `prepare` and `release` run on the control thread, `warm`, `process` and `cool` on the executor thread. Thread-bound handles are created in `warm` (which must not block) or on the node's own worker. Encoders and outputs in the render partition (no partition assigned) run in the render tick and may only hand work to their workers through queues that never block; assign them their own partitions instead (ADR-0026).

Failures: `process`, `warm`, `cool`, `begin_tick`, `end_tick`, `attach` and `detach` run inside `catch_unwind` (builds use `panic = "unwind"`; the panic path itself is not real-time). A returned error counts as a failed tick and partial event output of that tick is discarded; a video frame it wrote is kept. After `failure_limit` consecutive failed ticks (default 8), on any panic, or when `warm` fails, the node is bypassed: no longer called, video outputs keep the last frame, event outputs stay empty. `Executor::health` and `Collected::health` expose the counters. A bypassed node keeps its `prepare` resources until it leaves the plan. A failed `prepare` (error or panic) calls `release` and leaves the node idle. `NodeError::Permanent` from `process` bypasses at once.

Restarts and containment ([ADR-0024](../adr/0024-node-restart-with-back-off.md)): a node whose `prepare` failed, or that waits for a restart, stays in the plan without an instance; its video outputs keep the last frame of the instance that left, and its dependants (and the nodes upstream of a failed output) keep running. The controller keeps a failure record per node: a failed `prepare`, or a bypass seen in the health of a retired schedule (counted once per instance epoch; health of an older instance is ignored). `RestartPolicy` waits `initial`, doubling up to `max` (defaults 0.5 s and 30 s), optionally gives up after `max_attempts`, and forgives a node whose current instance was not bypassed at a swap at least `stable_after` (10 s) after its restart. A scheduled instance that failed, or that `Controller::retry` superseded, is taken out (cooled, released) on the next `apply` and prepared and warmed again after its wait. `NodeError::Permanent` from `prepare`, `warm` or `process` is never restarted automatically. `Controller::retry(id)`, switching the node to Standby, or removing it clears the record (`Cleared`). A node whose wait is over but that the plan skips for another reason is parked until an edit schedules it. The executor bumps a shared counter on every bypass. The engine's control loop (`rbs_engine::Engine::service`, [engine spec](engine.md)), on each wake-up: `collect`; if `restart_due()`, `apply` the current description; sleep at most `next_due()` (a `Duration` on the controller's clock, set with `set_clock`). `ApplyReport::events` and `Collected::events` carry `RuntimeEvent::{Failed, Restarted, Recovered, Cleared}`; `ApplyReport::failed` is a snapshot of every node held without an instance. `NodeHealth::last_error` holds a static summary of the last error.

Parameters of running nodes ([ADR-0023](../adr/0023-node-parameter-channels.md)): values that change while a node keeps running (the compositor's layer list, mixer gains, transition progress) do not go through `apply`. `runtime::param::channel(initial)` returns a `ParamSender<T>` for the control thread and a `ParamReceiver<T>` that the node owns.

| Call | Side | Contract |
| --- | --- | --- |
| `send(value)` | control | Makes `value` the newest; returns an unread older value it displaced, for reuse. Values from one sender arrive in order; `send` takes `&mut self` |
| `recycle()` | control | Returns the value the receiver last replaced, to refill without allocating. Call it before `send`, which otherwise drops that value on the control thread |
| `pending()` / `is_closed()` | control | A sent value is not taken yet / the node (and its receiver) was dropped |
| `update()` | node, in `process` | Takes the newest value, returns true if the current value changed. No allocation, no freeing, never waits: it uses `try_lock`, and if the sender holds the exchange the value is taken on the first call after the sender lets go (normally the next tick; later if the control thread is preempted inside `send`). With nothing new it is one atomic load |
| `get()` / `get_mut()` | node | The current value |

Latest-value semantics: values are seen in send order, intermediate ones may be skipped. A value and a schedule swap are not applied together, so a value may meet an older or newer schedule. Parameters that refer to inputs therefore name producers by `NodeId` and treat a link index only as a hint (`Inputs::source` / `find`); a producer that is not scheduled is skipped for that tick, so a mismatch can hide a layer for a tick but never draws the wrong source. Fades, transitions and gain changes are sent as targets with timing that the node evaluates against tick time, not as a value per frame. Keep `T` small or heap-backed. Values are dropped on the control thread, except the receiver's own value, which drops with the node.

Invariants, each covered by a test in `crates/rbs-graph/tests/runtime.rs` and `no_alloc.rs`: `prepare` runs before `warm`, which runs before the first `process`; staying nodes are not prepared or warmed again and keep their state and last frame across a swap; skipped nodes are never prepared; leaving nodes are cooled on the executor and released on the controller; a failed `prepare` or `warm` affects only that node (its dependants keep running); failed nodes are released before they are restarted with back-off, a bypass is counted once per instance and stale health never revives a cleared failure (`tests/restart.rs`); steady-state ticks and the swapping tick allocate nothing; `ParamReceiver::update` neither allocates nor frees, never waits on a held exchange and never drops a value on the receiving thread, and `Inputs::find` follows a producer when link positions shift (`tests/param.rs`); a node whose `warm` failed is not cooled again; `drain` cools and releases every node.

Cost (reference VM, Xeon 2.1 GHz, `cargo bench -p rbs-graph --bench executor`): a tick with near-empty nodes costs 120 ns for 6 nodes, 0.53 µs for 20 and 1.66 µs for 68, about 25 ns per node. `apply` plus the swapping tick for a 32-source canvas (68 nodes) costs about 56 µs on the control thread. `ParamReceiver::update` costs about 2 to 3 ns with nothing new and about 20 ns of work when it takes a 64-element `Vec` (bench `param_update`: about 50 ns including the timer's 31 ns).

## Graph description (implemented in `rbs-graph`)

The control thread edits a `GraphDescription`; allocation is allowed there. Implemented types:

| Type | Contract |
| --- | --- |
| `NodeKind` | The kinds in the table above |
| `LinkKind` | `Video`, `Audio`, `Packets`, `Bytes`, `Data` |
| `Pin` | Name, `LinkKind`, and for inputs whether many links may connect (`many`). The name is the pin's stable identity |
| `NodeDescriptor` | `type_id` (reverse-DNS `NodeTypeId`, validated), kind, input and output pins. `validate()` checks the id, unique pin names, and that the media pins fit the kind as in the table above. `Data` pins are allowed on every kind |
| `Lifecycle` | `rbs_core::lifecycle`, shared with adapters and plugins: `Installed`, `Standby`, `Active`; `transition` allows one step at a time; only `Active` is scheduled |
| `GraphDescription` | `add_node` (validates, starts Installed), `remove_node` (Standby or Installed only; returns the node and its links), `restore` (puts them back under the same id), `set_state`, `connect` by pin name, `disconnect`, `link_pin_names` |
| `SchedulePlan` | Output of `plan()`: scheduled nodes in dependency order, links between them, and skipped Active nodes with a `SkipReason` |

Invariants, each covered by a test:

- Node ids are issued in increasing order and never reused; `restore` brings back a removed node under its old id, all or nothing. Ids exhausted is a typed error.
- `connect` refuses unknown nodes or pins, kind mismatches, a second link into a single-link input, duplicate links, self links and cycles. The graph is always a DAG (property-tested on random graphs).
- `plan()` starts from Active nodes and the links between them, then repeatedly drops nodes that are **starved** (a filter, encoder, muxer or output with no media link from a scheduled node) or have **no sink** (no path along media links to a scheduled `Output`). Compositors, transitions and mixers are never starved: with no input they still produce a background, black or silence, so an on-air canvas with an empty scene or muted microphones keeps streaming. Work that reaches no output costs nothing (ADR-0013).
- Data-only nodes (timers, chat events) are event-driven and never in the tick schedule; `plan()` lists them as skipped with `NoSink`. Their runtime is not specified yet.
- Every producer precedes its consumers in `plan().order`; ties are broken by node id, so the same description always gives the same plan.
- A scheduled node can still see no input on some pins (a hidden layer, one input of a muxer). That is part of its normal `process`, not a feature check.

Control API and command log: commands name nodes by scene-level ids and pins by name, never by pin index (indices may change when a node type is updated). Undo of a removal uses `restore`; whole-graph snapshots are also cheap because descriptors are shared.

Versioning: the public enums and structs are `#[non_exhaustive]` with constructors, so a settings schema on descriptors, format descriptors on links, new link kinds and new errors are minor changes. `SchedulePlan` is expected to be partitioned per canvas, audio graph and encoder worker when the scheduler lands; that is also additive.

Cost: `connect` and each pruning round of `plan()` are O(nodes + links), on the control thread only.

Format negotiation on links and converter insertion are not implemented yet.


## Failure handling

A node that returns an error, panics or exceeds its time budget for N consecutive ticks is bypassed (last good frame or silence), restarted with exponential back-off, and reported through the event stream. Implemented so far: bypass on errors, panics, permanent errors and failed warm-up, restart with back-off and the events (see Runtime); time budgets and the event stream's transport (control API) are not.

## Metrics per node

Processing time (p50, p99, max), queue depth, frames in/out, drops by reason, restarts. Exported with `tracing` and readable through the control API.

## Open questions

- Scene document contract for UI rule U15 is in the [scene model spec](scene-model.md); how the scene binder turns it into graph descriptions is still open.

- Pull vs push for video sources with their own frame rate higher than the canvas.
- How much of the scene tree is evaluated on GPU vs CPU (transform animation).
