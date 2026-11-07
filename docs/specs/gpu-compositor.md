# Spec: GPU context, texture pool and compositor

- Status: Draft
- Owner crate(s): `rbs-gpu`
- Related ADRs: [0005](../adr/0005-gpu-compositing-with-wgpu.md), [0013](../adr/0013-zero-cost-when-unused.md), [0022](../adr/0022-native-ui-toolkit.md), [0025](../adr/0025-encoder-surface-export.md), [0028](../adr/0028-cross-process-preview-surfaces.md)
- Related specs: [media-graph.md](media-graph.md), [performance-budgets.md](performance-budgets.md)

## Purpose

`rbs-gpu` owns every GPU resource of the engine. It gives the render loop a device, a pool of reusable textures addressed by small ids, a compositor that draws a flat list of layers into a canvas texture, and the conversion of a canvas into encoder formats. Higher layers (`rbs-scene`, `rbs-graph`) decide *what* to draw; this crate decides *how*, without allocating per frame in its own code.

## Scope

In scope:

- Device creation and adapter selection by capability (headless, no window).
- Texture pool with allocation-free reuse, stale-id detection and leases for consumers on other threads.
- Compositor: one per canvas; draws layers with a 2D affine transform, source crop, opacity and premultiplied alpha blending, in linear light on `Rgba16Float`.
- Conversion of a canvas to NV12 or P010 (two planes, or one multi-planar texture; BT.709) on the GPU.
- Native export of converted frames to encoders (IOSurface for VideoToolbox), preview export to a UI in another process, and native import of producer surfaces, all below.
- Explicit, counted CPU readback (fallback and tests only).

Out of scope (later specs or PRs):

- Export on D3D12 (NT handle) and Vulkan (exportable memory, DMA-BUF); same frame shape as the macOS exporter (ADR-0025). Cross-process preview on Linux and Windows (ADR-0028).
- Scene tree evaluation (groups, nesting, animation): `rbs-scene` flattens a scene into the layer list below.
- Filters, image masks, blend modes other than "normal", transitions and HDR output (PQ, HLG, BT.2020). Analytic shapes and borders of layers are in scope (ADR-0053).

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
- Among adapters of the same kind, the backend whose native interop RBS implements comes first: on Windows D3D12 before Vulkan (every shared surface there is a D3D12 object).
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
    pub style: LayerStyle,       // shape and border (ADR-0053); the default draws the picture as it is
}

pub enum LayerShape { Box, Rounded { radius: f32 }, Circle, Ellipse }
pub struct LayerStyle {
    pub shape: LayerShape,
    pub border_width: f32,       // box units, inside the shape's edge; 0 for none
    pub border_color: [f32; 4],  // linear, premultiplied (LayerStyle::new converts sRGB8 straight alpha)
    pub box_size: [f32; 2],      // the box in its own units (the item's |Layout::size|, before nesting)
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
- `UvRect` is ordered (`u0 < u1`, `v0 < v1`). A layer whose crop is not (`u1 <= u0` or `v1 <= v0`, or NaN: a crop larger than the picture) is skipped in both loops of `render` and counted in `RenderStats::empty_crop_layers` (ADR-0057); without the skip, both pipelines would draw it mirrored.
- The target must be an `Rgba16Float` texture of the canvas size with render-attachment usage, else `GpuError::TargetMismatch`.
- **Styles (ADR-0053).** A layer whose style draws nothing extra (`LayerStyle::is_plain`: a box without a border, a box size with no area, values that are not finite) and a layer whose transform is singular are drawn by the plain pipeline exactly as before: same shader, same 64-byte record. Other layers are drawn by a second pipeline (`vs_styled`, `fs_styled`; same pipeline layout, blend state and target) that reads a 48-byte style record (kind, radius, border width; box size; border colour) from a second storage binding in group 0, indexed by the layer record's `params.w`; the plain shader never reads it. `render` fills both arrays in one loop over the drawn layers (skipped layers leave no record, so instance and style indices stay in step) and switches pipelines only between consecutive layers that differ. Both arrays are sized by `new` and `reserve_layers`. The styled vertex shader grows the quad by one canvas pixel on each side; the fragment shader clamps the texture coordinate to the crop, evaluates the signed distance to the shape on `unit × box_size` (rounded rectangle with the radius clamped to half the shorter side, the largest centred circle, or an ellipse by its gradient-normalised implicit function), converts it to canvas pixels along the edge normal (`w = length(Jᵀ n)`, `J` from `dpdx`/`dpdy` of the box position taken before any branch, `n` by central differences) and computes `outer = clamp(0.5 - d / w)` and `inner` the same for `d + border_width`. The colour is `(border · t + content · (1 - border.a · t)) · outer · opacity` with `t = (outer - inner) / outer` (0 where `outer` is 0), after the XRGB and sRGB handling of plain layers.

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

### YUV source decode

Cameras, hardware decoders and native capture surfaces can deliver 4:2:0 frames. The compositor samples RGB, so these are decoded once per frame into a canvas-format texture. This lives in `rbs_gpu::yuv_decode`.

```rust
pub enum YuvSource { Planes { luma: TextureId, chroma: TextureId }, Surface(TextureId) }
impl YuvDecoder {
    pub fn new(ctx: &GpuContext) -> YuvDecoder;
    /// Decodes into `target` (a pool texture of `decoded_desc(w, h)` at the luma size).
    pub fn decode(&mut self, ctx: &GpuContext, pool: &TexturePool, source: YuvSource, color: ColorSpace, target: TextureId)
        -> Result<wgpu::SubmissionIndex, GpuError>;
    /// Drops cached bind groups that reference pool slot `id`. Call when planes are released.
    pub fn forget(&mut self, id: TextureId);
}
pub fn check_color(color: ColorSpace, bits: u32) -> Result<(), GpuError>; // decodable? no GPU work
pub fn decoded_desc(width: u32, height: u32) -> TextureDesc;   // = TextureDesc::canvas
pub fn decoded_color(source: ColorSpace) -> ColorSpace;         // linear, source primaries
```

- Input: two sampleable planes, `R8Unorm` + `Rg8Unorm` (NV12) or `R16Unorm` + `Rg16Unorm` (P010, 10-bit codes in the high bits), with the chroma plane half the luma size. Alternatively, one sampleable multi-planar `NV12` / `P010` texture read through its plane views. Width and height must be even. Other formats are `GpuError::TargetMismatch`.
- Colour: the source's `ColorSpace` decides the matrix (BT.601, BT.709, BT.2020 NCL) and the range (limited: Y 16-235 and C 16-240, scaled by 4 for 10-bit; full: 0 to 2^bits - 1, chroma centred at 2^(bits-1)). It also decides the transfer that is undone: the inverse BT.709 OETF (exact round trip with the encoder conversion), the sRGB EOTF, or none for linear. PQ and HLG (HDR) and an `Rgb` matrix are `GpuError::Unsupported`. The output is linear light with the source's primaries, alpha 1; draw it with `Transfer::Linear`. Gamut conversion between primaries is out of scope, as it is for RGB sources.
- Chroma is sampled bilinearly at each pixel centre (centre siting, as the encoder conversion writes it). MPEG-2 left siting is a follow-up, if cameras need it.
- One render pass with one full-screen triangle, submitted per frame. The scale, range and matrix fold into a 3x4 affine matrix computed on the CPU. The transfer is a uniform branch, the same for every pixel of the draw. The bind group and parameter buffer are cached per source (by plane ids, which carry a generation, at most `MAX_DECODE_SOURCES` = 64, least recently used replaced). The buffer is rewritten only when the colour space changes. A cached bind group keeps its plane textures alive, so callers call `forget` when they release a source's planes (the importer does so when it closes a surface); entries whose planes left the pool are also dropped on the next cache miss. Steady-state cost assumes stable plane slots per source (as the importer's are); a caller that acquires new planes every frame creates a bind group every frame. Steady-state decoding allocates no more than wgpu's own recording and submission (`alloc_decode` test: 56 allocations per frame on lavapipe against 66 for one NV12 conversion; it must not grow and must stay within 4 of the conversion).
- Hot path (`Colour conversion` in [hot-paths.md](../architecture/hot-paths.md)). Benchmark: `frame_1080p/nv12_decode`.

