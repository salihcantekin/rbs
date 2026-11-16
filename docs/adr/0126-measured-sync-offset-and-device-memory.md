# ADR-0126: Measure a source's sync offset with a clap, and remember it with the device

- Status: Accepted
- Date: 2026-11-16
- Deciders: Salih (owner)
- Builds on: [ADR-0062](0062-built-in-video-filters-and-properties-tabs.md) decision 10 (sync offset on the source), [ADR-0037](0037-audio-graph.md) (audio input offset), [ADR-0086](0086-recording-sources-separately-iso.md) (per-source taps), [ADR-0095](0095-audio-ducker-and-key-tap.md) decision 4 (read-only audio tap), [ADR-0096](0096-capture-audio-companions-and-camera-microphone.md) (camera microphone companion), [ADR-0077](0077-source-health-retarget-and-settings-window.md) (retarget), [ADR-0013](0013-zero-cost-when-unused.md), [ADR-0052](0052-ai-agent-control-through-mcp.md) (API parity)

## Context

A per-source sync offset already exists end to end: `SourceAudio::sync_offset_ms` in `-200..=200` (scene format 4), the Properties › Audio › *Sync offset* row with a reset, `Studio::set_audio_offset`, and the control API method `source.set_sync_offset` (protocol 1.8). The audio input applies it (positive delays: the audio captured at `t - offset` plays at `t`), so the program, recordings, streams and the monitor all hear the shifted sound.

What the user still has to do is find the value. In OBS, and in RBS today, that is trial and error: record, look at lips or a clap, change the number, record again. And the value is a property of the hardware (a camera's pipeline latency against a microphone's), but it is stored on a source: a second source for the same microphone or a new collection starts at 0 again, and a retarget to another device keeps the old device's value (ADR-0077 keeps a source's settings and audio on retarget).

The owner asked for features beyond what OBS offers out of the box ([vision](../vision.md#product-expectations-owner-2026-11-08)). OBS ships no built-in way to measure the offset. RBS has per-source taps and timestamps it already mixes with, so it can measure on demand.

Facts this design relies on (checked on `main` at e8b00c3):

- Nodes can be added to a running studio and removed again (`add_active`, used by separate files, `crates/rbs-record/src/studio/iso_audio.rs`).
- The audio tap after a source's filters and before its fader is the mixer input producer of a source the program mix hears (ADR-0095 decision 4; separate files refuse other sources with `NotHeard`).
- A capture node puts its newest frame on its output every render tick, so a 30 fps camera on a 60 fps canvas repeats frames; video packets are stamped with the canvas tick time, audio with the block time (`docs/specs/recording.md`).
- `rbs_gpu::readback` blocks; the non-blocking pattern is the staging-buffer ring of `YuvDownload`/`RgbaDownload`, mapped without waiting and collected after `GpuContext::poll` (`crates/rbs-gpu/src/download.rs`).
- `InputNode` clamps an advance to `latency - period - 5 ms` and counts `resyncs`.
- A source's device is its type plus its `adapter` and `target` settings (`rbs_sources::types::keys`); a camera microphone companion carries the camera's id in its `companion_of` hint.

## Decision

### Measuring

1. **A measurement is a short, explicit action on one audio source against one *reference* picture source.** The reference defaults to the camera of a camera-microphone companion (ADR-0096), else the only camera in the program scene, else the caller must name one; any picture source is allowed (a screen flash works too). The measured source must be heard by the program mix (else `not_heard`); the reference must be open. A measurement lasts at most `MEASURE_WINDOW` (10 s) and ends early once `MEASURE_CLAPS` (5) usable pairs are found. One measurement runs at a time per studio (else `busy`).
2. **The user claps in view of the reference camera**, several times with pauses of at least half a second. No test signal, app or second device is needed.
3. **Probes exist only while a measurement runs** (P13). Starting adds two nodes; finishing, cancelling or failing removes them. Their rings are drained by `Studio::service` on the control thread; no thread is added. Nothing is installed, scheduled, polled or allocated for this feature otherwise.
   - **Audio probe** (`rbs_audio`, kind `Sink`) on the measured source's mixer input producer, so it hears the sound with the offset the input actually applies, after the filters, before the fader and mute. Per block it computes the energy of each 1 ms sub-block over all channels and pushes `(block time of the sub-block, energy)` into an SPSC ring sized at `prepare` for the whole window. `process` must not allocate, block or lock (P6).
   - **Picture probe** (`rbs_nodes`, GPU) on the reference source's chain tail. It acts only on a **new** frame (by the capture's frame sequence); repeated frames are skipped. The compute pipeline and the staging slots are created when the probe is prepared and released with the node, never per frame. For a new frame a compute pass reduces the picture to one value, the mean absolute luma difference against the previous new frame on a 64 x 36 grid, into a ring of at least 3 mappable staging slots, mapped without waiting (the `RgbaDownload` pattern) and collected after a later poll. Each slot carries the **canvas tick time at which that frame first reached the tap**, the time the program uses for the picture. A slot not ready when its turn comes again is dropped and counted. This is a bounded exception to P4: 4 bytes per new frame, only during a measurement the user started; it counts in `frames_cpu_copied`. wgpu's per-map callback allocation on the render thread is covered in the hot-path review.
   - A change of either chain tail, a studio rebuild, a settings change that rebinds either source, any `set_audio_offset` for the measured source, or a movement of its input's `resyncs` counter ends the measurement with a reason (decision 5). The offset the input applies at the start (the **effective** offset, after the advance clamp, exposed in the input's stats) is recorded with the measurement.
