# Spec: Audio graph

- Status: Draft
- Owner crate(s): `rbs-audio`; audio values and the audio queue in `rbs-graph`; stream pacing in `rbs-engine`
- Related ADRs: [0004](../adr/0004-single-master-clock.md), [0013](../adr/0013-zero-cost-when-unused.md), [0020](../adr/0020-node-contract-and-schedule-swap.md), [0023](../adr/0023-node-parameter-channels.md), [0026](../adr/0026-partitions-and-queues.md), [0033](../adr/0033-audio-input-port.md), [0037](../adr/0037-audio-graph.md)
- Related specs: [media-graph.md](media-graph.md), [engine.md](engine.md), [audio-io-port.md](audio-io-port.md), [media-types.md](media-types.md), [output-ports.md](output-ports.md)

## Purpose

Turns audio from input streams into mixed, master-clock-timed blocks for audio encoders: alignment and drift correction per source, rate and channel conversion, gain and mute, level meters for the UI. It runs as nodes in an audio partition of the engine (ADR-0037).

## Scope

In scope: the graph format and block timing, the audio partition's pacing, audio values on links and across partitions, the input node, the resampler and channel matrix, the mixer, level meters, the audio encoder node.

Out of scope (later): buses other than `main`, audio filters, audio output and monitoring, the scene model's audio items and the binder that creates these nodes, mux and transport nodes, adaptive latency.

## Contract

### Graph format, time and pacing (`rbs_audio::AudioGraphConfig`)

```rust
#[non_exhaustive]
pub struct AudioGraphConfig {
    pub format: AudioFormat,     // default AudioFormat::STEREO_48K
    pub block_frames: u32,       // default DEFAULT_BLOCK_FRAMES (480), 1..=MAX_BLOCK_FRAMES (8192)
    pub latency: Duration,       // default DEFAULT_LATENCY (50 ms), MIN_LATENCY (10 ms)..=MAX_LATENCY (1 s)
}
impl AudioGraphConfig {
    pub fn new(format: AudioFormat, block_frames: u32, latency: Duration) -> Result<Self, AudioGraphError>;
    pub fn cadence(&self, origin: Timestamp) -> Cadence;   // sample_rate / block_frames per second
    pub fn period(&self) -> Duration;                      // one block
    pub fn block_time(&self, tick: Tick) -> Option<Timestamp>; // tick.time - latency; None before latency
}
```

Invariants (tests: `config.rs` unit tests, `tests/pipeline.rs`):

1. The audio partition runs on `Pacing::stream(config.cadence(origin), catch_up)` ([engine spec](engine.md), invariant 2): every due tick in order while at most `catch_up` ticks late (a few blocks; 10 is a good start). Block `i` has timestamp `block_time(Tick::of(cadence, i))`; consecutive blocks differ by exactly `block_frames` at the graph rate (to the nanosecond, from the index), and ticks before `latency` produce no block.
2. Every audio link inside an audio partition carries blocks of exactly `format` and `block_frames` frames, timestamped with `block_time` of the tick.
3. Every controller whose partition has audio pins (the audio partition, and an encoder partition with an audio queue reader) is set to the graph's block size with `Controller::set_audio_block_frames(block_frames)`.

### Audio values on links (`rbs_graph::runtime`)

| Call | Contract |
| --- | --- |
| `Controller::set_audio_block_frames(frames)` | Buffer size of each audio output pin in schedules built from now on, frames per channel (clamped to `1..=MAX_BLOCK_FRAMES`; default `DEFAULT_BLOCK_FRAMES`). Allocated on the control thread when the schedule is built |
| `Outputs::audio(pin)` | `Result<AudioOut<'_>, OutputError>`; `WrongKind` for a pin of another kind |
| `AudioOut::capacity()` / `channel_mut(c)` / `channels_mut(n)` | The buffer's frames per channel; channel `c < MAX_CHANNELS` (else `None`); the first `min(n, MAX_CHANNELS)` channels in order |
| `AudioOut::commit(format, timestamp, frames)` | Publishes the first `frames` of each of the format's channels as this tick's block. `frames > capacity` is `OutputError::AudioTooLarge`; `write` of a sample layout it cannot copy is `OutputError::UnsupportedSamples`. `frames == 0` publishes nothing. Without a commit nothing is published |
| `AudioOut::write(block)` | Copies a planar or interleaved block and commits it |
| `Inputs::audio(pin, link)` / `audios(pin)` | The planar `AudioBlock` the producer committed this tick, or `None` (consumers treat it as silence) |

Audio slots are events: emptied before their producer runs each tick and after a failed tick; nothing is carried across a swap. None of these calls allocates. Tests: `crates/rbs-graph/tests/audio.rs`.

### Audio queue (`rbs_graph::runtime::queue::audio`)

