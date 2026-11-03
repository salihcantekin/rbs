# ADR-0033: Audio input port: pull planar `f32` from a lock-free sample ring, no resampling in adapters

- Status: Accepted
- Date: 2026-11-03
- Deciders: Salih Cantekin (owner; early-phase policy lets threads accept ADRs), independent AI session (review)

## Context

RBS needs audio from microphones, from what the computer plays (system loopback) and from single applications, on Windows (WASAPI, process loopback), macOS (CoreAudio, process taps) and Linux (PipeWire, ALSA). The overview and [backend ports](../specs/backend-ports.md) name an `rbs-audio-io` port but no contract existed. The capture port ([capture-port.md](../specs/capture-port.md)) excludes audio, and its hand-off ("latest frame wins") does not fit audio: dropping samples is audible, so every sample must be delivered once, and losses must be visible.

Forces:

- OS audio APIs deliver packets (typically 10 ms; down to 1 ms) on their own callback thread, with an OS capture time per packet, in the device's own clock, which drifts against the master clock (ADR-0004).
- The audio graph runs fixed 10 ms blocks of planar `f32` (media types spec) on a real-time thread (P6) and must align every source by master time and correct its drift once.
- Device formats vary: 16, 24 (packed), 32-bit integer or float samples, interleaved or planar, rates from 8 to 192 kHz, 1 to 8 channels.
- Unused devices must cost nothing (P13), and the microphone indicator must not light up before a source is live.

## Decision

1. **A separate port, `rbs-audio-io`,** with `AudioBackend` (factory: `enumerate`, `open_input`, permissions) and `AudioInputStream` (Standby/Active stream: `start`, `stop`, `read`, `available`, `stats`). Endpoint kinds are `Input`, `OutputLoopback` and `Application`. Audio output is added later to the same port with defaulted methods. Contract: [audio-io-port.md](../specs/audio-io-port.md).
2. **Pull.** The audio thread calls `read` with planar `f32` buffers. A read returns one contiguous run with the master time of its first frame and a discontinuity flag. Adapters never call into the graph.
3. **One hand-off for every adapter:** the port's adapter-support module `ring`, a single-producer single-consumer ring of `f32` samples stored as atomic bits (no `unsafe`), with a second fixed ring of packet marks carrying timestamps and run starts. When full, the newest packet is dropped and counted and the next audio starts a new run; the producer never waits and never overwrites unread audio. Sample format conversion to `f32` happens on the push.
4. **Timestamps at the edge** with the port's `EdgeClock` (a `DriftEstimator` and `ClockMapping`) from the OS capture time of each packet; a run's timestamps are the packets' own, extrapolated within a packet at the nominal rate.
5. **No resampling, channel mixing or drift correction in adapters.** A stream delivers a rate the device offers natively (the first requested one, else the native rate) and the device's channels. The audio graph resamples and corrects drift in one place, against the master clock. Adapters do not enable OS resamplers and do not change a device's rate for other applications. Exceptions, because the OS offers no other path: `Application` streams (WASAPI process loopback, CoreAudio process taps) take the format the adapter asks for and the OS mixer converts; PipeWire may convert inside the daemon after a graph-rate change.
6. **A device format change ends the stream** (`FormatChanged`, recoverable); the engine opens a new one. A stream's format never changes.
7. **The ring and the clock edge live in the port crate** as adapter-support modules, as `rbs_capture::handoff` does, not in a separate support crate (ADR-0031). They are small, contain no vendor code, and the port contract is defined in their terms (runs, discontinuities, drops), so every adapter of the port needs them, and a reference adapter in the port's own tests exercises them. ADR-0031's support crate fits the opposite case: a large shared implementation for one family of adapters.
8. **A conformance suite** in the port (feature `conformance`), run strictly against a deterministic reference adapter (`rbs-audio-io-test`) on every CI runner with fake time, and with negative wrappers proving each check can fail.

## Consequences