4. **Estimation is pure logic** (`rbs_record::sync_measure`, no I/O, no clock), run from `Studio::service`.
   - Audio events: a rise of at least `ONSET_DB` (12 dB) in 1 ms energy over the mean of the previous 50 ms, then a 300 ms refractory time. Event time = start of that sub-block.
   - Picture events: a clap is a reversal of the hands, so its frame `k` (the first new frame that shows the hands together) is a **peak** of the motion value `m_k = |f_k - f_(k-1)|`, followed within two new frames by a fall to at most half of it or by a local minimum (hands that bounce apart keep some motion). The event is the peak frame `k`, never the frame after it. The contact happened between frames `k-1` and `k`, so the event time is `T_k - Δ_k / 2`, where `T_k` is frame `k`'s tick time and `Δ_k = T_k - T_(k-1)` the **measured** interval between those two new frames (not the nominal rate, so a camera that drops to 15 fps in low light stays correct). The rebound case is a known risk checked in the Mac measurement.
   - Each audio event is paired with the nearest picture event within `SEARCH` (±500 ms). Residual `r = audio_time - picture_time`; positive means the sound is late.
   - Estimate: the mean of the inner half (interquartile) of the residuals. Claps fall at random phases of the frame grid, so this averages below one frame period.
   - Suggested offset: `effective offset - r`. A sound 60 ms **early** gives `r = -60` and a suggestion of `+60` (delay it). A sound 20 ms **late** gives `r = +20` and `-20` (advance it).
5. **A result is only offered when it is trustworthy**: at least 3 pairs, and the interquartile range of the residuals at most the median of the measured frame intervals in the window. Otherwise it ends with a reason: `too_few_claps` (with a hint when the chain has a gate, noise suppression or ducker, which can remove onsets), `no_picture_motion`, `inconsistent`, `not_heard`, `source_closed`, `offset_changed`, `chain_changed`, `studio_restarted`, or `cancelled`. Nothing changes on a failure. A result is never applied by itself. Delay added by the source's filters is correctly included, since the program hears it.
6. **A result outside what the input can do is shown, not hidden.** The suggestion is clamped to `-MAX_SYNC_OFFSET_MS..=MAX_SYNC_OFFSET_MS` and to the input's advance limit; the result reports the unclamped value and `clamped: true`. Widening the range is a separate decision.

### Remembering with the device