```rust
pub const AUDIO_QUEUE_BLOCKS: usize = 50;
pub const AUDIO_FILL_LIMIT: Duration = Duration::from_secs(1);
pub fn audio(blocks: usize, frames: u32, doorbell: Arc<Doorbell>) -> (AudioWriter, AudioReader, Arc<QueueStats>);
```

- `blocks` pre-allocated buffers of `MAX_CHANNELS x frames` samples move between a free and a full `sync_channel`. Nothing allocates or frees per block on either side.
- The writer copies each block of its input into a free buffer, sends it (`sent`) and rings `doorbell`. With no free buffer, or a block longer than `frames`, the block is lost and counted (`overflows`). With the reader gone it keeps the buffer.
- The reader passes one block per tick (format and timestamp kept, `received`) and rings its own partition's doorbell while blocks are queued (a shared count, so a released and re-joined reader with an empty queue does not ring). When a block's timestamp is more than half a block after the end of the previous one, and at most `AUDIO_FILL_LIMIT` after it, it first passes on silent blocks at the missing times, one per tick (`filled`), so the consumer's timeline has no gap. `release` empties the queue (`skipped`).
- The lowering turns an `Audio` link between partitions into such a queue (`QueueEnds::Audio`), with the consumer partition's doorbell and `Lowering::set_audio_queue(blocks, frames)` (defaults `AUDIO_QUEUE_BLOCKS`, `DEFAULT_BLOCK_FRAMES`).

### Input node (`rbs_audio::InputNode`)

```rust
impl InputNode {
    pub fn new(stream: Box<dyn AudioInputStream>, config: AudioGraphConfig, max_offset: Duration)
        -> (Self, ParamSender<InputParams>, Arc<InputStats>);
}
#[non_exhaustive] pub struct InputParams { pub offset_ns: i64 }   // NONE, delayed(d), advanced(d)
impl InputStats { pub fn read(&self) -> InputCounters; }
// InputCounters { frames_in, resyncs, late_frames, discarded_frames, gaps_filled,
//                 overlaps_dropped, read_errors, correction_ppm }
```

Descriptor `org.rbs.audio.input`, kind `Source`, output `out` (Audio).

- `prepare` sizes the FIFO and the resampler from the stream's format and the configuration (allocation allowed) and calls `start` (may block up to 500 ms). `release` calls `stop` and frees them.
- `process` (real time) reads every run the stream has into the FIFO, then writes exactly `block_frames` frames at `block_time(tick)` on `out` (nothing before the latency).
- **FIFO.** Holds `latency + max_offset + MAX_PACKET (100 ms) + 2 blocks` at the source rate plus the resampler's window. With less than 256 frames free before a read, the oldest quarter is dropped (`discarded_frames`).
- **Time map.** FIFO frame `k` is at master time `head + k x 1e9 / source_rate` ns. The first read after `prepare` anchors it. A read whose timestamp is within `MAP_TOLERANCE` (2 ms) of the map's end continues the map; the map then moves by a sixteenth of the block's mean timestamp error. A read further off (or flagged as a discontinuity and at least a frame off) is placed by its timestamp: a gap is filled with silence (`gaps_filled`) if the FIFO has room; audio at most 1 s older than the map's end is dropped as far as it overlaps (`overlaps_dropped`), and so is a run behind a head that late output moved (never mapped as continuous); otherwise the map's pending errors are cleared and the map restarts at the new run (the old content is `discarded_frames`).
- **Steering.** `want = block_time - offset`. Before the first block after an anchor the resampler's position `p` is set so that `time(p) = want`. Otherwise, with `err = time(p) - want`: if `|err| > RESYNC_THRESHOLD` (5 ms) `p` jumps to `want` in either direction (`resyncs`; frames skipped forward are `discarded_frames`); the first lock after a map restart also counts as a resync. The step per output frame is `source_rate / graph_rate x (1 - clamp(err_seconds x STEER_GAIN, ±MAX_CORRECTION))`, `STEER_GAIN` = 0.5 per second, `MAX_CORRECTION` = 0.002; `correction_ppm` reports it. Steady state: `|err|` about drift / `STEER_GAIN` (0.2 ms at 100 ppm).
- **Late audio.** An output frame whose input window is not complete (audio not arrived, or older than the FIFO) is silence (`late_frames`) and `p` still advances; audio arriving later is dropped as an overlap. The window needs `TAPS / 2` frames after `p` (about 0.7 ms at 48 kHz), so the graph latency must cover the largest packet plus this look-ahead plus scheduling slack.
- **Offset.** `offset_ns` is clamped to `-(latency - period - 5 ms) ..= max_offset`; positive delays the source, negative advances it. A change below 5 ms slews; above, it resyncs once.
- **Resampler** (`rbs_audio::resample`): polyphase windowed sinc, `TAPS` = 64, `PHASES` = 256 with linear interpolation between neighbouring phases, Kaiser window (`BETA` 7), transition band 0.07 cycles per input frame placed below the lower Nyquist frequency (cutoff `0.5 x min(1, graph_rate / source_rate) - 0.035`), unit DC gain per phase. Tests: 1 kHz keeps its level within 0.1 dB (44.1 to 48, 48 to 44.1, 48 to 48 kHz); 18 kHz within 0.5 dB at 48 to 44.1 kHz; 23 kHz (48 to 44.1 kHz) and 30 kHz (96 to 48 kHz) at least 60 dB down.
- **Channels.** Output channel `o` is `sum_i m[o][i] x in[i]`, `m` fixed at `prepare`: identity for the same layout; mono feeds L and R; anything to mono averages; 3.1, 5.1 and 7.1 to stereo `L + 0.7071 C + 0.7071 Ls (+ 0.7071 Lb)` (and likewise right), LFE dropped, divided by the sum of the weights so full scale stays full scale; otherwise channel `i` to channel `i`, the rest silent. `Discrete(n)` sources (WASAPI 7.1 arrives as `Discrete(8)`) map channel by channel.
- **Errors.** A read error returns `NodeError::Backend`; `EndpointLost`, `FormatChanged` and `PermissionDenied` return `Permanent` (`read_errors` counts all). A start error is `Permanent` for lost, missing, refused or unsupported endpoints, else `Failed`. A bypassed input is silence for the mixer.

