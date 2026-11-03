# ADR-0038: Graph nodes for capture, encoding and outputs in an engine crate

- Status: Accepted
- Date: 2026-11-03
- Deciders: Salih (owner)

## Context

The graph runtime (ADR-0020), partitions and queues (ADR-0026) and the engine loop (ADR-0027) exist, and so do adapters for capture (WGC, Media Foundation cameras, ScreenCaptureKit, AVFoundation, PipeWire), video encoders (NVENC, AMF, QSV, VideoToolbox, VA-API), muxers and transports. The only node type is the compositor. To record a scene, someone has to turn a capture session into a source node, a canvas into encoder input, an encoder session into an encoder node, and a muxer plus transport into an output node.

Forces:

- **P1:** engine code never depends on an adapter; adapters arrive as port trait objects from the composition root (`rbs-app`, `rbs-cli`).
- **P4:** zero copy by default. But the Windows encoders that exist today (NVENC, AMF) take only system-memory NV12/P010 (`rbs-codec-hw`), and the native export of ADR-0025 exists only for macOS IOSurfaces.
- **ADR-0025:** a native frame has exactly one consumer and never travels on a graph link; the video mailbox between partitions moves `VideoFrame::share` copies, which native frames refuse.
- **P6:** `process` does not allocate, block or lock; capture `start` and encoder or transport creation may block and run on the control thread.
- **P13:** an Installed or Standby node holds no device session.
- Windows first (owner, 2026-11-03), with macOS and Linux adapters plugging into the same nodes right after.

## Decision

