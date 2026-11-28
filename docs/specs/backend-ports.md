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

- Status: Reviewed (independent review 2026-11-28; ADR-0139 phase 1c).
- Crate: `rbs-codec` (`video_decoder.rs`, `h264.rs`, `VideoDecodeError`, `conformance::video_decoder`). Adapters: `rbs-codec-videotoolbox` (macOS) and `rbs-codec-mf` (Windows) add a decoder factory beside their encoder. A decoder factory has its own `AdapterId` and probe, so `codec.sessions.max` on it counts decode sessions.
- Why a second decode port: `rbs-media-decode` ([spec](media-decode-port.md)) opens a file and owns demuxing and seeking. A live stream has neither; the caller depacketizes and submits access units.

```rust
pub trait VideoDecoderFactory: AdapterFactory {
    fn create(&self, config: &VideoDecoderConfig) -> Result<Box<dyn VideoDecoder>, CreateError>;
}
pub trait VideoDecoder: Send {
    fn output_format(&self) -> Option<VideoFormat>;            // None until the first SPS
    fn submit(&mut self, packet: &EncodedPacket) -> Result<(), VideoDecodeError>;
    fn drain(&mut self, out: &mut Vec<VideoFrame>) -> Result<(), VideoDecodeError>;
    fn reset(&mut self) -> Result<(), VideoDecodeError>;
    fn flush(&mut self, out: &mut Vec<VideoFrame>) -> Result<(), VideoDecodeError>;
    fn max_surfaces(&self) -> usize { 0 }
    fn stats(&self) -> DecoderStats;  // pictures, dropped_no_surface, cpu_copies, need_keyframe
}
```

`VideoDecoderConfig { codec, bitstream, extradata, max_size, target: DecodeOutput, min_surfaces }`. The live case (the guest source) is `AnnexB` with empty `extradata`: one access unit per packet, SPS and PPS in band, pts already on the receiver's master clock, no B-frames. `LengthPrefixed` with an `avcC` `extradata` is accepted but untested in phase 1. `DecodeOutput { adapter_luid, accept }` (not the file port's `DecodeTarget`) lists the native memory kinds the caller can import and the Direct3D adapter the studio renders on.

Invariants:

