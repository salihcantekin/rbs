# Spec: Audio input port

- Status: Draft
- Owner crate(s): `rbs-audio-io` (port), adapters `rbs-audio-io-*`
- Related ADRs: [0003](../adr/0003-ports-and-adapters.md), [0004](../adr/0004-single-master-clock.md), [0013](../adr/0013-zero-cost-when-unused.md), [0032](../adr/0032-fast-deterministic-tests.md), [0033](../adr/0033-audio-input-port.md)
- Related specs: [backend-ports.md](backend-ports.md), [media-types.md](media-types.md), [capture-port.md](capture-port.md)

## Purpose

The port through which RBS gets audio from the operating system: microphones and line inputs, what an output device plays (system audio loopback), and the audio of one application. Every OS audio API (WASAPI and its process loopback, CoreAudio and its process taps, PipeWire, ALSA) is an adapter behind this one contract, so adapters can be added, replaced and chosen at runtime by capability.

## Scope

In scope:

- Discovering endpoints of each kind, and the permission state for each kind.
- Opening an input stream on one endpoint, starting and stopping it, and reading its samples on the audio thread.
- Sample hand-off: `f32` planar samples, master-clock timestamps per run, explicit discontinuities, counted drops.
- The two adapter-support modules every adapter uses: the lock-free sample ring and the clock edge.
- Errors, capability keys and the conformance suite every audio input adapter passes.

Out of scope:

- Audio output (monitoring to headphones, a virtual microphone): added to this port later as `AudioBackend::open_output` with a default that returns `Unsupported` (a minor version).
- Resampling, drift correction against the master clock, channel up- and down-mixing, gain, filters and mixing: the audio graph (`rbs-audio`). The port delivers the device's own rate (or a rate the device offers natively) and its own channels.
- Device hot-plug watching and following the OS default device when it changes: one shared watcher owned by the registry (ADR-0013 rule 2). The port only offers `enumerate` on demand and marks the current default.
- Video: the capture port. A camera's microphone is an `Input` endpoint of this port.
- Compressed audio from devices (none of the targeted APIs deliver it for capture).

## Contract

Media types (`AudioFormat`, `ChannelLayout`, `MAX_CHANNELS`), `SampleRate`, `Cadence`, `DriftEstimator`, `ClockMapping`, `Clock` and `Timestamp` come from `rbs_core` ([media-types.md](media-types.md)). The Rust types in `rbs_audio_io` are the source of truth; the signatures below are kept in sync with them.

### Endpoints and permissions

```rust
#[non_exhaustive] pub enum EndpointKind { Input, OutputLoopback, Application }   // as_str: input, output_loopback, application

/// Opaque, adapter-defined id. Stable for the lifetime of the endpoint; for devices
/// also across restarts where the OS allows it (WASAPI endpoint id, CoreAudio device
/// UID, PipeWire node.name), because scenes store it.
pub struct EndpointId(String);

#[non_exhaustive]
pub struct AudioEndpoint {
    pub id: EndpointId, pub kind: EndpointKind, pub name: String,
    pub format: AudioFormat,            // native: the OS mix format or the device's current format
    pub sample_rates: Vec<SampleRate>,  // rates deliverable without software resampling; includes the native rate
    pub is_default: bool,               // the OS default endpoint of its kind
}   // + new / with_sample_rates / with_default

#[non_exhaustive] pub enum Permission { Granted, Denied, NotDetermined }
```

### Backend (factory)

```rust
pub trait AudioBackend: AdapterFactory {
    /// Endpoints of `kind` available now. Not real-time; up to 100 ms. Never prompts,
    /// keeps no resources. Unsupported for kinds the adapter cannot list.
    fn enumerate(&self, kind: EndpointKind) -> Result<Vec<AudioEndpoint>, AudioIoError>;

    /// A stream in Standby. Never prompts and never activates the device (the
    /// microphone indicator stays off): no threads, OS streams or device handles until
    /// `start`. Decides the format with `InputRequest::format_for`.
    fn open_input(&self, request: &InputRequest) -> Result<Box<dyn AudioInputStream>, AudioIoError>;

    /// Current permission state; fast, never prompts. Default: Granted.
    fn permission(&self, kind: EndpointKind) -> Permission;

    /// Shows the OS prompt if not determined; blocks (control thread only).
    fn request_permission(&self, kind: EndpointKind) -> Result<Permission, AudioIoError>;
}

#[non_exhaustive]
pub struct InputRequest {
    pub endpoint: EndpointId,
    pub sample_rates: Vec<SampleRate>,  // preferred, best first; empty = native
    pub buffer: Duration,               // ring capacity, clamped to MIN_BUFFER (50 ms)..=MAX_BUFFER (2 s); default 200 ms
    pub clock: Arc<dyn Clock>,          // master clock (ADR-0004)
}   // + new / with_sample_rates / with_buffer / format_for / clamped_buffer
```