### System-memory upload

Sources that deliver CPU buffers (Media Foundation cameras, the synthetic test adapter) are uploaded by `rbs_gpu::upload::SourceUpload` in the capture source node ([io-nodes.md](io-nodes.md), ADR-0038).

```rust
pub const UPLOAD_FORMATS: [PixelFormat; 4] = [Nv12, P010, Bgra8, Rgba8];
pub fn supports(pf: PixelFormat) -> bool;
impl SourceUpload {
    pub fn new() -> SourceUpload;                       // holds nothing
    /// `System` frame -> `Texture` frame of the same timestamp, sequence and source time;
    /// other frames are returned unchanged. Hot path.
    pub fn upload(&mut self, ctx: &GpuContext, pool: &mut TexturePool, frame: VideoFrame)
        -> Result<VideoFrame, GpuError>;
    pub fn prepare_for(&mut self, ctx: &GpuContext, pool: &mut TexturePool, format: &VideoFormat)
        -> Result<(), GpuError>;                        // optional: creates what `upload` would on first use
    pub fn release(&mut self, pool: &mut TexturePool);  // returns the plane textures
    pub fn uploaded(&self) -> u64;
}
```

- `Bgra8` / `Rgba8`: written with `Queue::write_texture` (any row stride) into a pool texture acquired per frame (`Bgra8Unorm` / `Rgba8Unorm`, sampled, `COPY_DST | COPY_SRC`) and released at once, so the frame's lease keeps it, as for the canvas. The frame keeps its own format and colour space; the compositor applies the sRGB transfer when the colour space says so.
- `Nv12` / `P010`: written into two plane textures the upload keeps (`R8Unorm` + `Rg8Unorm`, or the 16-bit pair), one pair per size and bit depth, then decoded by the upload's own `YuvDecoder` (rule 6 of "Native import") into a canvas-format pool texture acquired per frame. The result is `Rgba16Float` with `decoded_color(...)`. One pair is enough: queue order puts the next frame's writes after the previous decode. Colour spaces the decode cannot handle are refused before anything is written (`check_color`).
- Not real-time on first use: a new size or format creates its plane textures and pool slots, and the first 4:2:0 frame creates the decoder (P13), like the importer's first sight of a surface. Later frames of that size allocate nothing in RBS code; wgpu stages the bytes internally.
- Other system formats (YUY2, I420, ...) are `GpuError::Unsupported`.

### System-memory download

`rbs_gpu::download::YuvDownload` is the CPU fallback of the encoder conversion, for encoders that take only system memory (NVENC and AMF on Windows today; ADR-0038). It is used by the video export node ([io-nodes.md](io-nodes.md)).

```rust
pub const DOWNLOAD_RING: usize = 4;      // staging buffers in flight
pub const DOWNLOAD_BUFFERS: usize = 6;   // recycled frame buffers
impl YuvDownload {
    pub fn new(ctx: &GpuContext, format: YuvFormat, width: u32, height: u32) -> Result<YuvDownload, GpuError>;
    pub fn format(&self) -> &VideoFormat;  // size, NV12/P010, BT709_LIMITED
    pub fn stride(&self) -> usize;         // bytes per row of both planes, a multiple of 256
    /// Converts and starts the readback; `Ok(false)` (counted `busy`) when every staging
    /// buffer is in flight. Hot path, never waits.
    pub fn submit(&mut self, ctx: &GpuContext, pool: &TexturePool, canvas: &VideoFrame) -> Result<bool, GpuError>;
    /// The newest completed readback as a `System` frame; older completed ones are dropped
    /// (`stale`). Hot path, never waits.
    pub fn take_ready(&mut self, ctx: &GpuContext) -> Result<Option<VideoFrame>, GpuError>;
    pub fn take_oldest(&mut self, ctx: &GpuContext) -> Result<Option<VideoFrame>, GpuError>; // ADR-0045
    pub fn stats(&self) -> DownloadStats;  // converted, delivered, busy, stale, no_buffer, failed
    pub fn in_flight(&self) -> usize;
}
```

