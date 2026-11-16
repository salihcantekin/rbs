# ADR-0126: Measure a source's sync offset with a clap, and remember it with the device

- Status: Proposed
- Date: 2026-11-16
- Deciders: Salih (owner)
- Builds on: [ADR-0062](0062-built-in-video-filters-and-properties-tabs.md) decision 10 (sync offset on the source), [ADR-0037](0037-audio-graph.md) (audio input offset), [ADR-0086](0086-recording-sources-separately-iso.md) (per-source taps), [ADR-0095](0095-audio-ducker-and-key-tap.md) decision 4 (read-only audio tap), [ADR-0096](0096-capture-audio-companions-and-camera-microphone.md) (camera microphone companion), [ADR-0013](0013-zero-cost-when-unused.md), [ADR-0052](0052-ai-agent-control-through-mcp.md)

## Context

A per-source sync offset already exists end to end: `SourceAudio::sync_offset_ms` in `-200..=200` (scene format 4), the Properties › Audio › *Sync offset* row with a reset, `Studio::set_audio_offset`, and the control API method `source.set_sync_offset` (protocol 1.8). The audio input applies it, so the program, recordings, streams and the monitor all hear the shifted sound (audio graph spec, "Offset").

What the user still has to do is find the value. In OBS, and in RBS today, that is trial and error: record, look at lips or a clap, change the number, record again. Two more problems follow from where the value lives:

