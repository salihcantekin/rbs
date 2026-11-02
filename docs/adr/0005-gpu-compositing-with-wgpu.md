# ADR-0005: GPU compositing with wgpu and zero-copy frames

- Status: Accepted
- Date: 2026-11-02

## Context

We need one renderer for Windows (D3D12/Vulkan), macOS (Metal) and Linux (Vulkan), with the ability to import and export native textures for zero-copy capture and encode.

## Decision

- Compositing, filters and colour conversion use `wgpu`, with WGSL shaders.
- Native texture interop (D3D11/12 shared handles, IOSurface, DMA-BUF, Vulkan external memory) is done through `wgpu`'s HAL layer inside `rbs-gpu`, isolated behind a small interop API.
- Video frames are pooled GPU textures passed by handle. CPU readback is an explicit, measured fallback.

## Consequences

- One shader language and one renderer for all platforms.
- Interop code is platform-specific `unsafe` and needs careful review (`risky-review` skill).
- If `wgpu` blocks a needed interop path, `rbs-gpu` may use a native backend for that platform behind the same API; that would require a new ADR.

## Alternatives considered

- Raw Vulkan (`ash`) + Metal + D3D12: maximum control, three renderers to maintain.
- Skia or a game engine: heavy, not designed for zero-copy media pipelines.
