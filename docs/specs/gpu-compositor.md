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
- Conversion of a canvas to NV12 or P010 (two planes, or one multi-planar texture; BT.709) on the GPU.
- Native export of converted frames to encoders (IOSurface for VideoToolbox) and native import of producer surfaces, both below.
- Explicit, counted CPU readback (fallback and tests only).

Out of scope (later specs or PRs):

- Export on D3D12 (NT handle) and Vulkan (exportable memory, DMA-BUF); same frame shape as the macOS exporter (ADR-0025).
- Scene tree evaluation (groups, nesting, animation): `rbs-scene` flattens a scene into the layer list below.
- Filters, masks, blend modes other than "normal", transitions and HDR output (PQ, HLG, BT.2020).

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
- Reported capability keys: `gpu.software` (bool), `gpu.max_texture_dimension` (int), `gpu.timestamp_query` (bool), `gpu.import.dmabuf` (bool: the importer accepts Linux DMA-BUFs), `gpu.import.sync_file` (bool: the importer accepts `SurfaceSync::SyncFile` with DMA-BUFs, see [Native import](#native-import-interop)), `gpu.nv12_surface` (bool: the device creates multi-planar NV12 textures), `gpu.p010` (bool: `R16Unorm` and `Rg16Unorm` are render targets, needed for P010 planes; the 16-bit normalized feature alone guarantees only storage use, so the device also enables wgpu's adapter-specific format features and checks the adapter's format usages), `gpu.p010_surface` (bool: the device creates multi-planar P010 textures). Each optional feature is enabled on the device only when the adapter has it.
- A Vulkan device whose adapter supports `VK_KHR_external_semaphore_fd` with `SYNC_FD` semaphore import (Vulkan 1.1 instance and device), and on which DMA-BUF import is enabled, is opened through wgpu's HAL (`open_with_callback`, then `create_device_from_hal`) with the same features, limits and memory hints, so the importer can add that extension, which wgpu does not enable. This path skips wgpu-core's request-time checks, including the `WGPU_STRICT_WEBGPU_COMPLIANCE` filtering, which RBS does not use. `gpu.import.sync_file` is reported only for such devices. Every other device uses `request_device`, so CI (lavapipe) never takes the HAL path.
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
    pub opaque: bool,            // ignore source alpha (XRGB layouts with an undefined 4th byte)
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

### Encoder conversion (NV12, P010)

```rust
pub enum YuvFormat { Nv12, P010 }

impl YuvConverter {
    /// P010 needs `gpu.p010`; otherwise `GpuError::Unsupported`.
    pub fn new(ctx: &GpuContext, format: YuvFormat, width: u32, height: u32) -> Result<YuvConverter, GpuError>;
    /// Both return the submission of their GPU work, so callers can tell when it completed.
    pub fn convert(&mut self, ctx: &GpuContext, pool: &TexturePool, canvas: TextureId, luma: TextureId, chroma: TextureId)
        -> Result<wgpu::SubmissionIndex, GpuError>;
    /// Needs `gpu.nv12_surface` (NV12) or `gpu.p010_surface` (P010).
    pub fn convert_to_surface(&mut self, ctx: &GpuContext, pool: &TexturePool, canvas: TextureId, surface: TextureId)
        -> Result<wgpu::SubmissionIndex, GpuError>;
}
pub fn luma_desc(format: YuvFormat, width: u32, height: u32) -> TextureDesc;    // R8Unorm / R16Unorm, w x h
pub fn chroma_desc(format: YuvFormat, width: u32, height: u32) -> TextureDesc;  // Rg8Unorm / Rg16Unorm, w/2 x h/2
pub fn surface_desc(format: YuvFormat, width: u32, height: u32) -> TextureDesc; // NV12 / P010, both planes
// PooledTexture::plane_view(plane) gives the plane views of a multi-planar pool texture;
// readback::read_plane(ctx, pool, id, plane) reads one plane (fallback and tests).
```

- Input: a linear `Rgba16Float` canvas, composited over black (alpha is ignored). Output: Y plane (w x h) and interleaved CbCr plane (w/2 x h/2), both render targets: `R8Unorm` and `Rg8Unorm` for NV12, `R16Unorm` and `Rg16Unorm` for P010. Width and height must be even.
- Colour: linear light is encoded with the BT.709 OETF, then converted with BT.709 coefficients to limited range: Y 16-235 and CbCr 16-240 for NV12; Y 64-940 and CbCr 64-960 for P010. P010 stores each 10-bit code in the high bits of a 16-bit word (code << 6, low six bits zero); the shader rounds to whole codes before scaling so the low bits stay zero. The 10-bit output is SDR BT.709 (for example HEVC Main10); HDR transfer functions are out of scope. Chroma is the average of each 2x2 block (centre siting).
- Two render passes per frame (luma at full size, chroma at half size), one full-screen triangle each. Both formats share one shader; the code levels are pipeline-overridable constants, so there is no per-frame branch. Render passes are used instead of compute because the plane formats are render targets, while storage writes to them need optional features.
- The frame's colour fields (primaries, transfer, matrix, range) travel with the converted frame so encoders can write VUI information.
- Hand-off target: hardware encoders (NVENC, AMF, QSV, VideoToolbox, VA-API, Vulkan Video) consume one multi-planar NV12 or P010 surface. Where the device has `gpu.nv12_surface` or `gpu.p010_surface` (D3D12 and Vulkan), `convert_to_surface` writes a single pool texture of `surface_desc`.
  - Hardware adapters render straight into its two plane views.
  - Software adapters (`gpu.software`) render into converter-owned plane textures, then copy them into the multi-planar texture through one GPU buffer. Rendering into plane views is not reliable there: lavapipe writes plane 1 over plane 0. wgpu has no texture-to-texture copy into planes. The staged path costs about 7% more than the plain planes on lavapipe at 1080p (24.3 ms against 22.7 ms, `frame_1080p/nv12_surface` against `nv12_planes`). It never reaches the CPU.
  - Without the surface capability (Metal), the planes are rendered into an IOSurface that VideoToolbox reads; see [Native export](#native-export).
  - Encoders receive the platform surface as a native frame from an exporter ([ADR-0025](../adr/0025-encoder-surface-export.md)). D3D12 (NT handle) and Vulkan (exportable memory, DMA-BUF) exporters are follow-ups with the same frame shape.
- Lease: an exported frame holds a pool lease. The encoder releases it after it has finished reading (fence or semaphore signal, or an explicit release), and the pool never reuses a texture while a lease on it is open.
- CPU fallback for software encoders: on request per output, the converted planes are copied into a mapped staging buffer (NV12 or I420 with per-plane strides). It is opt-in, counted in `frames_cpu_copied`, and never on the default path. Encoders advertise what they accept with the `input.memory` and `input.pixel_format` capabilities of `rbs-codec`.

### Native export

Implements [ADR-0025](../adr/0025-encoder-surface-export.md): encoders get the converted frame as a native frame they are the only consumer of, and never depend on `rbs-gpu`. Lives in `rbs_gpu::export`, a marked low-level module (P11). macOS only so far.

```rust
impl SurfaceExporter {
    /// Creates `surfaces` (2..=MAX_EXPORT_SURFACES = 32) IOSurfaces and aliases their planes.
    /// Non-real-time. P010 needs `gpu.p010`; width and height must be even; the device must
    /// have unified memory, else `GpuError::Unsupported`.
    pub fn new(ctx: &GpuContext, format: YuvFormat, width: u32, height: u32, surfaces: usize)
        -> Result<SurfaceExporter, GpuError>;
    /// Converts `canvas` (a pool canvas) into the next free surface. `GpuError::ExportBusy`
    /// when every surface is pending or held by the encoder: the caller drops the frame.
    pub fn export(&mut self, ctx: &GpuContext, pool: &TexturePool, canvas: TextureId, timestamp: Timestamp, sequence: u64)
        -> Result<(), GpuError>;
    /// The oldest converted frame whose GPU work completed, or `None`. Never blocks.
    /// `GpuError::DeviceLost` once the device is lost (pending frames are discarded).
    pub fn take_ready(&mut self, ctx: &GpuContext) -> Result<Option<VideoFrame>, GpuError>;
    pub fn format(&self) -> &VideoFormat;   // NV12 or P010, ColorSpace::BT709_LIMITED
    pub fn stats(&self) -> ExportStats;     // converted, delivered, busy
    pub fn held(&self) -> usize;            // frames the encoder still holds
    pub fn lease_owner(&self) -> Arc<dyn LeaseOwner>; // kept by the engine until encoder shutdown
}
```

- Surfaces: bi-planar IOSurfaces, `'420v'` (`kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange`, NV12) or `'x420'` (`kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange`, P010), limited range. Row pitch is aligned to IOSurface's row alignment and Metal's minimum linear texture alignment for the plane format; plane offsets are page aligned (4096). After creation the plane count, sizes, bytes per element and row pitch are checked against the request.
- Each plane is aliased as a Metal render target (`R8Unorm`/`RG8Unorm`, or `R16Unorm`/`RG16Unorm`) owned by the exporter; it uses no pool slots. The two-plane conversion renders into them, so there is no copy and no CPU readback.
- Unified memory only (Apple silicon) for now: whether GPU writes to an IOSurface-backed texture are visible to VideoToolbox on a discrete GPU without a synchronize step is not verified. Intel Macs with a discrete GPU fall back to the CPU path until it is.
- The lease owner is the ring: it retains every IOSurface and keeps one atomic count per surface. A frame's lease keeps the ring, and so its surface, alive whatever is dropped first (exporter, pool, device). All surfaces are created in `new`; the ring never grows. A surface is written only when it is not pending and its count is zero (`Acquire` load, paired with the `Release` decrement of the encoder's last lease).
- `take_ready` polls the conversion's submission without waiting (`PollError::Timeout` means not yet). When it has completed and the device is not lost, the frame is `VideoFrame::native(format, t, NativeSurface::io_surface(ptr), SurfaceSync::Ready, lease)` with the exporter's sequence number. Metal reports a failed command buffer as completed. The device-lost check stops delivery after a loss; a non-fatal command buffer error (for example a GPU timeout that does not lose the device) is not detected and can deliver a frame with wrong contents (memory-safe; residual risk until wgpu exposes command buffer status).
- Frames come out in conversion order. Call `take_ready` until `None` once per render tick; a frame is normally ready on the tick after its conversion. The poll is the same non-blocking `device.poll` the importer's `retire` already runs every tick.
- Ring size: at least the encoder input queue's capacity plus the frames the encoder API keeps in flight (look-ahead, reordering) plus two. A capability for frames in flight is an open question; until then the encoder adapter documents its figure.
- Consumer rules (encoder adapters): wrap the surface with `CVPixelBufferCreateWithIOSurface`, set the colour attachments from the frame's `ColorSpace`, and drop the frame once VideoToolbox has finished with it: after its output callback, including when the frame was dropped (`kVTEncodeInfo_FrameDropped`) or the session was invalidated. A cache of pixel buffers keyed by surface address is allowed only while it also holds the frame's lease or retains the surface; clear it when the encoder stops, so it never keeps a ring alive.
- The encoder node wrapper keeps `lease_owner()` until the encoder worker has shut down and drops it on a background thread, so the last ring reference, which releases the surfaces, is never dropped on a real-time thread.
- Installed but unused costs nothing: an exporter exists only while an encoder that takes Metal memory is Active (P13). Dropping it frees its GPU objects; surfaces live on only while frames hold them.

### Native import (interop)

Implements the importer side of [media-types.md, Native surfaces](media-types.md#native-surfaces). Lives in `rbs_gpu::interop`, a marked low-level module (P11) with one submodule per backend, compiled only on its platform.

```rust
impl Importer {
    pub fn new() -> Importer;   // holds no GPU objects until the first import
    /// Takes a native frame by value and returns a `Texture` frame of the same format and time.
    pub fn import(&mut self, ctx: &GpuContext, pool: &mut TexturePool, frame: VideoFrame)
        -> Result<VideoFrame, GpuError>;
    /// Releases native leases whose reading GPU work has completed. Once per render tick.
    /// `now` is the engine's monotonic time (the tick's master-clock time).
    pub fn retire(&mut self, ctx: &GpuContext, pool: &mut TexturePool, now: Timestamp);
    /// Closes cached surfaces that no frame and no producer lease use. Call when a source stops.
    pub fn close_unused(&mut self, pool: &mut TexturePool);
}
```

Backend matrix (implemented rows in bold):

| Surface | Render backend | Sync accepted | Notes |
| --- | --- | --- | --- |
| **`D3dSharedHandle` (NT)** | **D3D12** | **`Ready`, `TimelineFence` (shared D3D11/D3D12 fence)** | `OpenSharedHandle`; the adapter LUID must match the render device |
| `D3d12Resource` | D3D12 | `Ready`, `TimelineFence` | same-process resource |
| `D3d11Texture` | none | none | not importable: the producer must share it with an NT handle |
| **`IoSurface`** | **Metal** | **`Ready`** | `newTextureWithDescriptor:iosurface:plane:`; IOSurfaces are not tied to a device, so there is no identity check. Single-plane `'BGRA'`, `'RGBA'` and `'RGhA'` (RGBA half float) surfaces |
| `MetalTexture` | Metal | `Ready` | `device_registry_id` must match |
| **`DmaBuf`** | **Vulkan** | **`Ready`, `SyncFile` (with `gpu.import.sync_file`)** | wgpu-hal `texture_from_dmabuf_fd` (`VK_EXT_external_memory_dma_buf` with `VK_EXT_image_drm_format_modifier`), capability `gpu.import.dmabuf`. Single plane, `AR24`/`XR24` (ARGB8888/XRGB8888) and `AB24`/`XB24` (ABGR8888/XBGR8888). For the X formats the source sets `Layer::opaque`, because their fourth byte is undefined. The importer duplicates the producer's fd; the cache key is the buffer's inode, not the fd number. The size comes from the frame's `VideoFormat`. Device identity is not checked: an incompatible modifier fails the import |

`GpuContext` reports what the importer accepts, for capture requests (`GpuImportTarget { device, sync }` of the capture port spec): the render adapter's identity (D3D LUID as `u64` = `HighPart << 32 | LowPart`, from `GpuContext::adapter_luid`; Metal registry id for `MetalTexture`) and the sync kinds in the table below. Producers create surfaces on that adapter.

Rules:

1. **Alias, do not copy.** A supported surface is wrapped zero-copy as a pool texture (`create_texture_from_hal`). A surface that cannot be aliased (unsupported format or backend) is an error (`GpuError::Unsupported`), not a silent CPU path; the source falls back to its `System` payload if it offers one.
2. **Open once.** Producers reuse a small ring of surfaces. Shared fences come from a process-wide producer pool and are never closed, so the fence cache needs no eviction. The importer caches opened surfaces and fences by handle value or object address (at most 64 entries per importer, enough for about ten capture sessions with rings of 5 plus resize spares) and reuses the pool slot of a cached surface, so steady-state import does not open handles or allocate. First sight of a surface opens it (allocates, counted in `GpuMetrics::surfaces_opened`). Entries unseen for 8 s of engine time (`SURFACE_IDLE`, whatever the tick rate) are closed by `retire`; when every entry is in use and a new surface arrives, the importer returns `GpuError::Unsupported` rather than stall. Following [capture-port.md](capture-port.md), "Native surface rules": a producer keeps a handed-out surface's handle open for at least 10 s after its last reference, also after the session stops, and never closes shared fences. The 8 s idle limit therefore forgets a handle value before the OS can reuse it. The owner may also call `close_unused` when a source stops, to free the surfaces early. As a cheap extra guard, a cached entry that arrives with a frame size other than the one it was first seen with is treated as a different object and reopened.
3. **Keyed mutexes are not supported on D3D12** (D3D12 cannot acquire them). D3D producers use a shared timeline fence. The importer makes its queue wait for the fence value before the first submission that reads the surface. The sync kind is checked before a surface is opened.
4. **Sync files are waited on by the GPU.** For `SurfaceSync::SyncFile` the importer first checks the file with a zero-timeout `poll` (a sync file is readable once signalled); a signalled file needs no GPU work. Otherwise it duplicates the fd, imports the duplicate into a pooled binary semaphore (`vkImportSemaphoreFdKHR`, `SYNC_FD`, temporary import, so Vulkan owns and closes the duplicate) and stages a wait for all commands with wgpu-hal's `Queue::add_wait_semaphore`. The queue's next submission consumes the wait; on the render thread that is the render that reads the frame, so the reading batch itself waits on the fence. A later reader in another batch is ordered after it only through wgpu-hal's submission chaining, which relies on drivers ordering whole submissions; this is to be verified on hardware. While the fence is pending, the whole render queue waits (other canvases and encoder conversions too), the same trade-off as the D3D12 fence wait. `retire` moves consumed waits to a busy list and submits one empty batch to get an index to poll (completion tracking only); a wait that no submission consumed yet is staged again. A semaphore is reused once that index has completed; only the oldest busy one is polled. At most 16 waits (`MAX_SYNC_WAITS`) are in flight, after which the frame is rejected with `GpuError::Unsupported` rather than stall. The semaphores are created on the first pending sync file (P13) and destroyed when the importer is dropped, after a bounded wait; any the GPU may still use, or whose wait was never submitted, are leaked instead. The producer's own fd is never closed or kept. The CPU never waits on a sync file (P6). A sync file whose fence signalled with an error is treated as signalled. A negative fd is `GpuError::Import`: producers deliver `Ready` instead of Vulkan's "-1 means signalled" convention.
5. **Lease hand-off.** The native lease moves into the importer's record for the pool slot. `import` returns a `Texture` frame whose own pool lease keeps the slot alive for downstream consumers. The native lease is dropped by `retire` once the last queue submission that read the slot has completed (checked with the submission index, non-blocking) and no pool lease on the slot remains, so the producer never reuses a surface the GPU may still read. On D3D12 that hand-back submission also returns the texture to the `COMMON` state (wgpu `TextureUses::PRESENT`), which other APIs expect for shared resources.
6. **Formats.** BGRA8/RGBA8 (UNORM or sRGB) surfaces are aliased as the matching wgpu format; the colour space of the native frame's `VideoFormat` decides `Transfer` for the compositor. NV12/P010 surfaces need a YUV-to-RGB pass and are a follow-up.
7. **Same adapter only.** A surface from another adapter (LUID or registry id mismatch) is `GpuError::WrongAdapter`; cross-adapter copies are out of scope.

Conformance (per backend, on CI where the platform allows; WARP on Windows, the runner's Metal device on macOS; DMA-BUF only on Linux machines with a hardware Vulkan driver and `/dev/udmabuf`, since lavapipe has no DMA-BUF import, with `RBS_REQUIRE_DMABUF` to make it mandatory): a producer-side test creates a shared texture (and fence where the backend uses one), fills it, signals; the importer aliases it; the compositor renders it; readback matches; the native lease is released only after `retire` follows GPU completion; a second frame on the same surface opens nothing new (`surfaces_opened` unchanged). Sync files (same machines, with `gpu.import.sync_file`): a signalled sync file exported from the udmabuf (`DMA_BUF_IOCTL_EXPORT_SYNC_FILE`, Linux 6.0+) is accepted and renders; where `sw_sync` is available (debugfs, `CONFIG_SW_SYNC`, root), the render submitted after a pending sync file has not completed 200 ms later and completes with the right pixels once the timeline is signalled. Without the capability, `SyncFile` is `GpuError::Unsupported`. CI's lavapipe has neither extension, so CI checks only the rejections; the HAL device path, the semaphore wait and its retirement run only on hardware.

### Graph runtime and compositor node

`rbs_gpu::node` connects the GPU crate to the graph runtime of [media-graph.md, "Runtime"](media-graph.md) (ADR-0020). Following that ADR, `rbs-gpu` depends on `rbs-graph`; `rbs-graph` never names `wgpu`.

```rust
pub struct GpuRuntime;                       // Runtime { Shared = GpuContext, Local = RenderLocal }
pub struct RenderLocal { pub pool: TexturePool, pub importer: Importer }  // owned by the render executor

pub const COMPOSITOR_NODE_TYPE: &str = "org.rbs.compositor";
pub fn compositor_descriptor() -> NodeDescriptor;   // kind Compositor; input "layers" (many-link Video), output "canvas" (Video)

pub struct PixelCrop { pub left: u32, pub top: u32, pub right: u32, pub bottom: u32 }
pub struct NodeLayer {
    pub producer: NodeId,     // the graph node whose video this layer draws (BoundLayer::producer)
    pub link: usize,          // hint: index of its link on the "layers" pin when the list was made
    pub transform: Affine2,   // unit square of the cropped source to canvas pixels
    pub crop: PixelCrop,      // in source pixels; turned into texture coordinates with the real source size
    pub opacity: f32,         // 0.0..=1.0; the value a fade ends at
    pub fade: Option<Fade>,   // animates opacity, evaluated against tick time
    pub opaque: bool,         // ignore source alpha (XRGB sources)
}
pub struct Fade { pub from: f32, pub start: Timestamp, pub duration: Duration }  // linear from `from` to `opacity`

pub type LayerSender = ParamSender<Vec<NodeLayer>>;

impl CompositorNode {
    /// `capacity` is the most layers the node draws; it sizes the compositor in `prepare`.
    pub fn new(canvas: CanvasDesc, capacity: u32, layers: Vec<NodeLayer>) -> (CompositorNode, LayerSender);
    pub fn capacity(&self) -> u32;
}
```

- `GpuRuntime::end_tick` retires imported surfaces with the tick's time (`Importer::retire(ctx, pool, tick.time)`) and polls the device (`GpuContext::poll`). `begin_tick` does nothing.
- `prepare` (control thread) builds the `Compositor` for the canvas and reserves room for the layer list. `release` drops it.
- `warm` (render thread) acquires and releases three canvas textures, so the pool's free list holds a ring and `process` reuses them without allocating. `cool` does nothing: the textures stay in the pool's free list for the next user or a trim.
- `process` does the following each tick:
  1. Takes the newest layer list from the parameter channel, if one was sent (`ParamReceiver::update`; no allocation, no waiting).
  2. Builds the layer list in a reserved buffer. Each `NodeLayer` finds its link: the hint is used when `Inputs::source(LAYERS_PIN, link)` is its producer, else `Inputs::find(LAYERS_PIN, producer)` looks it up (a list can meet an older or newer schedule, and link positions shift when a producer leaves the plan). A layer whose producer is not scheduled, whose link has no frame, or whose frame is not a pool texture is skipped; native frames are imported by the source wrapper before they reach a link. Layers past `capacity` are not drawn.
  3. Converts the crop with the texture's size. The source transfer is `Srgb` when the frame's colour space says sRGB and the texture format does not decode it in the sampler.
  4. Evaluates a fade at the tick's time: before `start` the opacity is `from`, after `start + duration` it is `opacity`, linear in between.
  5. Acquires a canvas texture, renders, and publishes it on "canvas": `Rgba16Float`, `ColorSpace::LINEAR_BT709`, the tick's time, and sequence = tick index. The pool slot is released at once, so the frame's lease keeps it alive.
  6. RBS code allocates nothing here (hot path). A ring larger than three only grows when consumers hold more frames than that.
- Layer lists reach the node through the graph runtime's parameter channel ([ADR-0023](../adr/0023-node-parameter-channels.md)): the latest list wins, and the control side reuses the replaced list with `LayerSender::recycle` before `send`, so steady edits allocate nothing on either side. Fades are sent once as targets with timing (ADR-0023, decision 7), not as one list per frame.
- Growing past `capacity` needs a new node (the compositor's layer buffer is sized in `prepare`, a non-real-time call). The application picks a capacity with room for the scene, for example the scene's layer count rounded up to a multiple of 16.

### Readback (fallback only)

`readback::read_bytes(ctx, pool, id) -> Result<Vec<u8>, GpuError>` (tightly packed rows) and `readback::read_rgba16f(ctx, pool, id) -> Result<Vec<[f32; 4]>, GpuError>` copy a texture to the CPU. They block, allocate and increment `GpuMetrics::frames_cpu_copied`. They are not on the default path; scenario tests assert the counter stays 0 (performance budgets, hard rules).

`readback::upload(ctx, pool, id, bytes)` writes tightly packed rows into a `COPY_DST` texture. It is for still images, test fixtures and sources without GPU sharing; it is not counted as a readback.

## Errors and failure behaviour

All fallible calls return `GpuError` (no panics, no `unwrap` in library code). Variants: `NoAdapter`, `DeviceRequest`, `StaleTexture`, `UnknownTexture`, `TargetMismatch`, `TooManyLayers`, `Unsupported`, `WrongAdapter`, `Import`, `ExportBusy`, `Export`, `DeviceLost`, `PoolFull`, `InvalidSize`, `Readback`. Errors on the render path carry only `Copy` data so building them does not allocate.

Device loss is reported by wgpu through the device-lost callback and surfaced as a metric; recovery (recreate context, re-warm sources) is a graph-level concern for a later spec.

## Performance requirements

- `Compositor::render`, `YuvConverter::convert`, `YuvConverter::convert_to_surface`, `SurfaceExporter::export` and `take_ready`, and the pool's `acquire` (reuse), `release` and `get` are hot paths ([hot-paths.md](../architecture/hot-paths.md)). RBS code inside them does not allocate. wgpu allocates internally while recording and submitting command buffers (about 130 allocations per frame for render + NV12 in the first measurement); the `alloc_per_frame` test asserts this does not grow over time and adds at most a small constant per layer. Reducing it (for example re-recorded render bundles) is follow-up work with its own measurements.
- No CPU readback on the default path (P4).
- Installed but unused: a `GpuContext` is created only when the first canvas becomes Active. A compositor or converter that is not Active is dropped, releasing its GPU objects (P13).
- Benchmarks: `benches/compositor.rs` measures 1080p frame time (render, render + NV12, waiting for the GPU) with 1, 8 and 32 layers, and the CPU cost of recording and submitting 32 layers.

## Conformance tests

Run on any adapter, including software adapters in CI (lavapipe on Linux, WARP on Windows). Tests skip with a clear message when `GpuError::NoAdapter` is returned.

- Pool: reuse, generation and stale-id handling, trim, capacity, leases (a leased slot survives the owner's release, is reused after the last lease drops on another thread, frames share the lease).
- Golden images (sampled pixels with tolerance): single opaque layer placement, crop, opacity blend over a background layer, sRGB source decode, layer order, stale layer skipped, an `opaque` layer drawn opaque over a source whose alpha is 0.
- NV12: known colours (black, white, mid grey, pure red) produce BT.709 limited-range values within ±1.
- P010 (where `gpu.p010`; required on Linux CI): the same colours produce 10-bit values within ±2, and the low six bits of every word are zero.
- NV12 and P010 surfaces (where `gpu.nv12_surface` / `gpu.p010_surface`; required on Linux CI): both planes of `convert_to_surface` equal the two-plane output byte for byte on a two-colour canvas; a texture of another format is rejected.
- Allocations per frame do not grow over time and grow by at most a small constant per layer.
- Readback increments `frames_cpu_copied`.
- Export (macOS CI, the runner's Metal device): NV12 and P010 (where `gpu.p010`) surface bytes, read through the IOSurface API with its plane strides, equal the two-plane output byte for byte on a two-colour canvas; the frame is an `IoSurface` frame with `Ready` sync, the right pixel format, time and sequence; the surface wraps as a `CVPixelBuffer` of the same format, size and plane count; held surfaces are not reused and a full ring returns `ExportBusy`; frames come out in order, and frames dropped on another thread free their surfaces; a frame stays valid and unchanged after both the exporter and the pool are dropped; odd sizes and ring sizes outside 2..=32 are rejected; a steady export loop allocates at most 4 more per frame than the same conversion into pool planes and does not grow.

## Open questions

- Whether the canvas working format should be configurable (`Rgba16Float` vs `Rgba8UnormSrgb` for low-end GPUs).
- DMA-BUF device identity on multi-GPU Linux (DRM render node or device UUID in `DmaBufSurface`).
- Waiting on an imported fence on Metal (`MTLSharedEvent`) through wgpu's HAL (D3D12 uses `ID3D12CommandQueue::Wait`, Vulkan imports sync files). Metal producers deliver `Ready` surfaces until then.
- DRM syncobj timeline points (PipeWire explicit sync, `linux-drm-syncobj-v1`): a producer can export a point as a sync file today (`DRM_IOCTL_SYNCOBJ_EXPORT_SYNC_FILE` after the point materialized); a timeline-semaphore import of the syncobj would avoid that step and needs a new `SurfaceSync` kind.
- The Vulkan sync-file wait has not run on hardware yet (no CI machine has a hardware Vulkan driver); it needs a run on a Linux machine with `RBS_REQUIRE_DMABUF=1`.
- D3D cache key: an NT handle value can be reused by the OS after the producer closes the handle, so a cached entry could alias a different texture. Producers keep ring handles open for the session today; validating entries (for example `CompareObjectHandles` on a duplicated handle) is a follow-up. IOSurface addresses cannot be reused while cached, because the Metal texture retains the surface.
- Encoder frames in flight: a capability (for example `codec.video.frames_in_flight`) so the export ring can be sized from the encoder instead of the adapter's documentation.
- IOSurface export on Macs with a discrete GPU (storage mode and synchronization), and a sync-object hand-off (`MTLSharedEvent`) instead of `Ready` to save the poll latency.
- Rotation and non-affine transforms (perspective, corner pin) as filters vs layer properties.
