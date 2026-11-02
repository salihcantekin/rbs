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
| `Cadence` | Timestamps of a regular series: canvas ticks, audio blocks, samples | Item `n` is at `origin + floor(n * den * 1e9 / num)` ns, computed from `n`, so rounding never accumulates; `time_of(index_at_or_before(t)) <= t < time_of(index_at_or_before(t) + 1)` for rates up to 1e9 per second and times below the saturation point near `u64::MAX` ns (about 584 years) |
| `Timebase` | Container or protocol tick (`1/90000`, `1/1000`) | `ticks_in` and `duration_of` round to nearest; a round trip is within half a tick |
| `ClockMapping` | Source-clock ns to master time: `master = anchor + (source - anchor_source) * rate` | Rate clamped to `0.9..=1.1` |
| `DriftEstimator` | Least-squares fit of `(source, master)` observations into a `ClockMapping` | Observations are averaged into buckets (default 1 s of source time), window of 64 buckets; fixed size, no allocation; a source time that goes backwards restarts the estimate and returns `Observation::Restarted` |

Sources convert their own clock to master time at the edge with `ClockMapping`, fed by a `DriftEstimator` (ADR-0004). A new mapping is re-anchored at the newest fitted point, so a source swaps mappings at most once per bucket and keeps its output timestamps monotonic itself (`max(previous + 1 ns, mapped)`).

Encoders and muxers convert master time to container ticks with `Timebase::ticks_since(stream_origin, t)`. Packet durations in ticks are differences of rounded positions, not `ticks_in(duration)`, so they add up exactly.

### Video (`rbs_core::media::video`)

- `VideoFormat { size, pixel_format, color }`: the picture format of a frame.
- `PixelFormat` (non-exhaustive): `Bgra8`, `Bgra8Srgb`, `Rgba8`, `Rgba8Srgb`, `Rgb10a2`, `Rgba16Float` (canvas working format), `Nv12`, `P010` (encoder hand-off), `I420`, `I444`, `Yuy2`, `Uyvy`. Each format reports plane count, bit depth, minimum row bytes and rows per plane; chroma sizes round up for odd dimensions.
- `ColorSpace { primaries, transfer, matrix, range }`, with constants `SRGB`, `LINEAR_BT709` (SDR working space), `LINEAR_BT2020` (HDR working space), `BT709_LIMITED`, `BT709_FULL`, `BT2100_PQ`, `BT2100_HLG`.
- `MemoryKind` (non-exhaustive): `System`, `Gpu` (engine device texture), `D3d11`, `D3d12`, `Vulkan`, `Metal`, `DmaBuf`. `as_str()` gives the values used for the `input.memory` style capabilities.
- `VideoFrame` has private fields and accessors (`format`, `timestamp`, `sequence`, `source_time`, `payload`, `lease`), so the format always matches the payload and a native surface is never separated from its lease:

| Accessor | Meaning |
| --- | --- |
| `format()` | `VideoFormat` |
| `timestamp()` | Presentation time in master-clock time |
| `sequence()` | Per-producer counter, +1 per emitted frame; gaps are drops |
| `source_time()` | Producer-domain ns, diagnostics only, never used for timing |
| `payload()` | Where the pixels are (below) |
| `lease()` | The `Lease` keeping the memory alive, if any |

