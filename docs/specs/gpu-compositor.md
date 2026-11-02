# Spec: GPU context, texture pool and compositor

- Status: Draft
- Owner crate(s): `rbs-gpu`
- Related ADRs: [0005](../adr/0005-gpu-compositing-with-wgpu.md), [0013](../adr/0013-zero-cost-when-unused.md)
- Related specs: [media-graph.md](media-graph.md), [performance-budgets.md](performance-budgets.md)

## Purpose

`rbs-gpu` owns every GPU resource of the engine. It gives the render loop a device, a pool of reusable textures addressed by small ids, a compositor that draws a flat list of layers into a canvas texture, and the conversion of a canvas into encoder formats. Higher layers (`rbs-scene`, `rbs-graph`) decide *what* to draw; this crate decides *how*, without allocating per frame in its own code.

## Scope

In scope:

- Device creation and adapter selection by capability (headless, no window).
- Texture pool with allocation-free reuse, stale-id detection and leases for consumers on other threads.
- Compositor: one per canvas; draws layers with a 2D affine transform, source crop, opacity and premultiplied alpha blending, in linear light on `Rgba16Float`.
- Conversion of a canvas to NV12 (two planes, BT.709) on the GPU.
- Explicit, counted CPU readback (fallback and tests only).

Out of scope (later specs or PRs):

- Native texture export to encoders (single NV12/P010 surface); import is specified below.
- Scene tree evaluation (groups, nesting, animation): `rbs-scene` flattens a scene into the layer list below.
- Filters, masks, blend modes other than "normal", transitions, P010 and HDR output.

## Contract

### Device

```rust
pub struct GpuRequest {
    pub power: PowerPreference,        // LowPower | HighPerformance
    pub allow_software: bool,          // permit CPU adapters (WARP, lavapipe, llvmpipe)
}

impl GpuContext {
    pub fn new(request: &GpuRequest) -> Result<GpuContext, GpuError>;
    pub fn capabilities(&self) -> &CapabilitySet;   // rbs-core capability model (P3)
    pub fn metrics(&self) -> &Arc<GpuMetrics>;
    pub fn poll(&self);                              // non-blocking, once per render tick
    pub fn wait_idle(&self) -> Result<(), GpuError>; // blocking, never on real-time threads
}
```

- Adapter choice uses capabilities only. Vendor or device names are reported for diagnostics and never branched on (P3).
- Reported capability keys: `gpu.software` (bool), `gpu.max_texture_dimension` (int), `gpu.timestamp_query` (bool).
- Only the primary backends are used (Vulkan, Metal, D3D12); the adapter must support storage buffers in the vertex stage.
- `GpuError::NoAdapter` is returned when no adapter satisfies the request. Callers and tests treat it as "hardware absent", not as a crash.

### Texture pool

```rust
// Ids are the core media type: rbs_core::media::TextureId { slot, generation }.
pub struct TextureDesc { pub width: u32, pub height: u32, pub format: TextureFormat, pub usage: TextureUsage }

impl TexturePool {
    pub fn new() -> TexturePool;                          // DEFAULT_POOL_SLOTS (256) slots
    pub fn with_capacity(slots: u32) -> TexturePool;
    pub fn acquire(&mut self, ctx: &GpuContext, desc: &TextureDesc) -> Result<TextureId, GpuError>;
    pub fn release(&mut self, id: TextureId) -> Result<(), GpuError>;
    pub fn lease(&self, id: TextureId) -> Result<Lease, GpuError>;
    pub fn frame(&self, id: TextureId, format: VideoFormat, t: Timestamp) -> Result<VideoFrame, GpuError>;
    pub fn get(&self, id: TextureId) -> Option<&PooledTexture>;
    pub fn trim(&mut self);                                // drop every free texture
}
```

A slot is **Free**, **InUse** (handed out by `acquire`, owner holds it) or **Retired** (the owner released it while leases were open). Invariants (each has a test):

