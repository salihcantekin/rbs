# Spec: Person mask port

- Status: Draft (written before any code, ADR-0179 decision 2; nothing is implemented)
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

```rust
/// Registry entry (ADR-0162). Type id such as `org.rbs.segment.apple-vision`.
pub trait PersonMaskBackend: Send + Sync {
    fn type_id(&self) -> &'static str;
    /// Cheap, no allocation of GPU or model resources: is this backend usable on
    /// this computer, and what does it offer? Called when the UI lists methods.
    fn probe(&self, host: &dyn SegmentHost) -> BackendInfo;
    /// Creates a session. Loads the model, the only expensive call (P13).
    fn open(&self, request: SessionRequest) -> Result<Box<dyn PersonMaskSession>, SegmentError>;
}

#[non_exhaustive]
pub struct BackendInfo {
    pub available: bool,                  // false: not listed in the UI
    pub levels: Vec<QualityLevel>,        // 1 to 3, ordered low to high
    pub input: InputKind,                 // GpuTexture | CpuBuffer: what open() wants
    pub output: OutputKind,               // GpuTexture | CpuBuffer: what a matte comes as
    pub temporal: bool,                   // keeps state between frames (needs ordered frames)
    pub hardware_assisted: bool,
}

#[non_exhaustive]
pub struct QualityLevel {
    pub id: &'static str,                 // "low" | "medium" | "high"
    pub cost: CostHint,                   // measured numbers, see below
}

pub struct CostHint { pub ms_per_frame: f32, pub unit: ComputeUnit /* Gpu|Cpu|Npu */, pub note: &'static str }
```

```rust
pub trait PersonMaskSession: Send {
    /// Non-blocking. Hands over a lent frame; the session may drop it when it is
    /// still busy with an older one (the newest frame wins, no queue growth).
    fn submit(&mut self, frame: LentFrame, pts: Pts) -> SubmitOutcome;
    /// Non-blocking. The newest finished matte, or None while there is none yet.
    fn latest(&mut self) -> Option<Matte>;
    fn close(self: Box<Self>);
}

pub struct Matte { pub pts: Pts, pub size: Size, pub plane: MattePlane /* one channel, 0..1 */ }
```

Rules:

1. **One matte format.** Every backend gives one channel, 8 bit (or 16 bit float on the GPU), any size, with the frame's aspect. The filter scales it to the frame, so changing method or quality does not change the filter's look beyond edge quality (ADR-0179 decision 12).
2. **Never block the render or audio thread (P6).** `submit` and `latest` do no I/O, no allocation after warm-up, and take no contended lock. Inference runs on the session's own worker. A backend that cannot meet this is not a backend.
3. **No picture before a matte.** Until the first matte, the filter publishes no frame (ADR-0053 decision 6); a stale matte is reused until a newer one arrives and the filter smooths between them.
4. **Memory kinds are capabilities (P3, P4).** A backend that needs `CpuBuffer` input declares it; the filter then downscales on the GPU first and reads that small image back asynchronously. This is the named exception of ADR-0179 decision 3 and needs its own hot-path review. A backend that takes `GpuTexture` gets the lent frame with the same lease rules as any frame (a lease is released after GPU work that reads it has finished).
5. **Lifecycle (P13).** `probe` allocates nothing and starts no thread. `open` happens only when a filter is enabled on a running source; `close` when it is disabled or the source stops. An installed backend with no session has no thread, no GPU resource, no model in memory.
6. **Errors are typed (rule 8).** `SegmentError`: `Unavailable`, `ModelMissing`, `ModelLoadFailed`, `DeviceLost`, `Unsupported(level)`. A session that fails after `open` reports `DeviceLost` through `latest`'s companion `status()`; the filter then outputs nothing and the UI shows the reason; it never shows the unfiltered picture.
7. **Untrusted data.** Model files are data and are checked (size, hash, format) before loading. A model path is never taken from a scene document; the user's data folder or an add-on provides it.

## Quality levels and declared cost

Backends declare their levels; the UI shows each level's cost text built from `CostHint` (ADR-0179 decision 12). Numbers below are from the Mac spike (M4 Pro, 1620x1080 input) and are the first values for the Apple Vision backend; Windows and Linux backends fill theirs after their measurements.

| Backend (type id) | Low | Medium | High |
| --- | --- | --- | --- |
| `org.rbs.segment.apple-vision` (macOS) | fast, about 3 ms, Neural Engine | balanced, about 9 ms | accurate, about 30 ms |
| GPU-native network (later; model chosen after the Windows measurement) | to be measured | to be measured | to be measured |

Costs are hints for the UI, never used for decisions in engine code.

## Conformance suite (to be written with the port)

Each backend passes the same tests with a fixed small test clip: a matte arrives within a bounded number of frames; `latest` never blocks (checked with an injected clock, no sleeping, ADR-0032); closing releases all threads and device handles; a missing model gives `ModelMissing`; a frame submitted faster than the backend runs is dropped, not queued; the matte format rules above hold. Golden mattes for hand-checked frames use a mean absolute error band, never exact values.

## Open questions

- Which model the GPU-native backend runs (ADR-0179 step 1; waits for the Windows measurement).
- Whether Windows needs an OS backend (Windows ML) beside the GPU-native one.
- Whether a backend that runs in its own process (Process tier, ADR-0162) needs a different `submit` for texture sharing; deferred until that tier exists.
