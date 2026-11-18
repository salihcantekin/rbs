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
- Render on change (R-03, merged) repeats an unchanged canvas as a repeat entry with a new sequence. ADR-0146 (picture identity on frames, Proposed, written in parallel) gives frames a picture identity so encoders can reuse their input on a repeat; this ADR works without it.
- `VideoEncoderConfig::memory` already tells an adapter where its input lives. The capture port names a render device with `GpuDeviceId` (LUID or UUID, `rbs-capture`).

Forces: P4 zero copy, P3 capabilities not vendors, P6 nothing blocks the render tick, P13 zero cost when unused, P1 encoder adapters never depend on `rbs-gpu` (ADR-0025 decision 3), ADR-0143 shared work and right-sized buffers, and SC-PC's mixed setup (render on an RTX 4090, AMF on an AMD iGPU).

## Decision

1. **Selection by capability, fixed when the chain is built.** The composition root must give the export node and the encoder the same input memory kind: a native kind of the render device when the encoder's `input.memory` lists it and the encoder can open the render device, otherwise `system`. Engine code must not branch on vendor, platform or encoder name. Native kinds name the surface shape, not the vendor: `metal` (IOSurface), `d3d12` (shared texture), a new `d3d12_buffer` (shared linear buffer), later `vulkan`. An adapter lists the kinds it takes; the export node builds the matching exporter. `GpuDeviceId` moves to `rbs-core::media` (re-exported from `rbs-capture`), and `VideoEncoderConfig` gains `device: Option<GpuDeviceId>`, required for native kinds. The adapter resolves it itself (NVENC: `cuDeviceGetLuid` over CUDA devices) and refuses with `Unsupported` when it cannot open that device; the chain then uses `system`. The choice is made on the control thread by the encoder node's existing `open` probe (ADR-0038 decision 4: a throwaway session that is dropped again), run with the native kind first; if it refuses, the root builds the chain with `system`. The choice never changes per tick.
2. **Exported frames are marked, and only they may be shared.** An exporter builds frames through a new `unsafe` constructor, `VideoFrame::exported`, which sets a marker on the native payload. `VideoFrame::share` returns a second reference (a lease clone) only for marked frames; capture surfaces stay unshareable and imported exactly once. A marked frame is still read by one consumer only: the other holders (output slots, the mailbox) never touch the pixels.
3. **Exclusive output pins.** `Pin` gains `exclusive`: such an output may have at most one link, and its consumer must be an `Encoder` node, either on the same partition or through a video queue (writer, mailbox, reader). Lowering and graph validation must reject anything else. A native-mode export node uses a descriptor with an exclusive "video" output; system mode keeps today's descriptor. This makes the single reader a graph rule, checked before anything runs.
4. **Ring size from the real holders.** Each exporter has a fixed ring (ADR-0025 decision 4) sized at `prepare` from: the exporter's conversions in flight; the export output slot; the queue writer's held frame, the mailbox and the reader's output slot (when the link crosses partitions); the frames the encoder holds; one hold pin and one black surface when the export has a hold. For the encoder's share, the encoder port gains a contract: `VideoEncoder::max_frames_held()` (frames an adapter may keep after `submit` returns, B-frames and look-ahead included). It is a provided method with a conservative documented default, so existing adapters keep compiling; native-capable adapters override it. Sizes are never constants.
5. **Export node rules in native mode.** Every rule of the io-nodes spec's video export node keeps its meaning:
   - **Busy.** `ExportBusy` drops the new canvas and counts `busy`; that canvas is converted again on the next tick (today's `Ok(false)` path).
   - **Repeat.** An unchanged canvas re-hands the last exported surface (a lease clone) with the tick's time and a new sequence. It takes no new ring slot, so a repeat is never busy. With ADR-0146 it also keeps its picture identity; without it the encoder simply encodes the same surface again, which costs no copy.
   - **Hold.** Frozen: the last exported surface is pinned (a lease clone). Scene (ADR-0124 phase 2b): the hold scene's canvas is converted through the same exporter, and each new scene frame replaces the pin. Black: one ring surface is cleared to black at `prepare` and never written again. It replaces today's system black frame for that export, and it exists only when the export has a hold (P13).
   - **Ending (ADR-0045).** Every exporter must deliver frames in submission order (`take_oldest`) and report `in_flight()`, so the ending path drains as it does today.
6. **Exporters.** `rbs-gpu` provides one exporter per surface shape with the ADR-0025 frame shape: Metal (the existing `SurfaceExporter`, unified memory only); D3D12 texture and D3D12 buffer, both with a shared NT handle and a shared timeline fence (`SurfaceSync::TimelineFence`). The buffer is laid out like today's readback (plane offsets, 256-byte-aligned strides). `NativeSurface` gains `D3dSharedBuffer` (NT handle, size, adapter LUID, plane layouts). Vulkan follows later. All wgpu-hal and FFI code stays in one marked low-level `rbs-gpu` interop module with `SAFETY` comments.
7. **Encoder adapters.** An adapter that takes a native kind must import or wrap each surface once and cache it by handle. It waits on the frame's `SurfaceSync` on the GPU where the API allows (CUDA external semaphore, D3D fence), never writes the surface, and drops the frame only after the encoder has read it. Adapters still never depend on `rbs-gpu` (P1). NVENC keeps its CUDA context: `cuImportExternalMemory` per ring buffer, `nvEncRegisterResource` with a CUDA device pointer, and `cuImportExternalSemaphore` per fence.
8. **Lifetime and teardown off the real-time threads.** Each time a native export node prepares, it hands its exporter's `lease_owner()` to the composition root, restarts by the controller (P7, ADR-0024) included. The root keeps every owner it was handed for a chain. It drops them on the control thread only when the chain's export node and encoder node have both been released and collected and the chain's retired queue endpoints are dropped. Ordering across partitions is not assumed. A lease dropped on a worker thread or in an encoder callback therefore only gives a slot back and never frees GPU memory there (lease rules in the media types spec). The encoder adapter's import cache (including CUDA external memory and semaphores) is freed when its session is dropped in `release`, on the control thread.
9. **Failure and fallback.** If the native `open` probe refuses, the chain is built with `system` memory (decision 1). Every failure of a native-mode export node or of an encoder node in a native kind is permanent (`NodeError::Permanent`), so the controller does not restart it natively. Such failures include device lost, an import refused, and a failed `prepare`. A frame refused for its memory kind is counted and dropped like any refused frame (ADR-0038 decision 4), so the packets already pushed in that tick are kept; the node then fails permanently on its next tick, before it submits anything. Other refusals stay counted drops, as today. The studio (control thread) sees the permanent failure and acts as for any permanent encoder-chain failure today: a recording stops, and a stream goes through its restart, which rebuilds the chain. From then on the studio uses `system` memory for that encoder until settings change or the app restarts, and the chain's stats show it. Native mode is never retried on a device that was lost. System chains keep the controller's automatic restart.
10. **Zero cost when unused.** Exporters, interop objects and imported fences exist only while the chain is Active (P13).
11. **Sharing between outputs.** R-01's shared readback stays for `system` chains. Each native chain has its own exporter and ring. That costs one GPU conversion and about 20 MB of GPU memory per extra chain at 1080p, but no CPU copy. Sharing one surface between two encoders gets its own decision, once measurements show two native chains of one shape are common (the audit's split-chain proposal under E-03 and E-05).
12. **Interim step for system memory (E-06).** Until a chain is native, `YuvDownload` should hand the encoder `Bytes` that point into the mapped staging buffer (`Bytes::from_owner`; the owner's drop marks the slot for unmapping on the render thread). This drops the frame buffers. It changes the GPU compositor spec's "System-memory download" section: ring size grows with the encoder's held frames, and `take_ready` and `no_buffer` change meaning. That spec change lands with E-06, together with busy and lost counts measured before and after. E-06 stays for encoders on another device (AMF on SC-PC's iGPU while rendering on the 4090).

