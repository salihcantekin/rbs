# Spec: Media types and timing

- Status: Draft
- Owner crate(s): `rbs-core` (`rbs_core::media`, `rbs_core::timing`, `rbs_core::time`)
- Related ADRs: [0004](../adr/0004-single-master-clock.md), [0005](../adr/0005-gpu-compositing-with-wgpu.md), [0013](../adr/0013-zero-cost-when-unused.md), [0018](../adr/0018-media-frame-representation.md)

## Purpose

The data that flows between nodes, ports and adapters: video frames, audio blocks, encoded packets, their formats, and the timing arithmetic around them. Every port crate (capture, audio I/O, codec, mux, transport), `rbs-gpu` and `rbs-graph` uses these types, so they live in the foundation crate and name no vendor library (principle P1).

## Scope

In scope: value types, their invariants, lifetime of frame memory, exact rate and timebase arithmetic, clock drift estimation.

Out of scope: how `rbs-gpu` stores and imports textures (its own spec), port traits (each port's spec), link format negotiation (media graph spec), audio resampling.

## Contract

### Time and rates (`rbs_core::time`, `rbs_core::timing`)

| Type | Meaning | Invariants |
| --- | --- | --- |
| `Timestamp` | Master-clock time in ns since the clock origin (ADR-0004) | Arithmetic saturates; never wraps |
| `Rational` | Positive exact ratio `num/den` (`u32`) | Both parts non-zero |
| `FrameRate` | Video frames per second, exact (`60000/1001`) | Non-zero |
| `SampleRate` | Audio sample frames per second | Non-zero |
| `Cadence` | Timestamps of a regular series: canvas ticks, audio blocks, samples | Item `n` is at `origin + floor(n * den * 1e9 / num)` ns, computed from `n`, so rounding never accumulates; `time_of(index_at_or_before(t)) <= t < time_of(index_at_or_before(t) + 1)` for rates up to 1e9 per second |
| `Timebase` | Container or protocol tick (`1/90000`, `1/1000`) | `ticks_in` and `duration_of` round to nearest; a round trip is within half a tick |
| `ClockMapping` | Source-clock ns to master time: `master = anchor + (source - anchor_source) * rate` | Rate clamped to `0.9..=1.1` |
| `DriftEstimator` | Least-squares fit of `(source, master)` observations into a `ClockMapping` | Observations are averaged into buckets (default 1 s of source time), window of 64 buckets; fixed size, no allocation |

Sources convert their own clock to master time at the edge with `ClockMapping`, fed by a `DriftEstimator` (ADR-0004). Encoders and muxers convert master time to container ticks with `Timebase::ticks_since(stream_origin, t)`.

### Video (`rbs_core::media::video`)

- `VideoFormat { size, pixel_format, color }`: the picture format of a frame.
- `PixelFormat` (non-exhaustive): `Bgra8`, `Bgra8Srgb`, `Rgba8`, `Rgba8Srgb`, `Rgb10a2`, `Rgba16Float` (canvas working format), `Nv12`, `P010` (encoder hand-off), `I420`, `I444`, `Yuy2`, `Uyvy`. Each format reports plane count, bit depth, minimum row bytes and rows per plane; chroma sizes round up for odd dimensions.
- `ColorSpace { primaries, transfer, matrix, range }`, with constants `SRGB`, `LINEAR_BT709` (SDR working space), `LINEAR_BT2020` (HDR working space), `BT709_LIMITED`, `BT709_FULL`, `BT2100_PQ`, `BT2100_HLG`.
- `MemoryKind` (non-exhaustive): `System`, `Gpu` (engine device texture), `D3d11`, `D3d12`, `Vulkan`, `Metal`, `DmaBuf`, `Cuda`. `as_str()` gives the values used for the `input.memory` style capabilities.
- `VideoFrame` (non-exhaustive struct with public fields, built with `VideoFrame::new` and `with_*`):

| Field | Meaning |
| --- | --- |
| `format` | `VideoFormat` |
| `timestamp` | Presentation time in master-clock time |
| `sequence` | Per-producer counter, +1 per emitted frame; gaps are drops |
| `source_time` | Producer-domain ns, diagnostics only, never used for timing |
| `payload` | Where the pixels are (below) |
| `lease` | `Option<Lease>` keeping the memory alive (below) |

- `FramePayload` (non-exhaustive):
  - `Texture(TextureId)`: a texture in an `rbs-gpu` pool. `TextureId { slot: u32, generation: u32 }` is `Copy`; `rbs-gpu` resolves it in O(1) and rejects stale generations. The default path.
  - `Native { surface: NativeSurface, sync: SurfaceSync }`: a producer-owned platform surface (D3D11 texture pointer, D3D shared handle, D3D12 resource, IOSurface, Metal texture, Vulkan image, DMA-BUF planes with fourcc and modifier, CUDA memory), stored as raw integer values. Core never dereferences them. `SurfaceSync` says how to wait for the producer: `Ready`, `KeyedMutex`, `TimelineFence`, `SyncFile`.
  - `System(SystemFrame)`: CPU memory, one `Bytes` buffer plus `PlaneLayout { offset, stride }` per plane. `SystemFrame::new` checks plane count, stride and that every row fits in the buffer.

### Lifetime

- A frame or block passed by reference to a call is lent for that call only (plugin SDK rule 3).
- To keep a frame (for example in an encoder queue), clone it. A `Lease` is `Arc<dyn LeaseOwner>` plus a `u64` token: `clone` calls `owner.retain(token)`, `drop` calls `owner.release(token)`. The memory returns to its owner when the last clone is dropped.
- `LeaseOwner::retain` and `release` run on real-time threads: no allocation, blocking, contended locks, logging or panics. Creating a lease (`Lease::adopt`) does not allocate when the owner clones an `Arc` it already holds.
- Raw handles in `NativeSurface` are valid exactly as long as the frame's lease lives.

### Audio (`rbs_core::media::audio`)

- Inside the engine, samples are `f32`. Device formats are converted at the edge by audio adapters.
- `AudioFormat { sample_rate, channels: ChannelLayout }`; at most `MAX_CHANNELS = 8` channels; default graph block `DEFAULT_BLOCK_FRAMES = 480` at 48 kHz (10 ms).
- `AudioBlock<'a>`: a borrowed view with `format`, `timestamp` (master time of the first sample frame), `frames`, and `AudioSamples::Planar` (one slice per channel, the graph's layout) or `AudioSamples::Interleaved`. Constructors check channel count and lengths and never allocate.
- Owned, queued audio storage (ring buffers, pools) belongs to `rbs-audio`.

### Packets (`rbs_core::media::packet`)

- `CodecId` (non-exhaustive): `H264`, `Hevc`, `Av1`, `Vp9`, `Aac`, `Opus`; `as_str()` gives capability values.
- `BitstreamFormat` (non-exhaustive): `AnnexB`, `LengthPrefixed { length_size }`, `Obu`, `Raw`, `Adts`.
- `StreamDescriptor { codec, bitstream, extradata: Bytes, params }`: produced by an encoder, consumed by muxers and transports. `params` is `StreamParams::Video { format, frame_rate }` or `StreamParams::Audio { format, frame_size }`.
- `EncodedPacket { codec, bitstream, pts, dts, duration, keyframe, data: Bytes }`. `pts` and `dts` are master-clock times with `dts <= pts`.
- Payloads are `bytes::Bytes` (re-exported as `rbs_core::media::Bytes`): O(1) clone and zero-copy `slice`, so one encode fans out to many outputs without copying.
- The first clone of a `Bytes` built from a `Vec` allocates its shared header once (later clones do not). Producers therefore share a payload when they create it (clone once, or hand out buffers from a pool that are already shared), so the fan-out path itself never allocates.

## Errors and failure behaviour

Constructors that validate return `MediaError` (non-exhaustive, `Copy`, no allocation). Timing arithmetic saturates instead of failing. Nothing in this spec panics.

## Performance requirements

Everything in `rbs_core::timing` and `rbs_core::media` may be called per frame, per block or per packet and is listed in [hot paths](../architecture/hot-paths.md):

- No heap allocation in any method, except `SystemFrame` and `StreamDescriptor` construction where the caller supplies the buffer.
- `VideoFrame` stays at most 192 bytes so moving it through queues stays cheap (tested).
- Benchmarks: `cargo bench -p rbs-core --bench timing`.
- Allocation guard: `crates/rbs-core/tests/no_alloc.rs` (dev-dependency `allocation-counter`, test builds only) asserts zero allocations for timing, frame clone/drop with a lease, audio block construction, `SystemFrame` validation and packet fan-out.

## Conformance tests

Unit tests in `rbs-core` check: cadence consistency over random times and rates and no drift over long runs; timebase round trips; drift estimation within 2e-5 of the true rate through 0.5 ms read jitter; plane geometry for odd sizes; `SystemFrame` validation; lease retain/release balance; zero-copy packet fan-out. Port conformance suites must additionally check that adapters fill `timestamp` in master time, keep `sequence` increasing, and release every lease.

## Open questions

- Whether HDR metadata (mastering display, content light level) belongs in `VideoFormat` or in `StreamDescriptor` only.
- Whether audio needs an owned, pooled block type in core for cross-thread hand-off, or whether `rbs-audio` ring buffers are enough.
- Vulkan external memory (fd / Win32 handle) as an extra `NativeSurface` variant once a producer needs it.
