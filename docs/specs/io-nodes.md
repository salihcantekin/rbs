# Spec: Capture, encode and output nodes

- Status: Draft
- Owner crate(s): `rbs-nodes`, `rbs-gpu` (`upload`, `download`)
- Related ADRs: [0013](../adr/0013-zero-cost-when-unused.md), [0020](../adr/0020-node-contract-and-schedule-swap.md), [0024](../adr/0024-node-restart-with-back-off.md), [0025](../adr/0025-encoder-surface-export.md), [0026](../adr/0026-partitions-and-queues.md), [0038](../adr/0038-io-graph-nodes.md)
- Builds on: [media graph](media-graph.md) (node contract, partitions), [capture port](capture-port.md), [output ports](output-ports.md), [GPU compositor](gpu-compositor.md)

## Purpose

The built-in graph nodes that connect the ports to the media graph, so that a scene can be captured, composited, encoded and recorded or streamed with any adapter.

## Scope

In scope: the capture source, video export, video encoder and output nodes; what each does in `prepare`, `warm`, `process`, `cool` and `release`; their errors and counters.

Out of scope: audio nodes (audio input, mixer, audio encoder; ADR-0037), the scene-source registry that creates capture nodes from scene sources, choosing adapters (composition root), the control API.

## Contract

```rust
// rbs_nodes::capture
pub const CAPTURE_NODE_TYPE: &str = "org.rbs.capture";      // Source; output "video"
pub fn capture_descriptor() -> NodeDescriptor;
pub fn import_preferences(ctx: &GpuContext) -> (Vec<MemoryKind>, Option<GpuImportTarget>);
pub fn complete_request(ctx: &GpuContext, request: CaptureRequest) -> CaptureRequest;
impl CaptureSourceNode {                                      // Node<GpuRuntime>
    pub fn new(factory: Arc<dyn CaptureFactory>, request: CaptureRequest) -> Self;
    pub fn stats(&self) -> Arc<CaptureNodeStats>;             // frames, imported, uploaded, gpu_errors
}

// rbs_nodes::export
pub const VIDEO_EXPORT_NODE_TYPE: &str = "org.rbs.video-export"; // VideoFilter; "canvas" -> "video"
impl VideoExportNode {                                        // Node<GpuRuntime>
    pub fn new(format: YuvFormat, width: u32, height: u32) -> Self;
    pub fn frame_format(&self) -> VideoFormat;                // the encoder config's input format
    pub fn stats(&self) -> Arc<ExportNodeStats>;              // frames, busy, lost
}

// rbs_nodes::encode
pub const VIDEO_ENCODER_NODE_TYPE: &str = "org.rbs.video-encoder"; // Encoder; "video" -> "packets"
pub const CARRY_PACKETS: usize = 64;
impl VideoEncoderNode {                                       // Node<X> for every Runtime X
    pub fn open(factory: Arc<dyn VideoEncoderFactory>, config: VideoEncoderConfig)
        -> Result<Self, CreateError>;                         // creates the session now
    pub fn stream(&self) -> &StreamDescriptor;
    pub fn stats(&self) -> Arc<EncoderNodeStats>;             // submitted, packets, busy_drops, refused,
                                                              // overflow_drops, flush_lost
}

// rbs_nodes::output
pub const OUTPUT_NODE_TYPE: &str = "org.rbs.output";        // Output; inputs "s0", "s1", ...
pub fn output_descriptor(streams: usize) -> NodeDescriptor;
impl OutputNode {                                             // Node<X> for every Runtime X
    pub fn new(streams: Vec<StreamDescriptor>, factory: Arc<dyn TransportFactory>,
        config: TransportConfig) -> Self;
    pub fn with_muxer(self, factory: Arc<dyn MuxerFactory>, container: ContainerId,
        fragment_duration: Option<Duration>) -> Self;
    pub fn with_backlog(self, items: usize) -> Self;          // default 4096
    pub fn with_finish_timeout(self, timeout: Duration) -> Self; // default 5 s
    pub fn stats(&self) -> Arc<OutputNodeStats>;              // packets, sent, transport_drops, mux_drops,
                                                              // backlog_peak, keyframe_requests, finish_lost
}
```

A recording, one partition per encoder and per output (ADR-0026):

```
capture --video--> compositor --canvas--> video export ==video==> video encoder ==packets==> output
 (render)          (render)               (render)                (encoder)                  (output)
```

Counters are atomics in an `Arc`, read on any thread; nodes update them with relaxed adds.

### Capture source node

| Call | Does |
| --- | --- |
| `prepare` | Fills the request with [`complete_request`] (keeps what it already has), `CaptureFactory::open`, `CaptureSession::start` (may block up to 500 ms; control thread) |
| `process` | `latest_frame`: `None` keeps the last frame on the link. A `Native` frame goes through `RenderLocal::importer` (zero copy), a `System` frame through the node's `SourceUpload`; the `Texture` frame is set on "video" |
| `cool` | Returns the upload's plane textures to the pool |
| `release` | `stop`, then drops the session |

`import_preferences` decides by the device's graphics API: D3D12 offers `[D3d11, D3d12, System]` with `GpuImportTarget { Luid(adapter_luid), [TimelineFence, Ready] }`; Metal offers `[Metal, System]` (IOSurfaces need no device check); Vulkan and others `[System]` until DMA-BUF import is verified on hardware. Default pixel formats are `rbs_gpu::upload::UPLOAD_FORMATS` (`Nv12, P010, Bgra8, Rgba8`).

Invariants:

