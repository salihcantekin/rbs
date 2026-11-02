# Spec: Media graph

- Status: Draft
- Owner crate(s): `rbs-graph`, `rbs-core`
- Related ADRs: [0002](../adr/0002-headless-engine-and-media-graph.md), [0004](../adr/0004-single-master-clock.md), [0005](../adr/0005-gpu-compositing-with-wgpu.md)

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

## Links

Links are typed: `Video`, `Audio`, `Packets`, `Bytes` (muxed container output), `Data`. The graph is a DAG for every link type: feedback such as congestion events driving an encoder's bitrate goes through the control plane, not a graph link. A link carries a format descriptor (for video: size, pixel format, colour space, memory kind). The value types and formats are defined in the [media types spec](media-types.md). Video links carry only pool-texture or system-memory frames: native capture surfaces are imported by `rbs-gpu` in the source node wrapper and never travel on a link. Format negotiation happens when the link is created; a converter node is inserted automatically when needed and is visible in the graph.

## Lifecycle

Every node follows the Installed / Standby / Active states of [ADR-0013](../adr/0013-zero-cost-when-unused.md):

| State | In scheduled graph | Resources |
| --- | --- | --- |
| Installed | No | None; node type known from metadata only |
| Standby | No | Released, or kept warm when the scene or user policy says "keep warm" (shown in diagnostics) |
| Active | Yes | Allocated in `prepare` |

- A source becomes Active when it is visible in a scene that is on air, in preview, or feeding any output (including audio-only use). A source that is only in a scene that is not on air and not in preview is Standby.
- Transitions: Installed -> Standby (create, configure, no resources unless keep-warm), Standby -> Active (`prepare`, then included in the next graph swap), Active -> Standby (removed at the next swap; resources released on a background thread unless keep-warm), Standby -> Installed (destroyed).
- Hidden layers inside an Active scene are not pulled by the compositor; their sources are Standby unless another active scene or output uses them. Showing such a layer moves its source to Active, so it appears after warm-up; sources that must appear instantly use keep-warm.

## Scheduling

- Each canvas has a render tick on the master clock at its frame rate; tick times come from `rbs_core::timing::Cadence`, so they never drift. On each tick the compositor pulls the latest frame from each Active source at or before the tick time.
- The audio graph runs on the audio thread at a fixed block size; sources push blocks into lock-free ring buffers.
- Encoders and outputs run on their own workers and consume through bounded queues. A full queue drops according to a per-output policy (drop oldest non-keyframe data, never block the render tick) and records the drop with a reason.

## Live editing

- The control thread builds a new graph description and submits it. The render and audio threads swap to it at the next tick/block boundary.
- Nodes that are added are warmed up (opened, first frame ready) before the swap, so scene switches do not stutter.
- Removed nodes are torn down on a background thread.

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
- `plan()` starts from Active nodes and the links between them, then repeatedly drops nodes that are **starved** (a non-source with media inputs but no media link from a scheduled node) or have **no sink** (no path to a node without media outputs). Inactive producers therefore never leave consumers scheduled for nothing, and work that reaches no output costs nothing (ADR-0013).
- Every producer precedes its consumers in `plan().order`; ties are broken by node id, so the same description always gives the same plan.
- A scheduled node can still see no input on some pins (a hidden layer, one input of a muxer). That is part of its normal `process`, not a feature check.

Control API and command log: commands name nodes by scene-level ids and pins by name, never by pin index (indices may change when a node type is updated). Undo of a removal uses `restore`; whole-graph snapshots are also cheap because descriptors are shared.

Versioning: the public enums and structs are `#[non_exhaustive]` with constructors, so a settings schema on descriptors, format descriptors on links, new link kinds and new errors are minor changes. `SchedulePlan` is expected to be partitioned per canvas, audio graph and encoder worker when the scheduler lands; that is also additive.

Cost: `connect` and each pruning round of `plan()` are O(nodes + links), on the control thread only.

Format negotiation on links and converter insertion come with the scheduler.

## Node contract (sketch)

```rust
pub trait Node: Send {
    fn descriptor(&self) -> &NodeDescriptor;   // kind, ports, settings schema
    fn prepare(&mut self, ctx: &mut PrepareCtx) -> Result<(), NodeError>; // may allocate
    fn process(&mut self, ctx: &mut ProcessCtx) -> Result<(), NodeError>; // real-time: no alloc, no blocking
    fn reset(&mut self);
}
```

## Failure handling

A node that returns an error, panics or exceeds its time budget for N consecutive ticks is bypassed (last good frame or silence), restarted with exponential back-off, and reported through the event stream.

## Metrics per node

Processing time (p50, p99, max), queue depth, frames in/out, drops by reason, restarts. Exported with `tracing` and readable through the control API.

## Open questions

- Scene document contract for UI rule U15: scenes owned by one canvas or shared, and per-canvas independent layout of the same source. To be specified with the scene format spec.

- Pull vs push for video sources with their own frame rate higher than the canvas.
- How much of the scene tree is evaluated on GPU vs CPU (transform animation).
