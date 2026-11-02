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
2. A `VideoFrame` never contains pixels. Its `FramePayload` is one of: an opaque `TextureId { slot, generation }` resolved by `rbs-gpu`; a `NativeSurface` described as raw integer handle values plus a `SurfaceSync`; or a `SystemFrame` in CPU memory. Core never dereferences a handle; only `rbs-gpu` interop code (reviewed `unsafe`) does.
3. Memory lifetime uses a `Lease`: `Arc<dyn LeaseOwner>` plus a `u64` token. Clone calls `retain`, drop calls `release`; owners keep per-slot atomic counts. Leases never allocate after the owner is created.
4. Packet payloads and CPU frame buffers use `bytes::Bytes`, which becomes the first dependency of `rbs-core`.
5. Timing arithmetic is exact: rates and timebases are `u32` rationals; series positions are computed from their index (`Cadence`), never accumulated.

## Consequences

- All parallel crates share one vocabulary; adapters convert at the edge and nowhere else.
- Moving a frame is a copy of at most 192 bytes plus one atomic increment for a lease.
- Raw handle values in a safe crate rely on the lease contract for validity; a misbehaving producer could release early. Only `rbs-gpu` and adapters use them, inside reviewed `unsafe` code, and conformance suites check lease balance.
- `bytes` is now part of the public API of `rbs-core`; a `bytes` 2.0 would be a breaking change for us. It is the standard type in the `tokio` network stack the transports use, so conversions there are free.
- New pixel formats, codecs and surface kinds are added as variants of non-exhaustive enums without breaking users.

## Alternatives considered

- **Generic frame type over a GPU backend parameter** (`VideoFrame<B: GpuBackend>`): pushes generics into every port trait and plugin boundary, and trait objects become awkward. Rejected.
- **`Arc<dyn Texture>` per frame**: simple ownership, but one allocation per frame unless pooled, and a virtual call for every property. The lease gives the same lifetime guarantee without allocation.
- **Pass `wgpu::Texture` directly**: `rbs-core` and all ports would depend on `wgpu`, and capture adapters would need a `wgpu` device to deliver frames. Breaks P1.
- **Our own refcounted buffer instead of `bytes`**: avoids a dependency, but re-implements O(1) clone and zero-copy slicing and needs conversions at every transport. Not worth it.
- **Floating-point frame rates and durations**: simple, but drift over long sessions and cannot represent 30000/1001 exactly.

## Review

Pending: independent review per ADR-0014.
