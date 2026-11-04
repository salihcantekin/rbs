# Spec: Capture, encode and output nodes

- Status: Draft
- Owner crate(s): `rbs-nodes`, `rbs-gpu` (`upload`, `download`)
- Related ADRs: [0013](../adr/0013-zero-cost-when-unused.md), [0028](../adr/0028-cross-process-preview-surfaces.md), [0036](../adr/0036-windows-preview-layer.md), [0041](../adr/0041-studio-recording-on-a-running-engine.md), [0020](../adr/0020-node-contract-and-schedule-swap.md), [0024](../adr/0024-node-restart-with-back-off.md), [0025](../adr/0025-encoder-surface-export.md), [0026](../adr/0026-partitions-and-queues.md), [0038](../adr/0038-io-graph-nodes.md)
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
    pub fn end(&self) -> Arc<ExportEnd>;                      // ADR-0044
}
impl ExportEnd {                                              // shared with the control thread
    pub fn end_at(&self, at: Timestamp);                      // no canvas after `at`; earliest wins
    pub fn requested(&self) -> Option<Timestamp>;
    pub fn ended(&self) -> bool;                              // every canvas taken is on "video"
}

// rbs_nodes::encode
pub const VIDEO_ENCODER_NODE_TYPE: &str = "org.rbs.video-encoder"; // Encoder; "video" -> "packets"
pub const CARRY_PACKETS: usize = 64;
impl VideoEncoderNode {                                       // Node<X> for every Runtime X
    pub fn open(factory: Arc<dyn VideoEncoderFactory>, config: VideoEncoderConfig)
        -> Result<Self, CreateError>;                         // probes a session, then drops it
    pub fn stream(&self) -> &StreamDescriptor;
    pub fn stats(&self) -> Arc<EncoderNodeStats>;             // submitted, packets, busy_drops, refused,
                                                              // overflow_drops, flush_lost, flushes
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
    pub fn with_wait_clock(self, clock: Arc<dyn WaitClock>) -> Self; // default SystemWaitClock
    pub fn stats(&self) -> Arc<OutputNodeStats>;              // packets, sent, transport_drops, mux_drops,
                                                              // backlog_peak, keyframe_requests, finish_lost, start_drops,
                                                              // failure: OnceLock<String>
}
/// Time source of the blocking waits in `prepare` and `release` (control thread).
pub trait WaitClock: Send + Sync {
    fn now(&self) -> Timestamp;
    fn pause(&self, duration: Duration);                       // tests advance a TestClock
}
pub struct SystemWaitClock;                                   // MonotonicClock + thread::sleep
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
| `release` | `stop`, then drops the session and the upload's decoder |

`import_preferences` decides by the device's graphics API: D3D12 offers `[D3d11, D3d12, System]` with `GpuImportTarget { Luid(adapter_luid), [TimelineFence, Ready] }`; Metal offers `[Metal, System]` (IOSurfaces need no device check); Vulkan and others `[System]` until DMA-BUF import is verified on hardware. Default pixel formats are `rbs_gpu::upload::UPLOAD_FORMATS` (`Nv12, P010, Bgra8, Rgba8`), without P010 when the device lacks `gpu.p010`.

Invariants:

1. Nothing is opened before `prepare`; after `release` the session is stopped and dropped.
2. Every frame on "video" is a pool texture of the render runtime with the capture frame's timestamp, sequence and source time.
3. A frame that fails to import or upload is dropped (the producer gets its surface back) and counted; the last good frame stays on the link. `GpuError::Unsupported` and `InvalidSize` fail permanently (the source keeps sending what the device cannot take); other errors are `Backend`.

### Video export node

| Call | Does |
| --- | --- |
| `prepare` | Creates a `YuvDownload` for the size and format (converter, plane textures, staging ring, frame buffers) |
| `prepare` (again) | Clears `ExportEnd::ended`; the end time stays |
| `process` | Not ending: 1. `take_ready`: the newest completed readback, if any, goes on "video". 2. A canvas frame not seen before (timestamp, sequence) is submitted. Ending (`ExportEnd::end_at`, ADR-0044): 1. `take_oldest`: the oldest completed readback goes on "video" (none is dropped as stale). 2. Only a canvas at or before the end time is submitted. 3. With no readback in flight, `ended` is set |
| `release` | Drops the download |

Invariants:

