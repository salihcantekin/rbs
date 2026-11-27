# Spec: Person mask port

- Status: Accepted by the owner on 2026-11-24 after two independent review rounds (the second set of fixes was not re-reviewed, at the owner's request; nothing is implemented yet and the Windows backend table is still open). Two independent review rounds on 2026-11-24 asked for changes (8, then 6); all are applied, the second set not yet re-reviewed.
- Owner crate(s): `rbs-segment` (port), adapters `rbs-segment-*`, filter nodes in `rbs-filters-segment`
- Related ADRs: [0179](../adr/0179-camera-background-removal.md), [0053](../adr/0053-item-style-and-video-filter-chains.md), [0013](../adr/0013-zero-cost-when-unused.md), [0162](../adr/0162-extension-ready-registries-and-plugin-order.md), [0005](../adr/0005-gpu-compositing-with-wgpu.md)
- Related specs: [video-filters.md](video-filters.md), [backend-ports.md](backend-ports.md), [media-types.md](media-types.md), [params-schema in plugin-sdk.md](plugin-sdk.md)
- Evidence: [person matte spike](../research/person-matte-spike-2026-11.md)

## Purpose

The port that turns a camera frame into a person matte: one coverage value per pixel, 1 for the person and 0 for the background. A matte backend may be an OS API (Apple Vision), a bundled network run by us on the GPU, or an inference runtime. The video filter `org.rbs.filter.background-removal` uses the matte as the picture's alpha. Replacing or blurring the background are later filters that reuse the matte; they are not part of this port.

## Scope

In scope: backend discovery and capabilities, quality levels and their declared cost, opening a session, submitting frames, receiving mattes, errors and the conformance suite.

Out of scope: mask refinement (threshold, feather, expand, temporal smoothing; GPU shaders in the filter, shared by every backend), background replacement, model download and storage (add-on and user-data specs), reading or writing the scene document.

## Contract

Types `VideoFrame` (it carries its `Timestamp` and an optional `Lease`, and has `share()`), `Timestamp`, `Size` and `MemoryKind` come from `rbs_core::media` and `rbs_core`. New types below are `#[non_exhaustive]` where they may grow.

```rust
/// Registry entry (ADR-0162). Type id such as `org.rbs.segment.apple-vision`.
pub trait PersonMaskBackend: Send + Sync {
    fn type_id(&self) -> &'static str;
    /// Cheap, allocates no GPU or model resources and starts no thread: is this
    /// backend usable on this computer, and what does it offer? Control thread.
    fn probe(&self, host: &dyn SegmentHost) -> BackendInfo;
    /// Creates a session and loads the model, the only expensive call (P13).
    /// Control thread, never the render thread.
    fn open(&self, request: SessionRequest) -> Result<Box<dyn PersonMaskSession>, SegmentError>;
}

/// What a host lends a backend: the studio's GPU context (a handle type from
/// `rbs-gpu`'s port-side description, never `wgpu` itself in this crate), the
/// model files directory and a log sink.
pub trait SegmentHost: Send + Sync { /* adapters get only what a plugin could also be given */ }

#[non_exhaustive]
pub struct BackendInfo {
    pub available: bool,              // false: not listed in the UI
    pub levels: Vec<QualityLevel>,    // 1 to 3, ordered low to high
    pub input: MemoryKind,            // the kind of frame `open` wants (GPU surface or system memory)
    pub output: MemoryKind,           // the kind a matte comes in
    pub temporal: bool,               // keeps state between frames
    pub hardware_assisted: bool,
}

#[non_exhaustive]
pub struct QualityLevel { pub id: LevelId /* Low | Medium | High */, pub cost: CostHint }
#[non_exhaustive]
pub struct CostHint { pub ms_per_frame: f32, pub unit: ComputeUnit, pub note: &'static str }
#[non_exhaustive]
pub enum ComputeUnit { Gpu, Cpu, Npu }

#[non_exhaustive]
pub struct SessionRequest {
    pub level: LevelId,
    pub input_size: Size,             // the first frame's size; see "Size changes"
    pub model: Option<ModelRef>,      // a handle from the host, never a path from a scene
}
```

```rust
pub trait PersonMaskSession: Send {
    /// Render-side warm step, called once after `open` and after a re-open, on the
    /// control thread: allocates every buffer, texture and worker the session will
    /// ever use at `input_size` and the matte size.
    fn warm(&mut self) -> Result<(), SegmentError>;
    /// Non-blocking, render thread. Hands over a frame (a `VideoFrame::share()`);
    /// the session may drop it when it is busy (the newest frame wins). A frame
    /// whose `share()` returns `None` (a capture or decoder surface no exporter
    /// marked shareable) is not retained: the session gets `SubmitOutcome::NotShareable`
    /// and the filter treats it as a dropped frame (the previous matte stays in use).
    fn submit(&mut self, frame: &VideoFrame) -> SubmitOutcome;
    /// Non-blocking, render thread. The newest finished matte, `Ok(None)` while
    /// there is none yet, or the reason the session can no longer produce one.
    fn latest(&mut self) -> Result<Option<Matte>, SegmentError>;
    /// A cut, gap or source restart: a temporal backend forgets its state. Render thread.
    fn reset(&mut self);
    /// Control thread: stops the worker, releases leases and device handles.
    fn close(self: Box<Self>);
}

#[non_exhaustive]
pub enum SubmitOutcome { Accepted, DroppedBusy, NotShareable }

/// One channel, values 0..1 (8 bit, or 16 bit float on the GPU), `size` fixed per
/// session and level, declared at `open` (`Matte::size`), aspect of the input.
pub struct Matte { pub timestamp: Timestamp, pub size: Size, pub plane: MattePlane }
#[non_exhaustive]
pub enum MattePlane { Cpu(Arc<[u8]>), Gpu(GpuMatte /* texture handle + completion fence */) }

#[non_exhaustive]
pub enum SegmentError {
    Unavailable, ModelMissing, ModelLoadFailed, DeviceLost,
    Unsupported { level: LevelId },
    SizeChanged,                      // the session cannot take this input size: re-open it
}
```

Rules:

1. **One matte format.** Every backend gives one channel and a matte size that is fixed per session and level and known at `open`; the filter scales it to the frame, so changing method or quality does not change the filter's look beyond edge quality (ADR-0179 decision 12).
2. **Threads and ownership.** `probe`, `open`, `warm` and `close` run on the control thread (model load, ADR-0179 decision 10). The control thread hands the opened session to the filter node through the node's parameter channel (ADR-0023, latest value wins) and takes it back the same way before `close`; the render thread never drops a session, because dropping would run `close` on it. A node that is being torn down returns its session through the channel first. `latest` and `reset` run on the render thread. `submit` runs on the render thread or on one thread the filter owns for the readback wait (never both at once: the node keeps the session behind a mutex that is held only for these calls). All three do no I/O, no allocation after `warm`, and take no contended lock (P6). Inference runs on the session's own worker. A backend that cannot meet this is not a backend. Implementation (2026-11-27): the filter node opens and warms its session in `prepare` and closes it in `release`, both on the control thread, never in the executor's `warm` and `cool`. `warm` may wait, bounded, for the worker to be ready, so the first matte follows the first picture within a few ticks: the ONNX backend waits up to 1.5 s for its runtime session and two warm-up pictures (DirectML compiles on the first runs); a slower start goes on in the background.
3. **Frame pairing.** The filter picks the newest matte for a frame by timestamp, and all limits are measured on frame timestamps (the master clock), never wall time. `latest` returns each matte once; with nothing newer it returns `Ok(None)` and the filter keeps using the last matte it received. A matte older than 4 frame periods of the source is stale: the filter keeps using it but reports "mask late" through its stats, and after 1 second of frame time without a new matte it publishes nothing (the same rule as before the first matte). Until the first matte the filter publishes no frame (ADR-0053 decision 6; the compositor skips the layer, P7). When it starts to publish again, the result fades in over 0.2 s of master-clock time, starting at the last tick that showed nothing (owner, 2026-11-27): the whole premultiplied result is scaled, so nothing unfiltered shows during the fade.
4. **Temporal backends.** `temporal: true` backends still drop frames when busy; they must tolerate gaps (the state carries over) and the filter calls `reset()` on a scene cut, a source restart or a frame-timestamp gap above 1 second.
5. **Size changes.** A session is opened for one `input_size`. When the source's size changes (a camera switching format), the filter keeps the session and the layer visible: it scales the frame on the GPU to the session's `input_size` (a letterbox-free resample of the whole frame, the matte aspect follows the original) and the matte is scaled back by the filter, as ADR-0062 decision 5 requires for the filter's own textures. A new session is opened (control thread, never in `process`) only when the source's aspect ratio changes by more than 2 percent, and the old matte stays in use until the new session produces one, so the layer is never blank because of a size change. A backend that cannot take the scaled input returns `SizeChanged` from `open`.
6. **Leases.** A session holds at most two frame leases, one in flight and one pending. A dropped frame's lease is released at once, in `submit`. A GPU matte comes from a texture ring owned by the session (at least three textures). Each matte carries a completion fence the consumer waits on in GPU queue order, never on the CPU in `process`. The consumer gives a release fence back when its GPU work on the texture is submitted (`GpuMatte::release`); the session reuses a texture only after that fence has signalled, and never the one the consumer still holds. A CPU matte is an `Arc` the consumer may keep for as long as it likes.
7. **Memory kinds are capabilities (P3, P4).** A backend that needs system-memory input declares it in `BackendInfo::input`; the filter then downscales on the GPU first and reads that small image back asynchronously. This is the named exception of ADR-0179 decision 3 and needs its own hot-path review. A backend that takes a GPU surface gets the frame with the usual lease rules.
8. **Lifecycle (P13).** `probe` allocates nothing and starts no thread. `open` happens only when the filter is enabled on a running source; `close` when it is disabled or the source stops. An installed backend with no session has no thread, no GPU resource and no model in memory.
9. **Errors are typed (rule 8).** A session that fails after `open` reports it through `latest`; the filter then publishes nothing and the UI shows the reason; it never shows the unfiltered picture. `DeviceLost` asks the control thread to re-open with back-off (ADR-0024).
10. **Untrusted data.** Model files are data and are checked (size, hash, format) before loading. A model path is never taken from a scene document; the user's data folder or an add-on provides it through `ModelRef`.

### Stray regions (2026-11-24)

A network can take an object in the room (a hat on a shelf) for a second person. A backend may drop such regions with `rbs_segment::StrayFilter` on its own thread: it keeps the largest region of the matte and every region at least a tenth as large, zeroes the rest, and keeps any region that reaches the bottom row. The ONNX backend does this; it needs no setting and no per-frame work outside the backend's thread.

## Quality levels and declared cost

Backends declare their levels; the UI shows each level's cost text built from `CostHint` (ADR-0179 decision 12). Numbers below are from the Mac spike (M4 Pro, 1620x1080 input) and are the first values for the Apple Vision backend; Windows and Linux backends fill theirs after their measurements.

| Backend (type id) | Low | Medium | High |
| --- | --- | --- | --- |
| `org.rbs.segment.apple-vision` (macOS) | fast, about 3 ms, Neural Engine | balanced, about 9 ms | accurate, about 30 ms |
| `org.rbs.segment.onnx` (ONNX Runtime; DirectML on Windows; RTX 4090, 512x288 picture, 2026-11-24) | MediaPipe selfie, about 1.8 ms | MODNet, about 6.3 ms | Robust Video Matting, about 11 ms (backbone at 512 pixels) |
| GPU-native network (later) | to be measured | to be measured | to be measured |

The ONNX backend needs two kinds of data the app does not bundle (ADR-0179 decision 8): the ONNX Runtime library (`<config>/runtime/onnxruntime/onnxruntime.dll` with DirectML, or `RBS_ONNXRUNTIME`) and the model files `<config>/models/person-mask/<name>.onnx` (`mediapipe-selfie-256`, `modnet-webcam`, `rvm-mobilenetv3-fp32`). The composition root lends a model only when its size and SHA-256 match its list (`rbs-app` `segment_models.rs`); the backend lists only the levels whose model is present and is unavailable without the library. The AMD iGPU is not measured yet. Licences: ONNX Runtime MIT, `ort` MIT or Apache-2.0, MediaPipe and MODNet Apache-2.0, RVM GPL-3.0.

Costs are hints for the UI, never used for decisions in engine code.

## Conformance suite (to be written with the port)

Each backend passes the same tests with a fixed small test clip, with no sleeping (ADR-0032): the suite uses a test hook that lets a test wait for the worker to finish the submitted frame (a condition, with a hang guard only); a matte arrives for a submitted frame; `latest` never blocks (the test holds the worker inside the model with a test gate, asks `latest` and expects an immediate `Ok(None)`, then releases the gate; a hang guard bounds real time); closing releases all threads and device handles; a missing model gives `ModelMissing`; a frame submitted faster than the backend runs is dropped and its lease is released at once (counted); a temporal backend's `reset()` makes the next matte independent of earlier frames; a lost device (a fault hook, `SessionRequest::test_fault`, compiled only for tests, makes the backend report `DeviceLost`) is reported through `latest`; a changed input size keeps the session and the layer visible, and an aspect change opens a new session while the old matte stays in use; a frame with `share() == None` is dropped without a retained lease; the texture ring reuses a texture only after the release fence; OS-only backends (Apple Vision) run only where `probe` says `available` and are skipped, not failed, elsewhere; the matte format rules hold. Golden mattes for hand-checked frames use a mean absolute error band, never exact values.

## Open questions

- Which model the GPU-native backend runs (ADR-0179 step 1; waits for the Windows measurement).
- Whether Windows needs an OS backend (Windows ML) beside the GPU-native one.
- Whether a backend that runs in its own process (Process tier, ADR-0162) needs a different `submit` for texture sharing; deferred until that tier exists.