Order of work, each its own change with before/after numbers and the P15 sign-off: E-06; VideoToolbox native (Metal); D3D12 buffer exporter and NVENC; D3D12 texture exporter and AMF on the same device; QSV and Linux (Vulkan, VA-API, NVENC) when hardware is available. The ADR-0038 exception ends for each encoder as its native path lands.

## Consequences

- Native chains remove both CPU copies and the encoder's upload: per 1080p60 chain about 400 MB/s of copies and about 58 MB of RAM, plus the PCIe round trip on a discrete GPU. They add a ring of GPU surfaces (about 3 MB each at 1080p NV12, typically 7 to 10 per chain). `GpuMetrics::frames_cpu_copied` must read 0 for a native chain, and the budget tests can then cover recording.
- Contract changes, all additive (minor versions; the touched types are `non_exhaustive`):
  - `rbs-core`: `VideoFrame::exported` and the marker, `share` for marked frames, `NativeSurface::D3dSharedBuffer`, `MemoryKind::D3d12Buffer`, `GpuDeviceId` (moved, re-exported from `rbs-capture`).
  - `rbs-graph`: `Pin::exclusive` and its lowering and validation rule.
  - `rbs-codec`: `VideoEncoderConfig::device`, `VideoEncoder::max_frames_held` (provided method with a default).
  - `rbs-gpu`: the exporters' `take_oldest` and `in_flight`.