1. `acquire` reuses a free texture with an equal `TextureDesc` before creating a new one. Reuse costs O(distinct descriptions + retired slots), both small, and performs no heap allocation; `release` and `lease` never allocate either (free-list and retired-list capacity is reserved up front).
2. When a slot becomes Free its generation increments, so every copy of the old id becomes stale: `get` returns `None`, `lease` and a second `release` return `GpuError::StaleTexture`.
3. Leases implement `rbs_core::media::LeaseOwner` with one atomic counter per slot (token = slot index). A slot whose owner called `release` stays Retired, and its id stays valid, until the last lease is dropped; only then is it reused. Leases may be cloned and dropped on any thread. Because a slot is never reused while a lease exists, the token needs no generation. `frame` builds a `VideoFrame::texture` carrying such a lease; `VideoFrame::share` adds a reference.
4. The pool holds at most its capacity of textures; past that `acquire` returns `GpuError::PoolFull`. A new pool owns no GPU memory, only one counter per slot. `trim` drops every free texture (P13); textures in use or retired are kept.
5. The pool is owned and mutated by one render thread (`acquire`/`release` take `&mut self`). Other threads see pooled textures only through frames and leases. The engine drops a pool only after every consumer holding its leases has shut down, so the last reference to the lease counters is never dropped on a real-time thread (media-types spec, Lifetime).
6. Every sampleable texture carries a bind group for the shared sampled-texture layout, created with the texture, so passes never create bind groups per frame.
7. A consumer that reads a leased texture with its own GPU work (an encoder on another queue or device) drops the lease only after that work has completed. Work on the engine's queue is ordered, so the compositor and converter need no extra fence.

### Compositor

```rust
pub struct CanvasDesc { pub width: u32, pub height: u32 }   // format is always Rgba16Float, linear

pub struct Layer {
    pub texture: TextureId,
    pub transform: Affine2,      // maps the unit square (0,0)-(1,1) to canvas pixels
    pub crop: UvRect,            // source sub-rectangle in normalised texture coordinates
    pub opacity: f32,            // 0.0..=1.0, clamped
    pub source_transfer: Transfer, // Linear | Srgb: how the source texture encodes colour
}

impl Compositor {
    pub fn new(ctx: &GpuContext, canvas: CanvasDesc, max_layers: u32) -> Result<Compositor, GpuError>;
    pub fn reserve_layers(&mut self, ctx: &GpuContext, max_layers: u32) -> Result<(), GpuError>;
    pub fn render(&mut self, ctx: &GpuContext, pool: &TexturePool, layers: &[Layer], target: TextureId)
        -> Result<RenderStats, GpuError>;
}
```

- Layers are drawn in slice order, first is bottom. Blending is premultiplied "source over". The target is cleared to transparent black first.
- Sampling is bilinear with clamp to edge. Sources with `Transfer::Srgb` are decoded to linear in the shader before blending.
- `render` records one render pass with one draw call per layer and submits it. It never creates pipelines, buffers, bind group layouts or samplers. Bind groups for source textures are cached per pool slot and rebuilt only when the slot generation changes.
- `max_layers` sizes the per-layer uniform buffer. Growing it is a non-real-time call (`reserve_layers`), done by the control thread when the scene changes. `render` with more layers returns `GpuError::TooManyLayers` and draws nothing.
- A layer whose texture id is stale or missing is skipped and counted in `RenderStats::skipped_layers`; the rest of the canvas renders (P7).
- The target must be an `Rgba16Float` texture of the canvas size with render-attachment usage, else `GpuError::TargetMismatch`.

### Encoder conversion (NV12)

```rust
impl Nv12Converter {
    pub fn new(ctx: &GpuContext, width: u32, height: u32) -> Result<Nv12Converter, GpuError>;
    pub fn convert(&mut self, ctx: &GpuContext, pool: &TexturePool, canvas: TextureId, luma: TextureId, chroma: TextureId)
        -> Result<(), GpuError>;
}
pub fn luma_desc(width: u32, height: u32) -> TextureDesc;    // R8Unorm, w x h
pub fn chroma_desc(width: u32, height: u32) -> TextureDesc;  // Rg8Unorm, w/2 x h/2
```

