# Spec: Backend ports, capabilities and registry

- Status: Draft
- Owner crate(s): `rbs-core` (capability model, registry), `rbs-capture`, `rbs-audio-io`, `rbs-codec`, `rbs-mux`, `rbs-transport`
- Related ADRs: [0003](../adr/0003-ports-and-adapters.md), [0007](../adr/0007-ffmpeg-as-optional-adapter.md)

## Purpose

Define how RBS talks to any external technology (OS capture, audio devices, encoders, muxers, network protocols) so that each implementation can be added, replaced or removed without changing the engine.

## Terms

- **Port**: a trait family plus a capability model for one kind of technology.
- **Adapter**: an implementation of a port (for example `rbs-codec-nvenc` implements the video encode port).
- **Capability**: a typed fact an adapter declares about what it can do on this machine, discovered at runtime (`probe`).
- **Requirement**: what the engine needs for a task; matched against capabilities.
- **Registry**: holds adapter factories and selects one for a requirement.

## Ports

| Port crate | Trait(s) | Examples of adapters |
| --- | --- | --- |
| `rbs-capture` ([spec](capture-port.md)) | `CaptureFactory`, `CaptureSession` | Windows Graphics Capture, DXGI duplication, ScreenCaptureKit, PipeWire portal, X11, V4L2, Media Foundation, AVFoundation |
| `rbs-audio-io` ([spec](audio-io-port.md)) | `AudioBackend`, `AudioInputStream` | WASAPI, process loopback, CoreAudio, PipeWire, ALSA, `cpal` |
| `rbs-codec` | `VideoEncoderFactory`, `VideoEncoder`, `VideoDecoderFactory`, `VideoDecoder` ([live decode](#live-video-decode)), `AudioEncoder` | NVENC, AMF, QSV/oneVPL, VideoToolbox, VA-API, Vulkan Video, x264, SVT-AV1, rav1e, Opus, FFmpeg |
| `rbs-mux` | `Muxer` | fragmented MP4, MKV, MPEG-TS, FLV, HLS/DASH segmenter |
| `rbs-transport` | `Transport` | RTMP/RTMPS (incl. Enhanced RTMP), SRT, WHIP, file sink, virtual camera |

## Common shape of every port

Every port follows the same pattern so that tooling, docs and skills apply to all of them.

```rust
/// Implemented once per adapter. Cheap to construct; does no heavy work.
pub trait AdapterFactory: Send + Sync + 'static {
    /// Stable, unique, reverse-DNS style id, e.g. "org.rbs.codec.nvenc".
    fn id(&self) -> AdapterId;
    /// Human readable name for UIs and logs.
    fn display_name(&self) -> &str;
    /// Discover what this adapter can do on this machine right now.
    /// Must be fast (< 50 ms) and must not panic if hardware is missing:
    /// return an empty capability set instead.
    fn probe(&self) -> Result<CapabilitySet, ProbeError>;
}
```

Each port then adds a typed constructor, for example:

```rust
pub trait VideoEncoderFactory: AdapterFactory {
    fn create(&self, config: &VideoEncoderConfig) -> Result<Box<dyn VideoEncoder>, CreateError>;
}

pub trait VideoEncoder: Send {
    /// Submit one frame. Never blocks for longer than one frame interval.
    fn submit(&mut self, frame: VideoFrameRef<'_>) -> Result<(), EncodeError>;
    /// Drain encoded packets produced so far into `out`. Never allocates per call
    /// when `out` has capacity.
    fn drain(&mut self, out: &mut Vec<EncodedPacket>) -> Result<(), EncodeError>;
    /// Apply a runtime change (bitrate, keyframe request). Unsupported changes
    /// return `Err(Unsupported)`; they never silently do nothing.
    fn reconfigure(&mut self, change: EncoderChange) -> Result<(), EncodeError>;
    fn flush(&mut self, out: &mut Vec<EncodedPacket>) -> Result<(), EncodeError>;
}
```

These signatures are the starting point; the exact types are fixed when each port crate is created, and this spec is updated in the same PR. Frame, block and packet types (`VideoFrame`, `AudioBlock`, `EncodedPacket`, `StreamDescriptor`, formats) come from the [media types spec](media-types.md); in the sketch above, `VideoFrameRef<'_>` is `&VideoFrame`. The encode, mux and transport contracts are fixed in [output ports](output-ports.md).

## Live video decode

- Status: Draft (ADR-0139 phase 1c; needs the independent review of rule 11 before adapters merge).
- Crate: `rbs-codec` (`decoder.rs`, `DecodeError`, `conformance::video_decoder`). Adapters: `rbs-codec-videotoolbox` (macOS) and `rbs-codec-mf` (Windows) add a decoder factory beside their encoder; both register in `Registry<dyn VideoDecoderFactory>`.
- Why a second decode port: `rbs-media-decode` ([spec](media-decode-port.md)) opens a file and owns demuxing and seeking. A live stream has neither; the caller depacketizes and submits access units.

```rust
pub trait VideoDecoderFactory: AdapterFactory {
    fn create(&self, config: &VideoDecoderConfig) -> Result<Box<dyn VideoDecoder>, CreateError>;
}
pub trait VideoDecoder: Send {
    fn stream(&self) -> &StreamDescriptor;
    fn submit(&mut self, packet: &EncodedPacket) -> Result<(), DecodeError>;
    fn drain(&mut self, out: &mut Vec<VideoFrame>) -> Result<(), DecodeError>;
    fn reset(&mut self) -> Result<(), DecodeError>;
    fn flush(&mut self, out: &mut Vec<VideoFrame>) -> Result<(), DecodeError>;
    fn max_surfaces(&self) -> usize { 0 }
}
```

`VideoDecoderConfig { codec, bitstream, extradata, max_size, target }`; `DecodeTarget { adapter_luid, accept }` lists the native memory kinds the caller can import and the Direct3D adapter the studio renders on.

Invariants:

1. **Capabilities.** `codec.video.decode` (set of codec names; phase 1 needs `h264`), `decode.output_memory` (memory kinds the adapter can hand out; `system` always works), `codec.sessions.max` (optional). `probe` never opens a device that needs more than a capability query, and a factory's constructor does nothing (P13): no thread, no session, no GPU object before `create`.
2. **Untrusted input.** The caller has already checked every SPS and PPS against ADR-0139 decision 11. The adapter still refuses a packet it cannot frame (`InvalidInput`) and a stream whose picture exceeds `config.max_size` (`InvalidInput` for the packet that carries the SPS), and never reads outside the packet. It must not panic on any byte sequence; the fuzz targets of ADR-0139 decision 16 run against it through the port.
3. **Keyframe gating.** Until a keyframe with parameter sets has been decoded, and after `reset`, a non-key packet is dropped and `submit` returns `NeedKeyframe`. The caller sends a PLI and keeps submitting; the error is recoverable and counted, not logged per packet.
4. **Order and stamps.** Pictures come out in presentation order. Each picture's timestamp is the `pts` of the packet that produced it; the adapter keeps no clock and no jitter buffer. Live streams have no B-frames (ADR-0139 decision 9), so decode order equals presentation order; an adapter that reorders (a stream with B-frames) says so in its documentation and the suite skips nothing.
5. **Latency.** No decode-order lookahead: `drain` after `submit` returns the picture of that packet when the platform decoder produces it synchronously, and the adapter asks the platform for low-latency decode (no frame reordering delay). `flush` is end of stream only.
6. **Memory kinds.** Native output (`IoSurface` on macOS, a shared D3D11/D3D12 texture on Windows) is delivered only if `config.target` accepts the kind; otherwise system memory (NV12), counted as a CPU copy (P4). A native surface on a Direct3D adapter other than `target.adapter_luid` is never handed out (the adapter decodes on that adapter or gives system memory).
7. **Surfaces and leases (ADR-0109 decision 8).** A native picture carries a lease. The adapter reuses the surface only after the lease is released, which the studio does after its GPU work on the picture has finished; a decoder with every surface leased drops the new picture (counted) and never allocates beyond `max_surfaces()`; it never waits for a lease. The render thread never waits on the decoder.
8. **Reset.** `reset` drops queued packets and pictures, keeps the session and its surfaces, and waits for a keyframe. A change of picture size or format in the stream is `FormatChanged`: the caller recreates the session.
9. **Errors.** `DecodeError` is typed with `is_recoverable()`. A decoder failure is the source's, never the engine's: the guest source goes Unavailable (ADR-0055).
10. **Threads.** A session is driven from one worker thread at a time (never a real-time thread). Adapters that need their own threads create and stop them in `Drop`.

Conformance: `rbs_codec::conformance::video_decoder(&factory, &clip)` with a `DecodeClip` (an Annex B H.264 clip that starts with a keyframe carrying SPS and PPS, one picture per packet, no B-frames). Cases: `delta-before-keyframe`, `decode-clip` (picture count, size, timestamps), `second-flush-empty`, `reset-then-keyframe`, `foreign-codec`, `max-size`. A fake decoder in `rbs-codec/tests/decoder_suite.rs` checks the suite itself. Each adapter adds its own lease and pool tests and runs the suite on its platform; a machine without the decoder skips with a reason.

Out of scope here: the guest source node, depacketizing, the SPS/PPS validator and the token bucket (ADR-0139, owned by the guest source), audio decode (`rbs-codec-opus`).

## Capability model

Capabilities are data, not code, so they can be logged, shown in the UI, compared in tests and sent across plugin boundaries.

- A `CapabilitySet` is a list of `Capability` entries: a `CapabilityKey` (namespaced string such as `codec.video.encode`) and typed `CapabilityValue`s (flags, enums, integer ranges, lists).
- Examples for a video encoder: codec (`h264`, `hevc`, `av1`), profiles, bit depths, chroma formats, max resolution, max frame rate, rate-control modes, B-frames, input memory kinds (`system`, `d3d11`, `d3d12`, `vulkan`, `metal`, `dmabuf`), concurrent session limit, `hardware: true/false`.
- A `Requirement` is a set of constraints over capability keys (must have, at least, at most, one of). The registry evaluates requirements against probed capabilities.

The current Rust types live in `rbs_core::capability` and are the source of truth for the model.

## Registry and selection

1. Adapters are registered at start-up: built-in adapters by the composition root (`rbs-app`, `rbs-cli`) from the one list in `rbs-platform` (`rbs_platform::adapters`, ADR-0160), plugin adapters by the plugin host. Building that list constructs factories only; a factory's constructor must not probe, load a library or open a device.
2. The registry probes each adapter once (and again on device change events) and caches the result.
3. `Registry::select(port, requirement)` returns candidates that satisfy the requirement, ordered by:
   1. the user's explicit choice, if any;
   2. the configured preference order (default list per platform, shipped as data);
   3. a score from capabilities: zero-copy input for the current GPU memory kind first, then hardware over software.
4. If creation of the first candidate fails, the engine falls back to the next one and records why.
5. Every selection decision is logged with the reasons, and visible in diagnostics.

Replacing a technology (for example FFmpeg) therefore means: add adapters, make them pass conformance, move them up in the preference order, then remove the old adapter from the default build.

## Errors

- Probe errors never abort start-up; the adapter is marked unavailable with a reason.
- Runtime errors are typed per port and carry `is_recoverable()`. Recoverable errors trigger reconfigure or restart; unrecoverable ones trigger fallback to the next candidate.
- Adapters must never panic across the port boundary. The host wraps calls with `catch_unwind` as a last resort and treats a panic as unrecoverable.

## Threading

- Factories are `Send + Sync`. Session objects are `Send`, driven from one thread at a time.
- Adapters that need their own threads create and own them, and stop them in `Drop`.
- Port methods called from real-time threads are marked in the trait docs and must follow principle P6.

## Conformance tests

Each port crate exports a conformance suite as a library function, for example `rbs_codec::conformance::video_encoder(&factory)`. An adapter crate runs it in its own tests. Suites check at least:

- `probe` returns quickly and never panics without hardware.
- Declared capabilities are true: every advertised format, resolution and rate-control mode is actually created and exercised.
- Timestamps are preserved and monotonic; clock conversion is correct (capture, audio).
- Every `Lease` an adapter hands out is released, and frame `sequence` numbers increase.
- Capture adapters hand native frames only to the `rbs-gpu` importer (one consumer); the engine holds an `Arc` to every `LeaseOwner` until the adapter has shut down.
- `reconfigure` either works or returns `Unsupported`.
- No allocation on calls documented as real-time.
- Clean shutdown: no thread or handle leaks after `Drop`.

Hardware-dependent cases are skipped (not failed) when probe reports no hardware, and run on self-hosted runners that have it.

## Open questions

- How `rbs-gpu` imports each `NativeSurface` kind and which kinds each platform supports (owned by the `rbs-gpu` spec; the representation is fixed in [media types](media-types.md)).
- ~~Whether transports and muxers are one port or two for protocols with built-in framing (RTMP/FLV).~~ Two ports; a transport declares whether it takes packets or a container ([ADR-0019](../adr/0019-encode-mux-transport-ports.md), [output ports](output-ports.md)).