1. **Capabilities.** `codec.video.decode` (set of codec names; phase 1 needs `h264`), `codec.video.decode.profiles` (`h264.high`, ...), `codec.video.decode.width`/`.height` (ranges), `codec.video.decode.fps.max`, `decode.output_memory` (kinds the adapter can hand out; `system` always works), `codec.sessions.max` (optional). `probe` only queries; a factory's constructor does nothing (P13): no thread, session or GPU object before `create`.
2. **Untrusted input.** One safe-Rust Annex B splitter and SPS parser lives in `rbs_codec::h264` (no dependencies; mutation-tested in its unit tests, and the SPS fuzz target of ADR-0139 decision 16 calls it); every adapter calls it on every SPS before the platform decoder sees the bytes. The caller validates too (ADR-0139 decision 11); this is the second line. `max_size` is compared with the *cropped* display size (a 1080p stream is coded at 1088 lines) and pictures carry the cropped size. An oversized stream is `InvalidInput`; a stream the allow list admits but the hardware refuses (profile, level, size) is `Unsupported`, so the engine falls back to the next adapter. The adapter never panics on any bytes, never reads outside the packet, and answers garbage, truncated, empty or oversized-NAL packets with a typed error and a session that still decodes afterwards.
3. **Keyframe gating.** Until a keyframe with parameter sets has been decoded, and again after `reset`, after `flush`, and after a platform decode error on a packet, a non-key packet is dropped and `submit` returns `NeedKeyframe`. The caller sends a PLI and keeps submitting; the error is recoverable and counted, not logged per packet.
4. **Stamps are bit-exact.** A picture's timestamp is the submitted packet's `pts` exactly, not rounded to the platform's unit (Media Foundation counts 100 ns; RTP-derived stamps are not multiples of that). An adapter that must pass times through the platform keeps its own FIFO keyed by submission order. Order is presentation order; no clock, no jitter buffer, no B-frames (ADR-0139 decision 9).
5. **Latency.** The adapter asks the platform for low-latency decode (no reordering delay: Media Foundation `CODECAPI_AVLowLatencyMode`, VideoToolbox without reordering) and `drain` after `submit` returns the packet's picture, within `DecodeClip::lookahead` pictures (0 for a synchronous decoder, at most 1 otherwise; suite case `no-lookahead`). `flush` is end of stream and also makes the session usable again, gated on a keyframe.
6. **Back-pressure.** `submit` does at most 2 ms of adapter work outside the platform call. When the adapter's bounded input queue is full it returns `Busy`: `drain`, then submit the same packet again. A platform decode error on a packet re-enters keyframe gating (`NeedKeyframe` on the next packets).
7. **Format changes.** An in-band SPS change within `max_size` and the allowed formats is followed by the adapter itself (renegotiation, or a new platform session at that IDR) and updates `output_format`. `FormatChanged` is returned only from `submit`, only for a change the adapter cannot follow; that packet was not decoded and pictures before it stay drainable; the caller recreates the session. Media Foundation's stream-change notice is handled inside `drain` and is never an error by itself.
8. **Native pictures (P4, ADR-0109 decision 8).** Native output is handed out only for kinds `target.accepts`, and only as: Windows `NativeSurface::D3dSharedHandle { nt: true }` of NV12 or `B8G8R8A8` (as the file port; `rbs-gpu` import support is checked before the adapter merges) with `SurfaceSync::Ready` (the worker waited on the fence), on `target.adapter_luid`; macOS `NativeSurface::IoSurface` of NV12 (`'420v'`, `'420f'` for a full-range VUI). Never a raw texture pointer, never a surface on another adapter, never a sync the render thread would block on. `adapter_luid: None` on Windows means system memory only. Colour comes from the VUI, BT.709 limited when absent.
9. **Pool and leases.** Native pictures live in an adapter-owned pool *separate from the decoder's own reference pictures*: Windows copies each picture on the GPU into shared NT-handle textures (the decoder's texture array is its DPB and not shareable); macOS keeps a slot table of retained IOSurface-backed buffers, which are VideoToolbox's own output buffers (safe because VideoToolbox writes an output buffer only before handing it out and never reuses one while it is retained; its reference pictures are separate). A slot returns to the pool only when its lease is released (the studio does that after its GPU work on it has finished). The adapter enforces `max_surfaces()`: with every slot leased the new picture is released and counted in `dropped_no_surface`; it never allocates beyond, never waits, and the render thread never waits on it. `max_surfaces()` is 0 if and only if the session hands out system memory only, and may drop to 0 at runtime after a fallback to the copy path (and rise again when the adapter makes a new platform session); native adapters override it (at most 8, so four guests stay under the importer's `MAX_SURFACES` of 64); `create` gives at least `min_surfaces` (default 3) or fails with a typed `CreateError`. A dropped session's pool stays alive while a lease names a slot, and handles stay open 10 s after the last reference (the file port's rules).
10. **Hidden source.** A guest that is not drawn drops its decoder (Standby holds no hardware session, P13, and frees decode sessions for `guest_decode_cap`); it creates a new one and sends a PLI when drawn again. `reset` is for loss recovery on a live session and keeps the session and pool.
11. **Errors and threads.** `VideoDecodeError` is typed with `is_recoverable()` (`InvalidInput`, `NeedKeyframe`, `Busy` recoverable; `Unsupported`, `FormatChanged`, `DeviceLost` not). A session is driven from one worker thread, never a real-time thread. Threads are created in `create` and stopped and joined in `Drop`; a platform callback on an OS thread hands off through a bounded queue and never touches the caller's `Vec`. A decoder failure is the source's (ADR-0055).

Conformance: `rbs_codec::conformance::video_decoder(&factory, &clip)` with a `DecodeClip` (the live case above, stamps that are not multiples of 100 ns, `lookahead`). Cases: `delta-before-keyframe`, `hostile-input`, `decode-after-hostile-input`, `no-lookahead`, `decode-clip` (count, size, bit-exact stamps), `stats`, `output-format`, `max-surfaces`, `flush-gates-and-second-flush-empty`, `reset-then-keyframe`, `foreign-codec` (and decodes afterwards), `max-size` (and later deltas gate). An adapter whose probe declares no decoder on a machine that claims hardware fails; one that reports no hardware skips. The fake decoder in `rbs-codec/tests/decoder_suite.rs` checks the suite with one broken invariant per test. Each adapter adds its own tests for memory kinds, pool exhaustion (hold every picture; leases never exceed `max_surfaces()`; decoding continues after release), dropping the decoder while holding pictures, a two-resolution clip (invariant 7) and its own lease accounting; a machine without the decoder skips with a reason.