- `submit` records the conversion (the `YuvConverter` passes) into two plane textures the download owns, copies both planes into one staging buffer (luma rows, then chroma rows, both with `stride`), submits once and calls `map_async`; the callback only stores an atomic state.
- `take_ready` walks the ring from the oldest slot while slots are complete, after `GpuContext::poll` (the render runtime polls at the end of each tick). The newest mapped slot is copied (one `memcpy` of the whole buffer) into a frame buffer that no consumer holds (`Bytes::is_unique`, then `try_into_mut`; the buffers are created in the shared representation, so this keeps their allocation), unmapped, and returned as a `System` frame with the canvas frame's timestamp and sequence and plane layouts at offsets `0` and `stride * height`. With every buffer held by consumers the frame is dropped (`no_buffer`). `take_oldest` does the same for the oldest completed slot and leaves newer completed slots for later calls (no `stale` drops), for the end of a stream (ADR-0045); both share one implementation chosen at compile time.
- Every delivered frame increments `GpuMetrics::frames_cpu_copied` (P4).
- A map failure frees the slot and returns `GpuError::Readback` once.

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

### Preview export (cross-process)

Implements [ADR-0028](../adr/0028-cross-process-preview-surfaces.md) and the export side required by [ADR-0022](../adr/0022-native-ui-toolkit.md), decision 2: the engine shares program, preview and projector images with a UI in another process as native surfaces, never as pixels. Lives in `rbs_gpu::share`, a marked low-level module (P11). Backends: macOS (IOSurfaces over Mach, below) and Windows (shared D3D12 textures over a named pipe, [ADR-0036](../adr/0036-windows-preview-layer.md), see [Windows](#windows-backend-adr-0036)); Linux (DMA-BUF over a Unix socket) follows with the same messages.

Engine side, owned by the render executor:

```rust
pub const MAX_PREVIEW_SURFACES: usize = 8;

pub struct PreviewDesc { pub width: u32, pub height: u32, pub surfaces: usize }

impl PreviewPublisher {
    /// Creates `surfaces` (2..=8) 'RGBA' IOSurfaces, aliases them as render targets and
    /// registers a Mach service under a fresh name. Non-real-time.
    pub fn new(ctx: &GpuContext, desc: PreviewDesc) -> Result<PreviewPublisher, GpuError>;
    /// What a UI needs to connect: the service name and the access token.
    pub fn endpoint(&self) -> &PreviewEndpoint;          // { service: String, token: u64 }
    /// Scales `canvas` into the next free surface. Does nothing (counted in `skipped`)
    /// while no UI is subscribed. `GpuError::ExportBusy` when every surface is pending or
    /// held by the UI: the frame is dropped.
    pub fn publish(&mut self, ctx: &GpuContext, pool: &TexturePool, canvas: TextureId,
        timestamp: Timestamp, sequence: u64) -> Result<(), GpuError>;
    /// Handles subscribe and release messages and the UI's end, and sends every converted
    /// frame whose GPU work has completed. Never blocks. Once per render tick.
    pub fn tick(&mut self, ctx: &GpuContext) -> Result<(), GpuError>;
    pub fn size(&self) -> (u32, u32);
    pub fn subscribed(&self) -> bool;
    pub fn held(&self) -> usize;                         // surfaces the UI holds
    pub fn stats(&self) -> PreviewStats;                 // converted, sent, skipped, busy, dropped, subscribes, rejected
    pub fn surface_ids(&self) -> impl Iterator<Item = u32>; // IOSurfaceGetID, for tests
}
```

UI side, in the UI process:

```rust
impl PreviewSubscriber {
    /// Looks up the service, subscribes with the token and receives the surfaces.
    /// Blocks up to `timeout`; not for real-time threads.
    pub fn connect(endpoint: &PreviewEndpoint, timeout: Duration) -> Result<PreviewSubscriber, GpuError>;
    pub fn size(&self) -> (u32, u32);
    pub fn surfaces(&self) -> usize;
    pub fn surface_id(&self, slot: usize) -> Option<u32>;
    /// The next frame, waiting up to `timeout` (zero polls). `Ok(None)` on timeout,
    /// `GpuError::Disconnected` once the engine is gone.
    pub fn recv(&mut self, timeout: Duration) -> Result<Option<PreviewFrame>, GpuError>;
    /// Aliases every surface on the UI's device as `Rgba8UnormSrgb` with
    /// `TEXTURE_BINDING | RENDER_ATTACHMENT | COPY_SRC` (what Slint's `Image::try_from` takes).
    pub fn textures(&self, ctx: &GpuContext) -> Result<Vec<wgpu::Texture>, GpuError>;
}

impl PreviewFrame {                       // Send; dropping it releases the surface
    pub fn slot(&self) -> usize;
    pub fn sequence(&self) -> u64;
    pub fn timestamp(&self) -> Timestamp;
}
```

UI side, layer path (macOS, `rbs-gpu` feature `preview-layer`, [ADR-0034](../adr/0034-preview-as-native-layer.md)); the default way to show the preview:

```rust
pub struct LayerRect { pub x: f64, pub y: f64, pub width: f64, pub height: f64 } // points, top-left origin
pub const DEFAULT_RETIRE_AFTER: Duration;  // 34 ms: two refresh periods at 60 Hz

impl PreviewLayer {                       // not Send: the UI's main thread
    /// A Core Animation layer for the subscriber's surfaces (resize-aspect, above the
    /// toolkit's layer). Not in a window yet.
    pub fn new(subscriber: &PreviewSubscriber) -> PreviewLayer;
    pub fn surfaces(&self) -> usize;
    /// Ring the engine needs at this frame interval: shown + retiring + written + in flight.
    pub fn surfaces_needed(&self, frame_interval: Duration) -> usize;
    /// Adds it to an AppKit view's backing layer. Main thread, live layer-backed NSView.
    pub unsafe fn attach_to_ns_view(&mut self, ns_view: NonNull<c_void>) -> Result<(), GpuError>;
    /// Leaves the window; the shown frame retires by the normal rule.
    pub fn detach(&mut self, now: Timestamp);
    /// No-op when neither the rectangle nor the superlayer's height or flip changed.
    pub fn set_rect(&mut self, rect: LayerRect);
    pub fn set_hidden(&self, hidden: bool);        // while toolkit content overlaps it
    /// Filled rectangles over the picture (selection outline and handles, ADR-0048), in
    /// `set_rect`'s coordinates; replaces the last ones; empty removes them.
    pub fn set_marks(&mut self, marks: &[Mark]);
    pub fn set_retire_after(&mut self, after: Duration);
    /// Makes the frame's IOSurface the layer's contents and flushes the transaction to the
    /// window server. Never waits or allocates.
    pub fn show(&mut self, frame: PreviewFrame, now: Timestamp);
    /// Gives back replaced frames older than the retire time. Returns how many wait.
    pub fn retire(&mut self, now: Timestamp) -> usize;
    pub fn next_retire(&self) -> Option<Timestamp>;
    pub fn retiring(&self) -> usize;
    /// Diagnostics only: shown + retiring + one the engine writes fill the ring. The UI
    /// calls `retire` at `next_retire` (one-shot timer) while `retiring() > 0`.
    pub fn engine_may_starve(&self) -> bool;
}
```

- **Marks (ADR-0048).** `Mark { x, y, width, height, rgba }` (sRGB, straight alpha) in the same coordinates as `set_rect`. A container sublayer of the preview layer holds up to `MAX_MARKS` (32) solid-colour sublayers, made on first use and reused; `set_marks` changes frames, colours and `hidden` in one transaction without implicit animations, edges on the superlayer's device pixel grid (`contentsScale`); `set_rect` redraws them in the new place. Marks that are not finite or have a negative size are skipped. Marks hide, detach and drop with the layer.
- **Surfaces.** Single-plane `'RGBA'` IOSurfaces of the preview size, 4 bytes per pixel, created in `new`; the ring never grows. Each is aliased on the engine device as an `Rgba8UnormSrgb` render target owned by the publisher (no pool slots). `publish` draws one full-screen triangle that samples the linear `Rgba16Float` canvas bilinearly (any canvas size, scaled to the preview size), composites it over opaque black and writes it with sRGB encoding by the render target. One render pass, one submission, no copy, no CPU readback.
- **Transport.** A Mach service in the user's bootstrap namespace, named `org.rbs.preview.<pid>.<n>.<random>` (unique per publisher; the random part keeps a stale name from being registered by another process and mistaken for this engine), with a random 64-bit token from the system CSPRNG (`arc4random_buf`). The engine hands both to the UI through the control API (for the demo, on the command line). Surfaces cross the process boundary as Mach send rights (`IOSurfaceCreateMachPort`, `IOSurfaceLookupFromMachPort`); IOSurface global IDs (`kIOSurfaceIsGlobal`) are not used.
- **Messages.** Fixed-size, version 1, little structs of 32-bit words:
  - `SUBSCRIBE` (UI to the service port): version, token, and a send right to the UI's reply port. A wrong version or token is ignored. One UI at a time: a subscription is refused while another is alive, so surfaces a UI holds are never freed behind its back.
  - `SURFACES` (engine to UI): version, generation, width, height, count, then port descriptors: first a send right to a **session port** created for this subscription, then one copied send right per surface (the engine keeps its own; unused descriptors are null).
  - `FRAME` (engine to UI): generation, slot, sequence, timestamp in nanoseconds.
  - `RELEASE` (UI to the session port): generation, slot. Only the subscribed UI holds a send right to the session port, so no other process can release a surface. The engine accepts releases only there.
- **End of a subscription.** The engine asks the kernel for a no-senders notice on the session port. It arrives once the UI's send right is gone: the UI dropped its subscriber and every frame, or exited (also while it held every surface and no `FRAME` could be sent). A `FRAME` send that finds the UI's port dead ends it too. Either way the UI holds no surface any more, so every surface is free again. The UI learns that the engine is gone from a dead-name notice on the service port (`GpuError::Disconnected`).
- **Never block the render thread.** Every engine-side `mach_msg` call uses a zero timeout: in `tick`, at most ring size + 1 receives on the session port and 2 on the service port (so strangers flooding the service port cannot lengthen a tick), and the sends of `SURFACES` and `FRAME`. A `FRAME` that cannot be queued is dropped and its surface stays free (counted in `dropped`); since a UI holds at most one frame per surface, that needs a UI that stopped receiving. Queue limits are raised to `MACH_PORT_QLIMIT_MAX`, so releases (at most one per surface) cannot fill the session port. A send that times out has its rights returned by the kernel (pseudo-receive); they are destroyed at once, so no reference leaks.
- **Lifetime.** The engine writes a surface only when it is not pending and the UI does not hold it. A slot is held from its `FRAME` until the UI's `RELEASE`. The UI releases a frame by dropping `PreviewFrame`, which it does only after nothing in the UI reads the surface: for Slint, after a later frame's submission is done; for `PreviewLayer`, best effort, a retire time after the frame was replaced as the layer's contents and the change was flushed to the window server (Core Animation applies it at the next refresh, or the one after, and reports no completion; IOSurface use counts and `IOSurfaceIsInUse` do not track layer contents). The retire time is two refresh periods of the slowest display (default 34 ms). Too short a time can show one torn monitoring frame, never a memory hazard. At 60 fps the layer holds up to four surfaces, so a UI using it needs a ring of `surfaces_needed` (6); a smaller ring makes the engine drop frames, never deadlock. The UI process retains every IOSurface it looked up, so a crashed or slow engine never leaves the UI with a dangling surface; at worst it shows a stale image. IOSurfaces are reference counted across processes and survive until both sides drop them.
- **Colour.** The surface holds sRGB-encoded, opaque RGBA8 (`ColorSpace::SRGB`), the format the UI composites. The preview is for monitoring; the program output to encoders stays on the encoder export path.
- **Security.** Any process of the same user can look up the service; without the token it gets nothing, and without the session port it cannot release a surface. The token must reach the UI through the control API in the product; the demo passes it on the command line, where other processes of the same user can read it. The token is not a defence against a process that can already read the engine's memory or control API.
- **Namespace.** Engine and UI must share a bootstrap namespace (the same login session). An engine started over ssh or as a LaunchDaemon cannot be reached this way, and a sandboxed UI needs a mach-lookup exception (or an app-group name prefix). Handing a port over at spawn or through the control API is the fallback for those set-ups.
- **Installed but unused costs nothing (P13).** A publisher exists only while a preview output is Active. While nobody is subscribed, `publish` returns at once without GPU work and `tick` makes one non-blocking receive call. The UI side starts its receive thread only while a preview panel is visible.
- **Layer placement and overlap (ADR-0034).** The UI sets the layer's rectangle before every toolkit redraw (layout changes always redraw). The layer is above the toolkit's content: while toolkit content (menus, popups, toasts, dialogs) intersects the preview area, the UI hides the layer. Showing a frame costs one Core Animation transaction on the UI thread and no toolkit redraw.
- **Same device, other process.** IOSurfaces are not tied to a device. The UI aliases them on its own Metal device with the same `interop::metal` code as the importer. Unified memory only, as for the encoder export.

#### Windows backend (ADR-0036)

Same types and semantics as above; the differences:

```rust
// Engine side: as on macOS, without `surface_ids`.
impl PreviewSubscriber {
    pub fn adapter_luid(&self) -> u64;                   // the surfaces open only on this adapter
    pub fn surface_handle(&self, slot: usize) -> Option<u64>; // NT handle in this process, owned by the subscriber
    /// Opens every surface on the UI's D3D12 device (same LUID, else `WrongAdapter`) as
    /// `Rgba8Unorm` holding sRGB-encoded values, `TEXTURE_BINDING | RENDER_ATTACHMENT | COPY_SRC`.
    pub fn textures(&self, ctx: &GpuContext) -> Result<Vec<wgpu::Texture>, GpuError>;
}

pub struct LayerRect { pub x: f64, pub y: f64, pub width: f64, pub height: f64 } // physical pixels, top-left origin
pub const DEFAULT_RETIRE_POLL: Duration;  // 4 ms

impl PreviewLayer {                       // not Send: the window's thread
    /// D3D11 device on the engine's adapter, the shared textures as composition swapchain
    /// buffers, a DirectComposition visual showing them. `Unsupported` without a
    /// composition swapchain (WARP) or in a remote desktop session (`SM_REMOTESESSION`: the factory claims support there but nothing shows): use `textures` instead.
    pub fn new(subscriber: &PreviewSubscriber) -> Result<PreviewLayer, GpuError>;
    pub fn surfaces(&self) -> usize;
    pub fn surfaces_needed(&self, frame_interval: Duration) -> usize; // shown + retiring + written + in flight; 5 at 60 fps
    /// Topmost composition target of the window, above the toolkit's swapchain.
    pub unsafe fn attach_to_hwnd(&mut self, hwnd: NonZeroIsize) -> Result<(), GpuError>;
    pub fn detach(&mut self, now: Timestamp);
    pub fn set_rect(&mut self, rect: LayerRect);   // aspect fit; no-op when unchanged
    pub fn set_hidden(&self, hidden: bool);
    pub fn set_marks(&mut self, marks: &[Mark]);    // as on macOS, physical pixels
    pub fn set_retire_poll(&mut self, poll: Duration);
    /// `SetBuffer` + `Present` of the frame's buffer. Never waits or allocates.
    pub fn show(&mut self, frame: PreviewFrame, now: Timestamp);
    /// Gives back replaced frames whose buffer `IsAvailable`. Returns how many wait.
    pub fn retire(&mut self, now: Timestamp) -> usize;
    pub fn next_retire(&self) -> Option<Timestamp>; // last check + poll, while a frame waits
    pub fn retiring(&self) -> usize;
    pub fn lost(&self) -> bool;                     // presentation manager lost: recreate or fall back
}
```

- **Surfaces.** Committed `R8G8B8A8_UNORM` textures on the render device's D3D12 device, `D3D12_HEAP_FLAG_SHARED`, `ALLOW_RENDER_TARGET | ALLOW_SIMULTANEOUS_ACCESS`, one unnamed NT handle each. The preview pass encodes sRGB in the shader (`fs_preview_encode`), since flip-model and composition presentation take no sRGB formats; the bytes equal the macOS surfaces'. The render device must be D3D12 (`GpuContext` prefers D3D12 on Windows), else `new` returns `Unsupported`.
- **Transport.** A message-mode named pipe `\\.\pipe\org.rbs.preview.<pid>.<n>.<nonce>`: one instance, created with `FILE_FLAG_FIRST_PIPE_INSTANCE`, remote clients rejected, a protected DACL granting only the engine process's user and a medium mandatory label without read, write or execute up (`D:P(A;;GA;;;<user SID>)S:(ML;;NWNRNX;;;ME)`; the engine runs at medium integrity or above). Token from `BCryptGenRandom`. The UI opens it with identification-level impersonation and checks that the server's process id is the pid in the name.
- **Messages.** Little-endian 32-bit words, version 1: `SUBSCRIBE` (id, version, token: 4 words), `SURFACES` (id, version, generation, width, height, count, adapter LUID, then 8 handle values of 2 words: 24 words), `FRAME` (id, generation, slot, sequence, timestamp: 7 words), `RELEASE` (id, generation, slot: 3 words), and `ACCEPT` (UI to engine after `SURFACES`, id `0x52425005`, generation: 2 words). Ids as on macOS. The first message on a connection must be a valid `SUBSCRIBE`, else the engine disconnects. On `SUBSCRIBE` the engine duplicates every surface handle into the client process (`OpenProcess(PROCESS_DUP_HANDLE)` on the pid the pipe reports, `DuplicateHandle`) and sends `SURFACES`. The duplicates stay the engine's until `ACCEPT`: a UI that has not written `ACCEPT` never closes them, and the engine closes them in the UI process (`DUPLICATE_CLOSE_SOURCE`) only when the connection ends before `ACCEPT` on a decisive signal (the engine's read finds the UI's end closed, or `SURFACES` could not be sent); an engine-side end while offered (publisher dropped, unexpected message) leaves them to the client, a bounded leak until it exits. After `ACCEPT` the UI owns and closes them. Frames are sent only after `ACCEPT`. Undecodable or over-long messages (`ERROR_MORE_DATA`) are counted in `rejected` and do not end a subscription.
- **Never block the render thread.** The connect, one read and ring size + 2 writes are overlapped operations with their own boxed `OVERLAPPED`, buffer and event; completion is checked by reading `OVERLAPPED::Internal` and collected without waiting. At most ring size + 1 messages are read per tick. A `FRAME` without a free write buffer is dropped (`dropped`), its surface stays free.
- **End of a subscription.** The engine's read fails once the UI's end is closed (subscriber and every frame dropped, or the process exited, also while holding every surface), or a write finds the pipe closed: every surface is free and the next tick listens again. The UI's read fails once the engine is gone (`Disconnected`).
- **Lifetime with the layer.** A replaced frame is released when its presentation buffer reports `IsAvailable` (the desktop compositor no longer reads it): a completion signal, so no torn frame from early release. Checked on every `show` and, while a frame waits, by a one-shot timer at `next_retire`. `detach` removes the visual and waits for commit completion; the last shown frame is kept until a later `show` replaces it (it then retires by `IsAvailable`) or the layer is lost or dropped. A frame shown while not attached goes back at once, unpresented. After `attach` the visual is added only at the first `show`; since presentation and the commit are separate channels, the old, complete frame can appear for one compositor frame at worst. A lost presentation manager (lost event checked per `show`, or a failed `Present`) releases every frame at once; `lost()` tells the UI to recreate the layer or fall back. Dropping the layer waits for commit completion, then releases everything.
- **Fallback.** Without a composition swapchain the UI uses `textures` (ADR-0028 path); the toolkit must treat the `Rgba8Unorm` values as sRGB-encoded.
- **Marks (ADR-0048).** The target's root is a container visual: the picture visual, and above it an overlay visual made at the first `set_marks`. Each mark is a child visual of the overlay whose content is a shared 1 x 1 premultiplied `B8G8R8A8_UNORM` surface of its colour (filled once: `BeginDraw`, `ClearView` of the one pixel at the returned atlas offset, `EndDraw`; at most 8 colours kept), scaled to the mark's rectangle (edges on whole pixels) with nearest-neighbour sampling. Unused mark visuals are scaled to zero. At most `MAX_MARKS` (32) visuals; one `Commit` per call. A failure to make a visual or surface leaves marks out, never the picture.
- **Placement and overlap.** As on macOS, but in physical pixels (the toolkit's logical rectangle times its scale factor); the visual is in a topmost target, so the UI hides it while toolkit content overlaps the preview.

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
| **`IoSurface`** | **Metal** | **`Ready`** | `newTextureWithDescriptor:iosurface:plane:`; IOSurfaces are not tied to a device, so there is no identity check. Single-plane `'BGRA'`, `'RGBA'` and `'RGhA'` (RGBA half float) surfaces; bi-planar 4:2:0 `'420v'`, `'420f'` (8-bit) and `'x420'`, `'xf20'` (10-bit, needs `gpu.p010`), decoded per rule 6 |
| `MetalTexture` | Metal | `Ready` | `device_registry_id` must match |
| **`DmaBuf`** | **Vulkan** | **`Ready`, `SyncFile` (with `gpu.import.sync_file`)** | wgpu-hal `texture_from_dmabuf_fd` (`VK_EXT_external_memory_dma_buf` with `VK_EXT_image_drm_format_modifier`), capability `gpu.import.dmabuf`. Single plane, `AR24`/`XR24` (ARGB8888/XRGB8888) and `AB24`/`XB24` (ABGR8888/XBGR8888). For the X formats the source sets `Layer::opaque`, because their fourth byte is undefined. The importer duplicates the producer's fd; the cache key is the buffer's inode, not the fd number. The size comes from the frame's `VideoFormat`. Device identity is not checked: an incompatible modifier fails the import |

`GpuContext` reports what the importer accepts, for capture requests (`GpuImportTarget { device, sync }` of the capture port spec): the render adapter's identity (D3D LUID as `u64` = `HighPart << 32 | LowPart`, from `GpuContext::adapter_luid`; Metal registry id for `MetalTexture`) and the sync kinds in the table below. Producers create surfaces on that adapter.

Rules:

1. **Alias, do not copy.** A supported surface is wrapped zero-copy as a pool texture (`create_texture_from_hal`). A surface that cannot be aliased (unsupported format or backend) is an error (`GpuError::Unsupported`), not a silent CPU path; the source falls back to its `System` payload if it offers one.
2. **Open once.** Producers reuse a small ring of surfaces. Shared fences come from a process-wide producer pool and are never closed, so the fence cache needs no eviction. The importer caches opened surfaces and fences by handle value or object address (at most 64 entries per importer, enough for about ten capture sessions with rings of 5 plus resize spares) and reuses the pool slot of a cached surface, so steady-state import does not open handles or allocate. First sight of a surface opens it (allocates, counted in `GpuMetrics::surfaces_opened`). Entries unseen for 8 s of engine time (`SURFACE_IDLE`, whatever the tick rate) are closed by `retire`; when every entry is in use and a new surface arrives, the importer returns `GpuError::Unsupported` rather than stall. Following [capture-port.md](capture-port.md), "Native surface rules": a producer keeps a handed-out surface's handle open for at least 10 s after its last reference, also after the session stops, and never closes shared fences. The 8 s idle limit therefore forgets a handle value before the OS can reuse it. The owner may also call `close_unused` when a source stops, to free the surfaces early. As a cheap extra guard, a cached entry that arrives with a frame size other than the one it was first seen with is treated as a different object and reopened.
3. **Keyed mutexes are not supported on D3D12** (D3D12 cannot acquire them). D3D producers use a shared timeline fence. The importer makes its queue wait for the fence value before the first submission that reads the surface. The sync kind is checked before a surface is opened.
4. **Sync files are waited on by the GPU.** For `SurfaceSync::SyncFile` the importer first checks the file with a zero-timeout `poll` (a sync file is readable once signalled); a signalled file needs no GPU work. Otherwise it duplicates the fd, imports the duplicate into a pooled binary semaphore (`vkImportSemaphoreFdKHR`, `SYNC_FD`, temporary import, so Vulkan owns and closes the duplicate) and stages a wait for all commands with wgpu-hal's `Queue::add_wait_semaphore`. The queue's next submission consumes the wait; on the render thread that is the render that reads the frame, so the reading batch itself waits on the fence. A later reader in another batch is ordered after it only through wgpu-hal's submission chaining, which relies on drivers ordering whole submissions; this is to be verified on hardware. While the fence is pending, the whole render queue waits (other canvases and encoder conversions too), the same trade-off as the D3D12 fence wait. `retire` moves consumed waits to a busy list and submits one empty batch to get an index to poll (completion tracking only); a wait that no submission consumed yet is staged again. A semaphore is reused once that index has completed; only the oldest busy one is polled. At most 16 waits (`MAX_SYNC_WAITS`) are in flight, after which the frame is rejected with `GpuError::Unsupported` rather than stall. The semaphores are created on the first pending sync file (P13) and destroyed when the importer is dropped, after a bounded wait; any the GPU may still use, or whose wait was never submitted, are leaked instead. The producer's own fd is never closed or kept. The CPU never waits on a sync file (P6). A sync file whose fence signalled with an error is treated as signalled. A negative fd is `GpuError::Import`: producers deliver `Ready` instead of Vulkan's "-1 means signalled" convention.
5. **Lease hand-off.** The native lease moves into the importer's record for the pool slot. `import` returns a `Texture` frame whose own pool lease keeps the slot alive for downstream consumers. The native lease is dropped by `retire` once the last queue submission that read the slot has completed (checked with the submission index, non-blocking) and no pool lease on the slot remains, so the producer never reuses a surface the GPU may still read. On D3D12 that hand-back submission also returns the texture to the `COMMON` state (wgpu `TextureUses::PRESENT`), which other APIs expect for shared resources.
6. **Formats.** BGRA8/RGBA8 (UNORM or sRGB) surfaces are aliased as the matching wgpu format; the colour space of the native frame's `VideoFormat` decides `Transfer` for the compositor. 4:2:0 surfaces (so far bi-planar IOSurfaces) are aliased plane by plane (Y' as `R8Unorm` / `R16Unorm`, CbCr as `Rg8Unorm` / `Rg16Unorm`, two pool slots per surface, still opened once and cached). Each import then decodes them with the importer's own [`YuvDecoder`](#yuv-source-decode), created on the first such surface (P13), using the frame's `ColorSpace` (its range wins over the fourcc's). The decode writes into a pool slot acquired per frame. `import` returns a `Rgba16Float` frame of that slot with `decoded_color(...)`, and only that frame holds the slot. No frame references the plane slots, so `retire` hands the producer's lease back once the decode's submission (and the hand-back) has completed, even while the decoded frame lives on. This costs one copy at canvas precision per frame, which the compositor would otherwise do implicitly while sampling. A frame whose `ColorSpace` cannot be decoded (`check_color`) is rejected before anything is opened. A surface whose bit depth does not match the frame's `pixel_format` (`Nv12` for 8-bit, `P010` for 10-bit) is `GpuError::Unsupported`. The surface's range fourcc is not checked against the frame's `ColorSpace`, which wins. A decode error keeps the lease until the next `retire`; the hand-back transition covers both plane slots. D3D12 NV12 textures and multi-plane DMA-BUFs (`NV12`, `P010` fourccs) are follow-ups; wgpu-hal's DMA-BUF import takes one plane.
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
    pub style: LayerStyle,    // shape and border (ADR-0053), converted from the scene's ItemStyle by rbs-sources
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

- `Compositor::render`, `YuvConverter::convert`, `YuvConverter::convert_to_surface`, `SurfaceExporter::export` and `take_ready`, `PreviewPublisher::publish` and `tick`, and the pool's `acquire` (reuse), `release` and `get` are hot paths ([hot-paths.md](../architecture/hot-paths.md)). RBS code inside them does not allocate. wgpu allocates internally while recording and submitting command buffers (about 130 allocations per frame for render + NV12 in the first measurement); the `alloc_per_frame` test asserts this does not grow over time and adds at most a small constant per layer. Reducing it (for example re-recorded render bundles) is follow-up work with its own measurements.
- No CPU readback on the default path (P4).
- Installed but unused: a `GpuContext` is created only when the first canvas becomes Active. A compositor or converter that is not Active is dropped, releasing its GPU objects (P13).
- Benchmarks: `benches/preview.rs` (macOS) measures the CPU time of `PreviewPublisher::publish` and `tick` and the GPU frame time of the preview pass (1080p canvas to 720p). `benches/compositor.rs` measures 1080p frame time (render, render + NV12, waiting for the GPU) with 1, 8 and 32 layers, plain and with every tile styled (`composite_styled`), and the CPU cost of recording and submitting 32 layers, plain and half styled.

## Conformance tests

Run on any adapter, including software adapters in CI (lavapipe on Linux, WARP on Windows). Tests skip with a clear message when `GpuError::NoAdapter` is returned.

- Pool: reuse, generation and stale-id handling, trim, capacity, leases (a leased slot survives the owner's release, is reused after the last lease drops on another thread, frames share the lease).
- Golden images (sampled pixels with tolerance): single opaque layer placement, crop, opacity blend over a background layer, sRGB source decode, layer order, stale layer skipped, an `opaque` layer drawn opaque over a source whose alpha is 0.
- Styled layers (`tests/style.rs`, ADR-0053): rounded corners cut only the corners; a circle centred in a wide box; an ellipse filling the box; a half-transparent border over the picture; opacity multiplying shape and border; a rotated circle staying round; a flipped layer keeping its corners; a circle under a 4:1 squash following the picture with a one-pixel edge (0.75 coverage a quarter pixel inside the edge, where a mean pixel width would give 1.0); a cropped styled layer sampling exactly what a plain one does; plain layers around styled ones unchanged; boxes without area.
- NV12: known colours (black, white, mid grey, pure red) produce BT.709 limited-range values within ±1.
- P010 (where `gpu.p010`; required on Linux CI): the same colours produce 10-bit values within ±2, and the low six bits of every word are zero.
- NV12 and P010 surfaces (where `gpu.nv12_surface` / `gpu.p010_surface`; required on Linux CI): both planes of `convert_to_surface` equal the two-plane output byte for byte on a two-colour canvas; a texture of another format is rejected.
- YUV source decode (any adapter): NV12 and P010 (where `gpu.p010`) planes from the encoder conversion of black, white, 18 % grey, red, green and a blue mix decode back to the linear colour within 0.02 (NV12) or 0.01 (P010); BT.601 full-range codes decode to red; a multi-planar NV12 source (where `gpu.nv12_surface`) decodes like planes; a wrong target size, swapped planes and a PQ colour space are rejected; one source caches one bind group.
- 4:2:0 import (macOS CI): `'420v'` and `'x420'` IOSurfaces from `SurfaceExporter` import as `Rgba16Float` frames that match the original canvas; the producer's surface is handed back after the decode, also while the decoded frame is still held; a ring of two surfaces is opened twice, then reused.
- Allocations per frame do not grow over time and grow by at most a small constant per layer.
- Readback increments `frames_cpu_copied`.
- Export (macOS CI, the runner's Metal device): NV12 and P010 (where `gpu.p010`) surface bytes, read through the IOSurface API with its plane strides, equal the two-plane output byte for byte on a two-colour canvas; the frame is an `IoSurface` frame with `Ready` sync, the right pixel format, time and sequence; the surface wraps as a `CVPixelBuffer` of the same format, size and plane count; held surfaces are not reused and a full ring returns `ExportBusy`; frames come out in order, and frames dropped on another thread free their surfaces; a frame stays valid and unchanged after both the exporter and the pool are dropped; odd sizes and ring sizes outside 2..=32 are rejected; a steady export loop allocates at most 4 more per frame than the same conversion into pool planes and does not grow.

- Preview export (macOS CI): a child process subscribes, aliases the surfaces on its own device and reads back each frame; colours match the canvas of the frame's sequence (sRGB encoded, within ±1), the IOSurface IDs it sees equal the engine's (same memory, no copy), frames arrive in order, and a held surface is not reused. A wrong token gets no surfaces; nothing is converted while nobody is subscribed; when the child exits the engine ends the subscription and frees every surface; the steady publish loop allocates at most a small constant per frame more than the conversion alone and does not grow.
- Preview export (Windows CI on WARP, ADR-0036, `tests/share_cross_process.rs`, `tests/share_alloc_metal.rs`): the same child-process checks without IOSurface IDs (the colours reach the child only through the shared textures); a second UI is refused while one is alive; after a UI exits holding every surface, a new UI subscribes and gets correct frames; the steady loop's allocations as above.
- Layer arithmetic (every platform, `share::geometry`): aspect fit (letterbox, pillarbox, exact, degenerate sizes) and ring size (6 at 60 fps with the macOS 34 ms rule, 5 with the Windows one-refresh-plus-poll rule).
- Preview layer on Windows: no CI coverage (WARP has no composition swapchain, so `PreviewLayer::new` returns `Unsupported` there). Manual run of `remote_preview` on the owner's Windows machine: frames on screen at 60 fps, placement and DPI, `IsAvailable` releases (engine `busy` stays near 0 with a ring of 6), the Slint fallback's colours, CPU. Passed 2026-11-03 on the physical console (Desktop Duplication capture: layer pixels equal the Slint path's, same placement after move and resize; `busy` 0); over remote desktop the layer is refused and Slint draws.
- Preview layer (macOS CI, ADR-0034, `tests/share_layer_metal.rs`): the layer's contents are the engine's surface of each shown frame (same IOSurface ID); with fake time at an ideal 60 fps and a ring of `surfaces_needed`, the engine never finds every surface busy and the UI holds the shown frame plus at most three; with jittered frame times and retirement at `next_retire` the UI still holds at most `surfaces_needed - 2`; a replaced frame is released at its retire time, not before; `detach` retires the shown frame by the same rule; a ring of 3 reports `engine_may_starve` and recovers after the retire time; placement converts the top-left rectangle for flipped and unflipped superlayers, follows a height change and is a no-op for an unchanged rectangle. The on-screen check (placement, colours, no tearing, CPU) is a manual run of `remote_preview` on a Mac.
## Open questions

- Whether the canvas working format should be configurable (`Rgba16Float` vs `Rgba8UnormSrgb` for low-end GPUs).
- DMA-BUF device identity on multi-GPU Linux (DRM render node or device UUID in `DmaBufSurface`).
- Waiting on an imported fence on Metal (`MTLSharedEvent`) through wgpu's HAL (D3D12 uses `ID3D12CommandQueue::Wait`, Vulkan imports sync files). Metal producers deliver `Ready` surfaces until then.
- DRM syncobj timeline points (PipeWire explicit sync, `linux-drm-syncobj-v1`): a producer can export a point as a sync file today (`DRM_IOCTL_SYNCOBJ_EXPORT_SYNC_FILE` after the point materialized); a timeline-semaphore import of the syncobj would avoid that step and needs a new `SurfaceSync` kind.
- The Vulkan sync-file wait has not run on hardware yet (no CI machine has a hardware Vulkan driver); it needs a run on a Linux machine with `RBS_REQUIRE_DMABUF=1`.
- D3D cache key: an NT handle value can be reused by the OS after the producer closes the handle, so a cached entry could alias a different texture. Producers keep ring handles open for the session today; validating entries (for example `CompareObjectHandles` on a duplicated handle) is a follow-up. IOSurface addresses cannot be reused while cached, because the Metal texture retains the surface.
- Encoder frames in flight: a capability (for example `codec.video.frames_in_flight`) so the export ring can be sized from the encoder instead of the adapter's documentation.
- IOSurface export on Macs with a discrete GPU (storage mode and synchronization), and a sync-object hand-off (`MTLSharedEvent`) instead of `Ready` to save the poll latency.
- Preview over the control API: how the UI gets the endpoint (a `preview.subscribe` query returning service and token), and multiple UI subscribers per publisher.
- Rotation and non-affine transforms (perspective, corner pin) as filters vs layer properties.