- Every adapter is small: open the OS stream, convert its time with `EdgeClock`, push packets into the ring. The real-time rules, the drop policy and timestamp extrapolation are implemented and tested once (`cargo bench -p rbs-audio-io --bench ring`: about 1.8 µs per 10 ms stereo packet, push plus read).
- The audio graph (`rbs-audio`, not written yet) owns resampling and drift correction, and must handle sources at different rates and discontinuities. That work is needed anyway for files, network sources and plugins.
- One copy per sample from the OS buffer into the ring (unavoidable: OS buffers are only valid during the callback), and one from the ring into the block.
- A consumer that falls behind first reads old audio, then a discontinuity; it discards by timestamp. Dropping the oldest instead would need the producer to move the consumer's position.
- The ring and edge are adapter-support modules: third-party adapters that use them pin the port's minor version.

## Alternatives considered

- **Push callbacks into the graph.** Graph code would run on OS threads of unknown priority, and each source would still need a queue. Rejected for the same reasons as in the capture port.
- **Latest-wins hand-off, like video capture.** Skipping audio is audible; audio needs every sample once.
- **Resample in adapters (OS converters).** Hides the device's real rate, adds a second resampler in front of the graph's drift correction (two filters, two latencies), and the OS converters differ per platform. Rejected.
- **`cpal` as the only adapter.** Convenient across platforms, but it does not offer system or process loopback everywhere, hides some OS capture times, and its callback model would still need this ring. It can be added later as a fallback adapter behind the same port.
- **A lock-based queue (`Mutex<VecDeque>`).** Simple, but a contended lock on the audio thread breaks P6.
- **An `unsafe` `UnsafeCell<[f32]>` ring.** Avoids atomic stores per sample, but on x86-64 and ARM64 relaxed 32-bit atomics compile to plain moves, so it would add `unsafe` for no measured gain.

## Review

- Reviewer: independent AI session (fresh context, given the proposal, specs and code only), 2026-11-03.
- Verdict: changes requested; all blocking concerns resolved in the same PR, after which the ADR was accepted under the early-phase policy.
- Concerns and resolution:
  1. [blocking] Timestamps could step backwards when an OS packet is stamped early (jitter, mapping refresh): the ring now floors each read at the previous timestamp plus 1 ns; ring test `early_packets_never_move_reads_backwards`.
  2. [blocking] A packet larger than the ring (a 4096-frame PipeWire quantum with a 50 ms buffer) would be dropped forever: `ring::capacity_frames` sizes rings to at least two of the largest OS packets; the spec requires it.
  3. [blocking] Loopback and application semantics were unspecified (silence while nothing plays, process loopback has no mix format): the spec now requires continuous timed silence for these kinds and states the `Application` format rule as an exception to "no OS resampling".
  4. Rate negotiation is mostly the current rate on WASAPI and CoreAudio; PipeWire converts in the daemon: stated in the spec.
  5. More than 8 channels: the first 8 are delivered as `Discrete(8)`; a channel map is an open question.
  6. The suite could loop forever on an adapter that always returns audio: reads are bounded per poll.
  7. Negative wrappers added for backward timestamps, timestamps ahead of the clock and a new run presented as continuous. A `FormatChanged` case needs a hook: open.
  8. A `loom` model test of the ring: open (follow-up; the threaded test runs on x86 only).
  9. Support modules in the port crate versus ADR-0031: justified in decision 7.
  10. The reference adapter enabled the suite in runtime builds: `TestHooks` is now behind its `conformance` feature.
  11. A packet's end could be stamped after `now`: `EdgeClock::to_master` takes the packet duration and clamps the end; OS timestamp-error flags are handled by extrapolation (spec).
  12. Marks per OS period: documented with `DEFAULT_MARKS`.
  13. Unrelated `Cargo.lock` change: reverted.
  14. Semver notes for `InputStats::new` and the support modules: added to the spec.
- Record, 2026-11-03, PipeWire adapter (`rbs-audio-io-pipewire`): decision 5 places PipeWire's conversion after a graph-rate change "inside the daemon". It runs in libpipewire's stream adapter inside the capturing process, on PipeWire's data thread. It is still PipeWire's conversion, not adapter code, so the exception stands; the spec states the location. The adapter asks for the graph rate, so no conversion happens until the graph rate changes.
