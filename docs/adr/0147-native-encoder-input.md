# ADR-0147: Native encoder input chosen by capability (exit the ADR-0038 readback per encoder)

- Status: Proposed
- Date: 2026-11-18
- Deciders: Salih (owner)

## Context

Every encoder chain reads each canvas frame back to the CPU ([ADR-0038](0038-io-graph-nodes.md) decision 3): GPU conversion to NV12/P010, a copy into a mappable staging buffer, a CPU copy into recycled `Bytes` (`rbs_gpu::download`), the partition queue, a second CPU copy into the encoder's input buffer (`rbs-codec-hw` `copy_420`, the VideoToolbox pixel buffer pool), and an upload back to the encoder. The November 2026 audit ([docs/perf/audit-2026-11.md](../perf/audit-2026-11.md), theme 1, items E-03, E-05, E-06, R-10) estimates about 400 MB/s of CPU copies and 58 MB of RAM per chain at 1080p60, about 1.5 GB/s and 224 MB at 4K60, and on a discrete GPU about 200 MB/s each way over PCIe. Main stream, recording and the vertical output each pay it. ADR-0038 calls this a temporary exception to P4 with three exit criteria per encoder: (a) native encoder input, (b) a native frame can cross the partition queue to its single consumer, (c) the export node chooses native export or readback by capability. Issue [#83](https://github.com/salihcantekin/rbs/issues/83) tracks them.

What exists:

- [ADR-0025](0025-encoder-surface-export.md): export produces a native frame with one consumer, from a fixed ring that refuses when full. The macOS `SurfaceExporter` (`rbs-gpu/src/export.rs`, IOSurface ring with Metal-aliased planes, unified memory only) is built but no node uses it. ADR-0025 decision 6 leaves D3D12 and Vulkan exporters as follow-ups.
- VideoToolbox accepts `NativeSurface::IoSurface` frames (`input.memory` contains `metal`). NVENC (CUDA context), AMF (D3D11 context), QSV and VA-API take only system memory.
- `VideoFrame::share` returns `None` for native payloads, so the video queue writer counts such a frame `unshareable` and drops it (`rbs-graph/src/runtime/queue.rs`). The media graph spec says video links carry only pool-texture or system frames.
- `Lease::clone` already counts holders through `LeaseOwner::retain`; a lease dropped by the queue writer is released on the writer's thread without locks (media types spec).
- Render on change (R-03, merged) repeats an unchanged canvas as a repeat entry with a new sequence. ADR-0146 (picture identity on frames, Proposed, written in parallel) gives frames a picture identity so encoders can reuse their input on a repeat.
- `VideoEncoderConfig::memory` already tells an adapter where its input lives. The capture port names a render device with `GpuDeviceId` (LUID or UUID, `rbs-capture`).

Forces: P4 zero copy, P3 capabilities not vendors, P6 nothing blocks the render tick, P13 zero cost when unused, P1 encoder adapters never depend on `rbs-gpu` (ADR-0025 decision 3), ADR-0143 shared work and right-sized buffers, and SC-PC's mixed setup (render on an RTX 4090, AMF on an AMD iGPU).

## Decision

1. **Selection by capability.** When a chain is built, the composition root must give the export node and the encoder the same input memory kind: the render device's native kind (`metal`, `d3d12`, later `vulkan`) when the encoder's `input.memory` contains it *and* the encoder runs on the render device (same `GpuDeviceId`), otherwise `system`. Engine code must not branch on vendor, platform or encoder name. `GpuDeviceId` moves to `rbs-core::media` so the encoder port can use it, and `VideoEncoderConfig` gains `device: Option<GpuDeviceId>` (required for native memory kinds; adapters that cannot open that device refuse with `Unsupported`).
2. **Export node, native mode.** The export node gets a native mode that builds an exporter instead of a `YuvDownload`. Every rule of the io-nodes spec stays: one frame per tick, never wait, repeats, pause, hold, ending. Specifically:
   - a **repeat** (unchanged canvas) re-hands the last exported surface with the tick's time and a new sequence, keeping its ADR-0146 picture identity; no conversion runs;
   - a **frozen hold** pins the last exported surface (a lease clone); the **black hold picture** is one exporter surface cleared to black at `prepare` and never written again;
   - `ExportBusy` drops the canvas and counts it in `busy`, as a full staging ring does today.
3. **Exporters.** `rbs-gpu` must provide an exporter per graphics API with the ADR-0025 frame shape and fixed ring. The ring size must cover: one surface being written, one on the export output, one in the mailbox, the frames the encoder session declares it holds, the frozen pin and the black surface. It is computed at `prepare`, never a constant.
   - Metal: the existing `SurfaceExporter`.
   - D3D12: exportable device-local memory with a shared NT handle and a shared timeline fence (`SurfaceSync::TimelineFence`). For CUDA-based encoders the surface is a linear buffer laid out like today's readback (plane offsets and 256-byte-aligned strides); `NativeSurface` gains a `D3dSharedBuffer` variant carrying the NT handle, size, adapter LUID and plane layouts. A D3D12 shared texture (`D3dSharedHandle`) is offered for encoders that take textures (AMF, QSV).
   - Vulkan (opaque FD or DMA-BUF) follows the same rules later.
   All wgpu-hal and FFI code stays in one marked low-level `rbs-gpu` interop module with `SAFETY` comments.
