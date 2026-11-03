# ADR-0038: Graph nodes for capture, encoding and outputs in an engine crate

- Status: Proposed
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
   - **This is a temporary exception to P4 on Windows.** It is the only path the current Windows encoders accept. It is replaced per encoder once (a) NVENC/AMF accept D3D11/D3D12 input and a D3D12 exporter exists (ADR-0025 decision 6), and (b) the queue between partitions can move a native frame to its single consumer. Both are follow-ups; the node's frames and the encoder node do not change shape when they land.
   - The copy runs on the render thread: about 3 MB per 1080p frame.
4. **Video encoder node** (`org.rbs.video-encoder`, kind `Encoder`, own partition, any runtime). `VideoEncoderNode::open` creates the session on the control thread, so its `StreamDescriptor` is known when the output is built. Construct the node when its output starts; it then holds an encoder until `release`, like an Active node. After a restart `prepare` creates a new session and fails permanently if its descriptor differs (the output already wrote headers for the first one). `process` submits each new frame once (recognised by timestamp and sequence), drains once and retries on `Busy`, and passes packets on. Packets past the per-tick limit are carried to the next tick (64 at most); past that it drops packets up to the next keyframe and asks the encoder for one, so the output never gets a broken stream.
5. **Output node** (`org.rbs.output`, kind `Output`, own partition, any runtime). One single-link packet input per stream (`s0`, `s1`, ... in stream order). `prepare` creates the muxer (when the transport's input is a container) and the transport, starts it and writes the header. `process` writes packets through the muxer and sends items; `QueueFull` goes to a bounded backlog that is sent first on the next tick, and a full backlog stops the output with a permanent error rather than write a file with holes (output ports spec). Every other transport or muxer error is permanent too, because restarting a file output would truncate the recording; network transports reconnect inside the adapter. `release` delivers the backlog and the muxer's tail, then calls `Transport::finish`, waiting at most a configurable deadline (5 s).
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

Independent review per ADR-0014 pending.