Implementation status (2026-11-28, not merged; the contract is still Draft): `rbs-codec-mf` has `MfH264DecoderFactory` (`org.rbs.codec.mf-h264-decode`, module `decode`) over Microsoft's H.264 decoder MFT. Annex B only (`LengthPrefixed` is refused at `create` with `Unsupported`); native output is DXVA on `adapter_luid` copied into 1 to 8 shared NV12 NT-handle textures, otherwise software decode to system memory. The system-memory path is the counted CPU-copy fallback (P4): one new NV12 buffer per picture, `cpu_copies` per picture, `max_surfaces()` 0; it is not pooled, because the guest source asks for native output and the owner judged pooling unnecessary (2026-11-28). Both copies apply the SPS crop; colour comes from the VUI (BT.709 limited when absent). Fallbacks are logged once at Warn in `Area::Encode`. An MFT refusal of the stream from a decode call (`MF_E_UNSUPPORTED_FORMAT`, `MF_E_INVALIDMEDIATYPE`, `MF_E_INVALIDTYPE`, `MF_E_UNSUPPORTED_D3D_TYPE`, `MF_E_UNSUPPORTED_RATE`, `MF_E_HW_MFT_FAILED_START_STREAMING`, `E_NOTIMPL`) is `Unsupported`; a removed, reset or hung device is `DeviceLost`. `tests/studio_import.rs` imports native pictures through the `rbs-gpu` D3D12 importer and checks their colours and slot reuse. Its tests run the suite for both memory kinds with clips encoded at test time by Microsoft's H.264 encoder.

`rbs-codec-videotoolbox` has `VideoToolboxDecoderFactory` (`org.rbs.codec.videotoolbox-h264-decode`, module `mac::decode`, not merged) over `VTDecompressionSession`. Annex B only (`LengthPrefixed` is refused at `create` with `Unsupported`). Each packet with a slice becomes one 4-byte length-prefixed sample of its slices and SEI; SPS and PPS go into a `CMVideoFormatDescription` (`CMVideoFormatDescriptionCreateFromH264ParameterSets`), and the session is created at the first IDR and recreated at the IDR after a changed SPS or PPS (invariant 7). A refused packet carrying an SPS or a slice also forgets the staged parameter sets, so only a freshly accepted SPS and PPS lift the gate. An SPS with an HDR transfer (PQ or HLG) is `Unsupported`, since pictures are labelled with the BT.709 transfer. Decoding is synchronous without temporal processing, with `kVTDecompressionPropertyKey_RealTime`, so the picture is ready when `submit` returns (lookahead 0); each sample carries its submission index as `sourceFrameRefCon`, and the stamps stay in the adapter's FIFO. The output callback only retains the buffer and pushes it onto a bounded queue. `submit` allocates per packet: CoreMedia's `CMBlockBuffer` and `CMSampleBuffer` (the sample may outlive the call); `drain` does not allocate when `out` has spare capacity. With `metal` accepted, pictures are VideoToolbox's own IOSurface-backed `'420v'` buffers (`'420f'` for a full-range VUI; the range handed out is the one the buffer's fourcc states), retained in a slot table of `min_surfaces` (at most 8) slots and handed out as `NativeSurface::IoSurface` with `SurfaceSync::Ready`, with no GPU copy. VideoToolbox already crops the buffers to the display size (a 1080p stream gives 1920 x 1080 buffers); a buffer that is not the display size, not NV12 or not IOSurface-backed switches the session to the system copy until the next platform session (logged once at Warn). A left or top crop offset is applied by the copy (unit-tested placement) but untested with a real stream, as the VideoToolbox encoder never writes one. Because the surfaces are VideoToolbox's, the set it cycles through is its choice: the churn test (`slow_pool_and_importer_churn_stays_bounded`, 330 pictures of 1080p imported through the `rbs-gpu` Metal importer with leases handed back as the studio does, and all 8 slots held for 60 pictures) saw 9 distinct IOSurfaces and 4 importer opens on an M4 Pro (macOS 26.7.1), 0 lease misuses. A guest can hold up to 8 surfaces beyond VideoToolbox's own pool: about 25 MB at 1080p, 113 MB at 4096 x 2304. The system-memory path is the counted CPU copy (one new NV12 buffer per picture, crop applied). `kVTInvalidSessionErr`, `kVTVideoDecoderRemovedErr` and `kVTSessionMalfunctionErr` are `DeviceLost`; `kVTVideoDecoderMalfunctionErr`, `kVTAllocationFailedErr` and `kVTVideoDecoderNotAvailableNowErr` are recoverable `Failed` that drop the platform session (the next IDR makes a new one); `kVTCouldNotFindVideoDecoderErr`, `kVTVideoDecoderUnsupportedDataFormatErr` and `kVTCouldNotCreateInstanceErr` are `Unsupported`; other decode failures are recoverable `Failed` and gate. `probe` calls only `VTIsHardwareDecodeSupported` (for `adapter.hardware`); macOS always has the software decoder, whose use is logged once at Warn. `tests/decoder.rs` runs the suite for both memory kinds with clips encoded at test time by the crate's own VideoToolbox encoder, and `tests/studio_import.rs` imports native pictures through the `rbs-gpu` Metal importer and checks flat red and blue and slot reuse. Measured at 1080p30 (`examples/decode_1080p.rs`, M4 Pro, hardware decoder, `submit` plus `drain`): native p50 0.88 ms, p95 0.99 ms; system copy p50 1.57 ms, p95 2.05 ms.

