# Spec: Person mask port

- Status: Draft (written before any code, ADR-0179 decision 2; nothing is implemented). One independent review round on 2026-11-24 asked for changes; all eight are applied below, not yet re-reviewed.
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
    /// the session may drop it when it is busy (the newest frame wins).
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
pub enum SubmitOutcome { Accepted, DroppedBusy }

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
2. **Threads.** `probe`, `open`, `warm` and `close` run on the control thread (model load, ADR-0179 decision 10). `submit`, `latest` and `reset` run on the render thread and do no I/O, no allocation after `warm`, and take no contended lock (P6). Inference runs on the session's own worker. A backend that cannot meet this is not a backend.
3. **Frame pairing.** The filter picks the newest matte for a frame by timestamp. A matte older than 4 frame periods of the source is stale: the filter keeps using it but reports "mask late" through its stats, and after 1 second without a new matte it publishes nothing (the same rule as before the first matte). Until the first matte the filter publishes no frame (ADR-0053 decision 6; the compositor skips the layer, P7).
4. **Temporal backends.** `temporal: true` backends still drop frames when busy; they must tolerate gaps (the state carries over) and the filter calls `reset()` on a scene cut, a source restart or a timestamp gap above 1 second.
5. **Size changes.** A session is opened for one input size. When the source's size changes (a camera switching format), the filter asks the control thread to `close` and `open` again at the new size, publishing nothing in between; this follows ADR-0062 decision 5 for the filter itself (the filter's textures re-acquire once in `process`), while the session's models and buffers are re-created on the control thread, never in `process`. A backend that accepts any size says so by never returning `SizeChanged`.
6. **Leases.** A session holds at most two frame leases, one in flight and one pending. A dropped frame's lease is released at once, in `submit`. A GPU matte is owned by the session, lives until the next `latest` call returns a newer matte or the session closes, and carries the completion fence the consumer must wait on in GPU queue order, never on the CPU in `process`.
7. **Memory kinds are capabilities (P3, P4).** A backend that needs system-memory input declares it in `BackendInfo::input`; the filter then downscales on the GPU first and reads that small image back asynchronously. This is the named exception of ADR-0179 decision 3 and needs its own hot-path review. A backend that takes a GPU surface gets the frame with the usual lease rules.
8. **Lifecycle (P13).** `probe` allocates nothing and starts no thread. `open` happens only when the filter is enabled on a running source; `close` when it is disabled or the source stops. An installed backend with no session has no thread, no GPU resource and no model in memory.
9. **Errors are typed (rule 8).** A session that fails after `open` reports it through `latest`; the filter then publishes nothing and the UI shows the reason; it never shows the unfiltered picture. `DeviceLost` asks the control thread to re-open with back-off (ADR-0024).
10. **Untrusted data.** Model files are data and are checked (size, hash, format) before loading. A model path is never taken from a scene document; the user's data folder or an add-on provides it through `ModelRef`.

## Quality levels and declared cost

Backends declare their levels; the UI shows each level's cost text built from `CostHint` (ADR-0179 decision 12). Numbers below are from the Mac spike (M4 Pro, 1620x1080 input) and are the first values for the Apple Vision backend; Windows and Linux backends fill theirs after their measurements.

| Backend (type id) | Low | Medium | High |
| --- | --- | --- | --- |
| `org.rbs.segment.apple-vision` (macOS) | fast, about 3 ms, Neural Engine | balanced, about 9 ms | accurate, about 30 ms |
| GPU-native network (later; model chosen after the Windows measurement) | to be measured | to be measured | to be measured |

Costs are hints for the UI, never used for decisions in engine code.

## Conformance suite (to be written with the port)

Each backend passes the same tests with a fixed small test clip, with no sleeping (ADR-0032): the suite uses a test hook that lets a test wait for the worker to finish the submitted frame (a condition, with a hang guard only); a matte arrives for a submitted frame; `latest` never blocks (checked with an injected clock); closing releases all threads and device handles; a missing model gives `ModelMissing`; a frame submitted faster than the backend runs is dropped and its lease is released at once (counted); a temporal backend's `reset()` makes the next matte independent of earlier frames; a lost device is reported through `latest` as `DeviceLost`; a changed input size gives `SizeChanged` or is accepted, as declared; the matte format rules hold. Golden mattes for hand-checked frames use a mean absolute error band, never exact values.

## Open questions

- Which model the GPU-native backend runs (ADR-0179 step 1; waits for the Windows measurement).
- Whether Windows needs an OS backend (Windows ML) beside the GPU-native one.
- Whether a backend that runs in its own process (Process tier, ADR-0162) needs a different `submit` for texture sharing; deferred until that tier exists.