7. **The app keeps one remembered offset per audio device on this computer** in a user data file `device-sync.toml` (format 1, `kind = "device-sync"`), read and written by `rbs-app` like `calibration.toml`: never in a collection or profile, never exported, imported or synced, because device ids are per computer. An entry's key is the three strings `type`, `adapter` and `target`, stored as separate fields (a device UID may contain any character) and given as an object `{ type, adapter, target }` in the API; it holds `offset_ms`, `origin` (`measured` or `manual`), for a measured value the reference's key and display name, and the UTC time it was set. Only device-backed audio types have entries: `org.rbs.source.audio-input` (including camera microphone companions) and `org.rbs.source.audio-output`; application audio targets a process and has none. Known limit: an endpoint id can change when a device moves to another USB port; the entry is then not found. No matching by name (P3).
8. **The source stays the one source of truth for the engine** (ADR-0062 decision 10 is unchanged). The app writes a remembered value into the source's audio **inside the step that adds or retargets it**, so the step's undo record holds it and undo/redo restore exactly (ADR-0096 decision 1), and then calls `Studio::set_audio_offset`:
   - Add: the new device's entry, or 0.
   - Retarget (including the Source settings window, which changes device and settings in one step): if the source's offset equals the old device's entry (or there is no old entry and it is 0), it becomes the new device's entry, or 0. Otherwise it is kept. Because every committed edit updates the entry (decision 9), a value that differs from its device's entry exists only when several sources share one device and the user declined to apply the value to the others.
   - A value written by this fill is not a committed change: it never rewrites the entry and never triggers the offer of decision 9.
9. **A committed offset change updates the device's entry**: applying a measurement, a reset (which removes the entry), and an edit in the Sync offset row once it is committed (field commit or release, never per drag step, so the file is not written for every value). `source.set_sync_offset` from the control API updates it the same way, because both are the same command. Other sources of the same device are not changed silently: after a change, the row offers "Apply to N other sources" (one action, listed by name), and the API has a matching call (decision 10).

### Control API (next free minor version at merge)

10. For parity (ADR-0052):
    - `sync.measure_start { source, reference? }` (`control`) returns `{ measurement }` (a string id) or an error: `not_audio`, `not_picture`, `not_heard`, `source_closed`, `busy`, `no_reference`.
    - `sync.measure_cancel { measurement }` (`control`).
    - Topic `sync.measure.changed { measurement, state }`: `running { pairs }`, then `done { suggested_offset_ms, measured_offset_ms, effective_offset_ms, clamped, pairs, spread_ms }` or `failed { reason }` with the reasons of decision 5.
    - Applying uses the existing `source.set_sync_offset`; its permission follows whatever rule that method has (on-air edit checks when they land).
    - `sync.devices` (`read`) lists entries with their `key` objects; `sync.forget_device { key }` (the same object) and `sync.apply_to_device_sources { source }` (`control`, the same permission as `source.set_sync_offset` until scopes split them). `apply_to_device_sources` covers only sources of the current collection and applies, per source, the same on-air check as `source.set_sync_offset` once those checks land. Measuring changes no state, so it needs no more than applying does.

### Not decided here

Widening the offset range or delaying the picture when the sound is later than the advance limit allows; a virtual microphone and its output delay; continuous lip-sync monitoring. Each needs its own ADR.

## Consequences

- The user gets a value in about ten seconds instead of record-and-compare rounds, and keeps it across sources, collections and device changes.
- No cost when unused: no nodes, threads, polling or GPU resources until Measure, and the device file is read only when a device-backed audio source is added or retargeted, and written only on a committed change.
- New real-time code (the audio probe) and a per-new-frame GPU pass are hot-path code while they run: `hot-path-review` checklist, measurements and the owner's sign-off (P15), though they exist only during a measurement.
- Accuracy depends on the camera's frame rate. Target: within a quarter of a frame interval on synthetic tests; the real accuracy is measured on the Mac with a camera and microphone before the UI ships, and recorded here.
- Follow-up work: specs `audio-graph.md` (audio probe, effective offset in input stats), the nodes spec (picture probe), `recording.md` (`Studio` measurement calls and end reasons), `user-data.md` (`device-sync.toml`), `control-api.md` (methods, topic, reasons); tests: an end-to-end sign test through `InputNode` (early sound gives a positive suggestion), estimator property tests with synthetic contacts placed at known real times at random phases of the frame grid, with repeated frames, a frame rate drop, rebounding hands, jitter and missed events, asserting the error stays within a quarter of the frame interval, probe no-allocation and removal tests, each end reason, and device memory rules (fill inside the add/retarget step, retarget replacement rule, reset forgets, undo/redo exact); a UI proposal with dark and light screenshots (owner approval) for a Measure button on the Sync offset row and the measurement dialog.

