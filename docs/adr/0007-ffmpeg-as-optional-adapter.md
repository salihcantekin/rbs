# ADR-0007: FFmpeg only as an optional codec adapter

- Status: Accepted
- Date: 2026-11-02

## Context

FFmpeg gives access to almost every software and hardware codec quickly. Avoiding it entirely would delay a usable release by a long time. Depending on it everywhere would lock in its API, build complexity and licence.

## Decision

- FFmpeg is used only inside `rbs-codec-ffmpeg` (encode/decode adapters) and, if needed, a media-file source adapter.
- It is linked dynamically and built in an LGPL configuration by default.
- Muxing, transports, audio processing and compositing are native Rust and do not use FFmpeg.
- Native adapters (NVENC, VideoToolbox, AMF, QSV/oneVPL, VA-API, Vulkan Video) are added over time and preferred over FFmpeg when they pass conformance and offer zero-copy input.

## Consequences

- Fast path to a working encoder on all platforms in Phase 0.
- Removing FFmpeg later is a matter of adapter coverage, not refactoring.
- CI must build with and without the FFmpeg feature.
- 2026-11-12: the media-file source adapter named in the decision is specified by [ADR-0108](0108-media-file-source.md) (Proposed). This decision is unchanged.

## Alternatives considered

- FFmpeg for everything including mux and network: less code, but a hard dependency with a large surface.
- No FFmpeg at all: cleanest, but months of extra work before the first stream.
