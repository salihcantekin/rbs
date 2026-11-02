# Spec: Media graph

- Status: Draft
- Owner crate(s): `rbs-graph` (planned), `rbs-core`
- Related ADRs: [0002](../adr/0002-headless-engine-and-media-graph.md), [0004](../adr/0004-single-master-clock.md), [0005](../adr/0005-gpu-compositing-with-wgpu.md)

## Purpose

The runtime model of the engine: nodes, typed links, scheduling and live editing.

## Node kinds

| Kind | Inputs | Outputs | Examples |
| --- | --- | --- | --- |
| Source | none | video and/or audio | screen, window, camera, media file, browser, remote guest |
| VideoFilter | video | video | colour correction, chroma key, LUT, blur |
| AudioFilter | audio | audio | gain, noise suppression, compressor, CLAP plugin |
| Compositor | many video | video | one per canvas, renders a scene tree |
| Transition | two video | video | cut, fade, stinger, shader transition |
| AudioMixer | many audio | audio | buses and mixes |
| Encoder | video or audio | packets | via codec port |
| Muxer | packets | packets or bytes | via mux port |
| Output | packets or bytes | none | via transport port |

## Links

Links are typed: `Video`, `Audio`, `Packets`, `Data`. A link carries a format descriptor (for video: size, pixel format, colour space, memory kind). The value types and formats are defined in the [media types spec](media-types.md). Video links carry only pool-texture or system-memory frames: native capture surfaces are imported by `rbs-gpu` in the source node wrapper and never travel on a link. Format negotiation happens when the link is created; a converter node is inserted automatically when needed and is visible in the graph.

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
