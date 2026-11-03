# Spec: Output ports (encode, mux, transport)

- Status: Draft
- Owner crate(s): `rbs-codec`, `rbs-mux`, `rbs-transport`
- Related ADRs: [0003](../adr/0003-ports-and-adapters.md), [0007](../adr/0007-ffmpeg-as-optional-adapter.md), [0013](../adr/0013-zero-cost-when-unused.md), [0019](../adr/0019-encode-mux-transport-ports.md)
- Builds on: [backend ports](backend-ports.md) (common factory shape, registry), [media types](media-types.md) (`rbs_core::media`: `CodecId`, `VideoFormat`, `AudioFormat`, `MemoryKind`, `VideoFrame`, `AudioBlock`, `EncodedPacket`, `StreamDescriptor`, `Bytes`; `rbs_core::timing`: `FrameRate`, `Timebase`)

## Purpose

The contracts every encoder, muxer and transport adapter implements, so that recording and streaming backends (FFmpeg, NVENC, x264, MPEG-TS, fMP4, RTMP, SRT, files, ...) can be added, replaced or removed without engine changes.

## Scope

In scope: encoder, muxer and transport traits; their configuration, capability keys, errors, threading rules and conformance suites.

Out of scope: the shared media types (packets, stream descriptors, frames, audio blocks, and the container types `ContainerId`, `StreamIndex`, `MuxChunk`; see [media types](media-types.md#containers-rbs_coremediacontainer)), GPU frame export (see the GPU compositor spec), output graph nodes and the output builder (media graph spec), decoders.

## Output chain

```
 video frame ──> VideoEncoder ──┐                         ┌─> Transport (input = packets)          e.g. RTMP
                                ├─ EncodedPacket (shared) ┤
 audio block ──> AudioEncoder ──┘                         └─> Muxer ──> MuxChunk ──> Transport (input = container.X)  e.g. file, SRT
```

- One encoder feeds any number of outputs. Packets are cloned by reference count, never by copying payload bytes, as long as the consumer takes the encoder's bitstream format.
- Bitstream conversion: containers and protocols disagree on framing (FLV, fMP4 and Matroska carry length-prefixed H.264/HEVC with parameter sets in extradata; MPEG-TS carries Annex B with parameter sets repeated before each IDR). The muxer or packet transport that needs a different framing converts it; this is one counted copy per packet for that output only, measured like any other hot-path work. Conversion code is shared through the `rbs-media-format` helper crate (depends only on `rbs-core`, see [below](#rbs-media-format-format-helpers); see the layer table in the [overview](../architecture/overview.md#layers)). The output builder prefers the encoder bitstream format that most of its consumers take.
- A transport declares its input in `transport.input`. The output builder inserts a muxer for the requested container only when the transport asks for one.
- Streams of one output are numbered `0..n` in the order of the `streams` slice given to the muxer or transport (`StreamIndex`).

## Common rules

These extend the [backend ports](backend-ports.md) rules.

- Every factory implements `rbs_core::registry::AdapterFactory`. `probe` is fast, never panics, creates no threads and opens no device sessions beyond what is needed to answer and is closed before returning.
- `create` may allocate and open devices; it must not start threads or network connections that `Drop` does not stop. Transports start network work only in `start`.
- Session objects (`VideoEncoder`, `AudioEncoder`, `Muxer`, `Transport`) are `Send` and are driven by one thread at a time.
- No method may panic across the port boundary. Errors are typed and carry `is_recoverable()`.
- Unsupported runtime changes return an `Unsupported` error; they never silently do nothing.
- Calls marked **hot** in the trait docs run per frame, block, packet or chunk ([hot paths](../architecture/hot-paths.md)): no blocking I/O, no unbounded work, no contended locks. Allocation: at most one payload allocation per produced packet or chunk (the `Bytes` header, see [media types](media-types.md#packets-rbs_coremediapacket); pooled buffers via `Bytes::from_owner`), and none otherwise once the caller's output `Vec` and the adapter's working buffers have reached their steady-state size.
- Who drives sessions: an encoder worker owns each encoder session (one encoder may feed several outputs); an output worker owns the muxer and transport sessions of one Active output. Both call the hot methods as data arrives. The output worker calls `poll_events` after each batch of sends and at least once per second while the output is Active. Installed and Standby outputs have no sessions, so nothing polls them (P13).
- Installed adapters (registered, not used by any output) cost only their probe at start-up (P13).

## `rbs-codec`

### Configuration

All configuration and outcome types are `#[non_exhaustive]` and are built with constructors and `with_*` methods (for example `VideoEncoderConfig::new(codec, format, frame_rate, memory, rate_control).with_max_b_frames(2)`), so fields can be added in minor versions. The field lists below show the current content.

```rust
pub struct VideoEncoderConfig {
    pub codec: CodecId,                 // h264, hevc, av1, ...
    pub format: VideoFormat,            // size, input pixel format (nv12, p010, i420), colour to signal
    pub frame_rate: FrameRate,
    pub memory: MemoryKind,             // input memory: system, gpu, d3d11, vulkan, ...
    pub rate_control: RateControl,
    pub keyframe_interval: KeyframeInterval,
    pub max_b_frames: u32,              // 0 for lowest latency
    pub profile: Option<String>,        // codec profile name, e.g. "high", "main10"
    pub tuning: EncoderTuning,          // { preset: Quality | Balanced | Speed, low_latency: bool }
    pub adapter_options: Vec<(String, String)>, // opaque, adapter-specific; never read by the engine
}

pub enum RateControl {
    Cbr { bitrate_bps: u64 },
    Vbr { target_bps: u64, max_bps: u64 },
    ConstantQuality { level: u8 },      // 0 (best) ..= 51 (worst), mapped by the adapter to CQP/CRF/CQ
}

pub enum KeyframeInterval { Frames(u32), Duration(Duration) }

pub struct AudioEncoderConfig {
    pub codec: CodecId,                 // aac, opus, ...
    pub format: AudioFormat,            // sample rate and channel layout
    pub bitrate_bps: u64,
    pub adapter_options: Vec<(String, String)>,
}
```

### Traits

```rust
pub trait VideoEncoderFactory: AdapterFactory {
    fn create(&self, config: &VideoEncoderConfig) -> Result<Box<dyn VideoEncoder>, CreateError>;
}

pub trait VideoEncoder: Send {
    /// Codec parameters and extradata. Valid from `create` on and fixed for the
    /// session: no `EncoderChange` alters it. A change of codec, size, profile or
    /// extradata means a new encoder session and a new output chain (muxer and
    /// transport restart with new headers).
    fn stream(&self) -> &StreamDescriptor;
    /// hot. Submit one frame. Never blocks longer than one frame interval.
    fn submit(&mut self, frame: &VideoFrame) -> Result<(), EncodeError>;
    /// hot. Move packets produced so far to `out`.
    fn drain(&mut self, out: &mut Vec<EncodedPacket>) -> Result<(), EncodeError>;
    /// Apply a runtime change. Not hot, but must not block the caller for long.
    fn reconfigure(&mut self, change: &EncoderChange) -> Result<(), EncodeError>;
    /// Finish: encode all pending frames and move the remaining packets to `out`.
    fn flush(&mut self, out: &mut Vec<EncodedPacket>) -> Result<(), EncodeError>;
}

pub enum EncoderChange {
    Bitrate(RateControl),
    RequestKeyframe,
}

pub trait AudioEncoderFactory: AdapterFactory {
    fn create(&self, config: &AudioEncoderConfig) -> Result<Box<dyn AudioEncoder>, CreateError>;
}

pub trait AudioEncoder: Send {
    fn stream(&self) -> &StreamDescriptor;
    /// Samples per channel the codec consumes per packet (1024 for AAC-LC, 960 for Opus at 20 ms).
    fn frame_size(&self) -> u32;
    /// hot. Submit any number of samples; the adapter buffers to its frame size.
    fn submit(&mut self, block: &AudioBlock<'_>) -> Result<(), EncodeError>;
    /// hot.
    fn drain(&mut self, out: &mut Vec<EncodedPacket>) -> Result<(), EncodeError>;
    fn reconfigure(&mut self, change: &EncoderChange) -> Result<(), EncodeError>;
    fn flush(&mut self, out: &mut Vec<EncodedPacket>) -> Result<(), EncodeError>;
}
```

### Invariants (checked by the conformance suite)

1. `stream()` matches the config: codec, video format and frame rate, or audio format; extradata is present for codecs that need it (H.264 and HEVC unless the bitstream is Annex B with in-band parameter sets, AAC unless ADTS, Opus).
2. The first video packet is a keyframe. A keyframe appears at least once per `keyframe_interval`.
3. Packet `pts` values are the input timestamps (no invented times): every video packet's `pts` equals the timestamp of one submitted frame, and each submitted frame yields exactly one packet by the end of `flush`, unless the adapter counts it as dropped (an encoder may drop a frame under rate control; adapters that can drop say so in their section and count drops in their metrics). A drop never takes a pending keyframe with it: if the dropped frame was to be a keyframe (first frame, `RequestKeyframe`), the next submitted frame is.
4. `dts` is non-decreasing across packets and `dts <= pts`.
5. After `RequestKeyframe`, a keyframe is produced within `max_b_frames + 1` frames, or the call returned `Unsupported`.
6. `Bitrate` changes either succeed or return `Unsupported`; the adapter declares which in `codec.reconfigure`.
7. Frames whose format, size or memory kind differs from the config are refused with `EncodeError::InvalidInput`, not encoded wrongly.
8. Audio packets' `pts` advance by exactly `frame_size` samples (converted to master-clock time with `SampleRate::duration_of` from the timeline origin) and start at the first submitted block's timestamp. The adapter keeps its own sample-count timeline: a block whose timestamp differs from the expected continuation by less than one codec frame is treated as continuous (drift is the audio graph's job, ADR-0004); a larger gap or overlap is a discontinuity, and the adapter pads the partial frame with silence, emits it, and restarts the timeline at the new block's timestamp, or at the end of the previous timeline's last packet if that is later (encoder delay can push a drained tail past the new block; invariant 4 holds across timelines). The continuity check of later blocks still uses the new block's timestamp.

### Capability keys

| Key | Value | Meaning |
| --- | --- | --- |
| `adapter.hardware` | Flag | Hardware-backed (from `rbs-core`) |
| `codec.video.encode` | Set | Video codecs: `h264`, `hevc`, `av1`, `vp9` |
| `codec.audio.encode` | Set | Audio codecs: `aac`, `opus` |
| `codec.video.profiles` | Set | `<codec>.<profile>`, e.g. `h264.high`, `hevc.main10` |
| `codec.video.bit_depth` | Set | `8`, `10` |
| `codec.video.width` / `codec.video.height` | IntRange | Supported sizes |
| `codec.video.fps.max` | Int | Highest frame rate at the largest size |
| `codec.video.rate_control` | Set | `cbr`, `vbr`, `constant_quality` |
| `codec.video.b_frames.max` | Int | 0 when not supported |
| `codec.reconfigure` | Set | Runtime changes honoured: `bitrate`, `keyframe` |
| `codec.sessions.max` | Int | Concurrent sessions; absent means unlimited |
| `input.memory` | Set | `MemoryKind` names: `system`, `gpu`, `d3d11`, `d3d12`, `vulkan`, `metal`, `dmabuf` |
| `input.pixel_format` | Set | `PixelFormat` names: `nv12`, `p010`, `i420`, ... |
| `codec.audio.sample_rates` | Set | e.g. `44100`, `48000` |
| `codec.audio.channels.max` | Int | |

Native input memory (`metal`, `d3d11`, `d3d12`, `vulkan`, `dmabuf`): the encoder receives native frames exported by `rbs-gpu` ([ADR-0025](../adr/0025-encoder-surface-export.md); `metal` is a `NativeSurface::IoSurface` with `SurfaceSync::Ready`, NV12 `'420v'` or P010 `'x420'`). Encoder adapters read the handle from the payload and do not depend on `rbs-gpu`. They keep the frame (and with it the lease) until the encoder API has finished reading the surface, then drop it; holding frames longer starves the exporter's fixed ring, which then drops frames.

### VideoToolbox encoder (`rbs-codec-videotoolbox`)

- Two adapters over the same code: `org.rbs.codec.videotoolbox` (hardware encoders only, `RequireHardwareAcceleratedVideoEncoder`, `adapter.hardware = true`) and `org.rbs.codec.videotoolbox-software` (also Apple's software encoders, `adapter.hardware = false`, for machines without a media engine such as virtual machines). Codecs are those `VTCopyVideoEncoderList` reports (read once per process): `h264` (profiles baseline, main, high; default high) and `hevc` (main, main10; P010 input means main10). Width 64 to 4096, height 64 to 2304, even only. Rate control `cbr`, `vbr`, and `constant_quality` for the hardware adapter on Apple silicon only (Intel Macs and software encoders lack it). `codec.video.b_frames.max = 0`: frame reordering stays off until its depth is measured on hardware. `codec.reconfigure = {bitrate, keyframe}`; a bitrate change must keep the rate-control mode (another mode is `Unsupported`).
- Rate control mapping: CBR uses `ConstantBitRate` where the encoder supports it, else `AverageBitRate` with a 1 s `DataRateLimits` cap at the same rate; VBR is `AverageBitRate` plus a 1 s cap at `max_bps`; constant quality is `Quality = 1 - level / 51`. Keyframe interval is `MaxKeyFrameInterval` in frames; `RealTime` is on; `EncoderPreset::Speed` sets `PrioritizeEncodingSpeedOverQuality` where available. Colour primaries, transfer and matrix are signalled from `VideoFormat::color` as session properties and attached to every source buffer (`ShouldPropagate`: once per wrapped surface, per frame on pool buffers), so VideoToolbox reads the pixels in the signalled colour space; combinations without a CoreVideo constant are refused at `create`. CBR falls back to the capped average only when `ConstantBitRate` is `kVTPropertyNotSupportedErr`; any other error fails `create`.
- Input `input.memory = {metal, system}`, `input.pixel_format = {nv12}` plus `p010` with HEVC. `metal` frames are `NativeSurface::IoSurface` with `SurfaceSync::Ready` and a lease (other sync, or no lease, is `InvalidInput`) and a bi-planar `'420v'`/`'420f'` or `'x420'`/`'xf20'` surface matching the session's size and range; each surface is wrapped once with `CVPixelBufferCreateWithIOSurface` and cached (32 entries, keyed by address; the wrapper retains the surface, so an address cannot be reused while cached; a larger ring replaces entries round-robin). The frame's lease is cloned into one of 16 slots and released by the output callback, when VideoToolbox no longer reads the surface. `system` frames are copied row by row into a buffer from the session's pixel buffer pool (counted); pool buffers are taken with an allocation threshold of 17 (every slot plus the priming frame); a pool at its threshold is `Busy`, other pool errors are recoverable `Failed`. Presentation times must increase; a frame at or before the previous one is `InvalidInput`.
- Output: length-prefixed (4-byte) NAL units, extradata the `avcC` / `hvcC` record. VideoToolbox gives its parameter sets only with an encoded frame, so `create` encodes one black priming frame at VideoToolbox time 0, takes the record from its format description and discards the packet; the first real frame is forced to be a keyframe. Real frames run on a relative timeline: VideoToolbox time = master time - (first frame's time - one frame), so the priming frame always precedes them; packet times are mapped back. A keyframe whose `avcC` / `hvcC` record differs from the primed one fails the session unrecoverably, since `stream()` is fixed (other description extensions may differ).
- Threads: VideoToolbox calls the output callback on its own threads, in any order. Each submitted frame owns one of 16 slots. A slot is one atomic word holding a 24-bit generation and its state (`free -> in flight -> completing -> done`); the generation is also in the frame refcon. The callback claims its slot with one compare-exchange of the whole word (so a slot completes exactly once per use, and a late callback for an earlier use never completes a later one), releases the lease, builds the packet (one exact-size `Vec`, the one payload allocation per packet) and stores it in the slot. `drain` takes done slots in submission order from a fixed ring, so output order never depends on callback order (valid while `b_frames.max = 0`) and nothing waits on a lock. `submit` returns `Busy` at once when all 16 slots are in flight or not yet drained. A frame VideoToolbox refuses is taken back (`Failed`, recoverable) unless it is `DeviceLost`. Frames VideoToolbox drops are counted and produce no packet (invariant 3); a dropped frame that was to be a keyframe makes the next frame one. A callback error for one frame loses that frame (counted as dropped; a frame `submit` already reported as refused is not counted again); 16 callback errors in a row, `kVTInvalidSessionErr` and `kVTVideoEncoderMalfunctionErr`, panics (caught at the callback boundary) and parameter-set changes end the session and are reported by the next `submit` or `drain`; `kVTInvalidSessionErr` and `kVTVideoEncoderMalfunctionErr` are `DeviceLost`.
- `flush` waits for `VTCompressionSessionCompleteFrames`. `Drop` completes pending frames, then invalidates the session before the shared state is freed, so every lease is released and no callback runs afterwards.
- Off macOS the factories probe to an empty set and refuse sessions.
- Not yet: frame reordering (B-frames), low-latency rate control, `MTLSharedEvent` sync, the multi-GPU encoder choice of Intel Macs.

### AudioToolbox AAC encoder (`rbs-codec-audiotoolbox`)

- One adapter, `org.rbs.codec.audiotoolbox`: AAC-LC through Apple's `AudioConverter` (`'aac '`), a software encoder that is part of every supported macOS (`adapter.hardware = false`). It declares `codec.audio.encode = {aac}`, sample rates 44100 and 48000, at most 2 channels (mono, stereo). Rate control is CBR only: `bitrate_bps` from 16 kbit/s to 160 kbit/s per channel; `codec.reconfigure = {bitrate, keyframe}`: a CBR bitrate change while running and `RequestKeyframe` (a no-op: every AAC packet is a sync sample); anything else is `Unsupported`.
- Input: `f32` blocks of any length, planar or interleaved, matching the session format (else `InvalidInput`). Samples go into a pre-allocated planar staging frame of 1024 samples per channel; each full frame is handed to the converter synchronously on the calling thread. The session starts no threads and gets no callbacks on other threads. The converter's input callback hands over the staged frame and then returns a private "need more" status, so the converter keeps its state between `submit` calls.
- Output: raw AAC-LC access units (`BitstreamFormat::Raw`), `frame_size = 1024`, extradata a two-byte `AudioSpecificConfig`. Packet `pts = dts` = the timeline origin (first block's timestamp) plus 1024 samples per packet (invariant 8); every packet is a keyframe and lasts 1024 samples. Each packet is one exact-size `Vec` copied from a pre-allocated output buffer of the converter's maximum packet size. Packets wait in a queue for `drain` with room for 32 packets (about 0.68 s at 48 kHz); a longer block or more audio between drains grows it once. An empty packet from the converter is an unrecoverable `Failed` (it would shift every later timestamp).
- Encoder delay: the converter's priming (`AudioConverterPrimeInfo::leadingFrames`, 2112 samples on current macOS) is not signalled yet, so decoded audio is late by that delay (see the open question on encoder delay). `AacSession::priming` reports it.
- Discontinuities (invariant 8): a block whose timestamp is one codec frame or more away from the running sample timeline ends the timeline. The partial frame is padded with silence, the converter is drained to its end and reset, and a new timeline starts at the block, or at the end of the drained tail if that is later; in that case the new timeline's packets are late against their input by the difference (about three codec frames after a forward gap; the overlap plus about three frames after a jump back in time), which audio/video sync downstream sees as a step. `flush` does the same without starting a new timeline and forgets the tail: the next block after a flush starts a new stream exactly at its timestamp.
- Errors: a converter error, a failed reset, or a converter asking for an input layout the session did not set up is unrecoverable `Failed`; the callback never unwinds into AudioToolbox (a panic is caught and reported the same way).
- Off macOS the factory probes to an empty set and refuses sessions.
- Not yet: HE-AAC, more than two channels, signalled priming (edit lists / `iTunSMPB`), other sample rates.

### Hardware encoder sessions (`rbs-codec-hw`)

The hardware video encoder adapters (NVENC, QSV, AMF, VA-API) share their vendor-neutral logic through the adapter support crate `rbs-codec-hw` (ADR-0031), which depends on `rbs-codec`, `rbs-core` and `rbs-media-format` only. Each adapter keeps its loader, bindings and an engine that implements the `Engine` trait; the rules below hold for all of them, and the adapter sections only add what differs. The crate's API is internal to the workspace's adapters and is not a stable third-party contract. Shared limits for now: H.264 and HEVC only (AV1 profiles are added here with the first adapter that encodes AV1), system-memory input only (native surfaces with the `rbs-gpu` exporters, ADR-0025), no B-frames.

- Probe result: `DeviceCaps` lists, per codec (H.264, HEVC), the size range, macroblocks per second (0 when unknown), the profiles (`HevcMain10` also means P010 input), `cbr`, `vbr`, constant quality and bitrate changes. A codec without a profile or with an empty size range is not listed.
- Declared capabilities (`capability_set`): `adapter.hardware = true`; `codec.video.encode` the listed codecs; `codec.video.profiles` the union of their profiles; `codec.video.bit_depth` 8, plus 10 with P010 input where a codec has Main 10; width and height the range every listed codec supports; `codec.video.fps.max` the slowest reporting codec's macroblock rate at the largest common size (absent when no codec reports one); a rate-control mode, and `bitrate` in `codec.reconfigure`, only when every listed codec has it; `keyframe` always; `codec.video.b_frames.max = 0`; `input.memory = {system}`; `input.pixel_format` NV12, plus P010 where a codec has Main 10.
- Configuration (`plan`): refuses as `Unsupported` codecs the device does not list, input memory other than system, pixel formats other than NV12 (or P010 with 10-bit HEVC), odd sizes and sizes out of range, profiles the device does not list, rate-control modes the device does not list, and colours without H.273 code points; as `InvalidConfig` zero or above-32-bit bitrates, a peak below the target, quality levels above 51 and a zero frame rate. The default profile is H.264 High (else Main, else Baseline, by what the device lists) and HEVC Main or Main 10 by pixel format. The GOP is the keyframe interval in frames, P frames only. Constant quality `level` (0 best ..= 51) is on the codec's QP scale; each adapter maps it to its own quality parameter.
- Sessions (`Session`): the engine has a fixed number of slots (`Engine::slots`, the frames in flight, read once at creation). The session uploads into, encodes and reads slots strictly in submission order and never touches a slot again before its picture was taken; an engine whose driver keeps its own queue (AMF, QSV) may use the slot for its input buffer only and return the next picture of that queue, and the session's token check ends the session if that picture is not the expected one. `submit` refuses frames that are not system memory or whose `VideoFormat` differs from the session's, and timestamps that do not increase (`InvalidInput`); returns `Busy` when every slot is in flight; copies the frame into the next slot's input (counted in `SessionStats::copied`, so a frame refused with `Busy` and submitted again counts twice) and queues it, forcing an IDR with parameter sets for the first frame, after `RequestKeyframe` and after any `flush`. A frame the engine refuses with `Busy` is not consumed and keeps a pending keyframe pending. `drain` reads finished pictures in submission order without waiting and stops at the first unfinished one; with every slot in flight it waits for the oldest so that `submit` has room again. `flush` signals end-of-stream and waits for every queued picture; the session goes on afterwards. A picture out of submission order, a missing or empty picture, and any failed read other than "not finished yet" end the session unrecoverably; `DeviceLost` ends it with that error. A bitrate change must keep the rate-control mode, is refused (`Unsupported`) for constant quality or when the device has no bitrate changes, and leaves the old rate in place when the device refuses the new one.
- Output: Annex B, packet `pts = dts` = the frame timestamp, keyframe = IDR picture, one exact-size `Vec` per packet.
- The conformance suite runs against `Session` over a fake engine on every CI runner, so the contract is checked without hardware; each adapter also runs it against its real factory, which skips without the hardware unless `RBS_REQUIRE_<VENDOR>=1`.

### NVENC encoder (`rbs-codec-nvenc`)

- One adapter, `org.rbs.codec.nvenc` (`adapter.hardware = true`), on 64-bit Windows and Linux, built on the shared hardware session above. The driver libraries are opened at run time (`nvEncodeAPI64.dll` and `nvcuda.dll` from the Windows system directory only, `libnvidia-encode.so.1` and `libcuda.so.1` on Linux); bindings follow NVIDIA Video Codec SDK 12.1, so the driver must report API 12.1 or newer (driver 531+). A missing library or symbol, an older driver, no CUDA device or a device without NVENC probes to an empty set. The probe opens a CUDA context and an NVENC session on CUDA device 0 and caches a definitive result for the process (the first probe can exceed the 50 ms budget; later ones are free); a GPU without NVENC (`NV_ENC_ERR_NO_ENCODE_DEVICE`, unsupported device) is a definitive empty set, while no free session or no context is a `ProbeError` that is not cached, so the next probe tries again, and `create` meanwhile returns `ResourceExhausted`. Other platforms compile a factory that probes empty and refuses sessions.
- Declared from the device: `codec.video.encode` = `h264` and/or `hevc` (each needs NV12 input support), profiles `h264.baseline/main/high` (default high) and `hevc.main` plus `hevc.main10` with P010 input where `NV_ENC_CAPS_SUPPORT_10BIT_ENCODE` is set (P010 means Main10, NV12 means Main); width and height the range every declared codec supports (`NV_ENC_CAPS_WIDTH/HEIGHT_MIN/MAX`, documented minimums when the driver does not report them), even sides only; `codec.video.fps.max` from `NV_ENC_CAPS_MB_PER_SEC_MAX` at the largest size; `cbr`, and `vbr` plus `constant_quality` where the device reports those modes; `codec.video.b_frames.max = 0`; `codec.reconfigure = {keyframe}` plus `bitrate` where `NV_ENC_CAPS_SUPPORT_DYN_BITRATE_CHANGE` holds for every codec; `input.memory = {system}`. `codec.sessions.max` is not declared (NVENC does not report the GeForce session limit); a session refused for it (`NV_ENC_ERR_OUT_OF_MEMORY` or `NV_ENC_ERR_INCOMPATIBLE_CLIENT_KEY`) is `CreateError::ResourceExhausted`.
- Mapping: presets `Speed` P2, `Balanced` P4, `Quality` P5; tuning `LOW_LATENCY` when `low_latency`, else `HIGH_QUALITY`. CBR sets average = peak = bitrate, VBR average and peak, both with a one-second VBV at the peak; constant quality is VBR with no bitrate target and `targetQuality = level` (level 0 becomes 1; NVENC reads 0 as automatic). `gopLength = idrPeriod` = the keyframe interval in frames; `frameIntervalP = 1` and look-ahead off, so every frame produces one picture in order and none is dropped. The VUI signals H.273 primaries, transfer, matrix and range from `VideoFormat::color` (`rbs_media_format::h273`); combinations without code points (the `Rgb` matrix) are refused at `create`. A bitrate change must keep the rate-control mode and goes through `nvEncReconfigureEncoder` without an IDR; constant quality changes, and a new rate the device refuses (`NV_ENC_ERR_INVALID_PARAM`/`UNSUPPORTED_PARAM`), are `Unsupported` and the old rate stays.
- Sessions: one CUDA context per session (`CU_CTX_SCHED_BLOCKING_SYNC`) and one NVENC session in synchronous mode (Linux has no asynchronous mode), with 8 NVENC-allocated input buffers and 8 bitstream buffers used as a ring in submission order. Every NVENC call pushes the session's context on the calling thread and pops it afterwards, so a session may move between threads. `submit` refuses frames that are not system memory or whose `VideoFormat` differs from the session's, and timestamps that do not increase (`InvalidInput`), returns `Busy` when all 8 buffers are in flight, copies the planes row by row into the next input buffer (counted) and queues it, forcing an IDR with parameter sets for the first frame and after `RequestKeyframe`. A frame NVENC refuses with a recoverable error is not consumed and keeps a pending keyframe pending. `drain` locks finished bitstreams in submission order with `doNotWait` and stops at the first unfinished one; when all 8 are in flight it waits for the oldest, which was submitted 8 frames earlier, so `submit` has room again. `flush` sends end-of-stream and waits for every queued picture; the session goes on afterwards and the next frame is an IDR. A picture out of submission order (reordering), `NV_ENC_ERR_NEED_MORE_INPUT`, an empty picture or any failed bitstream lock (other than `NV_ENC_ERR_LOCK_BUSY` without waiting) ends the session unrecoverably; `NV_ENC_ERR_ENCODER_BUSY` from `nvEncEncodePicture` is a recoverable `Busy` and the frame is not consumed; `NV_ENC_ERR_DEVICE_NOT_EXIST` and a failed context push are `DeviceLost`. `Drop` sends end-of-stream, reads queued pictures back oldest first (NVENC requires bitstreams to be locked in submission order), then destroys the buffers, the session and the context.
- Output: Annex B, packet `pts = dts` = the frame timestamp, keyframe = IDR picture, one exact-size `Vec` per packet copied from the locked bitstream. Parameter sets (SPS/PPS, plus VPS for HEVC) are repeated in band on every IDR (`repeatSPSPPS`) and are the extradata, as Annex B, from `nvEncGetSequenceParams` at `create`.
- Not yet: native input (D3D11/D3D12 textures on Windows, CUDA-imported Vulkan or DMA-BUF images on Linux) once `rbs-gpu` exports them (ADR-0025), AV1, B-frames, choosing a GPU other than CUDA device 0, asynchronous mode with completion events on Windows.

### QSV encoder (`rbs-codec-qsv`)

- One adapter, `org.rbs.codec.qsv` (`adapter.hardware = true`), on x86-64 Windows and Linux, built on the shared hardware session above (4 slots). The oneVPL dispatcher is opened at run time: `libvpl.dll` from the application or system directory on Windows (Intel drivers install the runtime but not the dispatcher, so applications ship it), `libvpl.so.2` on Linux together with `libva.so.2` and `libva-drm.so.2`. Bindings follow oneVPL API 2.13 (libvpl headers, MIT). A missing library or symbol, no hardware implementation (`MFXEnumImplementations` finds none) or no render node probes to an empty set and is cached; a session, display or filter that fails now is a `ProbeError` that is not cached, and `create` meanwhile returns `ResourceExhausted`. Other platforms compile a factory that probes empty and refuses sessions.
- Device: the dispatcher's first implementation with `mfxImplDescription.Impl = MFX_IMPL_TYPE_HARDWARE`. On Linux the session gets a VA display (`MFX_HANDLE_VA_DISPLAY`) on the render node from `MFX_IMPLCAPS_DEVICE_ID_EXTENDED` (`/dev/dri/renderD128` when the runtime does not report one); on Windows the runtime creates its own device for system-memory input.
- Declared from `MFXVideoENCODE_Query` with test configurations: a profile when the runtime accepts it unchanged at 1280x720 CBR (H.264 Constrained Baseline, Main, High; HEVC Main, and Main 10 with P010); the largest accepted of 8192x8192, 8192x4320, 4096x4096, 4096x2304, 3840x2160, 1920x1080 and the smallest accepted of 32x32, 64x64, 128x128, 256x144 as the size range; `cbr`, `vbr` and `constant_quality` (ICQ) where accepted; `bitrate` in `codec.reconfigure`. `codec.video.fps.max` is not declared (oneVPL does not report throughput).
- Mapping: `TargetUsage` 2 (`Quality`), 4 (`Balanced`), 7 (`Speed`); `low_latency` is not mapped yet. Closed, strict GOP of `GopPicSize` = the keyframe interval (capped at 65535), `GopRefDist = 1`, one reference, every I picture an IDR (`IdrInterval` 0 for H.264, 1 for HEVC). CBR and VBR in kbit/s rounded up with a one-second buffer at the peak (initial delay half of it), scaled by `BRCParamMultiplier` so every field fits 16 bits; constant quality is ICQ with `ICQQuality = level` (0 becomes 1). Surfaces are aligned to 16 (H.264) or 32 (HEVC) with the picture as the crop; P010 is described with 10-bit depth and `Shift = 1`. The VUI signals H.273 colour codes through `mfxExtVideoSignalInfo`. A bitrate change waits for queued pictures, then calls `MFXVideoENCODE_Reset` with `StartNewSequence = OFF` (no IDR); a refusal (`INVALID`/`INCOMPATIBLE_VIDEO_PARAM`, `UNSUPPORTED`) is `Unsupported` and the old rate stays.
- Sessions: `MFXVideoENCODE_Init` with `AsyncDepth = 4` and system-memory input; one adapter-allocated surface (64-byte pitch) and one bitstream per slot, the bitstream sized from `BufferSizeInKB` and at least one raw frame. `submit` copies into the slot's surface (`Busy` while the runtime still has it locked) and calls `EncodeFrameAsync` with the token as `TimeStamp` and, for a forced IDR, an `mfxEncodeCtrl` of type I, reference, IDR. `MFX_WRN_DEVICE_BUSY` is a recoverable `Busy`; `MFX_ERR_MORE_DATA` (a buffered frame) and a missing sync point end the session; `DEVICE_LOST`, `DEVICE_FAILED` and `GPU_HANG` are `DeviceLost`. `drain` polls sync points with a zero timeout; waits use 10 s, after which the session ends. End-of-stream is a no-op (the encoder holds no frames back). `Drop` waits for queued pictures, closes the encoder, the session, the VA display and the loader in that order.
- Output: Annex B; keyframe = `MFX_FRAMETYPE_IDR` in the bitstream; the extradata is VPS (HEVC), SPS and PPS from `MFXVideoENCODE_GetVideoParam` (`mfxExtCodingOptionSPSPPS`, `mfxExtCodingOptionVPS`), with start codes added where the runtime omits them.
- Not yet run on hardware: `RBS_REQUIRE_QSV=1 cargo test -p rbs-codec-qsv` and `cargo bench -p rbs-codec-qsv --bench encode` on an Intel GPU (Windows with `libvpl.dll` next to the test binary, or Linux with the `libvpl2`, `libmfx-gen1.2` and `intel-media-va-driver-non-free` packages).

## `rbs-mux`

```rust
pub struct MuxerConfig {
    pub container: ContainerId,        // mp4.fragmented, matroska, mpegts, flv
    pub streams: Vec<StreamDescriptor>,
    pub fragment_duration: Option<Duration>, // fMP4 fragment / Matroska cluster target
}

pub trait MuxerFactory: AdapterFactory {
    fn create(&self, config: &MuxerConfig) -> Result<Box<dyn Muxer>, CreateError>;
}

pub trait Muxer: Send {
    /// Write the container header.
    fn begin(&mut self, out: &mut Vec<MuxChunk>) -> Result<(), MuxError>;
    /// hot. Add one packet of stream `stream`.
    fn write(&mut self, stream: StreamIndex, packet: &EncodedPacket, out: &mut Vec<MuxChunk>) -> Result<(), MuxError>;
    /// Write pending data and the trailer, plus patches for seekable sinks.
    fn finish(&mut self, out: &mut Vec<MuxChunk>) -> Result<(), MuxError>;
}

// From rbs_core::media (media types spec):
// MuxChunk { kind: ChunkKind, data: Bytes }
// ChunkKind::{Header, Media { keyframe, timestamp }, Trailer, Patch { offset }}
```

Invariants:

1. `begin` emits one or more `Header` chunks and nothing else; no `Header` follows a `Media` chunk; `finish` emits held-back `Media`, then `Trailer` and `Patch` chunks only.
2. Interleaving: a muxer may buffer to interleave streams, but media with dts `t` is emitted by the time a packet with dts `t + max_buffered` is written (`max_buffered` is `fragment_duration`, or 1 s when unset). The suite checks the observable consequence: the newest `Media` chunk never starts more than `2 x max_buffered` plus one packet interval before the packet being written.
3. A `Media` chunk with `keyframe = true` starts at a point where a reader can begin decoding every stream of the output (fMP4 fragment start, TS packet with random access indicator, FLV video keyframe tag; in an audio-only output, any chunk boundary).
4. The byte stream of `Header` + `Media` chunks alone is a valid, playable file when the muxer declares `mux.live`. Muxers for fragmented MP4, Matroska, MPEG-TS and FLV must declare it (ADR-0019), so recordings survive a crash.
5. Packets of codecs the container does not support are refused at `create` with `CreateError::Unsupported`.
6. A non-monotonic `dts` within a stream returns `MuxError::NonMonotonic` (recoverable: the packet is dropped).

Capability keys: `mux.container` (Set of `ContainerId`), `mux.codecs` (Set of codecs it can carry), `mux.live` (Flag: playable without trailer), `mux.patches` (Flag: emits patch chunks).

## `rbs-transport`

```rust
pub struct TransportConfig {
    pub url: String,                    // rtmp://..., srt://..., file:///...
    pub secret: Option<Secret>,         // stream key or passphrase; redacted in Debug and logs
    pub input: TransportInput,          // what this session will receive
    pub queue: QueueLimits,             // max bytes and max duration buffered
    pub reconnect: ReconnectPolicy,     // off, or back-off parameters
    pub adapter_options: Vec<(String, String)>,
}

pub enum TransportInput { Packets, Container(ContainerId) }

pub trait TransportFactory: AdapterFactory {
    fn create(&self, config: &TransportConfig) -> Result<Box<dyn Transport>, CreateError>;
}

pub trait Transport: Send {
    /// Begin delivery: open the file or start connecting in the background.
    /// Returns without waiting for the network.
    fn start(&mut self, streams: &[StreamDescriptor]) -> Result<(), TransportError>;
    /// hot. Queue one item. Never blocks. When the queue is full the adapter
    /// applies the drop policy and reports what it dropped.
    fn send(&mut self, item: TransportItem<'_>) -> Result<SendOutcome, TransportError>;
    /// Move pending events (connected, reconnecting, stats, errors) to `out`.
    fn poll_events(&mut self, out: &mut Vec<TransportEvent>);
    /// Current counters; a cheap snapshot that never waits on the I/O side.
    fn stats(&self) -> TransportStats;
    /// Deliver what is queued, close cleanly, and stop owned threads.
    /// May block up to `timeout`; never called from a hot path.
    fn finish(&mut self, timeout: Duration) -> Result<(), TransportError>;
}

pub enum TransportItem<'a> {
    Packet { stream: StreamIndex, packet: &'a EncodedPacket },
    Chunk(&'a MuxChunk),
}

pub enum SendOutcome {
    Queued,                                            // queued, nothing dropped
    QueuedAfterDrops { dropped: u32, reason: DropReason }, // queued after dropping older items
    Dropped { reason: DropReason },                    // this item was not queued
}

pub enum TransportEvent {
    Connected,
    Reconnecting { attempt: u32 },
    Disconnected { error: TransportError },
    KeyframeRequested,                                 // peer feedback (PLI/FIR, new viewer)
    BitrateHint { bps: u64 },                          // congestion control suggestion
    Dropped { items: u32, reason: DropReason },        // once per burst
}
```

Threading: `send` runs on the output worker; delivery runs on a thread or runtime the session owns (started in `start`, stopped in `finish` or `Drop`). The recommended shape, which every adapter must match in behaviour:

- A **staging queue** owned by the producer side (the session object, touched only in `send`, `poll_events` and `finish`) holds queued items as shared `Bytes` plus metadata (size, timestamp, keyframe, GOP number). All drop decisions are made here, so dropping "the oldest" items needs no synchronisation.
- A short bounded **single-producer single-consumer hand-off** (a few items, at most about 100 ms of media) carries items from staging to the I/O side. Items in the hand-off are committed and are no longer dropped.
- `send` and `poll_events` move items from staging into the hand-off while it has room. The byte and duration limits apply to staging plus hand-off, so `QueueLimits` bounds memory even when the I/O side is stalled. The I/O side reports progress (bytes sent, connection state) through atomics only (P6).
- Transports that never drop (files) make no drop decisions, so they may skip staging: the whole queue is the hand-off, bounded by `QueueLimits` and the hand-off size, and a full queue returns `QueueFull`.

Drop policy for network transports, when a queue limit is reached:

1. Video, packet input: drop whole GOPs from the oldest, keyframe included, while a later keyframe is staged. When only the current GOP is staged, drop its non-keyframes and refuse the rest of that GOP (`SendOutcome::Dropped`) until the next keyframe arrives. The stream therefore never continues from a missing reference picture. When a GOP is cut the transport also emits `KeyframeRequested`, so the encoder can shorten the gap.
2. Audio, packet input: dropped oldest first, only when dropping video did not free enough.
3. Container input (chunks): a transport never parses the container. It applies rule 1 at chunk granularity, using the `keyframe` flag of `Media` chunks as the GOP boundary (`keyframe` means an entry point for every stream of the output, [media types](media-types.md#containers-rbs_coremediacontainer)). Audio interleaved in the dropped chunks goes with them.
4. Headers (`ChunkKind::Header`, codec configuration) are never dropped.
5. Each drop is counted in `TransportStats::items_dropped` and reported once per burst as `TransportEvent::Dropped`.

Transports that must not drop (files) return the recoverable `TransportError::QueueFull` when their queue is full; nothing was queued, and the output worker keeps the item in its own bounded buffer and retries. If that buffer also overflows, the recording is stopped with a reported error rather than written with holes. File I/O never runs on the calling thread.

Feedback: `KeyframeRequested` is forwarded to the encoder as `EncoderChange::RequestKeyframe`; when the encoder feeds several outputs, requests are rate-limited (at most one per second). `BitrateHint` is applied to the encoder only when this output is its only adaptive consumer; otherwise the output builder gives adaptive outputs their own encoder, or the hint is shown in diagnostics.

Invariants:

1. `send` before `start` returns `TransportError::NotStarted`; an item of the wrong kind for `input` returns `TransportError::InvalidInput`.
2. `send` returns in bounded time regardless of network or disk state (checked by the suite against a destination that accepts nothing).
3. Byte-exact delivery for container input: a loopback destination receives exactly the `Header` and `Media` bytes in order (and applies `Patch` chunks when seekable).
4. After `finish` or `Drop`, no thread, socket or file handle owned by the session is left. One exception: a thread blocked inside an OS call that cannot be interrupted (a write to a hung network drive or a stalled pipe) is detached when the `finish` timeout or the `Drop` grace period runs out, so the caller is never blocked; it releases its handles as soon as that call returns. `finish(timeout)` never blocks longer than `timeout`.
5. Reconnect: with a policy set, a lost connection produces `Reconnecting` and, after recovery, resumes at the next keyframe with headers resent. Packet transports rebuild their headers from the stream descriptors given to `start`; container transports keep the `Header` chunks they received, resend them first, and resume at the next `Media` chunk marked `keyframe`, discarding staged chunks before it.
6. The secret never appears in `Debug` output, events or errors, including in failure and stall paths.
7. After a drop, delivered video resumes at a keyframe and headers are intact (checked by adapter tests that can read the destination; see Conformance tests).

Capability keys: `transport.schemes` (Set: `rtmp`, `rtmps`, `srt`, `file`, ...), `transport.input` (Set: `packets`, `container.mpegts`, ...), `transport.codecs` (Set, for `packets` input), `transport.reconnect` (Flag), `transport.bandwidth_estimate` (Flag), `transport.seekable` (Flag: applies patch chunks).

### File transport (`rbs-transport-file`)

- URLs (RFC 8089): `file:///absolute/path` and `file://localhost/absolute/path`; on Windows also `file:///C:/path`, `file://C:\path` and `file://host/share/path` (a UNC path). Percent-escapes are decoded. Relative paths, malformed escapes and (outside Windows) remote hosts are `CreateError::InvalidConfig`. Adapter options are refused (there are none); a secret and a reconnect policy are ignored.
- An existing file is truncated when the session starts. (A "never overwrite" option is a candidate for a later change.)
- `Patch` chunks are applied by seeking back; a patch that reaches past the bytes written so far, or any patch on a target that cannot seek (a pipe or device), fails the recording with `TransportError::Io`. Only regular files are synced to disk at `finish`.
- The duration limit counts from the last chunk written (or the first queued, before any write) to the chunk being sent, one chunk interval stricter than counting from the oldest queued chunk.
- A write error is reported once as `Disconnected`, returned once by `send`, and later `send` calls return `Closed`. `stats()` keeps its final counters after `finish`.

### RTMP transport (`rbs-transport-rtmp`)

- Input `packets`; codecs H.264 and AAC (at most one of each, checked in `start` by `FlvTracks::new`; anything else is `InvalidInput`). Capabilities: `transport.schemes = {rtmp}`, `transport.input = {packets}`, `transport.codecs = {h264, aac}`, `transport.reconnect`.
- URL `rtmp://host[:port]/app` (port 1935 by default, IPv6 in brackets). The application is the whole path without the outer slashes, query included; `tcUrl` is `rtmp://authority/app`. User info in the URL is refused, the stream key must be in the secret (an empty or missing key is `CreateError::InvalidConfig`), and adapter options are refused (there are none). Queue limits of 0 bytes or 0 seconds and a back-off starting at 0 are `CreateError::InvalidConfig`. `rtmps` is not supported yet.
- Publish sequence on every connection: the simple handshake (C0/C1, S0/S1/S2, C2 echoes S1; version 3), Set Chunk Size 4096, `connect` (`app`, `type: nonprivate`, `flashVer`, `tcUrl`, `objectEncoding: 0`), `releaseStream` and `FCPublish` with the key, `createStream`, then `publish(key, "live")` on the created stream, and success is `onStatus NetStream.Publish.Start`. Set-up has 5 s per TCP connect and 10 s from connect to publish start. Replies to `releaseStream` and `FCPublish`, and unknown commands, are ignored.
- Data: before the first packet of each connection, `@setDataFrame onMetaData` (from `FlvTracks::metadata_body`) and the AVC and AAC sequence headers, stamped with that packet's time. Each connection starts at a video keyframe; queued packets before it are discarded (counted in `items_dropped`). Message bodies are `FlvTracks::write_body` output, built on the I/O thread from the queued packet, so `send` copies no payload.
- Timestamps: 32-bit milliseconds of decode time from one origin per session (the first packet's dts minus 1 s, so streams that arrive later with earlier times stay non-negative; earlier packets are `InvalidInput`), wrapping at 2^32 as RTMP defines, and kept across reconnects. The composition offset is pts minus dts in the same rounded clock. Every message has a type 0 header; from 0xFFFFFF up the extended timestamp field is used and repeated after every type 3 header.
- Chunk streams: 2 protocol control, 3 commands, 4 audio, 5 data, 6 video.
- Socket: writes block without a timeout (a timed-out send leaves a Windows socket in an undefined state); an abort shuts the socket down through a clone registered while connected, which ends any blocked read or write. Set-up reads use 50 ms timeouts to meet their deadline.
- Server messages: the I/O thread reads them when idle and, while sending, between messages at least every 100 ms (a long write of one message delays this), at most 4 reads per poll so a flooding server cannot starve sending. Set Chunk Size, Abort, Window Acknowledgement Size and pings (answered with a ping response) are handled; Set Peer Bandwidth is answered with Window Acknowledgement Size; acknowledgements are sent per the server's window. Any message larger than 64 KiB is a protocol error from its chunk header, before the body is stored or decoded, and at most 16 chunk streams are tracked. Commands are AMF0 (`objectEncoding: 0`; an AMF3 command message's leading format byte is skipped); a command whose name decodes but whose other values do not (AMF0 references, AMF3 values) is ignored. An `onStatus` with level `error`, or an `_error` to `connect`, `createStream` or `publish`, is `TransportError::Rejected` and is not retried; server text in errors has the stream key (and its part before a `?`, which nginx-rtmp echoes) replaced and is cut to 256 bytes.
- Queue: staging (drop rules 1, 2, 4 and 5 for packet input) plus a hand-off of at most 8 items (about 100 ms at 30 fps with AAC). The byte limit counts the FLV body sizes of staged and committed packets; the duration limit runs from the oldest staged or committed packet's dts. A keyframe that cannot be queued cuts its GOP like a dropped inter frame. `KeyframeRequested` is raised when a GOP is cut and when a connection is lost.
- Reconnect (`ReconnectPolicy::Backoff`): connection errors are retried with doubling delays from `initial` up to `max`, also for the first connection; `Reconnecting { attempt }` is reported per attempt and `Connected` after each successful publish; `attempt` restarts at 1 after a connection that reached publish, so a server that accepts the publish and then drops the connection is retried without end even with an attempt limit. `TransportStats::reconnects` counts successful re-publishes. With the policy `Off`, or once attempts run out, the error is reported once as `Disconnected`, returned once by `send`, and later `send` calls return `Closed`.
- `finish(timeout)` hands what is staged to the I/O thread and lets it send everything, then sends `FCUnpublish` and `deleteStream`, shuts down its sending side and waits up to 2 s for the server to close (so unread server data cannot turn the close into a reset that discards sent data). A failed connection is retried during `finish` only while packets are still queued; otherwise `finish` returns the connection error, since the last packet's delivery is unknown. Packets lost with a failed connection, or left when the I/O thread has ended, count in `items_dropped`. Like the file transport it spends four fifths of `timeout` on a clean end, then aborts, and never blocks longer than `timeout`; `Drop` aborts with a 200 ms grace. A thread inside DNS resolution or `connect` past that is detached (invariant 4).
- Not yet: `rtmps` (TLS), enhanced RTMP (HEVC, AV1, Opus), AMF3 replies, the digest handshake, bandwidth estimation.

### SRT transport (`rbs-transport-srt`)

- Input `container.mpegts`: the chunks of an MPEG-TS muxer, never parsed. Capabilities: `transport.schemes = {srt}`, `transport.input = {container.mpegts}`, `transport.reconnect`. Caller mode and live mode only; pure Rust on `std` (no libsrt), checked against libsrt 1.5 (FFmpeg as the listener).
- URL `srt://host:port` (the port is required, an IPv6 address must be in brackets, a trailing `/` allowed). User info, paths and queries are refused: the stream id goes in the secret, and the one adapter option is `latency_ms` (0 to 65535, default 120), the TSBPD delay the sender asks for. Any other option, a stream id longer than 512 bytes, queue limits of 0 bytes or 0 seconds and a back-off starting at 0 are refused at `create`. The secret is optional (listeners that take any stream need none).
- Handshake (version 5, caller): the induction (version 4, extension field `UDT_DGRAM`, destination socket id 0) is sent every 250 ms until the listener answers with version 5 and the SRT magic `0x4A17` (anything else is `Rejected`: version 4 listeners are not supported); the conclusion echoes the listener's cookie and carries the HSREQ extension (version 1.5.0, flags TSBPD send and receive, crypto support, too-late drop, NAK reports, retransmission flag; both delays `latency_ms`) and, with a secret, the SID extension (the stream id padded to whole 32-bit words, each word's bytes reversed as libsrt writes them). The listener's conclusion gives its socket id, flow window and, in HSRSP, the latency it applies. A handshake type from 1000 up (below the special values from `0xFFFFFFFD`), in answer to the induction or the conclusion, is a rejection: libsrt sends 1000 plus the reason, so a listener's "bad stream id" (reason 1403) arrives as 2403. It is reported as `Rejected` with the reason and its meaning (libsrt's reasons below 1000, access-control reasons 1000 to 1999, the listener application's own from 2000) and not retried. Set-up gives up after 3 s. Socket ids are below 2^30; the initial sequence number is random.
- Data: each chunk is cut into payloads of at most 1316 bytes (seven TS packets); a packet never spans two chunks, so a chunk's last packet may be short. Every packet is one message (`PP = 11`, not in order, not encrypted), message numbers count from 1 and sequence numbers from the initial one, both wrapping as SRT defines. The timestamp of a packet is the time `send` took its chunk, in microseconds from the connection's start, never going back (header chunks sent again on a new connection take the current time). Payloads are slices of the queued chunk, so no payload is copied before the datagram is written.
- Reliability: sent packets are kept until an ACK covers them, for at most the listener's latency (at least 1 s) plus 20 ms after their timestamp (libsrt's sender drop threshold; the receiver would discard them as too late). A NAK resends the kept packets it lists with the retransmission flag set, at most 32 per look at the socket; for packets no longer kept it sends a DROPREQ for that range (message number 0), and reports for packets already acknowledged or never sent are ignored. At most 256 loss ranges are remembered. A receiver cannot report the loss of the last packets of a burst, so when no ACK advances for a retransmission timeout (RTT + 4 x RTT variance + 20 ms, from the ACKs; 100 ms and 50 ms before the first) and no loss is reported, every unacknowledged packet is queued for retransmission (libsrt's late retransmission); the timeout doubles, up to 16 times, until an ACK advances. Full and small ACKs are answered with an ACKACK; their RTT is reported in `TransportStats::rtt` and their free-buffer count bounds the packets in flight together with the listener's flow window (capped at 8192; one packet may always be in flight). A keep-alive goes out after 1 s without sending; 5 s without hearing from the listener, or its shutdown, loses the connection.
- Socket: one connected UDP socket per connection, owned by the I/O thread. Sends block only for room in the local socket buffer, never for the peer, so no send timeout and no shutdown are needed; every receive waits at most 5 ms (so `stop` and `abort` are seen within 5 ms, and an idle thread takes a newly queued chunk within 5 ms). While there is work, the thread sends up to 32 packets and then reads what arrived without waiting, at most 16 datagrams, so a flooding peer cannot starve sending. ICMP errors reported on the socket are ignored (silence is caught by the timeouts), and so is a send that finds the interface queue full (`ENOBUFS` on BSD, macOS and Windows; the packet is kept and a loss report recovers it), and on Windows a received datagram larger than the 2048-byte buffer. Timestamps are compared in wrapping order, so they keep advancing when they wrap after 71.6 minutes. An abort sends a best-effort shutdown, so the listener does not wait for its idle timeout.
- Queue: staging (drop rules 1, 3, 4 and 5 for container input: a `Media` chunk marked `keyframe` is the GOP boundary, `Header` and `Trailer` chunks are never dropped or refused, and a keyframe chunk that cannot be queued cuts its GOP) plus a hand-off of at most 8 chunks. The byte limit counts the chunk sizes; the duration limit runs from the oldest staged or committed `Media` chunk's timestamp (headers and trailers have none). `Patch` chunks are accepted and ignored: a live stream cannot be rewritten, and the output stays playable without them. `KeyframeRequested` is raised when a GOP is cut and when a connection is lost.
- Reconnect (`ReconnectPolicy::Backoff`): as the RTMP transport (doubling delays, also for the first connection; `Reconnecting { attempt }` per attempt, `Connected` after each handshake, `attempt` restarting at 1 after a connection that completed its handshake; `TransportStats::reconnects` counts successful reconnections). Every connection sends the `Header` chunks received so far first, then resumes at the next keyframe chunk; queued `Media` chunks before it are discarded (counted in `items_dropped`), as is a `Media` or `Trailer` chunk cut by the lost connection (a cut `Header` chunk is not lost: it goes first on the next connection). A keyframe chunk that would not fit even with all staged media dropped is refused without dropping the staged GOPs. With the policy `Off`, or once attempts run out, the error is reported once as `Disconnected`, returned once by `send`, and later `send` calls return `Closed`.
- `finish(timeout)` hands what is staged to the I/O thread, which sends it, waits until every packet is acknowledged (or expired), then for the listener's latency plus 20 ms after the last packet's timestamp (libsrt discards what it still holds back for its latency once the peer closes), and sends a shutdown; so with a latency near `timeout` a clean `finish` cannot complete in time and returns `Timeout`. A failed connection is retried during `finish` only while chunks are still queued; otherwise `finish` returns the connection error (also when it ends a reconnect handshake in progress). A first handshake still in progress when `finish` finds nothing queued is abandoned and `finish` returns `Ok`. Like the other transports it spends four fifths of `timeout` on a clean end, then aborts, and never blocks longer than `timeout`; `Drop` aborts with a 200 ms grace. A thread inside DNS resolution past that is detached (invariant 4).
- Not yet: encryption (passphrase, key material exchange; a listener that requires it rejects the handshake), listener and rendezvous modes, packet filters (FEC), congestion control beyond the flow window (no pacing; packets go out as chunks arrive), bandwidth estimation, other containers.

## `rbs-media-format` (format helpers)

Pure functions and immutable set-up values shared by muxer and transport adapters (ADR-0019, decision 7). No traits, no mutable session state, no I/O, no threads; bitstream errors are `FormatError::{Truncated, Invalid, Unsupported}`.

- `nal`: `annexb_units` and `length_prefixed_units` iterators (borrowing, never allocate), `annexb_first_header` (reads only up to the first start code), `annexb_to_length_prefixed` and `length_prefixed_to_annexb` (append to a caller-owned `Vec`, so a reused buffer stops allocating once it reaches the working size), `h264_type` and the `h264` NAL type constants.
- `avc`: `AvcConfig` parses and builds `AVCDecoderConfigurationRecord` (avcC), and builds one from the SPS and PPS of an Annex B access unit. Session set-up only; not for hot paths.
- `aac`: `AudioSpecificConfig` (parse, encode, AAC-LC constructor, sample rate) and the 7-byte ADTS header.
- `amf0`: AMF0 writers (number, boolean, string with long-string fallback, null, object, ECMA array) appending to a caller-owned `Vec`, and `decode`/`decode_all` into an owned `Decoded` tree (numbers, booleans, strings, objects, ECMA and strict arrays, dates, null, undefined) with nesting bounded at 16; XML documents decode as strings, typed objects as objects (class name dropped), the unsupported marker as undefined, and references and the AMF3 switch are refused (RTMP clients ask for `objectEncoding: 0`). Decoding allocates, never reserves from a count read off the wire, and is for size-bounded control messages (RTMP command replies) only.
- `flv`: tag header, `PreviousTagSize` and whole-tag writers, `millis_since`, and `FlvTracks`. `FlvTracks::new` checks the stream descriptors once (at most one H.264 and one AAC stream; Annex B H.264 needs SPS and PPS in the extradata) and fails with `ConfigError::{Unsupported, Invalid}`. It then gives the `onMetaData` body, the AVC and AAC sequence header bodies, and per packet `plan` (validates, returns the exact body size, `PacketError` otherwise) and `write_body` (appends the body: access unit delimiters left out, Annex B re-framed with 4-byte lengths, composition offset clamped to signed 24 bits). `plan` and `write_body` never allocate beyond the caller's buffer. The FLV muxer wraps these bodies in tags; RTMP sends them as message bodies. The caller owns the clock origin and timestamps.
- Planned: HEVC (hvcC), enhanced-RTMP bodies, Opus headers, as adapters need them.

Hot functions (iterators and conversions) are linear in the payload and benchmarked through the adapters that call them.

## Errors and failure behaviour

Each port has its own `CreateError`. All have `Unsupported { what }` and `InvalidConfig(String)`; the encoder's also has `ResourceExhausted` (for example no free hardware session; recoverable, the engine tries the next candidate) and `Device(String)`. Muxer and transport creation never touches devices or the network, so their creation errors are never recoverable.

`EncodeError`, `MuxError`, `TransportError` each implement `is_recoverable()`. Recoverable: retry, reconfigure or reconnect. Unrecoverable: the output is stopped and the engine falls back to the next candidate adapter (encoders) or reports the output as failed (muxers, transports). A failing output never stops other outputs fed by the same encoder (P7).

## Performance requirements

- `submit` / `drain` / `write` / `send` are hot: no blocking I/O, at most one payload allocation per produced packet or chunk (see Common rules). Encoder adapters keep pooled output buffers.
- Packet fan-out to N outputs costs N reference-count increments; a payload copy happens only where a consumer needs a different bitstream format, and is counted per output.
- Budgets per frame are in [performance budgets](performance-budgets.md); adapter PRs on these paths follow P15.

## Conformance tests

Each port crate exports suites as library functions, run by adapter crates in `tests/conformance.rs`. They are behind each port crate's `conformance` cargo feature, which adapter crates enable in `[dev-dependencies]`, and share `rbs_core::conformance` (behind `rbs-core`'s `conformance` feature: `Report`, `Case`, `Outcome`, the `probe` case with a best-of-three 50 ms budget, panic containment). None of this is compiled into runtime builds.

- `rbs_codec::conformance::video_encoder(&factory, &mut frames)` and `audio_encoder(&factory)`: probe time and no panic; for every declared codec and rate-control mode, create, encode 45 frames at 30 fps with a 15-frame keyframe interval, a keyframe request and a bitrate change, and check invariants 1-7 (invariant 7 for a wrong size and a wrong pixel format; a wrong memory kind is checked by adapters that have a second frame source); audio checks 1 s of 48 kHz stereo against invariant 8 (pts within 1 us). The caller provides a `FrameSource` for its declared input memory (`SystemFrameSource` covers system memory); undeclared or unavailable memory kinds are skipped and listed.
- `rbs_mux::conformance::muxer(&factory)`: for every declared container, mux 3 s of synthetic H.264 and AAC packets (those the adapter declares) and check invariants 1-6, including that every keyframe chunk is an entry point (`is_entry_point`: FLV video keyframe tag, `moof`/`styp`, Matroska Cluster, TS packet with random access indicator). When `mux.live` is declared, the header and media bytes alone must pass the suite's minimal reader for the container (`read_structure`: TS sync, PAT first, PMT present and continuity counters; FLV tag walk; fMP4 box walk with `ftyp`, `moov`, `moof` + `mdat`; Matroska element walk with unknown-size Segment and Cluster).
- `rbs_transport::conformance::transport(&factory, &mut destination)`: the suite's stream descriptors are valid H.264 (Annex B, SPS and PPS in the extradata) and AAC (raw, AudioSpecificConfig), so packet transports can build their headers. The caller supplies a `Destination` (`TempFileDestination` for files; a local SRT or RTMP listener for network adapters). For every declared input it checks invariants 1, 2 (each `send` under a generous 250 ms ceiling against a stalled destination with a 1 MiB queue and 1.9 MiB GOPs, so a blocking `send` is caught without flaking; drops counted and reported; the current GOP must be cut, and once a non-keyframe is refused every later non-keyframe of that GOP must be refused too), 3 (byte-exact for container input, patches applied when `transport.seekable`) and 6 (healthy and stalled sessions). File destinations cannot stall, so file adapters prove invariant 2 in their own tests with a slow writer. Packet framing (RTMP, WHIP), drop recovery (7), reconnect (5) and handle leaks (4) are checked by adapter tests until the suite has protocol readers and a portable leak probe.

Each suite returns a report listing passed, failed and skipped cases. `Report::assert_passed` fails on any failed case; adapters that must work on CI machines (software encoders, muxers, file transports) use `Report::assert_ran(&[..])` with their core case names, so they cannot pass by skipping everything.

## Versioning

The three port crates and the container types in `rbs-core` are Draft and pre-1.0. Traits are object-safe and are the hardest part to change: adding a trait method is a breaking change for adapters and follows semver of the port crate (ADR-0003). Configuration, outcome, event and error types are `#[non_exhaustive]` with constructors, so adding a struct field, an enum variant, a capability key or a chunk kind (for example HLS segment boundaries or caption data) is a minor change. Fields of struct-like enum variants (`ChunkKind::Media`, `RateControl::Cbr`, `SendOutcome::QueuedAfterDrops`, ...) are fixed: adding one is a major change, so new data goes into a new variant instead. The suite support types `rbs_core::conformance::{Report, Case, Outcome}` are `#[non_exhaustive]` too; suite case names may change in minor versions, and adapters that use `assert_ran` update them when they upgrade. Removing or renaming anything, or changing a capability value's meaning, is a major change.

## Open questions

- Allocation checks for hot calls: `rbs-core` has an allocation guard for tests; wiring it into the suites (it needs a counting global allocator in the adapter's test binary) is a follow-up, then the suites check the per-packet allocation rule.
- Encoder delay: AAC and Opus encoders prime with silence. Adapters hide it in packet timestamps today, which shifts audio by the delay (about 21 ms for AAC-LC at 48 kHz) unless muxers can signal it (MP4 edit list, Matroska `CodecDelay`, Opus pre-skip). A `StreamDescriptor` field for the delay is proposed for the first audio encoder adapter.
- `TransportError::QueueFull` retry: there is no "space available" signal yet; the output worker retries on its next packet or `poll_events` tick. An event may be added if measurements show it matters.
- `Secret` is not zeroized on drop; a zeroizing type can replace it when a dependency for that is justified.
- WHIP/WebRTC: codec and profile are negotiated in SDP, possibly after the encoder exists. The first WHIP adapter decides whether the transport offers only what `start` received, or the output builder negotiates before creating the encoder.
- Multi-pass or look-ahead tuning beyond `EncoderTuning` is left to `adapter_options` until a second adapter needs it.
- HLS/DASH segmenting: a muxer that emits segment boundaries, or a transport that segments; decided when the first HLS adapter is written.
