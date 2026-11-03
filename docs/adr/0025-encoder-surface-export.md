# ADR-0025: Hand encoders exported GPU surfaces as native frames

- Status: Proposed
- Date: 2026-11-03
- Deciders: Salih (owner)

## Context

Hardware encoders read a 4:2:0 frame from a platform surface: VideoToolbox takes a `CVPixelBuffer` backed by an IOSurface, NVENC/AMF/QSV take D3D11/D3D12 textures, VA-API and Vulkan Video take DMA-BUFs or Vulkan images. `rbs-gpu` converts the canvas to NV12/P010 in pool textures (`YuvConverter`, gpu-compositor spec), and `rbs-codec` already lets an encoder declare `input.memory = metal | d3d11 | d3d12 | vulkan | dmabuf`.

Two things were left open:

- How an encoder adapter gets the native handle. The output-ports spec asked whether encoder adapters may depend on `rbs-gpu` or receive exported handles through a port-level type. The media-types spec said encoders "only ever see `Texture` or `System` payloads" and get native textures "from `rbs-gpu` export", without saying what that export hands over.
- On macOS, Metal has no multi-planar textures, so `convert_to_surface` is not available and an NV12 pool texture cannot be shared with VideoToolbox at all.

Forces: zero copy on the default path (P4), no allocation per frame (P6), adapters depend on ports and `rbs-core` only (P1), encoders run on their own threads and never touch the render thread's pool, and the lease rules of ADR-0018.

## Decision

1. **Export produces a native frame.** `rbs-gpu` owns export. An exporter converts a canvas into a surface it owns and, once the GPU work has completed, hands out `VideoFrame::native(format, t, surface, SurfaceSync::Ready, lease)`: the platform surface as an existing `NativeSurface` variant (`IoSurface` on macOS), a `Ready` sync because the exporter delivers only completed frames, and a lease on the exporter's pool slot. No new `rbs-core` type is added.
2. **The encoder is the frame's only consumer.** A native frame still has exactly one consumer: the `rbs-gpu` importer for capture frames, the encoder adapter for exported frames. The encoder node's wrapper on the render executor calls the exporter, and the exported frame travels through the encoder worker's bounded input queue (media-graph spec, "Encoders and outputs run on their own workers"), never on a graph link: video links keep carrying only pool-texture and system frames. They cannot be shared (`share()` stays `None`).
3. **Encoder adapters do not depend on `rbs-gpu`.** They read the handle from the frame's payload, wrap it in their API's object (for VideoToolbox, `CVPixelBufferCreateWithIOSurface`, cached per surface), and drop the frame once the encoder API has finished reading it (for VideoToolbox, after the frame's output callback). Until then the exporter never writes that surface again.
4. **Fixed ring, refuse when full.** An exporter creates all its surfaces when it is created (non-real-time) and never grows. When every surface is pending or held, `export` returns `GpuError::ExportBusy` and the caller drops the frame, counted. A slow encoder therefore drops frames instead of stalling the render thread (P6, P7).
5. **macOS layout.** Surfaces are bi-planar IOSurfaces, `'420v'` (NV12) and `'x420'` (P010), both limited range. Each plane is aliased as a Metal texture (`R8Unorm`/`RG8Unorm`, `R16Unorm`/`RG16Unorm`) and inserted into the pool, so the existing two-plane conversion renders straight into the surface.
6. **Other platforms follow the same contract.** D3D12 (shared NT handle with `TimelineFence` or `Ready`) and Vulkan (exportable memory, DMA-BUF) exporters are follow-ups and use the same frame shape; their sync kinds may be fences instead of `Ready`.

## Consequences

- The VideoToolbox adapter can be written against `rbs-codec` and `rbs-core` only, with `input.memory = metal`. The codec conformance suite needs a frame source for Metal memory; `rbs-gpu`'s exporter can provide one in the adapter's tests.
- `YuvConverter::convert` and `convert_to_surface` return the `wgpu::SubmissionIndex` of their work, so an exporter can tell when a frame is complete without waiting.
- Waiting for completion adds latency of one GPU poll (the frame is delivered on the next `take_ready` after the GPU finishes, normally the same or the next render tick). A sync-object variant (`MTLSharedEvent`) can remove it later without changing encoders that accept `Ready`.
- The media-types and output-ports specs change: native frames have two possible single consumers, and the open question about encoder handles is closed.
- Leak risk: an encoder that never drops frames starves the ring; this shows as `ExportBusy` and dropped frames, not as unbounded memory.

## Alternatives considered

- **Encoder adapters depend on `rbs-gpu` and resolve `Texture` ids.** Violates P1 (adapters would pull in wgpu) and needs the render thread's pool on the encoder thread.
- **A new `FramePayload::Exported` variant in `rbs-core`.** Same data as `Native` with a different name; it would duplicate the handle types and the safety contract for no gain.
- **Copy the planes into a `CVPixelBuffer` from a VideoToolbox pool.** One extra GPU blit per frame and a dependency on CoreVideo in `rbs-gpu`; aliasing our own IOSurfaces costs nothing per frame. Taking the encoder's pixel buffer pool may still be useful for adapters that need special buffer attributes and can be added as an alternative constructor.
- **Block until the GPU finishes inside `export`.** Simple, but a blocking call on the render thread (P6).

## Review

Pending: independent AI session (ADR-0014).
