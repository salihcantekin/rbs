# ADR-0018: Media frames as small values with leased memory

- Status: Proposed
- Date: 2026-11-02
- Deciders: Salih (owner)

## Context

Capture adapters, `rbs-gpu`, encoders, muxers and transports are being written in parallel, and they all exchange frames, audio blocks and packets. Port crates may depend only on the foundation crate (principle P1, overview layer table), so the shared types must live in `rbs-core`, which cannot name `wgpu`, OS media APIs or vendor SDKs.

Forces:

- Zero copy (P4, ADR-0005): frames are GPU textures from capture to encoder. Capture APIs hand out their own surfaces (WGC D3D11 textures, IOSurface, PipeWire DMA-BUF); encoders want native textures; the compositor uses `wgpu` textures from pools.
- Real time (P6): handing a frame to another thread must not allocate or lock.
- Producers must know when consumers are done with a surface, or they cannot reuse it.
- One encode feeds several outputs without copying the payload (performance budgets).
- A CPU fallback must exist and be visible (P4).

## Decision

1. `rbs_core::media` defines `VideoFrame`, `AudioBlock`, `EncodedPacket`, `StreamDescriptor` and their format types as plain data. Contract: [media types spec](../specs/media-types.md).
2. A `VideoFrame` never contains pixels. Its `FramePayload` is one of: an opaque `TextureId { slot, generation }` resolved by `rbs-gpu`; a `NativeSurface` described as raw integer handle values plus a `SurfaceSync`; or a `SystemFrame` in CPU memory. Core never dereferences a handle; only `rbs-gpu` interop code (reviewed `unsafe`) does. Frame fields are private, so the format always matches the payload and a native surface is never separated from its lease.
3. Memory lifetime uses a `Lease`: `Arc<dyn LeaseOwner>` plus a `u64` token. Clone calls `retain`, drop calls `release`; owners keep per-slot atomic counts. Leases never allocate after the owner is created. `LeaseOwner` is an `unsafe trait` and `Lease::adopt` an `unsafe fn`, and a native frame can only be built by the `unsafe` `VideoFrame::native`, which requires a lease; the validity of raw handles rests on these explicit contracts, not on convention.
4. A native frame has exactly one consumer, the `rbs-gpu` importer. It waits on the producer's `SurfaceSync`, and drops the lease only after its own GPU work on the surface has completed. Everything downstream sees pool textures or system memory, and only those can be shared.
5. Packet payloads and CPU frame buffers use `bytes::Bytes` (1.9 or later), which becomes the first dependency of `rbs-core`.
6. These types are the in-process contract. The plugin SDK defines `#[repr(C)]` and WIT mirrors; leases become a C retain/release vtable for native plugins, and frames are pinned by the host for the duration of a call for WASM and process plugins, with handles duplicated into the plugin process where needed.
7. Public structs and struct-like variants that are likely to grow are `#[non_exhaustive]` with constructors.
8. Timing arithmetic is exact: rates and timebases are `u32` rationals; series positions are computed from their index (`Cadence`), never accumulated.

## Consequences

