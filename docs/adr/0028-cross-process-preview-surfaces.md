# ADR-0028: Share preview surfaces with the UI process as native handles

- Status: Proposed
- Date: 2026-11-03
- Deciders: Salih (owner)
- Builds on: ADR-0008 (UI as a control API client), ADR-0022 (Slint, decision 2 and the acceptance gate), ADR-0025 (encoder surface export), ADR-0013 (zero cost when unused)

## Context

ADR-0008 runs the desktop UI in its own process and says the preview is "a shared GPU surface, not pixels over IPC". ADR-0022 makes two processes the default topology, requires the export side of that contract in the GPU compositor spec before the UI shows engine output, and gates its acceptance on a zero-copy display of an engine surface imported from another process on at least one platform.

Three things are open:

- **Which surface.** The encoder export (ADR-0025) produces bi-planar NV12/P010 surfaces sized for encoders. The UI composites RGBA (Slint's `Image::try_from(wgpu::Texture)` takes only `Rgba8Unorm` or `Rgba8UnormSrgb` textures with `TEXTURE_BINDING` and `RENDER_ATTACHMENT` usage), usually at a smaller size than the canvas.
- **How a handle crosses the process boundary.** On macOS an IOSurface crosses as a Mach send right (`IOSurfaceCreateMachPort` / `IOSurfaceLookupFromMachPort`). Global IOSurface IDs (`kIOSurfaceIsGlobal`) are deprecated and let any process read the surface. On Linux a DMA-BUF crosses as a file descriptor over a Unix socket (`SCM_RIGHTS`); on Windows an NT handle is duplicated into the UI process.
- **Who may reuse a surface when.** The engine must not write a surface that the UI's GPU may still read, and a slow or crashed UI must not stall the render thread (P6, P7).

Only macOS has both a GPU export path and a GPU-backed CI runner today. DMA-BUF import is not runtime-verified (lavapipe has no DMA-BUF import), and Windows has no exporter yet.

## Decision

1. **A separate preview export.** `rbs_gpu::share::PreviewPublisher` owns a fixed ring (2 to 8) of single-plane `'RGBA'` IOSurfaces of the preview size, aliased as `Rgba8UnormSrgb` render targets on the engine device. One render pass scales the linear canvas into a surface, composited over black and sRGB encoded by the target. It does not reuse the encoder ring: format, size and consumer differ. No pool slots, no copy, no CPU readback.
2. **Mach service, token, and send rights.** The publisher checks in a Mach service `org.rbs.preview.<pid>.<n>` in the user's bootstrap namespace and draws a random 64-bit token. The control API hands the service name and token to the UI (for now the demo passes them on the command line). The UI subscribes with the token and a reply port; the engine answers with one copied send right per surface. IOSurface global IDs are never used.
3. **Four fixed-size messages.** `SUBSCRIBE`, `SURFACES`, `FRAME` (slot, sequence, timestamp) and `RELEASE` (slot), with a protocol version and a generation number that every new subscription increments. The message layout is part of the spec, so a UI built from another version fails cleanly.
4. **The engine never waits.** Every engine-side `mach_msg` uses a zero timeout and runs on the render thread from `publish` and `tick`: one bounded, non-blocking kernel call per delivered frame plus at most two receive calls per surface per tick. This is an accepted exception to "no I/O on hot paths" (P6), for the same reason as the GPU queue submit next to it: the call never waits, and a sender thread would need a wake-up system call per frame anyway. A frame that cannot be queued is dropped, counted. A dead UI port ends the subscription and frees every surface.
5. **The UI holds a surface until its GPU work is done.** A slot is held from `FRAME` until `RELEASE`. The UI drops a frame only after a later submission of its own that follows the read has completed. The engine writes only free, non-pending slots; when none is free, `publish` returns `ExportBusy` and drops the frame.
6. **The UI imports with `rbs_gpu` code.** The UI process creates its own `GpuContext`, hands its instance, adapter, device and queue to Slint (`WGPUConfiguration::Manual`), and aliases the looked-up surfaces once with `interop::metal::alias_plane`. Steady state opens nothing and copies nothing.
7. **Zero cost when unused (P13).** A publisher exists only while a preview output is Active. While no UI is subscribed, `publish` does no GPU work. The UI starts its receive thread only while a preview is shown.
8. **Other platforms follow with the same messages.** Linux: the same four messages over a Unix socket, surfaces as DMA-BUF fds with `SCM_RIGHTS`. Windows: shared NT handles duplicated into the UI process. Both need their own exporter and conformance run first.

## Consequences

- ADR-0022's gate item "zero-copy display of an engine surface imported from another process" can be met on macOS. The CI runner proves the import and the identity of the surfaces (same IOSurface IDs in both processes, pixels match); the Slint window is shown on a Mac by hand.
- `rbs-gpu` gains a macOS dependency on `mach2` (BSD-2-Clause OR MIT OR Apache-2.0) for Mach messages and bootstrap calls. `GpuContext` keeps its `wgpu::Instance` and `wgpu::Adapter` so a UI can hand them to Slint.
- `GpuError` gains `Disconnected` for a subscriber whose engine is gone.
- The preview costs one extra render pass per frame at preview size while a UI is subscribed. At 1080p preview size that is one 8 MiB write per frame on the GPU.
- The hot-path list gains the preview publish path, with a non-blocking IPC call per frame recorded as an exception in this ADR.
- Same-user processes can find the service name; the token keeps them from subscribing. A process that can read the engine's memory or control API is already trusted.
- One subscriber per publisher. Several UI windows in one process share it. A second UI process needs a second publisher or a later fan-out.
- Residual risk: Mach message layout and bootstrap behaviour are only tested on the macOS CI runner and the owner's Mac.

## Alternatives considered

- **Global IOSurface IDs (`IOSurfaceLookup`).** Simplest, but deprecated since macOS 10.11 and readable by any process.
- **XPC.** Carries IOSurfaces natively (`IOSurfaceCreateXPCObject`), but an anonymous XPC connection still needs a rendezvous, and XPC calls allocate per message. Raw Mach messages keep the per-frame path allocation free.
- **Reuse the encoder export ring (NV12).** The UI would need a YUV-to-RGB pass and Slint cannot display NV12; sizes also differ.
- **Send each frame's surface as a new port right.** Costs a kernel port object per frame and a lookup on the UI side. A fixed ring sent once is cheaper and opens nothing in steady state.
- **One process with a shared device.** Simpler and stays an optional mode (ADR-0022), but ADR-0008 makes two processes the default.
- **CPU copies through shared memory.** Violates P4.

## Review

Independent review per ADR-0014: pending.
