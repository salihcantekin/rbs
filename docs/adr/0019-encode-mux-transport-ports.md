# ADR-0019: Separate encode, mux and transport ports for outputs

- Status: Accepted
- Date: 2026-11-02
- Amends: ADR-0003 (adapters may also depend on the format-helper crate `rbs-media-format`, which depends only on `rbs-core`)
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
- Follow-up: the helper crate for shared framing and the adapters (`rbs-mux-mpegts`, `rbs-mux-fmp4`, `rbs-mux-matroska`, `rbs-transport-file`, `rbs-transport-rtmp`, `rbs-transport-srt`, `rbs-codec-ffmpeg`, native hardware encoders).

## Alternatives considered

- **One combined output port** (each adapter muxes and delivers): simplest for RTMP, but every file and stream adapter would reimplement or embed containers, and swapping a container would mean touching every transport.
- **Mux and transport as one port with an optional container**: hides the split but makes capabilities ambiguous (what does "supports MPEG-TS" mean for an RTMP adapter?) and conformance suites harder to write.
- **Async (`Future`-returning) transport traits**: natural for network code, but would tie the port to an async runtime and its executor, against P1, and put scheduling decisions in the port instead of the adapter.
- **FFmpeg's libavformat for muxing and protocols**: rejected by ADR-0007.

## Review

- Reviewer: independent AI sessions (fresh context; given only this ADR, the principles, related ADRs, specs and code), three rounds
- Date: 2026-11-02
- Verdict: approve (round 3), after changes requested in rounds 1 and 2
- Concerns and resolutions:
  1. Drop policy dropped single P-frames, had no audio rule and implied a contended lock -> GOP-tail drops with refusal until the next keyframe, audio rule, producer-side staging queue plus SPSC hand-off; the suite checks refusals and `KeyframeRequested`.
  2. File transports could block or fail on a disk stall -> recoverable `QueueFull`, worker-side buffering, file I/O off the calling thread.
  3. Bitstream conversion contradicted zero-copy and had no home -> counted per-output copy, `rbs-media-format` helper layer; this ADR amends ADR-0003.
  4. Allocation rule contradicted media types -> at most one payload allocation per produced packet or chunk; media types spec qualified.
  5. Mid-stream stream changes undefined -> the stream descriptor is fixed per session; changes need a new output chain.
  6. Contracts could not evolve -> `#[non_exhaustive]` with constructors and a versioning section (fields of struct-like variants are fixed).
  7. No WebRTC/SRT feedback -> `KeyframeRequested` and `BitrateHint` events with routing rules.
  8. Ambiguous outcomes and chunk keyframes -> `Queued` / `QueuedAfterDrops` / `Dropped`; `keyframe` means an entry point for every stream; chunk-input drop and reconnect rules.
  9. Weak or flaky conformance checks, test support in runtime builds -> tighter interleaving bound, entry-point checks, structural TS/Matroska readers, more negative cases, `assert_ran`, generous timing ceilings, `conformance` cargo features, docs CI with `--all-features`.
  10. Open, accepted as follow-ups: encoder delay signalling, a space-available signal for `QueueFull`, zeroizing `Secret`, allocation guard in the suites, whole-GOP dropping checked only through adapter tests.
- Owner approval: 2026-11-03, under the early-phase policy (Salih: ADRs may be marked Accepted by threads after the independent review)