Tests (`crates/rbs-audio/tests/input.rs`, scripted devices on `TestClock`): a perfect device aligned within 50 µs from the first blocks; drift of ±100 and ±1000 ppm corrected without resyncs, error within drift / gain after 4 s; 1 ms timestamp jitter without resyncs, gaps or overlaps; 44.1 kHz stereo converted at the right level and time; delay and advance offsets; an offset change slews; a 30 ms gap filled with alignment kept; a flagged 1.5 ms gap filled; a second of late delivery is silence, its audio dropped as overlaps, with no resync and alignment and no silence after it; lateness inside the look-ahead keeps sync; late audio in 1 ms packets is dropped, never mapped; a 2 s gap is bridged (the head moves with the position through the silence); a stray timestamp 3 s ahead or 2 s behind restarts the map and realigns. Removing the steering makes the drift tests fail.

### Mixer (`rbs_audio::MixerNode`)

```rust
impl MixerNode {
    pub fn new(config: AudioGraphConfig, strips: usize, params: MixerParams)
        -> (Self, ParamSender<MixerParams>, Arc<MixerStats>);
}
#[non_exhaustive] pub struct MixerParams { pub strips: Vec<Strip>, pub master: f32,
    pub master_meter: Option<Arc<LevelMeter>>, pub ramp: Duration }       // ramp default DEFAULT_RAMP (10 ms)
#[non_exhaustive] pub struct Strip { pub producer: NodeId, pub gain: f32, pub muted: bool,
    pub meter: Option<Arc<LevelMeter>> }
impl MixerStats { pub fn invalid_inputs(&self) -> u64; pub fn unknown_inputs(&self) -> u64; }
```

Descriptor `org.rbs.audio.mixer`, kind `AudioMixer`, input `in` (Audio, many), output `main` (Audio).

- Every tick writes one block at `block_time(tick)`: the sum over inputs of `strip gain x input`, times the master gain. Inputs are named by producer `NodeId`, so they must come from the same partition (a queue reader has a reserved id).
- An input whose producer has no strip is not heard (`unknown_inputs`). An input block whose format or length is not the graph's is skipped (`invalid_inputs`); the rest of the mix continues. With no inputs the block is silence.
- Gains are linear amplitude; negative and non-finite values are 0; a muted strip has gain 0. The first parameters apply at once. Later changes of a strip's or the master's gain ramp linearly from the current value to the new one over `ramp`, evaluated at block time (one gain per frame). An input that joins, or returns after a tick without a block, fades in from 0 over `ramp`. Inputs that leave stop at once.
- At most `strips` strips are honoured (the first ones); their state is allocated in `new`. Taking new parameters in `process` does not allocate and keeps each strip's ramp by producer id.
- Meters are post-fader: a strip's meter gets the peak of its input times its gain, the master meter the peak of the output, per channel.

Tests: `crates/rbs-audio/tests/mixer.rs`.

### Level meter (`rbs_audio::LevelMeter`)

```rust
impl LevelMeter {
    pub fn new(channels: usize) -> Self;              // clamped to 1..=MAX_CHANNELS
    pub fn channels(&self) -> usize;
    pub fn record(&self, channel: usize, peak: f32);  // engine: fetch_max on the f32 bits; NaN, <= 0 and bad channels ignored
    pub fn take(&self) -> Levels;                     // UI: peaks since the last take, reset to 0
}
impl Levels { pub fn channels(&self) -> usize; pub fn peak(&self, c: usize) -> f32; pub fn peak_db(&self, c: usize) -> f32; }
```