1. Frames on "video" have `frame_format()`: canvas size, NV12 or P010, `ColorSpace::BT709_LIMITED`; the canvas frame's timestamp and sequence; system memory.
2. Never waits for the GPU. A frame submitted on tick `i` appears on tick `i + 1` at the earliest; a full staging ring drops the new canvas frame (`busy`).
3. Once ended, every canvas the export took up to the end time is on "video" in order (one per tick), and no later canvas is taken. The run path costs one relaxed atomic load and a compare per tick.

### Video encoder node

| Call | Does |
| --- | --- |
| `open` | Creates a session (control thread), keeps its `stream()` and drops it: an installed node holds no encoder (P13) |
| `prepare` | Creates the session; refuses with `NodeError::Permanent` if its `stream()` differs from the probed one, or if creation fails with anything but `ResourceExhausted`. Reserves the drain and carry-over buffers |
| `process` | Passes carried packets first. Submits a frame not seen before (timestamp, sequence) once; on `Busy` drains and retries once, then drops it (`busy_drops`); a refused frame is dropped (`refused`). Drains and passes packets. On a final tick (`Tick::draining`, ADR-0043) it flushes instead of draining, passes every packet (carrying what the queue cannot take now), counts `flushes` and submits nothing afterwards; the flush may wait for the encoder, bounded by the drain timeout |
| `release` | Flushes and drops the session; packets flushed here (the node left without a final tick: a live edit or a timed-out drain) and carried packets are lost and counted (`flush_lost`) |

Invariants:

1. Each distinct input frame is submitted at most once.
2. Packets leave in the order the session produced them. At most `PER_TICK_EVENTS` per tick go on "packets"; up to `CARRY_PACKETS` more wait for the next ticks.
3. The packet stream is always decodable: when the carry-over is full, packets are dropped up to the next keyframe and `EncoderChange::RequestKeyframe` is sent.
4. A refused frame (`EncodeError::InvalidInput`) is counted and does not fail the tick: a failed tick discards the packets the node already pushed in it, which would break the stream. A device loss or non-recoverable failure is permanent.

### Output node