- Input: a linear `Rgba16Float` canvas, composited over black (alpha is ignored). Output: Y plane `R8Unorm` (w x h) and CbCr plane `Rg8Unorm` (w/2 x h/2), both render targets. Width and height must be even.
- Colour: linear light is encoded with the BT.709 OETF, then converted with BT.709 coefficients to limited range (Y 16-235, CbCr 16-240). Chroma is the average of each 2x2 block (centre siting).
- Two render passes per frame (luma at full size, chroma at half size), one full-screen triangle each. Render passes are used instead of compute because `R8Unorm`/`Rg8Unorm` are render targets on every backend, while storage writes to them need optional features.
- The frame's colour fields (primaries, transfer, matrix, range) travel with the converted frame so encoders can write VUI information.
- Hand-off target: hardware encoders (NVENC, AMF, QSV, VideoToolbox, VA-API, Vulkan Video) consume one multi-planar NV12 or P010 surface. The interop module therefore exports a single NV12/P010 texture, written through per-plane views where the backend allows it, and falls back to the two plane textures plus one GPU copy into a shared NV12 surface where it does not. The first implementation writes the two plane textures only; the single-surface export lands with the interop module.
- Lease: an exported frame holds a pool lease. The encoder releases it after it has finished reading (fence or semaphore signal, or an explicit release), and the pool never reuses a texture while a lease on it is open.
- CPU fallback for software encoders: on request per output, the converted planes are copied into a mapped staging buffer (NV12 or I420 with per-plane strides). It is opt-in, counted in `frames_cpu_copied`, and never on the default path. Encoders advertise what they accept with the `input.memory` and `input.pixel_format` capabilities of `rbs-codec`.

### Native import (interop)