- The value is a property of the hardware (a camera's pipeline latency against a microphone's), but it is stored on a source. A second source for the same microphone, a new collection or a retarget (ADR-0077) starts at 0 again.
- The owner asked for features that go beyond what OBS can do because of its design ([vision](../vision.md#product-expectations-owner-2026-11-08)). OBS has no per-source taps (ADR-0086 context) and timestamps that are not always honest; RBS has both, so it can measure the offset from the same times it mixes with.

Facts this design relies on (checked on `main` at e8b00c3):

- Every source has its own node and picture before the compositor, and one output pin can feed several consumers (ADR-0047), so a video tap costs no capture.
- The audio tap after a source's filters and before its fader exists (ADR-0095 decision 4), used by the ducker key and separate audio files.
- Nodes can be added to a running studio and removed again (`add_active`, used by separate files).
- A source's device is its type plus the `adapter` and `target` settings (`rbs_sources::types::keys`).
- `rbs_gpu::readback` exists and is never on the default frame path (P4).

## Decision

### Measuring

1. **A measurement is a short, explicit action on one audio source** against one *reference* picture source. The reference defaults to the camera an audio companion belongs to (ADR-0096), else the only camera in the program scene, else the user picks one. Both sources must be open (Active). A measurement lasts at most `MEASURE_WINDOW` (10 s) and ends early once `MEASURE_CLAPS` (5) usable events are found. One measurement runs at a time per studio.
2. **The user claps in view of the reference camera**, several times with pauses of at least half a second. The UI says so; no test signal, sync app or second device is needed.
3. **Probes exist only while a measurement runs** (P13). Starting adds two nodes; finishing, cancelling, an error or the removal of either source removes them. Nothing is installed, scheduled, polled or allocated for this feature otherwise.
   - **Audio probe** (`rbs_audio`, kind `Sink`) on the ADR-0095 tap of the measured source, so it hears the sound *with* the current offset applied, before the fader and mute. Each block it computes the energy of every 1 ms sub-block over all channels and pushes `(master time, energy)` into an SPSC ring sized at `prepare` for the whole window. `process` must not allocate, block or lock (P6).
   - **Picture probe** (`rbs_nodes`, GPU) on the reference source's picture after its filters. Each frame it reduces the picture to one motion value, the mean absolute luma difference against the previous frame on a 64 x 36 grid, in a compute pass, and reads back that one `f32` with a non-blocking map that is collected on a later frame. This is a bounded exception to P4: 4 bytes per frame, only during a measurement the user started; it counts in `frames_cpu_copied` like any readback. The probe never stalls the render loop; a value not ready in time is dropped and counted.
4. **Estimation is pure logic** (`rbs_record::sync_measure`, no I/O, no clock) run on a worker, not in a node. Audio events are onsets: a rise of at least `ONSET_DB` (12 dB) in 1 ms energy over the previous 50 ms, then a 300 ms refractory time. Picture events are the frames where motion falls sharply after a peak (the hands stop at contact). Each audio event is paired with the nearest picture event within `SEARCH` (±500 ms); the residual is `audio_time - picture_time`. The result is the median residual, refined to sub-frame precision because claps fall at random phases of the frame grid. The suggested offset is `current offset - median residual` (a late sound gives a negative residual and so a larger delay).
5. **A result is only offered when it is trustworthy**: at least 3 pairs, and the pairs' interquartile range within one frame period. Otherwise the measurement ends with a typed reason (`too_few_claps`, `no_picture_motion`, `inconsistent`, `source_closed`) and nothing changes. A result is never applied by itself; the user applies it (decision 9).
6. **A result outside what the input can do is shown, not hidden.** The suggestion is clamped to `-MAX_SYNC_OFFSET_MS..=MAX_SYNC_OFFSET_MS` and to the advance the input's latency allows (audio graph spec, "Offset"); the result reports the unclamped value and that it was clamped. Widening the range is a separate decision.

### Remembering with the device

7. **The app keeps one remembered offset per audio device on this computer** in a user data file `device-sync.toml` (format 1, `kind = "device-sync"`), read and written by `rbs-app` like `calibration.toml`: never in a collection or profile, never exported, imported or synced, because device ids are per computer. An entry is keyed by the source type and its `adapter` and `target` settings, and holds `offset_ms`, `origin` (`measured` or `manual`), the reference camera's key and display name for a measured value, and the UTC time it was set. Only device-backed audio types have entries: `org.rbs.source.audio-input` (which includes camera microphone companions) and `org.rbs.source.audio-output`. Application audio has none, because its target is a process.
8. **The source stays the one source of truth for the engine** (ADR-0062 decision 10 is unchanged). The remembered value only fills a source's offset when a device-backed audio source is added or retargeted to a device that has an entry and the source's own offset is 0; it then goes through `set_source_sync_offset` like any other change. A source with a non-zero offset is never overwritten.
9. **Applying a measurement, or changing the offset by hand, updates the device's entry.** Reset to 0 removes the entry. Other sources of the same device are not changed silently; the Sync offset row offers to apply the value to them when there are any (one action, listed by name).

### Control API (next free minor version at merge)

10. For parity (ADR-0052), with the `control` permission:
    - `sync.measure_start { source, reference? }` returns `{ measurement }` or a typed error (`not_audio`, `not_picture`, `source_closed`, `busy`).
    - `sync.measure_cancel { measurement }`.
    - Topic `sync.measure.changed`: `running` with the pairs found so far, then `done { suggested_offset_ms, measured_offset_ms, clamped, pairs, spread_ms }` or `failed { reason }`.
    - Applying uses the existing `source.set_sync_offset`.
    - `sync.devices` (read) lists the remembered entries; `sync.forget_device { key }` (`control`) removes one.

### What this ADR does not decide

Widening the offset range or delaying the picture when the sound is later than the advance limit allows; a virtual microphone and its output delay; continuous lip-sync monitoring. Each needs its own ADR.

## Consequences

- The user gets a value in about ten seconds instead of record-and-compare rounds, and keeps it across sources and collections. OBS cannot do this without re-rendering a source, because it has no per-source taps.
- No cost when unused: no nodes, threads, polling or GPU resources until the user presses Measure, and the device file is read only when a source is added or retargeted.
- New real-time code (the audio probe's `process`) and a new per-frame GPU pass are hot-path code while they run: they need the `hot-path-review` checklist, measurements and the owner's sign-off (P15), even though they exist only during a measurement.
- Accuracy depends on the camera's frame rate. The target is the median within half a frame period on synthetic tests; the real accuracy is measured on the Mac with a camera and microphone before the UI ships, and the result is recorded here.
- Follow-up work: specs `audio-graph.md` (audio probe), `video-graph` or the nodes spec (picture probe), `recording.md` (`Studio` measurement calls), `user-data.md` (`device-sync.toml`), `control-api.md` (methods and topic); tests: estimation property tests with synthetic claps at random frame phases, jitter and missed events; probe conformance and no-allocation tests; device memory rules (fill only at 0, never overwrite, reset forgets); a UI proposal with dark and light screenshots (U-rules, owner approval) for a Measure button on the Sync offset row and the measurement dialog.

## Alternatives considered

- **Cross-correlate speech with lip motion continuously.** Needs face tracking and runs all the time; costly and fragile. A deliberate clap is cheap, exact and only on demand.
- **A flashing screen and beep from a phone app.** More precise per event but needs a second device and an app. Claps need nothing; a flash also produces a motion event, so it works with the same probe if a user prefers it.
- **Read back whole frames and analyse on the CPU.** Breaks P4 far more than one `f32` per frame and costs a full readback per frame.
- **Store the offset only on the device, not on the source.** Would change ADR-0062 decision 10, the scene format and the CLI, and would make a collection behave differently on another computer. Filling the source from the device keeps one truth for the engine.
- **Remember per pair of camera and microphone.** More exact when one microphone is used with several cameras, but rare, and harder to explain. The entry records which camera it was measured against, so the UI can show it.

## Review

Independent review per ADR-0014. Pending.