- Specs to update with the first implementation of each part: [media types](../specs/media-types.md), [media graph](../specs/media-graph.md) ("Video links carry only..."), [io-nodes](../specs/io-nodes.md) (video export node native mode, encoder node), [output ports](../specs/output-ports.md) (input memory kinds, device, frames held), [GPU compositor](../specs/gpu-compositor.md) (exporters; the open question on encoder-held frames; the "System-memory download" section for E-06).
- More `unsafe` and FFI: wgpu-hal resource export, CUDA external memory and semaphores. Each step runs `risky-review` and the hot-path checklist. The teardown tests cover a controller restart of the export node and of the encoder node while the old ring's frames are still held.
- Testing: the cloud has no hardware encoder, and lavapipe cannot export D3D12 or Metal memory. Conformance runs over fake engines as today, plus native-mode tests of the export node against a fake encoder: busy, repeat, the three hold kinds, ending, and teardown order. Real runs are short device runs on the Mac (VideoToolbox) and SC-PC (NVENC, and AMF with render on the iGPU). Each checks the picture (PSNR against the readback path) and A/V sync. QSV and Linux wait for hardware.
- Behaviour change for the owner to accept: a native failure (an import refused, for example) stops a recording and every output that shares that chain, where today a transient encoder failure is restarted automatically. The next start uses `system` memory. Rebuilding a running chain in place with `system` is a possible later step.
- AMF on a different adapter than the render device keeps the readback by design; E-06 halves its copies.
- ADR-0025 decision 2 ("never travels on a graph link") is narrowed by decisions 2 and 3 for exported frames. ADR-0025 is not edited; this ADR supersedes that sentence.

## Alternatives considered

- **Export inside the encoder node on the render executor** (ADR-0025's original wording, no link). Puts encoder-specific code on the render thread, or needs a new runtime hook to pass a frame to another partition without a link. Decisions 2 and 3 reuse the existing writer, mailbox and lowering instead.
- **A move-only link** (the queue writer takes the frame out of the producer's output slot). Needs a new `Inputs` take API in the scheduler's hot path. Counting holders through the lease, plus an exclusive pin, gives the same single-reader guarantee without touching the scheduler.
- **Making every native frame shareable.** Simple, but breaks the import-exactly-once safety contract of capture surfaces.
- **One `d3d12` kind, with buffer or texture chosen by encoder.** That would be a vendor choice (P3). Separate kinds let adapters declare what they take.
- **NVENC D3D12 session** (`NV_ENC_DEVICE_TYPE_DIRECTX` with D3D12 input and fences, SDK 11.1+). Adds a second device type to the adapter and gives a Windows-only path. The CUDA route keeps one session type for Windows and Linux (Vulkan opaque FD later).
- **D3D11 shared textures for NVENC.** Needs a D3D11 device next to the D3D12 render device, plus keyed mutexes, and gains nothing over CUDA import.
- **Keep the readback and only do E-06.** Halves the CPU copies but keeps the PCIe round trip. It is the fallback, not the goal (P4).
- **Choose by platform** (macOS native, Windows readback). Violates P3 and gets SC-PC's cross-adapter case wrong.

## Review

Round 1: independent AI session (ADR-0014), 2026-11-18, given only this ADR, the specs, the principles and the code. Verdict: changes requested. Thirteen findings, each addressed in this revision:

1. Blocker: the single-consumer rule had no enforcement (the lease owner is opaque, and lowering knows only the link kind). Fixed by marked exported frames (decision 2) and exclusive output pins checked by lowering, with the same-partition case allowed (decision 3).
2. Major: the ring size missed real holders (writer held frame, reader slot, exporter pipeline depth), and "frames the encoder holds" had no contract. Fixed by decision 4 and `VideoEncoder::max_frames_held`.
3. Major: scene hold, and the move of the black picture into the ring. Fixed in decision 5; the black surface exists only with a hold.
4. Major: ending needed in-order draining. `take_oldest` and `in_flight` are required of every exporter (decision 5), plus a native ending test.
5. Major: the last ring `Arc` could be dropped on a real-time thread. Fixed by the owner hand-over and teardown order (decision 8).
6. Major: buffer or texture chosen by vendor. Fixed with separate memory kinds (decision 1).
7. Major: E-06 changes the "System-memory download" spec. Now stated in decision 12.
8. Minor: device matching across LUID and UUID. The adapter resolves the device or refuses (decision 1).
9. Minor: fallback trigger, stickiness, stream restart and lost device. Decision 9.
10. Minor: busy and repeat semantics. Decision 5.
11. Minor: ADR-0146 is optional. Stated in the context and in decision 5.
12. Minor: semver and layering. Listed in Consequences.
13. Minor: GPU cost of one exporter per chain, and the trigger for sharing. Decision 11.

Round 2: same reviewer, 2026-11-18. Verdict: changes requested. Findings 1-4, 6-8 and 10-13 were confirmed resolved against the code. Four findings remained, each addressed:

1. Major: controller restarts (P7) broke the fixed teardown order. Decision 8 now states an invariant: the root keeps every owner, including owners from restarts, and drops them only after both nodes are collected and the endpoints are retired. A restart case was added to the tests.
2. Major: fallback assumed a rebuild the runtime does not do. Decision 9 now makes native failures permanent, so the studio rebuilds with `system`.
3. Minor: `max_frames_held` is now a provided method with a default.
4. Minor: decision 1 now names the throwaway `open` probe.

Round 3: same reviewer, 2026-11-18. Verdict: approve. Round-2 findings 1-4 confirmed resolved against the code. Two minor findings, addressed: the refused-frame rule of ADR-0038 decision 4 is kept (decision 9: the refused frame is a counted drop, the permanent failure comes on the next tick), and stopping recordings on a native failure is stated in Consequences for the owner to accept.

Owner approval: pending.
