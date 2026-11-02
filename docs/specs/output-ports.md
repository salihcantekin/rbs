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
- Bitstream conversion: containers and protocols disagree on framing (FLV, fMP4 and Matroska carry length-prefixed H.264/HEVC with parameter sets in extradata; MPEG-TS carries Annex B with parameter sets repeated before each IDR). The muxer or packet transport that needs a different framing converts it; this is one counted copy per packet for that output only, measured like any other hot-path work. Conversion code is shared through the `rbs-media-format` helper crate (planned; depends only on `rbs-core`, see the layer table in the [overview](../architecture/overview.md#layers)). The output builder prefers the encoder bitstream format that most of its consumers take.
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
3. Packet `pts` values are the input timestamps (no invented times): every video packet's `pts` equals the timestamp of one submitted frame, and each submitted frame yields exactly one packet by the end of `flush`.
4. `dts` is non-decreasing across packets and `dts <= pts`.
5. After `RequestKeyframe`, a keyframe is produced within `max_b_frames + 1` frames, or the call returned `Unsupported`.
6. `Bitrate` changes either succeed or return `Unsupported`; the adapter declares which in `codec.reconfigure`.
7. Frames whose format, size or memory kind differs from the config are refused with `EncodeError::InvalidInput`, not encoded wrongly.
8. Audio packets' `pts` advance by exactly `frame_size` samples (converted to master-clock time with `SampleRate::duration_of` from the timeline origin) and start at the first submitted block's timestamp. The adapter keeps its own sample-count timeline: a block whose timestamp differs from the expected continuation by less than one codec frame is treated as continuous (drift is the audio graph's job, ADR-0004); a larger gap or overlap is a discontinuity, and the adapter pads the partial frame with silence, emits it, and restarts the timeline at the new block's timestamp.

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
4. After `finish` or `Drop`, no thread, socket or file handle owned by the session is left.
5. Reconnect: with a policy set, a lost connection produces `Reconnecting` and, after recovery, resumes at the next keyframe with headers resent. Packet transports rebuild their headers from the stream descriptors given to `start`; container transports keep the `Header` chunks they received, resend them first, and resume at the next `Media` chunk marked `keyframe`, discarding staged chunks before it.
6. The secret never appears in `Debug` output, events or errors, including in failure and stall paths.
7. After a drop, delivered video resumes at a keyframe and headers are intact (checked by adapter tests that can read the destination; see Conformance tests).

Capability keys: `transport.schemes` (Set: `rtmp`, `rtmps`, `srt`, `file`, ...), `transport.input` (Set: `packets`, `container.mpegts`, ...), `transport.codecs` (Set, for `packets` input), `transport.reconnect` (Flag), `transport.bandwidth_estimate` (Flag), `transport.seekable` (Flag: applies patch chunks).

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
- `rbs_transport::conformance::transport(&factory, &mut destination)`: the caller supplies a `Destination` (`TempFileDestination` for files; a local SRT or RTMP listener for network adapters). For every declared input it checks invariants 1, 2 (each `send` under a generous 250 ms ceiling against a stalled destination with a 1 MiB queue and 1.9 MiB GOPs, so a blocking `send` is caught without flaking; drops counted and reported; the current GOP must be cut, and once a non-keyframe is refused every later non-keyframe of that GOP must be refused too), 3 (byte-exact for container input, patches applied when `transport.seekable`) and 6 (healthy and stalled sessions). File destinations cannot stall, so file adapters prove invariant 2 in their own tests with a slow writer. Packet framing (RTMP, WHIP), drop recovery (7), reconnect (5) and handle leaks (4) are checked by adapter tests until the suite has protocol readers and a portable leak probe.

Each suite returns a report listing passed, failed and skipped cases. `Report::assert_passed` fails on any failed case; adapters that must work on CI machines (software encoders, muxers, file transports) use `Report::assert_ran(&[..])` with their core case names, so they cannot pass by skipping everything.

## Versioning

The three port crates and the container types in `rbs-core` are Draft and pre-1.0. Traits are object-safe and are the hardest part to change: adding a trait method is a breaking change for adapters and follows semver of the port crate (ADR-0003). Configuration, outcome, event and error types are `#[non_exhaustive]` with constructors, so adding a struct field, an enum variant, a capability key or a chunk kind (for example HLS segment boundaries or caption data) is a minor change. Fields of struct-like enum variants (`ChunkKind::Media`, `RateControl::Cbr`, `SendOutcome::QueuedAfterDrops`, ...) are fixed: adding one is a major change, so new data goes into a new variant instead. The suite support types `rbs_core::conformance::{Report, Case, Outcome}` are `#[non_exhaustive]` too; suite case names may change in minor versions, and adapters that use `assert_ran` update them when they upgrade. Removing or renaming anything, or changing a capability value's meaning, is a major change.

## Open questions

- Allocation checks for hot calls: `rbs-core` has an allocation guard for tests; wiring it into the suites (it needs a counting global allocator in the adapter's test binary) is a follow-up, then the suites check the per-packet allocation rule.
- Encoder delay: AAC and Opus encoders prime with silence. Adapters hide it in packet timestamps today, which shifts audio by the delay (about 21 ms for AAC-LC at 48 kHz) unless muxers can signal it (MP4 edit list, Matroska `CodecDelay`, Opus pre-skip). A `StreamDescriptor` field for the delay is proposed for the first audio encoder adapter.
- `TransportError::QueueFull` retry: there is no "space available" signal yet; the output worker retries on its next packet or `poll_events` tick. An event may be added if measurements show it matters.
- `Secret` is not zeroized on drop; a zeroizing type can replace it when a dependency for that is justified.
- WHIP/WebRTC: codec and profile are negotiated in SDP, possibly after the encoder exists. The first WHIP adapter decides whether the transport offers only what `start` received, or the output builder negotiates before creating the encoder.
- Hardware encoders that take native surfaces (D3D11, Vulkan, IOSurface, DMA-BUF) receive `FramePayload::Texture` ids today and need the `rbs-gpu` export path to get native handles; whether encoder adapters may depend on `rbs-gpu` or receive exported handles through a port-level type is decided with the GPU interop work.
- Multi-pass or look-ahead tuning beyond `EncoderTuning` is left to `adapter_options` until a second adapter needs it.
- HLS/DASH segmenting: a muxer that emits segment boundaries, or a transport that segments; decided when the first HLS adapter is written.