4. **Native frames cross only to an encoder.** `VideoFrame::share` must return a second reference for a native payload whose lease owner is an exporter (it clones the lease, so the ring sees every holder). A video link may carry a native frame only from an export node in native mode to a queue writer whose reader feeds one encoder; graph lowering must reject any other link from such an output (`LowerProblem`), and the export node must refuse native mode when its output has another consumer. Capture surfaces keep their rule: imported by `rbs-gpu`, never on a link. The queue writer moves the frame as today; a replaced frame drops its lease on the writer's thread.
5. **Encoder adapters.** An adapter that accepts a native kind must: wrap or import each surface once and cache it by handle (as VideoToolbox does by IOSurface), wait on the frame's `SurfaceSync` on the GPU (CUDA external semaphore, D3D fence) rather than on the CPU where the API allows, never write the surface, and drop the frame only after the encoder finished reading it. Adapters still never depend on `rbs-gpu`. NVENC uses its existing CUDA context: `cuImportExternalMemory` on the buffer, `nvEncRegisterResource` with a CUDA device pointer per ring surface, `cuImportExternalSemaphore` on the fence.
6. **Sharing between outputs.** R-01's shared readback stays for system memory. In native mode each encoder chain has its own exporter (the conversion runs once per chain on the GPU; there is no CPU copy to share). Sharing one exported surface between two encoders is a later decision.
7. **Failure and fallback.** A native chain that fails (device lost, an import refused, the encoder refusing native input) fails with a typed error; the next `prepare` of that chain uses system memory and the chain's stats say so. Native and readback modes must not switch per tick.
8. **Zero cost when unused.** Exporters, interop objects and imported fences exist only while the chain is Active (P13).
9. **Interim step for system memory (E-06).** Until a chain is native, `YuvDownload` should hand the encoder `Bytes` that point into the mapped staging buffer (`Bytes::from_owner`; the owner's drop marks the slot for unmapping on the render thread) instead of copying into frame buffers. This needs no contract change, only the `unsafe` and hot-path reviews. It stays for cross-device encoders (AMF on SC-PC's iGPU while rendering on the 4090).

Order of work, each its own change with before/after numbers and the P15 sign-off: E-06; VideoToolbox native (Metal); D3D12 exporter and NVENC; AMF on the same device; QSV and Linux (Vulkan, VA-API, NVENC) when hardware is available. The ADR-0038 exception ends for each encoder as its native path lands.

## Consequences

- Native chains remove both CPU copies and the encoder's upload: per 1080p60 chain about 400 MB/s of copies and about 58 MB of RAM, plus the PCIe round trip on a discrete GPU. They add a ring of GPU surfaces (about 3 MB each at 1080p NV12, 6 to 8 per chain). `GpuMetrics::frames_cpu_copied` must read 0 for a native chain, and the budget tests can then cover recording.
- Contract changes: `VideoFrame::share` for exporter frames, a `NativeSurface::D3dSharedBuffer` variant, `GpuDeviceId` in `rbs-core`, `VideoEncoderConfig::device`, a lowering rule for native links. Each needs the specs updated with its first implementation: [media types](../specs/media-types.md), [media graph](../specs/media-graph.md) ("Video links carry only..."), [io-nodes](../specs/io-nodes.md) (video export node, native mode), [output ports](../specs/output-ports.md) (input memory, device), [GPU compositor](../specs/gpu-compositor.md) (exporters).
- More `unsafe` and FFI: wgpu-hal resource export, CUDA external memory and semaphores. Each step runs `risky-review` and the hot-path checklist.
- Testing: the cloud has no hardware encoder; lavapipe cannot export D3D12 or Metal memory. Conformance runs over fake engines as today; real runs are short device runs on the Mac (VideoToolbox) and SC-PC (NVENC, AMF with render on the iGPU), with a picture check (PSNR against the readback path) and A/V sync. QSV and Linux paths wait for hardware.
- AMF on a different adapter than the render device keeps the readback by design; E-06 halves its copies.
- ADR-0025 decision 2 ("never travels on a graph link") is narrowed by decision 4 for exporter frames; ADR-0025 is not edited, this ADR supersedes that sentence.

## Alternatives considered

- **Export inside the encoder node on the render executor** (ADR-0025's original wording, no link). Puts encoder-specific code on the render thread or needs a new runtime hook to pass a frame to another partition without a link; decision 4 reuses the existing writer, mailbox and lowering instead.
- **A move-only link** (the queue writer takes the frame out of the producer's output slot). Needs a new `Inputs` take API in the scheduler's hot path; counting holders through the lease gives the same single-reader guarantee with no runtime change.
- **NVENC D3D12 session** (`NV_ENC_DEVICE_TYPE_DIRECTX` with D3D12 input and fences, SDK 11.1+). A second device type in the adapter and a Windows-only path; the CUDA route keeps one session type for Windows and Linux (Vulkan opaque FD later).
- **D3D11 shared textures for NVENC.** Needs a D3D11 device next to the D3D12 render device and keyed mutexes; no gain over CUDA import.
- **Keep the readback and only do E-06.** Halves the CPU copies but keeps the PCIe round trip and the frame buffers' RAM; it is the fallback, not the goal (P4).
- **Choose by platform** (macOS native, Windows readback). Violates P3 and gets SC-PC's cross-adapter case wrong.

## Review

Independent review per ADR-0014: pending.
