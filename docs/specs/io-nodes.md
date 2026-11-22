# Spec: Capture, encode and output nodes

- Status: Draft
- Owner crate(s): `rbs-nodes`, `rbs-gpu` (`upload`, `download`)
- Related ADRs: [0013](../adr/0013-zero-cost-when-unused.md), [0028](../adr/0028-cross-process-preview-surfaces.md), [0036](../adr/0036-windows-preview-layer.md), [0041](../adr/0041-studio-recording-on-a-running-engine.md), [0020](../adr/0020-node-contract-and-schedule-swap.md), [0024](../adr/0024-node-restart-with-back-off.md), [0025](../adr/0025-encoder-surface-export.md), [0026](../adr/0026-partitions-and-queues.md), [0038](../adr/0038-io-graph-nodes.md), [0102](../adr/0102-output-size-distinct-from-canvas.md)
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
    pub fn open_inline(self, inline: bool) -> Self;           // open in prepare (capture.start.instant)
    pub fn stats(&self) -> Arc<CaptureNodeStats>;             // frames, imported, uploaded, gpu_errors,
                                                              // picture_size() (ADR-0058), fault,
                                                              // warm_up() (ADR-0152 warm_up_ms)
}
pub const START_LIMIT: Duration;                              // 15 s (ADR-0152 decision 5)