- All parallel crates share one vocabulary; adapters convert at the edge and nowhere else.
- Moving a frame is a copy of at most 192 bytes. Sharing it costs two atomic operations (the `Arc` count and the owner's slot count) and a virtual call.
- Raw handle values rely on the `unsafe` lease contract; a producer that breaks it causes undefined behaviour in the importer. Producers are adapters (reviewed `unsafe`), and conformance suites check lease balance.
- `rbs-core` now contains `unsafe` contract declarations (an `unsafe trait` and `unsafe fn`s) but no `unsafe` blocks outside tests; `media/lease.rs` is a marked low-level module (P11).
- `bytes` is now part of the public API of `rbs-core`; a `bytes` 2.0 would be a breaking change for us. It is the standard type in the `tokio` network stack the transports use, so conversions there are free.
- New pixel formats, codecs and surface kinds are added as variants of non-exhaustive enums without breaking users.

## Alternatives considered

- **Generic frame type over a GPU backend parameter** (`VideoFrame<B: GpuBackend>`): pushes generics into every port trait and plugin boundary, and trait objects become awkward. Rejected.
- **`Arc<dyn Texture>` per frame**: simple ownership, but one allocation per frame unless pooled, and a virtual call for every property. The lease gives the same lifetime guarantee without allocation.
- **Pass `wgpu::Texture` directly**: `rbs-core` and all ports would depend on `wgpu`, and capture adapters would need a `wgpu` device to deliver frames. Breaks P1.
- **Our own refcounted buffer instead of `bytes`**: avoids a dependency, but re-implements O(1) clone and zero-copy slicing and needs conversions at every transport. Not worth it.
- **Import native surfaces at the producer so only `TextureId`s travel**: simplest lifetime story, but capture adapters would need `rbs-gpu` (an engine crate), which ports and adapters may not depend on. The single-importer rule gives the same effect one hop later.
- **Floating-point frame rates and durations**: simple, but drift over long sessions and cannot represent 30000/1001 exactly.

## Review

- Reviewer: independent AI session (fresh context; given only this ADR, the media types spec, the principles, related ADRs and specs, and the code)
- Date: 2026-11-02
- Verdict: approve (third pass, after "changes requested" on the first pass with 4 blocking and 6 non-blocking concerns, and on the second pass with one blocking item, 11). Non-blocking items left for the `rbs-gpu` spec: with an aliased keyed-mutex surface, release the key only when the slot's last reference is gone; DMA-BUF device identity on multi-GPU Linux; plugin SDK to link its type mirrors.
- Concerns:
  1. Lease contract not enforced by types -> `LeaseOwner` is an `unsafe trait`, `Lease::adopt` and `VideoFrame::native` are `unsafe fn`s, frame fields are private. Reviewer: resolved.
  2. No consumer-to-producer GPU completion signal; keyed mutex breaks under fan-out -> single-importer rule; native frames are not shareable. Reviewer: mostly resolved, see 11 and 12.
  3. Types cannot cross plugin boundaries -> decision 6 and spec "Plugin boundaries". Reviewer: resolved; `plugin-sdk.md` to link the mirrors with the plugin SDK work (open, non-blocking).
  4. Evolution and semver -> `#[non_exhaustive]` plus constructors, `PlanarChannels`, spec "Versioning". Reviewer: resolved.
  5. `bytes` pooling claim -> `bytes = "1.9"`, one allocation per packet at the producer documented. Reviewer: resolved.
  6. CUDA / Vulkan variants under-specified, no adapter identity -> removed; adapter LUID and Metal registry id added. Reviewer: resolved; spec leftover fixed. Open, non-blocking: device identity for DMA-BUF on multi-GPU Linux.
  7. Format not tied to payload -> `SystemFrame` keeps its format, private fields. Reviewer: resolved.
  8. Lease edge cases -> spec obligations; backend-ports conformance now checks that the engine holds each owner's `Arc`. Reviewer: resolved.
  9. Drift estimator monotonicity and restart -> `Observation::Restarted` (now `#[must_use]`, `#[non_exhaustive]`), `u64` counter, documented source rule. Reviewer: resolved.
  10. Cadence domain and packet durations -> documented. Reviewer: resolved.
  11. (second pass, blocking) Import copy vs alias unclear -> spec: a copying import drops the lease after its copy completes; an aliasing import moves the lease into the pool slot so the surface lives as long as the slot is referenced.
  12. (second pass) Single-importer rule only in one spec -> stated in media-graph and backend-ports; the import API takes `VideoFrame` by value, and any parts-based import is `unsafe`.
  13. (second pass) `EncodedPacket::new` silently clamped `dts` -> returns `MediaError::DtsAfterPts`; fields are private with accessors.
  14. (second pass) Review record written by the author -> this record now states the reviewer's verdicts per pass.
- Owner approval: pending