Out of scope here: the guest source node, depacketizing, the token bucket (ADR-0139, owned by the guest source), audio decode (`rbs-codec-opus`).

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

## Live decoders

`rbs-codec` also holds the decode traits for streams that arrive as packets (ADR-0139 decision 2). `rbs-media-decode` stays the port for files, because a file port owns demuxing and seeking that a live stream does not have. Only the audio half exists so far; `VideoDecoder` is specified with the first video decoder adapter (phase 1c).

```rust
pub trait AudioDecoderFactory: AdapterFactory {
    fn create(&self, config: &AudioDecoderConfig) -> Result<Box<dyn AudioDecoder>, CreateError>;
}

pub trait AudioDecoder: Send {
    fn format(&self) -> AudioFormat;
    /// Most sample frames one call can produce; reserve `max_frames() * channels` and `decode` never allocates.
    fn max_frames(&self) -> usize;
    /// hot. Clears `out`, fills it with interleaved `f32`, returns the sample frames per channel.
    fn decode(&mut self, packet: &EncodedPacket, out: &mut Vec<f32>) -> Result<usize, DecodeError>;
    /// hot. Concealment for a packet that never arrived.
    fn conceal(&mut self, frames: usize, out: &mut Vec<f32>) -> Result<usize, DecodeError>;
    fn reset(&mut self); // cannot fail: an adapter whose state cannot be cleared recreates it
}
```

- `AudioDecoderConfig` names the codec, the output `AudioFormat`, the codec's setup data (`extradata`, for Opus an optional `OpusHead`) and opaque `adapter_options`.
- The caller owns timing: the decoder sees packets and lengths, never clocks. Reordering, jitter buffering and the choice between `decode` and `conceal` belong to the guest source, not to the decoder.
- Errors are typed (`DecodeError`: `InvalidInput`, `Corrupt`, `Failed`). A corrupt packet leaves the session usable. No panic crosses the port.
- Capability key `codec.audio.decode` (Set of codec names) next to `codec.audio.sample_rates` and `codec.audio.channels.max`.
- Rounding: the caller rounds a concealment length to the codec's granularity (Opus: 120 sample frames); a decoder refuses other lengths with `InvalidInput`.
- Planned additions, as provided methods so existing adapters keep compiling (found by the independent review, needed by the guest source's jitter buffer): `packet_frames(&EncodedPacket)` (frames a packet decodes to, without decoding it), and `decode_fec(next, frames, out)` with a `codec.audio.fec` capability and a new `DecodeError::Unsupported`.
- Status: trait and `DecodeError` are in `rbs-codec`; the Opus adapter is `rbs-codec-opus`. There is no decoder conformance suite yet; the Opus tests and the RFC 8251 vectors stand in for it until a second adapter exists.

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