- `FramePayload` (non-exhaustive), each built by its own constructor:
  - `VideoFrame::texture(format, t, TextureId, Option<Lease>)`: a texture in an `rbs-gpu` pool. `TextureId::new(slot, generation)` is `Copy`; `rbs-gpu` resolves it in O(1) and rejects stale generations. The default path.
  - `unsafe VideoFrame::native(format, t, NativeSurface, SurfaceSync, Lease)`: a producer-owned platform surface (D3D11 texture, D3D shared handle, D3D12 resource, IOSurface, Metal texture, DMA-BUF planes with fourcc and modifier) stored as raw integer values. The lease is mandatory. See [Native surfaces](#native-surfaces).
  - `VideoFrame::system(t, SystemFrame, Option<Lease>)`: CPU memory, one `Bytes` buffer plus `PlaneLayout { offset, stride }` per plane. `SystemFrame::new(format, data, planes)` checks plane count, stride and that every row fits, and keeps the format it checked; the frame takes its format from it.
- `VideoFrame::share()` returns a second reference with its own lease reference for texture and system payloads, and `None` for native payloads.
- `VideoFormat`, `ColorSpace`, `NativeSurface` variants, `SurfaceSync` variants, `StreamDescriptor`, `StreamParams` variants, `EncodedPacket` and `AudioFormat` are `#[non_exhaustive]` and have constructors, so fields such as HDR metadata, a visible rectangle, a discontinuity flag or side data can be added in a minor version.

### Native surfaces

- `VideoFrame::native` is `unsafe`: its caller guarantees every handle refers to a live object of the described kind and format, on the adapter it names (`adapter_luid` for Direct3D, `device_registry_id` for Metal), valid and not written by the producer until the lease is released.
- A native frame has exactly one consumer: the `rbs-gpu` importer in the source node wrapper. It is not shareable (`share()` returns `None`); the importer takes it apart with `into_native()`.
- The importer waits on `SurfaceSync` (`Ready`, `KeyedMutex`, `TimelineFence`, `SyncFile`) before its GPU work. A keyed mutex is therefore acquired and released by one consumer only.
- An import either **copies** the surface into a pool texture, and drops the native lease only after that copy has completed on the GPU (its own fence); or **aliases** it (wraps the surface zero-copy as a pool texture), and then moves the native lease into the resulting `Texture` frame's pool slot, so the producer's surface lives as long as that slot is referenced by any frame or queued GPU work. Either way the producer never reuses a surface the GPU may still read.
- The `rbs-gpu` import API takes the `VideoFrame` by value. Any function that takes the separate `(NativeSurface, SurfaceSync, Lease)` parts is `unsafe`, because a `Copy` surface could otherwise be paired with an unrelated lease.
- Native payloads never travel on graph links: the source node wrapper imports them before the frame enters the graph.
- Downstream nodes, encoders and outputs only ever see `Texture` or `System` payloads. An encoder that needs a native texture gets it from `rbs-gpu` export, not from a capture surface.
- Surface kinds without a producer yet (Vulkan images, CUDA memory) are not defined; they are added as new non-exhaustive variants with full import information when a producer needs them.

### Lifetime

- A frame or block passed by reference to a call is lent for that call only (plugin SDK rule 3).
- A `Lease` is `Arc<dyn LeaseOwner>` plus a `u64` token: `clone` calls `owner.retain(token)`, `drop` calls `owner.release(token)`. The memory returns to its owner when the last reference is dropped. Sharing a frame costs two atomic operations (the `Arc` count and the owner's slot count) and one virtual call.
- `LeaseOwner` is an `unsafe trait`: implementers guarantee that a slot stays valid while its count is above zero and that counts are exact. `Lease::adopt` is `unsafe`: the caller hands over exactly one counted reference.
- `retain` and `release` run on real-time threads: no allocation, blocking, contended locks, logging or panics (a panic while unwinding aborts the process). Tokens should carry a generation in their upper bits so a stale release can be detected and ignored.
- The engine keeps its own `Arc` to every owner until the producer has been shut down on a background thread, so the last `Arc` (and the owner's destructor) is never dropped on a real-time thread.

### Plugin boundaries

These Rust types are the in-process contract between engine crates and adapters. They do not cross the plugin boundaries of ADR-0006 as they are: they have no fixed layout, and leases, `Bytes` and raw handles are process-local. The plugin SDK defines mirrors:

- **Native tier:** `#[repr(C)]` mirrors of the format types and a frame descriptor; leases become a C vtable with `retain` / `release` and a token.
- **Sandbox (WASM) tier:** WIT records for formats; frames and blocks are lent for the duration of a call and pinned by the host, so no lease crosses the boundary; GPU work is host-run WGSL.
- **Process tier:** the host duplicates NT handles or file descriptors into the plugin process and pins the frame host-side until the plugin acknowledges it.

The plugin SDK spec owns these mirrors; this spec only requires that every field here has a mirror or a documented reason not to.

### Audio (`rbs_core::media::audio`)

- Inside the engine, samples are `f32`. Device formats are converted at the edge by audio adapters.
- `AudioFormat { sample_rate, channels: ChannelLayout }`; at most `MAX_CHANNELS = 8` channels; default graph block `DEFAULT_BLOCK_FRAMES = 480` at 48 kHz (10 ms).
- `AudioBlock<'a>`: a borrowed view with `format`, `timestamp` (master time of the first sample frame), `frames`, and `AudioSamples::Planar(PlanarChannels)` (one slice per channel, the graph's layout) or `AudioSamples::Interleaved`. `PlanarChannels` hides its fixed array, so `MAX_CHANNELS` can grow without breaking users. Constructors check channel count and lengths and never allocate.
- Owned, queued audio storage (ring buffers, pools) belongs to `rbs-audio`.

### Packets (`rbs_core::media::packet`)

- `CodecId` (non-exhaustive): `H264`, `Hevc`, `Av1`, `Vp9`, `Aac`, `Opus`; `as_str()` gives capability values.
- `BitstreamFormat` (non-exhaustive): `AnnexB`, `LengthPrefixed { length_size }`, `Obu`, `Raw`, `Adts`.
- `StreamDescriptor { codec, bitstream, extradata: Bytes, params }`: produced by an encoder, consumed by muxers and transports. `params` is `StreamParams::Video { format, frame_rate }` or `StreamParams::Audio { format, frame_size }`.
- `EncodedPacket { codec, bitstream, pts, dts, duration, keyframe, data: Bytes }`, built with `EncodedPacket::new`, which returns `MediaError::DtsAfterPts` when `dts > pts`, and `with_duration`. Fields are read-only through accessors. `pts` and `dts` are master-clock times with `dts <= pts`.
- Payloads are `bytes::Bytes` (re-exported as `rbs_core::media::Bytes`): O(1) clone and zero-copy `slice`, so one encode fans out to many outputs without copying.
- Creating a payload costs one allocation per packet at the producer (encoder drain): either the `Vec` it is built from, or the shared header of `Bytes::from_owner` when the buffer comes from a pool (requires `bytes` 1.9 or later). The first clone of a `Bytes` built from a `Vec` also allocates its shared header once, so producers share a payload when they create it. After that, handing the same packet to several muxers and transports never allocates; what those consumers produce (muxer chunks, a converted bitstream for a container that needs another framing) costs its own payload allocation per chunk or packet, see [output ports](output-ports.md#common-rules).

### Containers (`rbs_core::media::container`)

Shared by the mux and transport ports ([output ports](output-ports.md), ADR-0019), which may depend only on `rbs-core`.

- `ContainerId` (non-exhaustive): `Mp4Fragmented`, `Matroska`, `MpegTs`, `Flv`; `as_str()` gives `mux.container` values (`mp4.fragmented`, `matroska`, `mpegts`, `flv`) and `transport_input()` gives `transport.input` values (`container.<name>`).
- `StreamIndex(u16)`: position of a stream in the descriptor list of one output.
- `MuxChunk { kind: ChunkKind, data: Bytes }` with `ChunkKind::Header`, `Media { keyframe, timestamp }`, `Trailer` or `Patch { offset }`. `keyframe` means an entry point for every stream of the output: a reader starting at this chunk can decode all of them (video from a keyframe; in an audio-only output every chunk can be one). Patches overwrite earlier bytes and are applied only by seekable transports; `is_sequential()` is false for them.

## Versioning

`rbs-core` is pre-1.0; these types are Draft. Adding variants or fields to the non-exhaustive types above is a minor change. `bytes::Bytes` is part of the public API, so a `bytes` 2.0 would be a breaking change for `rbs-core`.

## Errors and failure behaviour

Constructors that validate return `MediaError` (non-exhaustive, `Copy`, no allocation). Timing arithmetic saturates instead of failing. Nothing in this spec panics.

## Performance requirements

Everything in `rbs_core::timing` and `rbs_core::media` may be called per frame, per block or per packet and is listed in [hot paths](../architecture/hot-paths.md):

- No heap allocation in any method. Payload creation at the producer is the one documented exception (see Packets).
- `VideoFrame` stays at most 192 bytes so moving it through queues stays cheap (tested).
- Benchmarks: `cargo bench -p rbs-core --bench timing`.
- Allocation guard: `crates/rbs-core/tests/no_alloc.rs` (dev-dependency `allocation-counter`, test builds only) asserts zero allocations for timing, frame clone/drop with a lease, audio block construction, `SystemFrame` validation and packet fan-out.

## Conformance tests

Unit tests in `rbs-core` check: cadence consistency over random times and rates and no drift over long runs; timebase round trips; drift estimation within 2e-5 of the true rate through 0.5 ms read jitter; plane geometry for odd sizes; `SystemFrame` validation; lease retain/release balance; zero-copy packet fan-out. Port conformance suites must additionally check that adapters fill `timestamp` in master time, keep `sequence` increasing, and release every lease.

## Open questions

- Whether HDR metadata (mastering display, content light level) belongs in `VideoFormat` or in `StreamDescriptor` only.
- Whether audio needs an owned, pooled block type in core for cross-thread hand-off, or whether `rbs-audio` ring buffers are enough.
- Vulkan and CUDA surface variants, with the full import information (memory handle, layout, queue family, context, plane offsets), once a producer needs them.
- Whether `bytes` stays in the public API or is wrapped in a newtype before the plugin SDK 1.0 freeze.
