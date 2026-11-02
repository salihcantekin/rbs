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
- Verdict: changes requested, then changes made (listed below)
- Concerns:
  1. Lease contract not enforced by types, so `unsafe` importers could not rely on it -> `LeaseOwner` is an `unsafe trait`, `Lease::adopt` and `VideoFrame::native` are `unsafe fn`s, frame fields are private.
  2. No consumer-to-producer GPU completion signal; keyed mutex breaks under fan-out -> single-importer rule: native frames are not shareable, the importer drops the lease after its own GPU fence (decision 4, spec "Native surfaces").
  3. Types cannot cross plugin boundaries -> decision 6 and spec "Plugin boundaries": the plugin SDK defines `repr(C)` / WIT mirrors and maps leases.
  4. Evolution overstated, semver impact not stated -> `#[non_exhaustive]` plus constructors on growing types, `PlanarChannels` hides `MAX_CHANNELS`, spec "Versioning" section.
  5. `bytes` pooling claim needs `from_owner` (1.9) and still allocates per packet -> `bytes = "1.9"`, spec documents one allocation per packet at the producer.
  6. CUDA and Vulkan variants under-specified, no adapter identity -> both removed until a producer needs them; Direct3D variants carry `adapter_luid`, Metal carries `device_registry_id`.
  7. Format not tied to payload -> `SystemFrame` keeps its validated format; `VideoFrame::system` takes it from there; fields are private.
  8. Lease edge cases (owner teardown on a real-time thread, panicking release, stale tokens, atomic cost) -> spec "Lifetime" requires the engine to hold owners until background shutdown, forbids panics, recommends generation bits in tokens, states two atomics.
  9. Drift mapping can step backwards; backwards source time not detected; counter overflow -> sources keep output monotonic and swap at most once per bucket (documented); backwards source time restarts and returns `Observation::Restarted`; counter is `u64`.
  10. Cadence domain near `u64::MAX` and above 1e9 per second; packet durations -> domain stated in code and spec; muxers derive durations from rounded positions.
- Owner approval: pending