1. **A new engine-layer crate, `rbs-nodes`,** holds the built-in nodes that wrap ports. It depends on `rbs-core`, `rbs-graph`, `rbs-gpu`, the port crates (`rbs-capture`, `rbs-codec`, `rbs-mux`, `rbs-transport`) and `wgpu` (only to read the device's graphics API), never on an adapter. The composition root constructs nodes with adapter factories (`Arc<dyn CaptureFactory>`, and so on). Audio nodes (audio input, mixer, audio encoder) belong to the audio graph (ADR-0037) and are not in this crate; the output node takes their packets like any other stream.
2. **Capture source node** (`org.rbs.capture`, kind `Source`, render partition, `GpuRuntime`). `prepare` opens and starts the session on the control thread, filling memory kinds, the GPU import target and pixel formats from the render device by graphics API, not by vendor (D3D12: shared D3D handles on the render adapter's LUID; Metal: IOSurfaces; Vulkan: system memory until DMA-BUF import is verified on hardware). `process` takes `latest_frame` and puts a pool-texture frame on its link: native frames through the render runtime's `Importer` (zero copy), system frames through a new `rbs_gpu::upload::SourceUpload` (`Queue::write_texture`; NV12/P010 decoded by the existing `YuvDecoder`). `release` stops and drops the session. Permission, format and missing-target errors are permanent; the others restart with back-off (ADR-0024).
3. **Video export node** (`org.rbs.video-export`, kind `VideoFilter`, render partition). It turns canvas frames into encoder input. For now it uses a new `rbs_gpu::download::YuvDownload`: GPU conversion to NV12/P010 (the existing `YuvConverter`), a copy into one of four mappable staging buffers, `map_async` without waiting, and on a later tick one copy of the mapped bytes into a recycled frame buffer, delivered as a `System` frame. A full ring drops the frame (counted), never waits. Every delivered frame counts in `GpuMetrics::frames_cpu_copied`.
   - **This is a temporary exception to P4 on every platform.** The readback is the only path the current Windows encoders accept, and the node uses it everywhere, macOS included (where the ADR-0025 exporter exists but cannot cross the partition queue yet). Exit criteria, per encoder: (a) the encoder accepts native input (NVENC/AMF: D3D11/D3D12; a D3D12 exporter, ADR-0025 decision 6), (b) the queue between partitions can move a native frame to its single consumer, and (c) the export node then chooses native export or readback from the memory kinds the encoder declares (a capability, P3), not from the platform. All three are follow-ups (issues linked in the spec); the node's frames and the encoder node do not change shape when they land.
   - The copy runs on the render thread: about 3 MB per 1080p frame.
4. **Video encoder node** (`org.rbs.video-encoder`, kind `Encoder`, own partition, any runtime). `VideoEncoderNode::open` creates a session once on the control thread to learn its `StreamDescriptor` for the output, then drops it, so an installed node holds no encoder (P13). `prepare` creates the session that runs and fails permanently if its descriptor differs from the probed one (the output wrote headers for it); this relies on adapters producing the same extradata for the same configuration, which the hardware runs check. `process` submits each new frame once (recognised by timestamp and sequence), drains once and retries on `Busy`, and passes packets on. Packets past the per-tick limit are carried to the next tick (64 at most); past that it drops packets up to the next keyframe and asks the encoder for one, so the output never gets a broken stream. A frame the encoder refuses is counted and dropped without failing the tick, because a failed tick discards the packets the node already pushed in it.
5. **Output node** (`org.rbs.output`, kind `Output`, own partition, any runtime). One single-link packet input per stream (`s0`, `s1`, ... in stream order). `prepare` creates the muxer (when the transport's input is a container) and the transport, starts it and writes the header. `process` writes packets through the muxer and sends items; `QueueFull` goes to a bounded backlog that is sent first on the next tick, and a full backlog stops the output with a permanent error rather than write a file with holes (output ports spec). Every other transport or muxer error is permanent too, because restarting a file output would truncate the recording; network transports reconnect inside the adapter. For the same reason an output that wrote media refuses a second `prepare` (a restart after a panic in an adapter) with a permanent error. The first permanent failure's cause is kept in the node's stats for diagnostics. The waits in `prepare` and `release` use an injected wait clock (ADR-0032). `release` delivers the backlog and the muxer's tail, then calls `Transport::finish`, waiting at most a configurable deadline (5 s).
6. **Hot paths.** `rbs-nodes` and the two new `rbs-gpu` modules are listed in `docs/architecture/hot-paths.md`. Steady-state ticks of the encoder and output nodes allocate nothing (test). The GPU nodes allocate nothing in RBS code for a size they have seen; wgpu allocates internally, as on the compositor path. A capture source's first frame of a new size creates its pool textures, like the importer's first sight of a surface.

## Consequences

- Recording works end to end with the existing adapters: capture -> compositor -> video export ==> encoder ==> output. `rbs-app` and `rbs-cli` only construct nodes and partitions.
- macOS and Linux need no new nodes: their capture adapters already deliver frames the importer or the upload reads, and VideoToolbox, QSV and VA-API accept system memory. Their zero-copy export paths come with the same follow-up as Windows.
- The Windows default path reads frames back to the CPU until the follow-ups land. `frames_cpu_copied` shows it, and the budget tests that expect zero readbacks do not cover recording yet.
- The last frame or two an encoder holds back when it is released are lost (`EncoderNodeStats::flush_lost`). Flushing an encoder into the graph at end of stream is an open question of the engine spec.
- `KeyframeRequested` and `BitrateHint` from transports are counted but not forwarded to encoders yet (control-plane work).
- A new crate adds a little build time; it pulls nothing new into the dependency graph (`bytes` and `wgpu` are already there).

## Alternatives considered

- **Nodes in each port crate or in `rbs-gpu`.** Port crates must not depend on `rbs-graph` or `rbs-gpu` (layer rules), and putting codec, mux and transport nodes in `rbs-gpu` mixes GPU code with output code.
- **Nodes in the composition root (`rbs-app`).** `rbs-cli` would duplicate them, and they would build with Slint.
- **Encoder node on the render partition that does the readback itself.** The render thread would run encoder calls; ADR-0026 moved encoders to their own workers for P7.
- **Blocking readback (`readback::read_bytes`).** Waits for the GPU on the render thread (P6).
- **Waiting for D3D11/D3D12 encoder input before any recording.** Zero copy from day one, but it needs a D3D12 exporter, D3D input in two vendor adapters and native frames across the partition queue first; the owner asked for the shortest path to a Windows recording.
- **One output pin with many links, streams named by producer.** `Inputs::packets` merges all links of a pin, so the stream of a packet would be lost without a new per-link accessor in the runtime (a hot-path change); one pin per stream needs none.

## Review

Reviewer: independent AI session (ADR-0014), given only the diff, this ADR, the specs and the code. Date: 2026-11-03. Verdict: changes requested; all findings addressed in the same PR:

1. Blocker, packets lost when the encoder refuses a frame (a failed tick discards pushed packets): a refused frame is counted and the tick succeeds; test `a_refused_frame_loses_no_packet_of_the_same_tick`.
2. Major, a restarted file output truncates the recording: an output that wrote media refuses a second `prepare`; test `an_output_that_wrote_media_refuses_to_restart`. A capability for transports that may restart (network) is a follow-up.
3. Major, the encoder held a session while Installed (P13): `open` drops its probe session; test `an_opened_encoder_holds_no_session_until_prepare`.
4. Major, the P4 exception was wider than stated and not capability-driven: decision 3 restated for all platforms with exit criteria; follow-ups tracked as issues.
5. Minor, unsupported uploads retried forever, P010 offered without `gpu.p010`: `Unsupported`/`InvalidSize` are permanent; P010 is offered only with `gpu.p010`.
6. Minor, the upload's decoder survived into Standby: `release` drops it.
7. Minor, wall-clock waits on the control thread: `WaitClock` injected; test `release_gives_up_on_a_full_queue_without_sleeping`.
8. Minor, a frame lost without a count in `YuvDownload`: counted in `failed`.
9. Minor, non-recoverable creation errors restarted with back-off: muxer and transport creation, and encoder creation other than `ResourceExhausted`, are permanent; transport `start` follows `TransportError::is_recoverable`.
10. Minor, hot-path checklist incomplete: the GPU nodes' per-frame wgpu allocations and the before/after table go to the human hot-path sign-off, which stays pending (P15).

Owner approval: Salih, 2026-11-03, under the early-phase policy that lets threads accept ADRs after the independent review is recorded. The human hot-path sign-off (P15) remains open with the other pending hot-path reviews.