`record` and `take` never allocate or wait; a peak recorded between two takes is seen by exactly one take (tests in `meter.rs`). `peak_db` feeds `rbs_ui::meter::Meter::update`.

### Encoder node (`rbs_audio::EncoderNode`)

```rust
impl EncoderNode {
    pub fn new(encoder: Box<dyn AudioEncoder>, packets: usize) -> Self;
    pub fn dropped(&self) -> Arc<AtomicU64>;
}
```

Descriptor `org.rbs.audio.encoder`, kind `Encoder`, input `in` (Audio), output `packets` (Packets). Generic over the runtime. `process` submits the input block (if any), drains into a vector reserved in `new` and passes up to `PER_TICK_EVENTS` packets per tick; up to `packets` wait for the next ticks, more are dropped and counted. The first final tick (`Tick::draining`) without an input block is the end of its input (the queue reader rings once more after its last block on final ticks): the node flushes the encoder there, passes the packets on, counts it (`flushes()`) and takes no block until it is prepared again (ADR-0044); the flush may block and allocate on that tick only (ADR-0043 Decision 4). `release` flushes again and discards what it returns: that is a tail lost only when the node left without final ticks (a live edit, a drain that timed out). Submit and drain errors are `NodeError::Backend`.

### Meter sink (`rbs_audio::MeterSinkNode`, ADR-0041)

```rust
pub const METER_SINK_NODE_TYPE: &str = "org.rbs.audio.meter-sink";
pub fn meter_sink_descriptor() -> NodeDescriptor;   // Output, input "in" (Audio), no outputs
impl MeterSinkNode { pub fn new() -> Self; }
```

Generic over the runtime; runs in the mixer's partition (no queue). `process` does nothing: the node only gives the scheduler a path to an output, so a mixer whose meters a UI shows runs while nothing records or plays. `prepare` and `release` do nothing.

## Errors and failure behaviour

- `AudioGraphError` (non-exhaustive): `InvalidFormat`, `BlockFrames`, `Latency`; construction only.
- Node errors follow the executor's rules (bypass after the failure limit, restart with back-off). A bypassed input is silence for the mixer; a bypassed mixer produces no block, which the audio queue reader fills with silence for the encoder.

## Performance requirements

Hot path (audio thread, every 10 ms): no allocation, blocking, I/O or locks in `process` of the input and mixer nodes, the queue endpoints, `AudioOut` and `Inputs::audio`; the encoder node allocates only what its encoder's `drain` does (tested with the allocation guard in `tests/pipeline.rs` and `rbs-graph/tests/audio.rs`). Budgets on the reference VM, `cargo bench -p rbs-audio --bench nodes` ([performance budgets](performance-budgets.md)): input node at most 50 µs per block (27 µs at 48 kHz mono, 35 µs at 44.1 kHz stereo; the reference adapter's sine generation adds about 20 to 35 µs in the `adapter_*` cases); mixer of 16 stereo inputs with meters at most 30 µs (13 µs).

## Conformance tests

`crates/rbs-audio/tests/` (`input.rs`, `mixer.rs`, `pipeline.rs`: two reference-adapter inputs with a drifting device clock, one at 44.1 kHz, through mixer and audio queue to an encoder node, contiguous 10 ms blocks on the grid with no resync, queue overflow or fill, and no allocation per block on either partition), unit tests in `config.rs`, `meter.rs` and `resample.rs`; `crates/rbs-graph/tests/audio.rs`; `crates/rbs-engine/src/worker.rs` (`a_stream_worker_*`).

Hardware check (`cargo run --release -p rbs-app --example audio_graph -- <seconds>`): the default microphone and system loopback through input nodes, mixer, audio queue and the platform AAC encoder on the real engine workers. Windows (WASAPI + Media Foundation AAC, Shure MV7+ and NVIDIA HDMI loopback, 2026-11-03): 30 s idle and 60 s with all 32 cores busy both pass with no resync, read error, queue overflow, late audio tick or irregular packet, packet counts as expected, thread grant `RealTime`. Late frames occur only in the first second (before the first packets arrive). Under full load the loopback had 483 gap frames filled and the microphone's correction peaked at 409 ppm, settling within seconds.

## Open questions

- Adaptive graph latency, and a low-latency monitoring path once the port has audio output.
- Buses and tracks (several mixes for several outputs, multi-track recording); fading out an input that leaves.
- Starting several inputs in parallel instead of serially in `apply`; reopening a stream after `FormatChanged`.
- A resampler bypass for sources at the graph rate with negligible correction.
- Whether moving the per-packet run logic of the adapters into the port (audio-io open item) changes the time map's input.