| Call | Does |
| --- | --- |
| `prepare` | Refuses permanently once the node wrote media (a restart would truncate the destination). Creates the muxer (required exactly when the transport's input is `Container(c)`, and only for that `c`) and the transport, `start(streams)`, `begin`, and sends the header, waiting up to the finish timeout for queue room on the wait clock |
| `process` | Sends the backlog first; then for each stream `i` in order, each packet of pin `s{i}`: muxed into chunks, or forwarded as a packet item (after the aligned start below). `QueueFull` appends to the backlog. Polls transport events |
| `release` | Writes packets still held for the aligned start (no video packet ever came), then delivers the backlog and the muxer's `finish` chunks within the finish timeout, then `Transport::finish` with the time left |

Invariants:

1. Items reach the transport in the order they were produced; the backlog keeps that order.
2. The backlog is preallocated; a full backlog fails the node permanently (no holes in a recording).
3. Any transport error other than `QueueFull`, any muxer error other than `NonMonotonic` (dropped, counted) and a `Disconnected` event fail permanently, so a file output is never restarted (which would truncate it).
4. After `release` the transport has been finished; undelivered items are counted (`finish_lost`).
5. Muxer and transport creation errors and `begin` errors are permanent; a `start` error is retried only when `TransportError::is_recoverable`.
6. The first permanent failure's cause (for example `muxer: invalid packet: ...`) is kept in `failure`; the string is built only on that path.
7. **Aligned start.** An output with a video stream and other streams starts at the first video packet's pts: packets of other streams that end at or before it (pts plus duration, or pts when the packet has no duration) are dropped and counted (`start_drops`); packets that arrive before the first video packet are held (at most `START_HOLD` = 256, preallocated in `prepare`; past it the oldest are dropped and counted) and written when it arrives, in arrival order. So a recording never begins with audio before the first picture (video started 143 ms after audio on SC-PC, 3440x1440 NVENC,). Outputs with only video or no video pass every packet.

### Preview node (ADR-0041)

```rust
// rbs_nodes::preview
pub const PREVIEW_NODE_TYPE: &str = "org.rbs.preview";
pub fn preview_descriptor() -> NodeDescriptor;       // Output, input "canvas" (Video), no outputs
impl PreviewNode {
    pub fn new(desc: PreviewDesc) -> Self;            // configuration only (Installed)
    pub fn stats(&self) -> Arc<PreviewNodeStats>;
}
impl PreviewNodeStats {                               // relaxed atomics, endpoint behind a mutex
    pub fn endpoint(&self) -> Option<PreviewEndpoint>; // Some while prepared
    pub subscribed: AtomicBool, pub published: AtomicU64, pub busy: AtomicU64, pub errors: AtomicU64,
}
```

Render partition, `GpuRuntime`. An `Output`, so a canvas that nothing records stays scheduled while a UI shows it.

| Call | Does |
| --- | --- |
| `prepare` | Creates a `PreviewPublisher` (`rbs_gpu::share`, ADR-0028 on macOS, ADR-0036 on Windows) on the render device and stores its endpoint in the stats (control thread). On other platforms fails with `Permanent` |
| `process` | For a canvas frame not seen before (by timestamp and sequence): `publish` it (nothing is converted while no UI is subscribed; `ExportBusy` when the UI holds every surface is counted in `busy`, the frame dropped). Then `tick` the publisher every tick (accepts a UI, reads releases, sends finished frames). A lost device is `Backend`; other publish or tick errors are counted in `errors` |
| `release` | Drops the publisher (the UI's connection ends) and clears the endpoint |

Invariants: no CPU readback (P4); `process` does not allocate in RBS code and never waits (the publisher's hot-path rules); the endpoint is written only on the control thread; a restart creates a new endpoint (the UI reconnects).

## Errors and failure behaviour

Nodes report through `NodeError` (ADR-0020): `Permanent` for what retrying cannot fix (permissions, missing targets, unsupported formats, a changed encoder stream, a failed output), `Backend`/`Failed` for what may recover (a lost capture target, a busy device), which the controller restarts with back-off (ADR-0024).

## Performance requirements

- Hot paths ([hot-paths.md](../architecture/hot-paths.md)): every `process` above (the preview node's is `PreviewPublisher::publish` and `tick` plus a key comparison), `SourceUpload::upload`, `YuvDownload::submit` and `take_ready`.
- Encoder and output `process` allocate nothing in steady state (test `steady_state_ticks_do_not_allocate`). The GPU nodes allocate nothing in RBS code for a format and size they have seen; wgpu allocates internally while recording command buffers and for the map callback.
- The export's CPU copy is one `memcpy` of the frame (about 3 MB at 1080p NV12) per delivered frame on the render thread; measured in the PR that adds it.

## Conformance tests

- `crates/rbs-nodes/tests/encode_output.rs`: frames submitted once; header, media and trailer in order; backlog on `QueueFull` delivered in order; backlog overflow is permanent; carry-over in order; carry overflow resumes at a keyframe and requests one; a changed stream after restart is permanent; no session while installed or in Standby; a refused frame loses no packet; an output that wrote media refuses to restart; a failure keeps its cause; `release` gives up on a full queue on the wait clock without sleeping; no allocation per tick.
- `crates/rbs-nodes/tests/pipeline_gpu.rs` (any GPU adapter; CI uses lavapipe, WARP and Metal): a BGRA window and an NV12 camera from the synthetic adapter go through capture, compositor and export, and arrive as NV12 with the right format, times and luma; an ended export passes on every canvas up to the end and none after (ADR-0044).
- `crates/rbs-gpu/tests/upload_download.rs`: download codes, busy ring, stale frames, buffer recycling, `frames_cpu_copied`; upload of padded BGRA rows and NV12.
- Hardware (manual, `crates/rbs-nodes/examples/record.rs`): screen and camera to an H.264 Matroska file with the platform's adapters.

## Open questions

- Encoders with deep look-ahead or B-frames flush more packets than one final tick on a cadence partition can pass (`PER_TICK_EVENTS`); the rest is counted in `flush_lost`. Today's hardware adapters hold at most their few slots (ADR-0043). More final ticks while a node has output pending would close this.
- Transport feedback (`KeyframeRequested`, `BitrateHint`) to the encoder goes through the control plane; not wired yet.
- Outputs that may restart safely (network transports) need a transport capability; until then every output that wrote media is single-use ([#84](https://github.com/salihcantekin/rbs/issues/84)).
- Native export across the partition queue (move instead of share), D3D11/D3D12 input for NVENC and AMF, and a D3D12 exporter remove the readback; the export node then chooses by the encoder's memory kinds (ADR-0038, [#83](https://github.com/salihcantekin/rbs/issues/83)).
- YUY2 and I420 system frames (common on webcams) are not uploaded yet; a camera that offers only those fails `prepare` with `FormatUnsupported` ([#85](https://github.com/salihcantekin/rbs/issues/85)).