Implements the importer side of [media-types.md, Native surfaces](media-types.md#native-surfaces). Lives in `rbs_gpu::interop`, a marked low-level module (P11) with one submodule per backend, compiled only on its platform.

```rust
impl Importer {
    pub fn new(ctx: &GpuContext) -> Importer;   // holds no GPU objects until the first import
    /// Takes a native frame by value and returns a `Texture` frame of the same format and time.
    pub fn import(&mut self, ctx: &GpuContext, pool: &mut TexturePool, frame: VideoFrame)
        -> Result<VideoFrame, GpuError>;
    /// Releases native leases whose reading GPU work has completed. Once per render tick.
    pub fn retire(&mut self, ctx: &GpuContext, pool: &mut TexturePool);
}
```

Backend matrix (first implementation in bold):

| Surface | Render backend | Sync accepted | Notes |
| --- | --- | --- | --- |
| **`D3dSharedHandle` (NT)** | **D3D12** | **`Ready`, `TimelineFence` (shared D3D11/D3D12 fence)** | `OpenSharedHandle`; the adapter LUID must match the render device |
| `D3d12Resource` | D3D12 | `Ready`, `TimelineFence` | same-process resource |
| `D3d11Texture` | none | none | not importable: the producer must share it with an NT handle |
| `IoSurface`, `MetalTexture` | Metal | `Ready` | `device_registry_id` must match |
| `DmaBuf` | Vulkan | `Ready`, `SyncFile` | needs `VK_EXT_external_memory_dma_buf` and image DRM format modifiers; device identity is an open question |

`GpuContext` reports what the importer accepts, for capture requests (`GpuImportTarget { device, sync }` of the capture port spec): the render adapter's identity (D3D LUID as `u64` = `HighPart << 32 | LowPart`, from `GpuContext::adapter_luid`; Metal registry id) and the sync kinds in the table below. Producers create surfaces on that adapter.

Rules:

1. **Alias, do not copy.** A supported surface is wrapped zero-copy as a pool texture (`create_texture_from_hal`). A surface that cannot be aliased (unsupported format or backend) is an error (`GpuError::Unsupported`), not a silent CPU path; the source falls back to its `System` payload if it offers one.
2. **Open once.** Producers reuse a small ring of surfaces. The importer caches opened surfaces and fences by handle value (at most 16 entries per importer) and reuses the pool slot of a cached surface, so steady-state import does not open handles or allocate. First sight of a surface opens it (allocates, counted in `GpuMetrics::surfaces_opened`). Entries unseen for 120 ticks are closed by `retire`; when every entry is in use and a new surface arrives, the importer returns `GpuError::Unsupported` rather than stall.
3. **Keyed mutexes are not supported on D3D12** (D3D12 cannot acquire them). D3D producers use a shared timeline fence. The importer makes its queue wait for the fence value before the first submission that reads the surface. The sync kind is checked before a surface is opened.
4. **Lease hand-off.** The native lease moves into the importer's record for the pool slot. `import` returns a `Texture` frame whose own pool lease keeps the slot alive for downstream consumers. The native lease is dropped by `retire` once the last queue submission that read the slot has completed (checked with the submission index, non-blocking) and no pool lease on the slot remains, so the producer never reuses a surface the GPU may still read. On D3D12 that hand-back submission also returns the texture to the `COMMON` state (wgpu `TextureUses::PRESENT`), which other APIs expect for shared resources.
5. **Formats.** BGRA8/RGBA8 (UNORM or sRGB) surfaces are aliased as the matching wgpu format; the colour space of the native frame's `VideoFormat` decides `Transfer` for the compositor. NV12/P010 surfaces need a YUV-to-RGB pass and are a follow-up.
6. **Same adapter only.** A surface from another adapter (LUID or registry id mismatch) is `GpuError::WrongAdapter`; cross-adapter copies are out of scope.

Conformance (per backend, on CI where the platform allows; WARP on Windows): a producer-side test creates a shared texture and fence, fills it, signals; the importer aliases it; the compositor renders it; readback matches; the native lease is released only after `retire` follows GPU completion; a second frame on the same surface opens nothing new (`surfaces_opened` unchanged).

### Readback (fallback only)

`readback::read_bytes(ctx, pool, id) -> Result<Vec<u8>, GpuError>` (tightly packed rows) and `readback::read_rgba16f(ctx, pool, id) -> Result<Vec<[f32; 4]>, GpuError>` copy a texture to the CPU. They block, allocate and increment `GpuMetrics::frames_cpu_copied`. They are not on the default path; scenario tests assert the counter stays 0 (performance budgets, hard rules).

`readback::upload(ctx, pool, id, bytes)` writes tightly packed rows into a `COPY_DST` texture. It is for still images, test fixtures and sources without GPU sharing; it is not counted as a readback.

## Errors and failure behaviour

All fallible calls return `GpuError` (no panics, no `unwrap` in library code). Variants: `NoAdapter`, `DeviceRequest`, `StaleTexture`, `UnknownTexture`, `TargetMismatch`, `TooManyLayers`, `PoolFull`, `InvalidSize`, `Readback`. Errors on the render path carry only `Copy` data so building them does not allocate.

Device loss is reported by wgpu through the device-lost callback and surfaced as a metric; recovery (recreate context, re-warm sources) is a graph-level concern for a later spec.

## Performance requirements

- `Compositor::render`, `Nv12Converter::convert` and the pool's `acquire` (reuse), `release` and `get` are hot paths ([hot-paths.md](../architecture/hot-paths.md)). RBS code inside them does not allocate. wgpu allocates internally while recording and submitting command buffers (about 130 allocations per frame for render + NV12 in the first measurement); the `alloc_per_frame` test asserts this does not grow over time and adds at most a small constant per layer. Reducing it (for example re-recorded render bundles) is follow-up work with its own measurements.
- No CPU readback on the default path (P4).
- Installed but unused: a `GpuContext` is created only when the first canvas becomes Active. A compositor or converter that is not Active is dropped, releasing its GPU objects (P13).
- Benchmarks: `benches/compositor.rs` measures 1080p frame time (render, render + NV12, waiting for the GPU) with 1, 8 and 32 layers, and the CPU cost of recording and submitting 32 layers.

## Conformance tests

Run on any adapter, including software adapters in CI (lavapipe on Linux, WARP on Windows). Tests skip with a clear message when `GpuError::NoAdapter` is returned.

- Pool: reuse, generation and stale-id handling, trim, capacity, leases (a leased slot survives the owner's release, is reused after the last lease drops on another thread, frames share the lease).
- Golden images (sampled pixels with tolerance): single opaque layer placement, crop, opacity blend over a background layer, sRGB source decode, layer order, stale layer skipped.
- NV12: known colours (black, white, mid grey, pure red) produce BT.709 limited-range values within ±1.
- Allocations per frame do not grow over time and grow by at most a small constant per layer.
- Readback increments `frames_cpu_copied`.

## Open questions

- Whether the canvas working format should be configurable (`Rgba16Float` vs `Rgba8UnormSrgb` for low-end GPUs).
- DMA-BUF device identity on multi-GPU Linux (DRM render node or device UUID in `DmaBufSurface`).
- Waiting on an imported fence on Vulkan and Metal through wgpu's HAL (D3D12 uses `ID3D12CommandQueue::Wait`).
- Rotation and non-affine transforms (perspective, corner pin) as filters vs layer properties.
