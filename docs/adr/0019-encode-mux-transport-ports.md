# ADR-0019: Separate encode, mux and transport ports for outputs

- Status: Proposed
- Date: 2026-11-02
- Deciders: Salih (owner)

## Context

Every recording and stream goes through the same three steps: compress frames and audio blocks (encode), wrap the packets in a container (mux), and deliver the result somewhere (a file, an RTMP server, an SRT peer). ADR-0003 makes each kind of external technology a port, and ADR-0007 keeps FFmpeg to codecs only, so muxing and network delivery are native Rust. The [backend ports spec](../specs/backend-ports.md) left open whether muxers and transports are one port or two, because some protocols carry their own framing:

- RTMP and WHIP send individual encoded packets with protocol-specific framing (FLV tag bodies, RTP); they never see a container byte stream.
- SRT, RIST, UDP and HLS carry a container byte stream (usually MPEG-TS, or fMP4 segments).
- A file sink writes whatever container the user picked (fragmented MP4, Matroska, MPEG-TS) and may patch earlier bytes (durations, cues) when the file is seekable.

Other forces: one encode must feed several outputs without copies (P4, overview memory model); a slow network output must never stall the render tick or other outputs (P7, media-graph spec); outputs that are not running cost nothing (P13); ports may depend only on `rbs-core` (P1), so they cannot force an async runtime on the engine.

## Decision

1. There are three output ports, each its own crate with traits, capability keys, typed errors and a conformance suite:
   - `rbs-codec`: video and audio encoders (`VideoEncoderFactory`/`VideoEncoder`, `AudioEncoderFactory`/`AudioEncoder`).
   - `rbs-mux`: muxers (`MuxerFactory`/`Muxer`) that turn packets of one or more streams into container chunks.
   - `rbs-transport`: transports (`TransportFactory`/`Transport`) that deliver to a destination.
2. A transport must declare which input it takes with the `transport.input` capability: `packets` (it does its own framing, for example RTMP, WHIP) or `container.<name>` (it takes the chunks of a muxer, for example SRT takes `container.mpegts`, the file sink takes every container). The engine builds an output as encoder(s) -> [muxer] -> transport by matching these capabilities; a muxer is inserted only when the transport asks for a container.
3. Packets, stream descriptors and frame types are shared foundation types in `rbs_core::media` (owned by the media types spec). Port crates must not define their own copies.
4. Port traits are synchronous and must not require an async runtime. A transport's `send` must not block, for network and disk alike: it queues into a bounded, adapter-owned single-producer queue and reports what happened. Drop decisions are made on the producer side; the I/O side shares only the queue and atomics. Network transports drop video only from a non-keyframe up to the next keyframe, then audio, never headers. Transports that must not drop (files) return a recoverable `QueueFull` and the output worker buffers and retries. Adapters that need a network runtime or threads create and own them, start them only in `start`, and stop them in `finish` or `Drop`.
5. Encoders must expose their stream descriptor (codec parameters, extradata) right after `create`, so muxers and transports can write headers before the first packet. The descriptor is fixed for the session; changing it means a new output chain.
6. Muxers must produce chunks tagged as header, media (with keyframe flag and timestamp) or trailer, plus optional patch chunks (offset + bytes) that only seekable transports apply. Muxers for fragmented MP4, Matroska, MPEG-TS and FLV must declare `mux.live`: their output stays playable when the trailer and patches never arrive, for example after a crash.
7. Framing code shared by several adapters (for example FLV tag bodies used by both the FLV muxer and the RTMP transport, Annex B and length-prefixed NAL conversion) lives in the `rbs-media-format` helper crate, a layer of its own that depends only on `rbs-core`; adapters may use it but never depend on each other. The muxer or packet transport that needs a different bitstream framing than the encoder produced converts it, as a counted copy for that output only.
8. Transports report peer feedback as events (`KeyframeRequested`, `BitrateHint`); the engine routes them to the encoder, rate-limited or isolated when the encoder has several consumers.
9. The types shared by the mux and transport ports (`ContainerId`, `StreamIndex`, `MuxChunk`, `ChunkKind`) live in `rbs_core::media`, because ports may depend only on `rbs-core`. Conformance-suite support shared by all ports lives in `rbs_core::conformance` behind a `conformance` cargo feature, so it is never compiled into runtime builds.
10. Configuration, outcome and event types are `#[non_exhaustive]` with constructors, so the contracts can grow (HLS segment boundaries, caption data, WebRTC feedback) in minor versions.

Contract details: [output ports spec](../specs/output-ports.md).

## Consequences

- RTMP stays a pure transport with no fake "FLV muxer" step, while SRT, files and future HLS reuse the same muxers.
- Swapping a muxer (for example a faster MPEG-TS writer) or a transport needs no change to encoders or the engine.
- One encoder can feed a file and two streams at once; the packet payload is reference counted and shared, except where a consumer needs another bitstream framing (for example MPEG-TS next to FLV), which costs one copy for that consumer.
- Recordings never silently lose data: a slow disk leads to backpressure and, past a bound, a stopped and reported recording, never a file with holes.
- The engine needs a small output builder that matches transport input to muxers; it is part of the graph work, not of the ports.
- A transport sees per-packet or per-chunk calls, so per-call overhead in adapters matters; muxers and transport send loops are on the [hot paths](../architecture/hot-paths.md) list already.
- Follow-up: the helper crate for shared framing, the conformance suites, and adapters (`rbs-mux-mpegts`, `rbs-mux-fmp4`, `rbs-mux-matroska`, `rbs-transport-file`, `rbs-transport-rtmp`, `rbs-transport-srt`, `rbs-codec-ffmpeg`, native hardware encoders).

## Alternatives considered

- **One combined output port** (each adapter muxes and delivers): simplest for RTMP, but every file and stream adapter would reimplement or embed containers, and swapping a container would mean touching every transport.
- **Mux and transport as one port with an optional container**: hides the split but makes capabilities ambiguous (what does "supports MPEG-TS" mean for an RTMP adapter?) and conformance suites harder to write.
- **Async (`Future`-returning) transport traits**: natural for network code, but would tie the port to an async runtime and its executor, against P1, and put scheduling decisions in the port instead of the adapter.
- **FFmpeg's libavformat for muxing and protocols**: rejected by ADR-0007.

## Review

Independent review per ADR-0014. Record: reviewer (human handle, or "independent AI session"), date, verdict (approve / changes requested), main concerns and how each was resolved.