Format rule (`InputRequest::format_for`): the delivered rate is the first requested rate that the endpoint lists in `sample_rates`, otherwise the endpoint's native rate; the channels are the endpoint's. A rate the device cannot deliver is never an error, because the audio graph resamples anyway; adapters do not ask the OS to resample (WASAPI `AUTOCONVERTPCM`, a CoreAudio converter) and do not change the device's rate for other applications. Sample formats (16-bit, packed 24-bit, 32-bit integer, `f32`, `f64`, unsigned 8-bit) are converted to `f32` at the edge by the ring (`ring::Sample`). An endpoint whose sample format the adapter cannot convert is refused at `open_input` with `FormatUnsupported`.

Per kind and platform:

- **`sample_rates` is usually just the current rate.** Shared-mode WASAPI and CoreAudio deliver only the device's current rate without a converter, so their endpoints list one rate. Adapters list more only where the OS opens the device at another rate for this stream alone. The reference adapter lists several rates so the format rule is tested.
- **PipeWire** nodes exchange audio at the graph rate (a device at another hardware rate is converted by its own node in the daemon), so endpoints list the graph rate (`clock.force-rate`, else `clock.rate`) and a stream at that rate is not converted. When the graph rate changes later, libpipewire's stream adapter (`audioconvert`, on PipeWire's data thread in this process) resamples and the stream keeps its format, so `FormatChanged` does not occur there. That conversion is PipeWire's, not the adapter's, and is accepted (ADR-0033 decision 5).
- **PipeWire endpoints.** `Input` endpoints are `Audio/Source*` and `Audio/Duplex` nodes (their output ports), `OutputLoopback` endpoints the monitor ports of `Audio/Sink*` nodes. Loopback and application streams set `node.always-process`, so they are scheduled, and deliver silence, while nothing plays; input streams do not, so a microphone the session manager never links delivers nothing rather than silence that looks like a quiet room.
- **PipeWire `Application` endpoints** are every `Stream/Output/Audio` node with one `application.name`, captured in stereo at the graph rate: the adapter links the application's ports to the stream's two ports itself, including streams the application opens later and links someone else removes. Left positions feed `FL`, right positions `FR`, centre and mono positions both, LFE neither, and unknown positions (`AUXn`) go by port index; PipeWire's port mixer sums what is linked at unity gain, so several streams and surround audio are downmixed in the daemon's mixer, not by the adapter. The application is lost (`EndpointLost`) when its last PipeWire client disconnects, not when it merely stops playing.
- **`Application` endpoints have no device format.** WASAPI process loopback has no mix format (the OS mixes and converts to whatever the client asks), and CoreAudio process taps are read through an aggregate device. For this kind the adapter chooses the format: the first requested rate it can ask the OS for, else the default output's rate, with stereo channels; the OS mixer converts. This is the one accepted exception to "no OS resampling"; the endpoint lists the rates the adapter may ask for.
- **WASAPI endpoints.** `Input` endpoints are active capture endpoints, `OutputLoopback` endpoints active render endpoints read with `AUDCLNT_STREAMFLAGS_LOOPBACK`, both in the shared-mode mix format without `AUTOCONVERTPCM`. `Application` endpoints are processes with a live audio session (not the system sounds session) on some render endpoint, captured with process loopback of their process tree (the process and its children, so a browser's audio process is included) as 32-bit float stereo at 44.1, 48 or 96 kHz (else the default render endpoint's rate). A packet's time is its `u64QPCPosition`; a packet more than 1 ms away from where the previous one ended starts a new run. Speaker masks are named only where WASAPI's channel order matches the layout (mono, stereo, 3.1, 5.1); 7.1 orders back before side and is delivered as `Discrete(8)`. Endpoint loss is `AUDCLNT_E_DEVICE_INVALIDATED` with the endpoint no longer active, or the captured process exiting; an invalidated stream whose endpoint is still active is `FormatChanged`.
- **CoreAudio endpoints.** `Input` endpoints are devices with input channels, read with an IO proc in the HAL's float format at the nominal rate (no AUHAL, so no converter). `OutputLoopback` endpoints are devices with output channels, read through a private process tap of the device's stream mixed down to stereo; `Application` endpoints are processes whose `kAudioProcessPropertyIsRunningOutput` is set, read through a private stereo tap of that one process (unlike WASAPI, not its children). Taps are read through a private aggregate device clocked by the output device (the default output for applications) with drift compensation, so they deliver continuous audio, silence included. A cycle's time is its input host time; a cycle whose device sample time does not continue the previous one, or whose time is more than 1 ms off, starts a new run. Taps need the "System Audio Recording" permission, which has no public query: the first tap shows the OS prompt and a tap without permission delivers silence, so `permission` reports `Granted` for those kinds.
- **Presentation latency (ADR-0042).** `presentation_latency` is how long after their timestamps a stream's audio is presented (heard) at the render endpoint, measured at the last `start`; loopback timestamps stay the OS mix time, so invariant 2 holds. `None` for `Input` streams, in Standby and when not measured. The audio graph's input node adds it to the source's sync offset. WASAPI measures it for `OutputLoopback` (that endpoint) and `Application` (the default render endpoint) streams on a helper thread during `start`: a shared render stream fed with silence for 150 ms, the mean of the frames the engine has taken minus `IAudioClock`'s playing position (extrapolated from its QPC stamp) after a 40 ms warm-up; used if it arrives within the start budget and lies in 0..=500 ms (SC-PC, NVIDIA HDMI/DP audio to the monitor: 14.8 ms; `GetStreamLatency` reports 0 there). CoreAudio and PipeWire report `None` for now (follow-up: device latency plus safety offset, sink node latency).
- **Loopback and application streams are continuous.** While nothing plays, WASAPI loopback and process loopback deliver no packets. Adapters for these kinds push timed silence for such periods (for example by keeping a silent render stream on the endpoint, the usual WASAPI practice, or by a timer on the adapter's capture thread), so `read` delivers continuous audio and `delivery` passes on a silent machine. Input devices always deliver packets.
- **More than `MAX_CHANNELS` channels** (8; multi-input audio interfaces): the adapter delivers the first 8 as `ChannelLayout::Discrete(8)`. Choosing other channels (a microphone on input 11) needs a channel map in the request, an open question.

### Input stream

```rust
pub trait AudioInputStream: Send {
    fn format(&self) -> AudioFormat;                       // fixed for the stream's life
    fn start(&mut self) -> Result<(), AudioIoError>;       // Standby -> Active, may block up to 500 ms; no-op when Active
    fn stop(&mut self);                                    // Active -> Standby, idempotent; unread audio is discarded and counted as dropped
    fn read(&mut self, out: &mut [&mut [f32]]) -> Result<AudioRead, AudioIoError>; // REAL-TIME
    fn available(&self) -> usize;                          // REAL-TIME
    fn stats(&self) -> InputStats;                         // captured, delivered, dropped, discontinuities
    fn presentation_latency(&self) -> Option<Duration> { None } // loopback kinds: output latency measured at start (ADR-0042)
}

#[non_exhaustive]
pub struct AudioRead { pub frames: usize, pub timestamp: Timestamp, pub discontinuity: bool } // + EMPTY / new
```

Dropping a stream releases everything it holds, including what `start` acquired.

`read` rules (real-time, called by the audio thread every block):

- `out` holds exactly `format().channels.channels()` slices of equal length; otherwise nothing is read and `BufferMismatch` is returned. At most that length is read, oldest audio first, into the start of every slice.
- The frames of one read are one contiguous **run**. A read stops before a discontinuity; the next read starts the new run and reports `discontinuity: true`. A run starts with the first audio after `start`, after audio was dropped, and after a gap the OS reports (a WASAPI data discontinuity flag, a PipeWire xrun, a CoreAudio overload).
- `timestamp` is the master time of the first frame read, and is later than the previous non-empty read's (the ring floors it at the previous timestamp plus 1 ns, so OS timestamp jitter never moves time backwards). Within a run, a read's timestamp is the previous read's timestamp plus its frames at the stream's rate, up to the device's drift (the run's timestamps come from the packets' own timestamps, so drift shows as small steps, never as accumulated error). An adapter whose OS packet times jitter (PipeWire's cycle times carry the scheduler's wake-up latency) smooths them within a run: `rbs-audio-io-pipewire` starts each packet where the previous one ended, moved a sixteenth of the way towards its measured time and at most 1 ms either way, so it follows devices running fast or slow against the master clock (a continued packet may start up to 1 ms before the previous one ended). A packet whose measured time is further off, or that would have to move more than 1 ms to end by the time it is handled (with 0.5 ms of slack for wake-up jitter), starts a new run, which counts as a discontinuity even when nothing was dropped; a new run never starts before the previous packet ended. A packet that cannot be placed at all is dropped and counted: a graph quantum or rate change costs about 30 ms of audio this way (an audible click, counted as dropped). Quantum changes are routine on a desktop: a client asking for another latency causes one, including another stream of an application being captured starting or stopping. If the WASAPI and CoreAudio adapters need the same smoothing, it moves into the port next to `EdgeClock` (a port change, with its own review).
- `EMPTY` (zero frames) in Standby and when nothing is available. A consumer that wants a full block calls again or waits for the next block; the port never pads with silence.
- No allocation, no freeing, no blocking, no I/O, no logging, no locks of any kind. The hand-off from the OS callback is the sample ring below.

Device format changes (the user changes the device rate in the OS settings) end the stream: `read` returns `FormatChanged` (recoverable); the engine opens a new stream, which reads the new format. A stream never changes its format, so the graph never sees a block with an unexpected rate.

### Invariants

Each is checked by the conformance suite, with the hook it needs in brackets. Without the hook the check is skipped and reported; with `strict` a skip fails the run.

1. **Zero cost before start.** A backend and an opened but not started stream own no threads, OS streams or device handles (P13), and `enumerate` keeps none. [`resources`]
2. **Master time, at capture.** A read's timestamp is the capture instant of its first frame in master time, converted at the edge from the OS capture time of its packet (WASAPI `u64QPCPosition`, CoreAudio `mHostTime`, PipeWire and ALSA `CLOCK_MONOTONIC` times) with the clock edge below. Timestamps strictly increase across reads of a stream, including across `stop` and `start`, and a read never ends after the master time at which it was made (with `timestamp_tolerance`, default 2 ms). Stamping at callback time instead of capture time is a bug the suite cannot always see; the reference adapter has its own offset and drifting device clock and a test that bounds the conversion error.
3. **Runs are honest.** The first non-empty read after `start` is a discontinuity. A read that is not a discontinuity continues the previous one: its timestamp is within `timestamp_tolerance` of the previous timestamp plus the previous frames.
4. **Format honoured.** `format()` is decided by the format rule and never changes; every read fills exactly that many channels.
5. **Nothing silently lost.** The producer never blocks and never overwrites unread audio: a packet that does not fit is dropped whole and counted, and the next audio starts a new run. After `stop`, `captured == delivered + dropped`, and `delivered` equals the frames `read` returned. Audio the OS loses before the adapter sees it is reported as a gap when the OS says so, otherwise it is invisible.
6. **Endpoint loss is an error, not silence.** An unplugged device or exited application makes `read` return `Err(EndpointLost)` within `audio_timeout`. [`remove`]
7. **Stop and drop release.** After `stop` and after dropping the stream, resources return to the level before `open_input` (ADR-0013 rule 5); `read` after `stop` returns nothing; `start` after `stop` delivers audio again, starting a new run. [`resources`]
8. **Real-time calls do not allocate.** `read`, `available` and `stats` neither allocate nor free. [`measure_allocations`]

### Capability keys

Constants in `rbs_audio_io::caps`.

| Key | Value | Meaning |
| --- | --- | --- |
| `audio.input.kinds` | Set of `input`, `output_loopback`, `application` | Kinds `open_input` accepts |
| `audio.input.enumerate` | Set of kinds | Kinds `enumerate` can list |
| `audio.input.channels.max` | Int | Most channels a stream delivers (at most `MAX_CHANNELS`) |
| `audio.input.permission` | Flag | Some kind needs a user permission (macOS microphone privacy) |
| `adapter.hardware` | Flag | `true` for OS audio paths (from `rbs-core`) |

## Sample ring

`rbs_audio_io::ring` is an adapter-support module: versioned with the port crate but outside the port contract, and it may change in a minor version (third-party adapters that use it pin `rbs-audio-io` to a minor version), like `rbs_capture::handoff`.

```rust
pub fn channel(format: AudioFormat, capacity_frames: usize, marks: usize) -> (RingProducer, RingConsumer); // allocates: in start
pub fn frames_for(rate: SampleRate, duration: Duration) -> usize;   // rounded up
pub const DEFAULT_MARKS: usize = 256;
pub trait Sample: Copy { fn to_f32(self) -> f32; }  // f32, f64, i16, i32, u8, I24 (packed little-endian)

impl RingProducer {   // the OS callback or the adapter's thread
    pub fn push<S: Sample>(&mut self, timestamp: Timestamp, interleaved: &[S]) -> Push;   // Written | Dropped
    pub fn push_planar<S: Sample>(&mut self, timestamp: Timestamp, channels: &[&[S]]) -> Push;
    pub fn push_silence(&mut self, timestamp: Timestamp, frames: usize) -> Push;          // OS "silent" packets
    pub fn lost(&mut self, frames: u64);   // counted as captured and dropped; new run
    pub fn gap(&mut self);                 // the next packet starts a new run (OS-reported gap)
    pub fn free(&self) -> usize;  pub fn channels(&self) -> usize;
}
impl RingConsumer {   // the stream, on the audio thread
    pub fn read(&mut self, out: &mut [&mut [f32]]) -> Result<AudioRead, AudioIoError>;    // implements AudioInputStream::read
    pub fn available(&self) -> usize;  pub fn stats(&self) -> InputStats;
    pub fn close(self) -> InputStats;      // after the OS stream stopped: unread frames count as dropped
}
```

- Single producer, single consumer, fixed size, lock-free and allocation-free after `channel`. Samples are stored as `f32` bits in `AtomicU32`s, so the ring has no `unsafe`; on every supported CPU a relaxed 32-bit atomic load or store is a plain load or store. Frame positions are `u64` totals, so they never wrap in practice.
- Every pushed packet leaves a **mark** (frame position, master time of its first frame, starts-a-run flag) in a second fixed ring of `marks` entries, published before the packet's samples, so a consumer that sees samples also sees the mark that times them. A read takes its timestamp from the newest mark at or before its first frame, extrapolated at the nominal rate with `Cadence`, crosses marks of the same run and stops before a mark that starts a new run.
- **Capacity.** A packet larger than the free space is dropped, so a ring must hold at least two of the largest packets the OS may deliver: adapters size it with `ring::capacity_frames(rate, request.clamped_buffer(), max_packet_frames)`, which is `max(buffer, 2 × max_packet_frames)` (a PipeWire quantum of 4096 frames, a CoreAudio buffer of 4096 frames, a WASAPI period). Marks lose only timing detail when they run out, so `DEFAULT_MARKS` (256) suits OS periods down to `buffer / 256`; adapters with smaller periods (CoreAudio 32 to 64 frames) pass more marks or push every few periods as one packet.
- When the sample ring is full, the incoming packet is dropped whole (newest audio is lost, the oldest unread audio is kept) and the next stored packet starts a new run. When the mark ring is full, a packet that continues a run is stored without a mark (its time is extrapolated from the run's last mark) and a packet that starts a run is dropped.
- Dropping the newest audio rather than the oldest keeps the ring single-producer: discarding unread audio would need the producer to move the consumer's position. A consumer that falls behind sees old audio first and then a discontinuity; the audio graph aligns sources by timestamp and discards what is too old.
- Benchmark: `cargo bench -p rbs-audio-io --bench ring`. Release build, cloud container, 2026-11-03: a 10 ms stereo 48 kHz packet pushed as `f32` and read back costs about 1.8 µs (as `i16`, about 2.1 µs), 0.02% of the 10 ms period; a read with nothing available about 5 ns. Allocation guard: `crates/rbs-audio-io/tests/no_alloc.rs`.

## Clock edge

`rbs_audio_io::edge::EdgeClock` is an adapter-support module like the ring. It turns the OS capture time of a packet into master time (ADR-0004):

```rust
impl EdgeClock {
    pub fn new(bucket: Duration) -> EdgeClock;                         // default bucket 1 s
    pub fn observe(&mut self, source_nanos: u64, master: Timestamp);   // a pair read back to back
    pub fn to_master(&mut self, source_nanos: u64, duration: Duration, now: Timestamp) -> Timestamp; // duration: the packet's
    pub fn restart(&mut self);                                         // new OS stream; results keep increasing
}
```

- The adapter calls `observe` with the OS clock and `clock.now()` read back to back (per callback is enough) and `to_master` with the packet's OS capture time. A `DriftEstimator` fits the mapping; the mapping is refreshed when a bucket completes, so it changes at most once per bucket.
- Results are clamped so the packet ends at `now` at the latest (it is never stamped in the future) and are strictly increasing.
- When the OS flags a packet's time as invalid (WASAPI `AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR`), the adapter stamps it with the previous packet's time plus that packet's duration instead. Fixed size, no allocation: safe on the OS callback thread.

## Pull, not push

The audio thread pulls with `read`; adapters do not call into the graph.

- **Chosen: pull from a sample ring.** The OS callback thread only converts and copies into the ring; the audio thread reads one block per tick, aligned by timestamp. OS threads of unknown priority never run graph code, and one rule (no allocation, no locks) covers every adapter because they share the ring.
- **Alternative: push callback** (the adapter calls the graph per packet). Graph code would run on OS threads, and every source would need its own queue and drift handling in the callback.
- **Alternative: latest wins, like video capture.** Audio cannot skip samples without an audible click; every sample is delivered once, and losses are explicit discontinuities.

## Errors and failure behaviour

```rust
#[non_exhaustive]
pub enum AudioIoError {
    Unsupported(&'static str), EndpointNotFound, EndpointLost,
    PermissionDenied, PermissionNotDetermined,
    FormatUnsupported,             // the endpoint's sample format cannot be converted
    FormatChanged,                 // the device format changed; open a new stream
    DeviceBusy,                    // opened exclusively elsewhere
    BufferMismatch,                // read buffers do not match the channels (caller bug)
    Backend { domain: ErrorDomain, code: i64, recoverable: bool },   // HResult | Errno | OsStatus | Library
}
impl AudioIoError { pub const fn is_recoverable(&self) -> bool; }   // EndpointLost, DeviceBusy, FormatChanged, recoverable Backend
```

- `EndpointLost`, `DeviceBusy` and `FormatChanged` are recoverable: the source node outputs silence and the engine reopens with back-off (ADR-0024). Permission errors are never retried automatically; they are surfaced to the user.
- Errors carry no heap data, so building them never allocates on the audio thread.
- Adapters never panic across the port boundary; the host wraps calls with `catch_unwind` as a last resort.

## Evolution and semver

The port follows semver like the capture port.

- Enums and structs are `#[non_exhaustive]` with constructors or builders, so kinds, errors and fields (a channel map, a latency hint) can be added in a minor version.
- New trait methods (audio output, a latency query) come with a default implementation that returns `Unsupported`.
- New capability keys are minor; changing the meaning of a key is major.
- The conformance suite (`rbs_audio_io::conformance`, feature `conformance`) is test support for adapter crates in this workspace, not a stable API yet.
- `InputStats::new` keeps its four arguments when fields are added; new fields get builders.
- The adapter-support modules `ring` and `edge` are unstable for third parties while the port is `0.x`.

## Performance requirements

- `read`, `available` and `stats` are on the audio thread and listed in [hot paths](../architecture/hot-paths.md), as are the adapters' OS callbacks and the ring's producer side. Rules above; costs in the sample ring section.
- Idle cost: installed adapters cost nothing; opened streams in Standby own nothing (invariant 1). An Active stream costs the OS callback and one copy of its samples.
- Memory: a stream's ring holds `buffer` of audio (default 200 ms: 75 KiB for 48 kHz stereo) plus 256 marks (6 KiB), allocated in `start` and freed in `stop`.
- Latency added by the port: none beyond the OS packet size; a consumer that reads every block sees each packet within one block of its arrival.

## Conformance tests

`rbs_audio_io::conformance::run(&backend, &mut pacer, &options) -> Report` (the `rbs_core::conformance::Report`).

- `pacer`: `TestPacer` for software adapters (a `TestClock` that advances on each wait, so runs are deterministic and take milliseconds), `RealTimePacer` for hardware adapters, whose tests are gated by an environment variable and ignored by default (ADR-0032).
- `options`: `poll_interval` (10 ms), `audio_timeout` (2 s, a hang guard for first audio and for endpoint loss), `run_length` (300 ms of audio for the timestamp checks), `timestamp_tolerance` (2 ms), `max_endpoints` per kind (2), `measure_allocations`, `hooks` (`remove(id)`, `resources()`), `strict`.

Cases: `probe` (shared probe check), `enumerate` (agrees with `audio.input.enumerate` and `audio.input.kinds`, only the asked kind, unique ids, native rate listed, channels within `audio.input.channels.max`, other kinds `Unsupported`, no resources kept), `unknown endpoint` (`EndpointNotFound`), then per endpoint: `format` (format rule with listed, unlisted and no rates), `standby` (no resources, no audio, zero counters), `buffer mismatch`, `delivery` (`run_length` of audio within `audio_timeout`), `timestamps` (invariants 2 and 3 over the run), `no allocation`, `overflow` (stop reading for twice the buffer: drops counted, a discontinuity follows), `accounting` (invariant 5 after `stop`), `stop` (nothing read after, resources back), `reads` (invariants 2 and 3 over every read of the stream), `restart` (new run, later timestamps), `drop`, `endpoint lost`.

Reads are bounded per poll (64), so an adapter that returns audio forever fails instead of hanging the suite. The `no allocation` case repeats its measured read, one poll interval apart, until it returns audio (within `audio_timeout`), because an OS packet can be longer than the poll interval (a PipeWire quantum of 1024 frames is 21 ms); every attempt must neither allocate nor free.

`rbs-audio-io-test` runs the suite strictly against the reference adapter and against wrappers that each break one invariant (allocates in `read`, starts in `open_input`, shifts timestamps inside a run, steps timestamps backwards, stamps in the future, no discontinuity on the first read, presents a new run as continuous, hides drops, delivers after `stop`, ignores endpoint loss, ignores the requested rate) to prove each check fails.

## Adapters planned

| Adapter crate | Platform | Kinds | Notes | Phase |
| --- | --- | --- | --- | --- |
| `rbs-audio-io-test` | All | Input, OutputLoopback | Reference adapter: sine waves, drifting device clock, no threads | Done |
| `rbs-audio-io-pipewire` | Linux | Input (`Audio/Source` nodes), OutputLoopback (sink monitor ports), Application (stream nodes of one `application.name`) | Strict conformance in CI against a private daemon with test endpoints in child processes; `cargo bench -p rbs-audio-io-pipewire --bench cycle` | Done |
| `rbs-audio-io-wasapi` | Windows | Input (`capture:<endpoint id>`), OutputLoopback (`loopback:<endpoint id>`, loopback flag), Application (`app:<pid>`, process loopback of the process tree, Windows 10 2004+) | Event-driven shared mode on one MMCSS ("Pro Audio") capture thread; timed silence while loopback gets no packets; `cargo bench -p rbs-audio-io-wasapi --bench packet`; hardware suite `RBS_AUDIO_HW=1` with a tone-playing fixture process | Done |
| `rbs-audio-io-coreaudio` | macOS | Input (`input:<device UID>`), OutputLoopback (`output:<device UID>`) and Application (`app:<pid>`) through process taps (macOS 14.4+) | IO proc on the device or on a private aggregate device reading a private tap (no AUHAL, so nothing converts input); microphone permission; `cargo bench -p rbs-audio-io-coreaudio --bench cycle`; hardware suite `RBS_AUDIO_HW=1` with a tone-playing fixture process | Done |
| `rbs-audio-io-alsa` | Linux | Input | For systems without PipeWire | Later |

## Review

Independent review per ADR-0014: see [ADR-0033](../adr/0033-audio-input-port.md#review). The PipeWire adapter's review (pacing, application linking, test scope) is recorded in its pull request. Open items from it: a `loom` model test of the ring's publication order (the threaded test runs on x86 only, which is strongly ordered), and a `FormatChanged` case in the suite (needs a hook that changes a device's format).

## Open questions

- Audio output (monitoring) and a virtual microphone: the `open_output` contract, when the monitoring feature needs it.
- Following the OS default device as it changes: a registry-level watcher reopens the stream (a discontinuity), or adapters that can follow it themselves declare a capability.
- Following a render endpoint's latency while a loopback stream runs (it is measured once per `start`, ADR-0042).
- Whether a latency hint (requested OS packet size) is needed for low-latency monitoring, or the OS default packet (10 ms on WASAPI shared mode) is always enough.
- Exclusive-mode capture (WASAPI exclusive, hog mode): not planned; a capability if a user need appears.