## Alternatives considered

- **Cross-correlate speech with lip motion continuously.** Needs face tracking and runs all the time; costly and fragile. A clap is cheap, exact and on demand.
- **A flashing screen and beep from a phone app.** Needs a second device and an app. It still works here: a flash is a motion event.
- **Read back whole frames and analyse on the CPU.** Breaks P4 far more than one `f32` per frame.
- **Diff on every canvas tick.** A camera slower than the canvas repeats frames, giving false "motion fell" events; only new frames count.
- **Store the offset only on the device.** Would change ADR-0062 decision 10, the scene format and the CLI, and make a collection behave differently on another computer.
- **Remember per pair of camera and microphone.** More exact when one microphone serves several cameras, but rare and harder to explain; the entry records the reference so the UI can show it.

## Review

Independent review per ADR-0014.

- Round 1, 2026-11-16, independent AI session: changes requested. Blocking: (1) the sign explanation in decision 4 was backwards; (2) the picture time base was undefined and repeated frames would give false events. Should-fix: the readback fact (blocking API, use the download ring), retarget keeps the old offset, filling outside the add step breaks undo, the offset changing or being clamped mid-measurement, tap conditions and chain changes, filters hiding claps, control API gaps (ids, reasons, apply-to-others, permissions, write on commit), an underspecified estimator. Nits: reference defaulting, device-key stability, who drains the rings, the OBS claim. Resolved in this revision: worked sign examples and an end-to-end sign test; tick time of a new frame and a measured interval; the `RgbaDownload` ring with per-slot tick times; the retarget rule and filling inside the step; the effective offset and end reasons; the heard-by-program rule and chain-change endings; the filter hint; API ids, reasons, `sync.apply_to_device_sources` and permissions; the estimator and bias correction; the other nits.
- Round 2, 2026-11-16, same independent AI session: changes requested. Blocking: the picture event used the frame after the motion peak, about one frame late. Should-fix: claps rebound rather than stop; decisions 8 and 9 unclear together (whether a fill counts as a committed change); on-air rules for `sync.apply_to_device_sources`. Nits: key format with `|`, the IQR gate under a varying frame rate, when probe GPU resources are created. Resolved in this revision: the event is the peak frame at `T_k - Δ_k / 2`, with a peak-then-fall-or-minimum rule, rebound tests and a known-risk note; the fill is not a committed change and the "own value" case is stated; per-source on-air checks and collection scope; the key as three fields and an API object; the gate uses the median measured interval; resources created at prepare.
- Round 3, 2026-11-16, same independent AI session: **approve**. One nit (`sync.devices` keys are objects, as in decision 7) fixed in this revision. The ADR stays Proposed until the owner approves; the audio probe and the picture probe still need the hot-path review, measurements and the owner's sign-off when implemented.
- Owner approval: Salih, 2026-11-16 ("onaylıyorum" in the project thread). Status Accepted.
- Implementation note, 2026-11-16 (estimator step, not a change of decision): one pair is off by up to half a frame interval, so with six claps at random phases the estimate is within half an interval always and within a quarter in about nine sessions of ten, not every time. The spec states this and the tests assert it.

- 2026-11-16: decisions 4 and 5 are amended by [ADR-0130](0130-voice-filters-noise-suppression-compressor-limiter-eq.md) decision 4 (filter latency compensated by input advance; the heard alignment `h` replaces the effective offset in the suggestion and in `effective_offset_ms`).