// rbs_nodes::export
pub const VIDEO_EXPORT_NODE_TYPE: &str = "org.rbs.video-export"; // VideoFilter; "canvas" -> "video"
impl VideoExportNode {                                        // Node<GpuRuntime>
    pub fn new(format: YuvFormat, canvas_width: u32, canvas_height: u32, width: u32, height: u32) -> Self;
    // `with_color` and `with_tone` take the same canvas size and output size.
    // `width` x `height` is the encoded picture (ADR-0102). Equal to the canvas: 1:1.
    pub fn frame_format(&self) -> VideoFormat;                // the encoder config's input format
    pub fn kernel(&self) -> ScaleKernel;                      // after prepare, the download's kernel; before, scale_kernel of the sizes
    pub fn give_way_after(self, budget: Duration) -> Self;   // ADR-0086 decision 9; default never
    pub fn yield_to(self, master_drops: MasterDrops, hold_ticks: u64) -> Self; // ADR-0086 decision 9
    pub fn every_tick(self) -> Self;                          // ADR-0086: every tick, stamped with the tick
    pub fn stats(&self) -> Arc<ExportNodeStats>;              // frames, busy, lost, gave_way
    pub fn end(&self) -> Arc<ExportEnd>;                      // ADR-0045
    pub fn native(self, native: NativeExport) -> Self;        // ADR-0147: exported surfaces, exclusive "video"
    pub fn is_native(&self) -> bool;
}
pub struct NativeExport { pub encoder_frames: usize, pub owners: Arc<LeaseOwners> } // ADR-0147
impl LeaseOwners {                                            // kept by the composition root
    pub fn keep(&self, owner: Arc<dyn LeaseOwner>);           // one per native `prepare`
    pub fn kept(&self) -> usize;
    pub fn take_all(&self) -> Vec<Arc<dyn LeaseOwner>>;       // dropped on the control thread
}
pub const NATIVE_HOLDERS: usize = 6;                          // ADR-0147 decision 4
pub fn export_preference(ctx: &GpuContext) -> Option<(MemoryKind, GpuDeviceId)>; // by graphics API
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
    pub fn frames_held(&self) -> usize;                       // the probe's max_frames_held (ADR-0147)
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
                                                              // connects, reconnect_attempts, connected: AtomicBool,
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
| `prepare` | Fills the request with [`complete_request`] (keeps what it already has), notes the master time, and starts a thread (`rbs-capture-open`) that runs `CaptureFactory::open` and `CaptureSession::start` and pushes the result into a one-slot `rtrb` ring. Returns without waiting (ADR-0152 decision 5). A node built with `open_inline(true)` (the registry does this for an adapter declaring `capture.start.instant`) opens and starts on the calling thread instead, as before |
| `process` | While no session runs: pops the ring (wait-free); a session is taken and used on the same tick, an error is mapped as below and returned on every tick until the executor bypasses the node (ADR-0024 restarts it), nothing yet leaves the output empty. Past `START_LIMIT` (15 s on the master clock) without a result: `Backend("capture start timed out")` and fault `Other` (ADR-0152's `start_timeout`). Then `latest_frame`: `None` keeps the last frame on the link. A `Native` frame goes through `RenderLocal::importer` (zero copy), a `System` frame through the node's `SourceUpload`; the `Texture` frame is set on "video". The first frame after `prepare` stores `warm_up_us` (its capture time minus the `prepare` time) |
| `process`, mapping | With a cell lent by `with_mapping_cell` and a switch in the request (ADR-0165 decision 5): after each new frame, one atomic load of the switch; while it is on, `latest_mapping` goes into the cell; when it went off, `None` once. Nothing per tick without a new frame, nothing without a cell |
| `cool` | Returns the upload's plane textures to the pool |
| `release` | `stop`, then drops the session, the ring and the upload's decoder, and clears a cell it published to while the cell still holds its own last mapping. An open still running sees the ring abandoned and stops the session it made on its own thread |

`import_preferences` decides by the device's graphics API: D3D12 offers `[D3d11, D3d12, System]` with `GpuImportTarget { Luid(adapter_luid), [TimelineFence, Ready] }`; Metal offers `[Metal, System]` (IOSurfaces need no device check); Vulkan and others `[System]` until DMA-BUF import is verified on hardware. Default pixel formats are `rbs_gpu::upload::UPLOAD_FORMATS` (`Nv12, P010, Bgra8, Rgba8`), without P010 when the device lacks `gpu.p010`.

Invariants:

1. Nothing is opened before `prepare`; after `release` the session is stopped and dropped (by the opening thread when its open returns after `release`).
2. Every frame on "video" is a pool texture of the render runtime with the capture frame's timestamp, sequence and source time.
3. A frame that fails to import or upload is dropped (the producer gets its surface back) and counted; the last good frame stays on the link. `GpuError::Unsupported` and `InvalidSize` fail permanently (the source keeps sending what the device cannot take); other errors are `Backend`.
4. One device is never opened twice at once: a `prepare` while the last open still runs starts a thread that first joins the previous one. If that open is older than `START_LIMIT`, `prepare` fails instead (`Failed`, fault `Other`) and is retried with back-off, so a hung device costs one thread.
5. `process` never waits on the opening thread, never allocates and never frees the ring (it is dropped in `release`). Tests: `crates/rbs-nodes/tests/capture_open.rs` (a gated `start`: `prepare` returns at once, the first frame follows the open with its warm-up, a never-ending open fails after the start limit and a restart starts no second thread, a release while opening stops the session, a source shown again opens after the last open).

### Still source node (ADR-0074)

`rbs_nodes::StillSourceNode` (`org.rbs.still`, kind `Source`, output "video", render partition). `StillSourceNode::new(width, height, rgba)` takes straight-alpha sRGB `Rgba8` pixels (`StillError::Empty` for a zero side, `WrongLength` unless `width * height * 4` bytes). It holds the pixels and nothing on the GPU until it is warmed.

| Call | Does |
| --- | --- |
| `prepare` | Nothing |
| `warm` | Uploads the picture with `SourceUpload` into a pool texture (not real time); the frame's sequence is a new generation (`rbs_core::media::next_generation()`, ADR-0117 decision 4), so each upload has a key no earlier frame had, though the timestamp is always zero |
| `process` | Publishes the uploaded frame once; every later tick returns at once and the output slot keeps the frame (P13) |
| `cool`, `release` | Drop the frame and give the textures back |

Counters are those of the capture node: `picture_size` is known from construction, `uploaded` and `frames` count one per warm-up. Test: `crates/rbs-nodes/tests/still_gpu.rs` (one upload and one publication over many ticks; the exported canvas shows the picture; an upload after Standby has a new generation).

### Text source node (ADR-0129)

`rbs_nodes::TextSourceNode` (`org.rbs.text`, kind `Source`, output "video", render partition). `TextSourceNode::new(initial: Option<TextFrame>) -> (node, ParamSender<Option<TextFrame>>)`: the text worker keeps the only sender (ADR-0023 parameter channel). `TextFrame::new(request, settings_generation, Arc<TextPicture>)` is built on the worker; its system frame shares the picture's pixels (`Bytes::from_owner`), so the render thread never copies or allocates a picture.

| Call | Does |
| --- | --- |
| `prepare` | Nothing |
| `warm` | Takes the channel's newest picture; if there is one within the caps (`rbs_text::MAX_SIDE`, `MAX_PIXELS`), reserves two textures of its size (`ReservedUpload`, gpu-compositor spec) and uploads it, outside the upload budget. A node with no picture publishes nothing (the layer is not drawn) |
| `process` | 1. A frame `warm` uploaded is published with the tick's time. 2. `ParamReceiver::update` (one atomic load when nothing is new). 3. Nothing pending: return. 4. Over the caps: held, counted in `over_cap`, never uploaded. 5. Box size changed: give the textures back and reserve two of the new size (not real time, once per size change); a failure is counted in `reserve_failures`, retried after `RESERVE_FAULT_AFTER` (5 s) or on a new picture, and a failure still there 5 s after the first sets `fault` to `SourceFault::Other`. 6. `upload_budget.try_take(tick, bytes, waiting_since)`; refused: remember the first tick waited on, count `budget_deferred`, try next tick. 7. `upload_into` the free texture; both leased (an export holds one): count `upload_deferred`, keep the picture for the next tick, never a third texture, never a wait. 8. Publish with the tick's time and a new generation, store `request` in `uploaded_request` (Release) |
| `cool` | Gives both textures back; the next `warm` uploads the current picture again |
| `release` | Drops everything |

`TextNodeStats`: `frames`, `uploaded`, `uploaded_request` (what the host waits on before a switch, ADR-0129 decision 7), `upload_deferred`, `budget_deferred`, `over_cap`, `reserve_failures`, `gpu_errors`, `picture_size`, `fault`. Tests: `crates/rbs-nodes/tests/text_gpu.rs` (no picture publishes nothing; each new picture is uploaded once with a new generation, and a size change re-reserves; a picture over the caps is held; three 8 MiB pictures take two ticks under the budget; Standby gives the textures back and `warm` uploads again; a steady text node adds no allocation to a tick, measured against a still source).

### Slide show source node (ADR-0171)

`SlideshowNode` (`rbs-nodes` `slideshow.rs`, type `org.rbs.slideshow`, kind `Source`, one output `video`) shows the pictures of a slide show at a fixed output size.

- **Inputs.** `SlideFrame`s arrive through a parameter channel (ADR-0023) whose sender the decode thread keeps. Each one is fitted pixels shared through an `Arc` (`Bytes::from_owner`), with a `serial` (never 0), `now` (show at once) and `cut` (show without a change). A displaced frame goes back to the sender and drops on the decode thread. `SlideshowShared` holds atomics only:
  - `paused`, `interval_ms` (0 for manual) and `transition_ms`, written by the host;
  - `shown` and `shown_at_ns`, written by the node;
  - the counters `frames`, `uploaded`, `changes`, `budget_deferred`, `upload_deferred`, `draw_skipped` and `gpu_errors`.
- **`warm` and `cool`.** `warm` reserves two `ReservedUpload` textures at the output size. When the transition is not a cut it also reserves `ReservedTargets` and a `SlidePass`. `cool` gives all of them back; the picture is uploaded again on the next `warm`, without a change.
- **`process`.**
  1. While a change runs, it draws the change each tick (`SlidePass::draw`; a tick whose targets are both read is skipped and counted). When `change_length()` has passed, it publishes the incoming picture. Nothing is uploaded while a change runs.
  2. Otherwise a new frame is uploaded ahead of its change, under the partition's upload budget, into the picture texture no reader leases. The node keeps only texture ids; the output slot's lease protects the picture on screen.
  3. An uploaded picture changes when it is the first, when it is marked `now`, or when the interval has passed and the node is not paused. A first picture or a `cut` one is published at once; otherwise a change starts.
  4. A pause restarts the interval when the timer resumes.
- **Other rules.** `change_length()` is `transition_ms`, at most half the interval. Every published frame gets a new `next_generation()` sequence (ADR-0117 decision 4). Between changes a tick is a few atomic loads and a compare, with no allocation.
- **Tests.** `crates/rbs-nodes/tests/slideshow_gpu.rs` covers:
  - nothing shows before the first picture, which shows without a change;
  - the next picture is uploaded ahead and shown on time;
  - a fade draws for its length;
  - a picture sent during a change waits for it to end;
  - paused and manual hold, and a command still shows at once;
  - the cap at half the interval;
  - Standby gives everything back;
  - steady ticks allocate nothing (baseline: a still node).

### Image track node (ADR-0117)

`rbs_nodes::ImageTrackNode` (`org.rbs.image-track`, kind `VideoFilter`, input "picture", output "video", render partition) writes a still source's picture as images, one readback per change. `new(width, height)` sizes its `RgbaDownload`; `give_way_after(budget)` and `yield_to(master_drops)` set the ADR-0086 decision 9 rules.

| Call | Does |
| --- | --- |
| `prepare` | Creates the `RgbaDownload`; forgets the last picture taken |
| `process` | 1. A completed readback goes on "video" (`System`, `Rgba8`, sRGB, straight alpha, stamped with the tick's time and index of the submit). 2. The input's `(timestamp, sequence)` equal to the last taken: nothing more (one compare). 3. Otherwise, at `MAX_IMAGES_PER_TRACK` (1 000) taken: counted in `over_cap`, not taken. 4. Inside `MIN_IMAGE_INTERVAL` (100 ms of tick time) of the last taken, with a readback in flight, or giving way (the previous render tick over the budget, or `master_drops` grew): the change waits for a later tick, counted once in `coalesced` (`gave_way` counts the ticks it gave way); a newer change replaces it. 5. Otherwise `submit_at` with the tick's time and index (`taken`). A failed conversion is counted in `failed` and not retried for that picture |
| `release` | A readback still in flight is counted in `failed` |

Pause (ADR-0124, `with_pause(window)`, separate files of a pausable recording): in step 4 the window is read (only when the picture changed, so the steady state stays one compare); a change on a tick inside the window waits like a change inside the interval and is taken on the first tick after it, at the join. Every picture taken is stamped earlier by the time left out, so the writer's `at_ns` is file time.

Test: `crates/rbs-nodes/tests/image_track_gpu.rs` (`a_change_inside_a_pause_*`; an unchanged picture is taken once over 30 ticks; a change on every tick for one second at 60 fps gives one image per interval, each change in between waits and is counted, images stamped at least 100 ms apart, bytes copied as published).

### Sync picture probe (ADR-0126)

`rbs_nodes::SyncPictureProbeNode` (`org.rbs.sync-picture-probe`, kind `Output`, input "picture", render partition) logs the motion of a reference picture while a sync offset measurement runs. `new(window)` returns the node and its `Arc<SyncPictureLog>`, sized for `window` at `SYNC_PICTURE_MAX_FPS` (240) new pictures per second, plus one.

| Call | Does |
| --- | --- |
| `prepare` | Creates its `MotionProbe` (gpu-compositor spec, "Motion probe"); forgets the last picture |
| `process` | 1. Completed values go into the log (`take`). 2. The input's `(timestamp, sequence)` equal to the last seen: nothing more (one compare), so a camera slower than the canvas gives one value per new frame. 3. Otherwise `submit` stamped with the tick's time, the time the program shows that picture at. A full ring is counted in the log's `busy`; a failed dispatch in `failed` and returns `Backend` |
| `release` | Drops the probe; values still in flight are lost |

`SyncPictureLog` is a fixed array of atomics with a published length (release/acquire), one writer, any number of readers: `capacity`, `len`, `is_empty`, `full` (values not logged because it was full; it never wraps), `busy`, `failed`, `read_from(from, &mut Vec<(i64, f32)>) -> usize` (returns the next `from`). It mirrors the audio probe's log (`rbs_audio::SyncProbeLog`); the two crates do not depend on each other.

Test: `crates/rbs-nodes/tests/sync_probe_gpu.rs` (a 30 fps feed on a 60 fps cadence, alternating dark and light: one value per new picture, each stamped with the tick it arrived on, every value a full change, nothing busy or failed).

### Video export node

| Call | Does |
| --- | --- |
| `prepare` | Creates a `YuvDownload` for the output size and format (converter, plane textures, staging ring, frame buffers). Output size equal to the canvas: `with_tone` (1:1 `textureLoad`). A different size: `scaled_tone` from the canvas size to the output size, one pass (ADR-0102) |
| `prepare` (again) | Clears `ExportEnd::ended`; the end time stays |
| `process` | Not ending: 1. `take_ready`: the newest completed readback, if any, goes on "video". 2. A canvas frame not seen before (timestamp, sequence) is submitted. A canvas already submitted (render on change left it unchanged, R-03) is queued again as a repeat entry (`YuvDownload::submit_repeat`) stamped with the tick's time and a new sequence (`next_generation()`, as the hold path does): no conversion and no readback, and the encoder still gets one frame per tick (ADR-0143 R5; counted in `ExportNodeStats::repeated`). A canvas counts as converted only when `submit_at` took it (`Ok(true)`), not inside a pause window, when giving way or when the ring was full. After a readback that never reached the output (`no_buffer` or `failed` grew), the next tick converts the canvas again instead of repeating; a refused repeat (`busy`) changes nothing. A hold tick also forgets the last canvas, so the program is converted again after a hold. A canvas drawn on an earlier tick and converted again (after a hold or a lost readback) takes the tick's time and a new sequence, like a repeat, so times never go back. Ending (`ExportEnd::end_at`, ADR-0045): 1. `take_oldest`: the oldest completed readback goes on "video" (none is dropped as stale). 2. Only a canvas at or before the end time is submitted. 3. On a tick past the end with no readback in flight, `ended` is set. Every delivered frame's time is stored in `ExportNodeStats::last_frame`. Giving way (`give_way_after`, only the separate video files set it): when `Tick::previous_busy` is over the budget, step 2 does not submit the canvas and counts `gave_way`; readbacks still go on "video". Yielding (`yield_to`, the separate video files): when the master's drop count (`MasterDrops`, read once per tick, atomics only) has grown since the last tick, step 2 is skipped for that tick and the rest of the hold, counted in `gave_way`. The hold is `hold_ticks`; a drop within `hold_ticks` after a hold ended doubles it, up to eight times `hold_ticks`, and a later drop starts again from `hold_ticks`. Every tick (`every_tick`, the separate video files): step 2 takes the canvas on each tick, and the frame carries the tick's time and index, so a still or slower source gives one frame per tick; a picture already converted is a repeat entry, not a new conversion (E-02 of the November 2026 audit). The default budget is never reached and the default takes each canvas once with its own time, so the master's export makes one branch (it has no track rules) and no other change |
| Shared readback | `with_share(Arc<DownloadShare>)` (perf audit R-01): `prepare` makes the download with `YuvDownload::scaled_tone_shared`, so exports of one share with the same shape convert and read a canvas picture back once; each still submits, repeats, gates and stamps on its own, and a picture it queued that another already queued counts in `ExportNodeStats::joined`. Without a share the export has a readback of its own, as before |
| `release` | Counts readbacks still in flight in `released` (not in `lost`: the stop cut them, so the stream and recording summaries show no drop for them), then drops the download |

Invariants:

1. Frames on "video" have `frame_format()`: the output size (the canvas size when they are equal), NV12 or P010, and the colour given to `new` (default `ColorSpace::BT709_LIMITED`) or `with_color` (`ColorSpace::SRGB_VIDEO` for a screen match); the canvas frame's timestamp and sequence (a repeat: the tick's time and a new sequence; an ISO track's frames: the tick's time and index); system memory. The planes are encoded with that same colour. `with_tone(..., open_shadows)` lifts only the darkest tones before a Rec. 709 encode and still tags `BT709_LIMITED`. A different output size is scaled once from the linear canvas inside that download; the node does not read another export's planes, except through a shared readback of the same shape (`with_share`), where both take the same frame buffer.
2. Never waits for the GPU. A frame submitted on tick `i` appears on tick `i + 1` at the earliest; a full staging ring drops the new canvas frame (`busy`).
3. Once ended, every canvas the export took up to the end time is on "video" in order (one per tick), and no later canvas is taken. Canvases dropped as on the run path (`busy`, no free buffer, a failed readback) are counted in `busy` or `lost`. Whether the encoder took the last one is the studio's check (`EncoderNodeStats::last_frame`, recording spec 5a). The run path costs one relaxed atomic load and a compare per tick.

Built (ADR-0124; `VideoExportNode::with_pause`, `with_hold`): the run-path atomic is `next`, the earliest of the end time and the next pause window instant (lowered by compare-and-swap; only the export raises it again, by compare-and-swap from the value it observed; only the end path sets `ended`), so the run path keeps one load and one compare. With `PauseGate` the export takes no canvas with a time in `[pause_at, resume_at)` and stamps later frames `t - paused_total`. With `with_hold(Arc<ExportHold>)` the export reads the hold switch only when `next` is lowered (`Studio::hold_output` lowers it to zero), so the run path is unchanged. While held it submits nothing; on the hold's first tick the newest readback goes out as on the run path and is pinned as the frozen picture (no copy), and on every later tick the hold picture (frozen, or the `ExportHold`'s black frame, made on the control thread by `black_frame(format)` for NV12 or P010 in either range) goes out with the previous tick's time and a new sequence. The ring works one buffer short. On the first tick after the hold it sends the hold picture once more when no readback is ready, drops the pin and raises `next` by compare-and-swap (and lowers it again if a hold was taken between its read and the swap). **Hold scene (phase 2b).** `with_hold` also gives the export the host-only input "hold" (`HOLD_PIN`, `video_export_hold_descriptor`), linked only while a hold scene is bound. While the switch says `Scene`, each hold tick submits the canvas on "hold" (once per timestamp and sequence) instead of the program's, and each scene frame read back becomes the held picture (the frozen program frame's buffer goes back then); until the first one, and on a tick whose readback is late, the last picture goes out again, always with the previous tick's time and a new sequence. The export knows which pin a ring slot came from by counting: slots complete in submission order, so it keeps how many program slots were in flight when the hold began and drops that many before taking a scene frame, with no change to `rbs-gpu`. On release every readback still in flight came from the hold path: the export counts them and drops them as they complete, staying on its slow path (`wake_at(0)`) until they are gone; the first tick after the hold sends the last held picture once, as above. A test (`pipeline_gpu`, `a_scene_hold_sends_the_scene_with_running_times_across_both_edges`) checks strictly increasing times one frame apart across both edges, with readbacks left in flight there, and no program frame inside the scene run. `cargo bench -p rbs-nodes --bench export` measures one tick for `run`, `freeze` and `scene`.

**Native mode** ([ADR-0147](../adr/0147-native-encoder-input.md); `VideoExportNode::native`, built on macOS and Windows). The composition root chooses it on the control thread when `export_preference` names a native memory kind of the render device (Metal: IOSurfaces, `metal`; D3D12: shared buffers, `d3d12_buffer`, with the render adapter's LUID as `GpuDeviceId::Luid`) and the encoder node's `open` probe accepts a config with that kind and `device` (decision 1); otherwise the chain reads back as above. Then:

- The "video" output pin is exclusive (`Pin::exclusive`, decision 3): one link, to an encoder node only. Native mode is not combined with a shared readback (`with_share`); each native chain has its own exporter (decision 11).
- `prepare` creates the platform's `rbs-gpu` exporter (`PlatformExporter`: `SurfaceExporter` or `D3dBufferExporter`; scaled when the output size differs) with a ring of `NATIVE_HOLDERS + encoder_frames` surfaces, plus two (the pinned picture and a black surface) when the export has a hold (decision 4). More than `MAX_EXPORT_SURFACES` fails permanently. The exporter's lease owner goes into `NativeExport::owners`, which the root drops on the control thread after the chain's nodes are released (decision 8). The black surface is cleared once in `prepare` (`black`), not on the control thread.
- `process` follows the same rules as the readback (`NativeDownload` over `ExportQueue`): busy, repeat entries for an unchanged canvas (the previous exported frame shared again with the tick's time and a new sequence), hold freeze, hold scene and black, and the ending path (`take_oldest`, `in_flight`). Frames on "video" are exported native frames (`VideoFrame::exported`; `SurfaceSync::Ready` for IOSurfaces, `TimelineFence` for D3D12 buffers) of `frame_format()`, never system memory.
- Every failure in native mode (an export or conversion refused, a lost surface) is `NodeError::Permanent` (decision 9). The studio then stops the outputs on that chain and builds the next chain with system memory.
- Other platforms and APIs (Vulkan) have no exporter yet: `prepare` in native mode fails permanently, and `export_preference` returns `None`, so the root never chooses it there.

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
5. With a native input memory kind (ADR-0147 decision 9), every failure is permanent: a refused native frame fails the next tick before it submits anything, so the refusing tick's packets are kept, and recoverable errors are not restarted. `open` keeps the probe's `max_frames_held()` (`frames_held`) for the export's ring.

Keyframe at a time (ADR-0124 decision 7): `with_keyframe_requests()` returns the node and a `KeyframeAt` handle for the control thread. `KeyframeAt::request(t)` asks for a keyframe at the first frame whose time is at or after `t` (of two waiting requests the earlier wins; `pending()` shows one not yet served). For each new frame, before its `submit`, the node takes a due request (one acquire load, a compare-and-swap only when due) and sends `EncoderChange::RequestKeyframe`, counted in `keyframes_requested`, or `keyframes_refused` when the session refuses (the stream goes on). Only a node built with it reads the request; the studio builds it for a pausable recording's own video encoder and requests a keyframe at each join (a refusal is only counted). `cargo bench -p rbs-nodes --bench encoder` (cloud container, 2026-11-15, fake encoder, one packet a tick): `plain` 259.5 and 269.8 ns on main, 269.0 and 265.8 ns with this change; `keyframe_requests` 270.5 and 270.7 ns; within run-to-run noise. Tests: `crates/rbs-nodes/tests/encode_output.rs` (`a_keyframe_request_*`, `a_time_*`, `of_two_waiting_requests_*`, `a_refused_keyframe_request_*`, `keyframe_requests_add_no_allocation_per_tick`).

### Output node

| Call | Does |
| --- | --- |
| `prepare` | Refuses permanently once the node wrote media (a restart would truncate the destination). Creates the muxer (required exactly when the transport's input is `Container(c)`, and only for that `c`) and the transport, `start(streams)`, `begin`, and sends the header, waiting up to the finish timeout for queue room on the wait clock |
| `process` | Sends the backlog first; then for each stream `i` in order, each packet of pin `s{i}`: muxed into chunks, or forwarded as a packet item (after the aligned start below). `QueueFull` appends to the backlog. Polls transport events |
| `release` | Writes packets still held for the aligned start (no video packet ever came), then delivers the backlog and the muxer's `finish` chunks within the finish timeout, then `Transport::finish` with the time left. When the abandon switch is set (below) it delivers nothing: held and backlogged items count in `finish_lost`, and the muxer and transport are dropped without `finish` |

Invariants:

1. Items reach the transport in the order they were produced; the backlog keeps that order.
2. The backlog is preallocated; a full backlog fails the node permanently (no holes in a recording).
3. Any transport error other than `QueueFull`, any muxer error other than `NonMonotonic` (dropped, counted) and a `Disconnected` event fail permanently, so a file output is never restarted (which would truncate it).
4. After `release` the transport has been finished, or dropped when abandoned; undelivered items are counted (`finish_lost`).
4b. Abandon (ADR-0127 decision 5): `abandon_switch()` gives an `Arc<AtomicBool>` the owner sets before it removes the node without a drain (`Studio::abort_output`, a destination that failed or stalls). It is read once, in `release`, never per tick, so `process` is unchanged. A transport's `Drop` is bounded (output ports spec), so `release` never waits for a stalled destination.
4a. Connection state: `Connected` events are counted in `connects` and set `connected`, `Reconnecting` events are counted in `reconnect_attempts` and clear it, `Disconnected` clears it (and fails the node, invariant 3, with the cause `transport disconnected: <the transport's error>`); `release` clears it. The events are read where they already were (`process`, once per tick), so the counters cost nothing per packet. `connects` stays 0 for a transport that never reached its destination.
5. Muxer and transport creation errors and `begin` errors are permanent; a `start` error is retried only when `TransportError::is_recoverable`.
6. The first permanent failure's cause (for example `muxer: invalid packet: ...`) is kept in `failure`; the string is built only on that path.
7. **Aligned start.** An output with a video stream and other streams starts at the first video packet's pts: packets of other streams that end at or before it (pts plus duration, or pts when the packet has no duration) are dropped and counted (`start_drops`); packets that arrive before the first video packet are held, one hold per stream (ADR-0170 decision 5; at most `START_HOLD` = 256 each, all preallocated in `prepare`, none for the video stream), and written when it arrives, merged by dts with ties broken by stream index; `release` writes them in the same merged order when no video ever came. When a hold is full, every hold drops its packets at or before the oldest packet of the full one, so the streams of a multi-track recording drop the same span by rule; while waiting, a later packet of any stream at or before that time is dropped on arrival, so a track that starts late loses the same span. `start_drops` counts the first stream that is not the video (with one audio stream, every start drop, as before); `OutputNodeStats::start_drops_of(stream)` counts each stream (`stream_start_drops`). The abandon path counts every hold in `finish_lost`. With one audio stream the behaviour is what one shared hold gave. So a recording never begins with audio before the first picture (video started 143 ms after audio on SC-PC, 3440x1440 NVENC,). Outputs with only video or no video pass every packet.
8. **Span (ADR-0089 decision 6).** `OutputNode<S: SpanPolicy = NoSpan>`. The policy is chosen when the node is built and never tested per packet (P13): the default `NoSpan` has an empty, always-inlined `extend`, so a default output runs no span code and `OutputNodeStats::span()` stays `None`. `OutputNode::track_span()` gives `OutputNode<TrackSpan>`: each packet the node delivers (after the muxer accepted it, or forwarded without a muxer; not packets dropped at the aligned start or as `NonMonotonic`) widens the span: its `pts` against the earliest so far, and `pts` plus its duration (or its stream's nominal one: a frame of the stream's rate, `frame_size` samples of audio) against the latest end. Each edge is a node-local compare and, only when it moves, a relaxed store into `first_pts_ns` and `end_ns` (no read-modify-write, no lock, no allocation). `span()` reads `(first, end)` on the master clock. A muxer starts media time at the earliest sample, so `first` is where the file begins. Anything that later shifts the timestamps the node writes must pass the shifted packet to `extend`. Built for the master of a recording that writes an editor project and for every separate file (recording spec, "Recording manifest"). `benches/output.rs` measures a tick of one video and two audio packets for both policies.

8. Built for recordings (ADR-0124, `OutputNode<S, G: PacketGate = NoCut>`; a stream needs no output gate, its chains hold before the encoders): `with_cut(Arc<OutputCut>)` builds a pausable recording's node with `CutGate`, which loads the cut's version once per tick (relaxed) and cuts only the streams in the cut's mask; `NoCut` compiles to nothing. `cargo bench -p rbs-nodes --bench output` (cloud container, 2026-11-15, one H.264 and two AAC packets a tick): main `plain` 241.5 ns, this change `plain` 248.2, `tracked` 249.5, `pausable` 267.5, `pausable_tracked` 265.2 ns. As planned: For a shared encoder's cut it drops video packets in decode order from the first one whose pts is at or after `pause_at` and passes again from the requested keyframe at or after `resume_at`, pts and dts shifted by `paused_total`; the keyframe starts a closed GOP, pictures that would show before the join are dropped, and each dts is clamped per packet to `max(dts + shift, last_dts + 1)` while that stays at or below pts. Audio packets with pts in `[pause_at, resume_at)` are dropped, later ones shifted, and the last kept one is trimmed to end at `pause_at`.

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
| `process` | For a canvas frame not handed to the current UI yet (by timestamp, sequence and the publisher's `subscribes` count): `publish` it (nothing is converted while no UI is subscribed; `ExportBusy` when the UI holds every surface is counted in `busy`, and the canvas is tried again on the next tick). A canvas counts as handed over only when `publish` succeeded with a UI subscribed, and a frame the publisher dropped (`dropped` grew) is converted again (any drop, even of an older frame, costs one extra conversion; rare, and simpler than tracking which frame was dropped), so a canvas that stays unchanged (render on change, R-03) still reaches a UI that subscribes later. Then `tick` the publisher every tick (accepts a UI, reads releases, sends finished frames). A lost device is `Backend`; other publish or tick errors are counted in `errors` |
| `release` | Drops the publisher (the UI's connection ends) and clears the endpoint |

Invariants: no CPU readback (P4); `process` does not allocate in RBS code and never waits (the publisher's hot-path rules); the endpoint is written only on the control thread; a restart creates a new endpoint (the UI reconnects).

### Virtual camera node (ADR-0066)

```rust
// rbs_nodes::vcam
pub const VIRTUAL_CAMERA_NODE_TYPE: &str = "org.rbs.virtual-camera";
pub const CAMERA_BUFFERS: usize = 5;                  // node + 3 session slots + sender copy
pub fn virtual_camera_descriptor() -> NodeDescriptor; // Output, input "canvas" (Video), no outputs
impl VirtualCameraNode {
    pub fn new(session: Box<dyn VirtualCameraSession>, config: VirtualCameraConfig,
               canvas_width: u32, canvas_height: u32) -> Self; // owns the session
    pub fn with_share(self, share: Arc<DownloadShare>) -> Self; // audit F-04: one readback
    pub fn stats(&self) -> Arc<VirtualCameraNodeStats>;
    pub fn session_stats(&self) -> VcamStats;
}
pub struct VirtualCameraNodeStats {                   // relaxed atomics
    pub sent: AtomicU64, pub consumer_busy: AtomicU64, pub no_reader: AtomicU64,
    pub busy: AtomicU64, pub lost: AtomicU64, pub refused: AtomicU64, pub failed: AtomicU64,
    pub repeated: AtomicU64,                          // unchanged canvas sent again (R-03)
}
```

Render partition, `GpuRuntime`, not essential. The session is opened by the studio on the control thread and handed in; the node is dropped (and the session with it, joining the adapter's sender thread) on the control thread after the studio takes it back (ADR-0066 decision 6). A canvas size change rebuilds the node; the camera keeps its size and rate.

| Call | Does |
| --- | --- |
| `prepare` | Refuses anything but NV12 (`Failed`). Creates `YuvDownload::scaled` from the canvas size to the camera size with `CAMERA_BUFFERS` frame buffers, or `scaled_tone_shared` on the share from `with_share` (NV12, Rec. 709 limited, closed shadows): a live encode of that shape converts and reads the picture back once. Resets the cadence. Equal sizes stay the 1:1 `textureLoad` path. A different size uses the ADR-0102 kernel (box, Catmull-Rom or the existing bilinear) on ADR-0066's fitted rectangle |
| `process` | 1. `take_ready`: each submission that ended this tick (delivered, stale, no buffer, failed) gives its reserved token back in submission order; the delivered frame goes to `session.send` with the last one (`sent`, or `refused` when `send` errs), the others are dropped and free their slots (`lost`). 2. The newest canvas is taken only when a camera frame is due on `Cadence::frames(first tick time, camera rate)` (index at or before the tick's time past the last one taken; tick time, because an unchanged canvas keeps its own time, R-03): a 60 fps canvas feeds a 30 fps camera every other tick, a slower canvas every tick. 3. A due frame is skipped before any GPU work while `session.consumer_present()` is `false`: no app reads the camera, so nothing is converted, read back or copied (`no_reader`; audit R-02). Otherwise it first reserves a session slot; none free skips it before any GPU work (`consumer_busy`). Then `submit_at` with the tick's time, or, for the canvas converted last, `submit_repeat` (no GPU work; only a conversion the ring took is repeated later, and a lost readback makes the next one convert again; an older canvas converted again takes a new sequence); a full staging ring (`busy`) or a failed conversion (`failed`) drops the token. Always returns `Ok` |
| `release` | Counts readbacks still in flight in `lost`, drops the download and the tokens |

Invariants:

1. Never waits for the GPU, the session or its consumer; nothing the camera does fails a tick or stops the studio's other outputs.
2. A readback in flight holds a reserved session slot, so a completed one always has room; at most three tokens are held (the session's slot count).
3. Frames sent are NV12, the camera's size (the canvas fitted with its aspect ratio kept, black bars), `BT709_LIMITED`, system memory, with the tick's time; the canvas frame's sequence, or a new one for a repeat.

## Errors and failure behaviour

Nodes report through `NodeError` (ADR-0020): `Permanent` for what retrying cannot fix (permissions, missing targets, unsupported formats, a changed encoder stream, a failed output), `Backend`/`Failed` for what may recover (a lost capture target, a busy device), which the controller restarts with back-off (ADR-0024).

## Performance requirements

- Hot paths ([hot-paths.md](../architecture/hot-paths.md)): every `process` above (the preview node's is `PreviewPublisher::publish` and `tick` plus a key comparison; the virtual camera node's is a cadence check, `reserve`, `YuvDownload::submit`/`take_ready` and `send`), `SourceUpload::upload`, `YuvDownload::submit` and `take_ready`.
- Encoder and output `process` allocate nothing in steady state (test `steady_state_ticks_do_not_allocate`). The GPU nodes allocate nothing in RBS code for a format and size they have seen; wgpu allocates internally while recording command buffers and for the map callback.
- The export's CPU copy is one `memcpy` of the frame (about 3 MB at 1080p NV12) per delivered frame on the render thread; measured in the PR that adds it.

## Conformance tests

- `crates/rbs-nodes/tests/encode_output.rs`: frames submitted once; header, media and trailer in order; backlog on `QueueFull` delivered in order; backlog overflow is permanent; carry-over in order; carry overflow resumes at a keyframe and requests one; a changed stream after restart is permanent; no session while installed or in Standby; a refused frame loses no packet; an output that wrote media refuses to restart; a failure keeps its cause; `release` gives up on a full queue on the wait clock without sleeping; an abandoned output releases with no wait, no tail and no `finish`, counting what was backlogged; no allocation per tick.
- `crates/rbs-nodes/tests/pipeline_gpu.rs` (any GPU adapter; CI uses lavapipe, WARP and Metal): a BGRA window and an NV12 camera from the synthetic adapter go through capture, compositor and export, and arrive as NV12 with the right format, times and luma; an ended export passes on every canvas up to the end and none after (ADR-0045).
- `crates/rbs-nodes/tests/vcam_gpu.rs` (any GPU adapter, the `rbs-vcam-test` camera): a 60 fps canvas feeds a 30 fps camera on even ticks only, every due frame sent or counted, delivered in order; a stalled consumer fills the three slots and then skips before conversion without failing a tick, and on resume the newest frame wins; a frame the session refuses is counted and frees its slot.
- `crates/rbs-gpu/tests/upload_download.rs`: download codes, busy ring, stale frames, buffer recycling, `frames_cpu_copied`; upload of padded BGRA rows and NV12.
- Hardware (manual, `crates/rbs-nodes/examples/record.rs`): screen and camera to an H.264 Matroska file with the platform's adapters.

## Open questions

- Encoders with deep look-ahead or B-frames flush more packets than one final tick on a cadence partition can pass (`PER_TICK_EVENTS`); the rest is counted in `flush_lost`. Today's hardware adapters hold at most their few slots (ADR-0043). More final ticks while a node has output pending would close this.
- Transport feedback (`KeyframeRequested`, `BitrateHint`) to the encoder goes through the control plane; not wired yet.
- Outputs that may restart safely (network transports) need a transport capability; until then every output that wrote media is single-use ([#84](https://github.com/salihcantekin/rbs/issues/84)).
- Native export is built for Metal (IOSurface, VideoToolbox) and D3D12 buffers (NVENC through CUDA). A D3D12 texture exporter for AMF and QSV, and Vulkan, follow ([ADR-0147](../adr/0147-native-encoder-input.md) order of work, [#83](https://github.com/salihcantekin/rbs/issues/83)).
- YUY2 and I420 system frames (common on webcams) are not uploaded yet; a camera that offers only those fails `prepare` with `FormatUnsupported` ([#85](https://github.com/salihcantekin/rbs/issues/85)).