1. Nothing is opened before `prepare`; after `release` the session is stopped and dropped.
2. Every frame on "video" is a pool texture of the render runtime with the capture frame's timestamp, sequence and source time.
3. A frame that fails to import or upload is dropped (the producer gets its surface back) and counted; the last good frame stays on the link.

### Video export node

| Call | Does |
| --- | --- |
| `prepare` | Creates a `YuvDownload` for the size and format (converter, plane textures, staging ring, frame buffers) |
| `process` | 1. `take_ready`: the newest completed readback, if any, goes on "video". 2. A canvas frame not seen before (timestamp, sequence) is submitted |
| `release` | Drops the download |

Invariants:

1. Frames on "video" have `frame_format()`: canvas size, NV12 or P010, `ColorSpace::BT709_LIMITED`; the canvas frame's timestamp and sequence; system memory.
2. Never waits for the GPU. A frame submitted on tick `i` appears on tick `i + 1` at the earliest; a full staging ring drops the new canvas frame (`busy`).

### Video encoder node

| Call | Does |
| --- | --- |
| `open` | Creates the first session (control thread); its `stream()` is kept |
| `prepare` | Creates a session if `release` dropped it; refuses with `NodeError::Permanent` if its `stream()` differs from the first. Reserves the drain and carry-over buffers |
| `process` | Passes carried packets first. Submits a frame not seen before (timestamp, sequence) once; on `Busy` drains and retries once, then drops it (`busy_drops`). Drains and passes packets |
| `release` | Flushes and drops the session; flushed and carried packets are lost and counted (`flush_lost`) |

Invariants:

1. Each distinct input frame is submitted at most once.
2. Packets leave in the order the session produced them. At most `PER_TICK_EVENTS` per tick go on "packets"; up to `CARRY_PACKETS` more wait for the next ticks.
3. The packet stream is always decodable: when the carry-over is full, packets are dropped up to the next keyframe and `EncoderChange::RequestKeyframe` is sent.
4. A refused frame (`EncodeError::InvalidInput`) fails the tick (`NodeError::InvalidInput`); a device loss or non-recoverable failure is permanent.

### Output node

| Call | Does |
| --- | --- |
| `prepare` | Creates the muxer (required exactly when the transport's input is `Container(c)`, and only for that `c`) and the transport, `start(streams)`, `begin`, and sends the header, waiting up to the finish timeout for queue room |
| `process` | Sends the backlog first; then for each stream `i` in order, each packet of pin `s{i}`: muxed into chunks, or forwarded as a packet item. `QueueFull` appends to the backlog. Polls transport events |
| `release` | Delivers the backlog and the muxer's `finish` chunks within the finish timeout, then `Transport::finish` with the time left |

Invariants:

1. Items reach the transport in the order they were produced; the backlog keeps that order.
2. The backlog is preallocated; a full backlog fails the node permanently (no holes in a recording).
3. Any transport error other than `QueueFull`, any muxer error other than `NonMonotonic` (dropped, counted) and a `Disconnected` event fail permanently, so a file output is never restarted (which would truncate it).
4. After `release` the transport has been finished; undelivered items are counted (`finish_lost`).

## Errors and failure behaviour

Nodes report through `NodeError` (ADR-0020): `Permanent` for what retrying cannot fix (permissions, missing targets, unsupported formats, a changed encoder stream, a failed output), `Backend`/`Failed` for what may recover (a lost capture target, a busy device), which the controller restarts with back-off (ADR-0024).

## Performance requirements

- Hot paths ([hot-paths.md](../architecture/hot-paths.md)): every `process` above, `SourceUpload::upload`, `YuvDownload::submit` and `take_ready`.
- Encoder and output `process` allocate nothing in steady state (test `steady_state_ticks_do_not_allocate`). The GPU nodes allocate nothing in RBS code for a format and size they have seen; wgpu allocates internally while recording command buffers and for the map callback.
- The export's CPU copy is one `memcpy` of the frame (about 3 MB at 1080p NV12) per delivered frame on the render thread; measured in the PR that adds it.

## Conformance tests

- `crates/rbs-nodes/tests/encode_output.rs`: frames submitted once; header, media and trailer in order; backlog on `QueueFull` delivered in order; backlog overflow is permanent; carry-over in order; carry overflow resumes at a keyframe and requests one; a changed stream after restart is permanent; no allocation per tick.
- `crates/rbs-nodes/tests/pipeline_gpu.rs` (any GPU adapter; CI uses lavapipe, WARP and Metal): a BGRA window and an NV12 camera from the synthetic adapter go through capture, compositor and export, and arrive as NV12 with the right format, times and luma.
- `crates/rbs-gpu/tests/upload_download.rs`: download codes, busy ring, stale frames, buffer recycling, `frames_cpu_copied`; upload of padded BGRA rows and NV12.
- Hardware (manual, `crates/rbs-nodes/examples/record.rs`): screen and camera to an H.264 Matroska file with the platform's adapters.

## Open questions

- End of stream: frames an encoder holds back at `release` are lost; a final flush into the graph needs a drain hook in the engine (engine spec).
- Transport feedback (`KeyframeRequested`, `BitrateHint`) to the encoder goes through the control plane; not wired yet.
- Native export across the partition queue (move instead of share), D3D11/D3D12 input for NVENC and AMF, and a D3D12 exporter remove the readback on Windows (ADR-0038).
- YUY2 and I420 system frames (common on webcams) are not uploaded yet; a camera that offers only those fails `prepare` with `FormatUnsupported`.
