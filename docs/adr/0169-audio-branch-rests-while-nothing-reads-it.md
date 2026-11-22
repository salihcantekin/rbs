# ADR-0169: The audio branch rests while nothing reads it

- Status: Proposed
- Date: 2026-11-22
- Deciders: Salih (owner)
- Supersedes: [ADR-0041](0041-studio-recording-on-a-running-engine.md) decision 2 in part (the meter sink is no longer in the graph for the studio's whole life)

## Context

ADR-0041 decision 2 links a `MeterSinkNode` to the main canvas mixer's `main` output for the studio's whole life, so the mixer and its inputs are scheduled "for the meters" while nothing records. It named Standby for this sink (`set_meters(bool)`) as follow-up work, to be decided when the cost matters.

F-15 of the [2026-11 resource audit](../research/resource-and-structure-audit-2026-11.md) then took the meters themselves off when nobody reads them: the studio host calls `Studio::set_meters_on(false)` unless the Mixer is shown in a window that is not hidden or a control client subscribes to `audio.levels` (F-15a/F-15b). What still runs then, with no reader at all, is the whole audio branch: the audio partition's worker wakes about 100 times a second, every input node ingests its device stream and resamples, every audio filter (noise suppression included) processes, the mixer mixes, and the sink throws the block away. On macOS the microphone indicator stays on the whole time. That breaks P13 and ADR-0013 (Installed and Standby cost nothing); the owner asked for the branch to stop when nothing reads it and restart without a seam when a reader appears (2026-11-22).

Engine facts this builds on:

- `GraphDescription::plan` leaves out nodes with no path to an `Output` (`SkipReason::NoSink`). Every real reader of the mixer is already such an output: the audio encoders of recordings and streams, the monitor output, the loudness tap, the replay and hold chains, sync probes.
- A node that leaves the schedule is released and kept idle by its partition (`Controller::release_one`); it is not dropped. An audio input's `release` stops its stream and frees its FIFO; `prepare` starts it again. A media source's sound half stops playing when neither of its nodes is prepared (audio graph spec, "Media sound").
- A partition whose schedule is empty has no worker thread (ADR-0027); tick indices continue across a restart.
- The studio instantiates a source only when the plan schedules it, and keeps the instance while the source stays bound.

## Decision

1. **The studio's model keeps the sink; only the engine's graph drops it.** The studio's logical graph keeps the `MeterSinkNode` on the mixer's `main` for the studio's life, as ADR-0041 says, and all its bookkeeping follows the plan of that graph: which entries and filters are `scheduled`, which sources are instantiated (and get a `LevelMeter`, filter meters and an input offset), the strips, `apply_input_offsets`, `sources()` and `source_health()`. While the branch **rests**, the graph sent to the engine is that graph with the sink removed. A source added, replaced or removed while resting goes through `reconcile` as today, so it is opened, given its meter and compensated exactly as when the branch runs; only its nodes stay idle. Every graph the studio sends to the engine (its own and the trial graphs of monitor, loudness, separate files, sync probes, the virtual camera and break before make) goes through one private `Studio::send_graph(graph, drain)`, which strips the sink while resting and maps the apply's failed nodes onto the source, filter and picture entries; no `engine.apply*` call is left elsewhere in `studio/`. In this ADR and the recording spec, an entry's `scheduled` means scheduled by that model plan, not running.
2. **Two separate flags.** `Studio::set_meters_on(bool)` stays as it is: meters on or off in the mixer's parameters, nothing applied (F-15). A new `Studio::set_audio_awake(bool) -> Vec<StartFailure>` adds or removes the sink from the engine's graph and applies with a plain `apply` (the sink holds no data). A repeat of the current state applies nothing. `StudioSettings::audio_awake` (default `true`, so `rbs-cli`, `Recording` and existing callers see no change) gives the state at start.
3. **With no reader the branch rests.** Without the sink and with no other output on the mixer, the engine's plan leaves out the mixer, inputs, audio filters and media and browser sound halves that only feed it (`SkipReason::NoSink`). The engine releases them and keeps the instances; the audio worker stops (ADR-0027). If the last real reader leaves while resting (`stop_output`, a recording's stop, the monitor, loudness), the audio partition empties inside that `apply_draining`, so it runs its final tick in pipeline order before the inputs are released: the encoder flushes what it holds, then the worker stops.
4. **A reader brings it back without a rebuild.** Any output that reads the mixer (the sink, an encoder, the monitor, the loudness tap, a replay or hold chain, a sync probe, a plugin output later) schedules the branch through the normal apply. The kept instances are prepared again: input streams start (`prepare`, on the control thread, so it can wait for the device as a scene switch can today), an input that joins fades in over the strip ramp (`MixerNode::release` marks it fresh), and tick indices continue. Failures of that apply are returned and the host logs them (`Area::Audio`, `warn`).
5. **Resting is not a source state.** A resting source keeps its instance and is reported as it would be while running: `sources()` and `source_health()` list it with its last known health, so the Sources panel, the control API's source status and the Record and Go live silence check see no change. No UI text, glossary term or API field changes. Instantiating only opens a stream; it starts in `prepare`. So a busy, refused or unplugged device shows its fault when the branch next wakes, whichever apply wakes it (`set_audio_awake`, an output, the monitor, loudness, a probe), recorded by `send_graph`.
6. **Who wants the branch awake.** The host keeps it awake while meters are wanted (the window reads levels: the Mixer is shown in a window that is not hidden, or a Properties editor shows an audio filter's live meter, the window folding both into the one bool `SetMetersUi` already carries, no new command) or a control client subscribes to `audio.levels`. The studio does not know the reasons and no list of reader kinds is added (P16, ADR-0162); real readers are outputs in the graph and need no flag.
7. **A short grace before resting.** Meters come off at once (F-15b). The host calls `set_audio_awake(false)` only `AUDIO_REST_AFTER` (2 s, studio host spec) after the last meter reader left, measured on the host's injected clock and armed as one control-loop wake; a reader that comes back before cancels it. 2 s covers a tab switch or a quick minimise, so device streams do not stop and start for them, and keeps the idle cost of a hidden Mixer to two seconds of the branch.
8. **The host's state survives a studio swap.** The host passes its current awake state in `StudioSettings::audio_awake` on start and on `restart` (a canvas change), and a pending grace deadline is moved to the new engine clock as other host deadlines are. The host starts with the branch resting (`audio_awake: false`, meters off); the window's first `SetMetersUi(true)` wakes it.
9. **No hot-path change.** `process` of the mixer, inputs, filters and sink do not change, and no per-block check asks whether anything reads (rule 10). Only the control thread decides what is scheduled.

## Consequences

- With the Mixer hidden (or every window hidden) and nothing recording, streaming, monitoring or measuring, the audio worker thread, every device stream and every audio filter stop. Whether the macOS microphone indicator goes off with the instance kept and its stream stopped is checked on the Mac.
- A recording or stream that starts while resting begins with up to the device's start time of silence (tens of milliseconds on built-in devices, more on Bluetooth), filled as for an input that joins mid-recording. Measured with the device tests (owner: later).
- The macOS microphone permission prompt and a device's start failure move from studio start to the first wake of the branch.
- The input streams' stop and start cycle runs more often on real adapters (CoreAudio, WASAPI, PipeWire); it already ran when a scene switch left a mic out.
- Tests (cloud, TestClock and tick indices, no sleeps): `rbs-record`: resting leaves the audio partition without a worker and the inputs idle, while `sources()` still lists them; waking gives the worker back and meters move; a source added while resting is instantiated and has a meter; a recording started while resting records audio; the last recording stopped while resting flushes (`flush_lost == 0`) and the worker stops; a scene edit while resting keeps it resting; every attach and detach path (monitor, loudness, separate files, sync probe, virtual camera, outputs) run while resting leaves the audio partition without a worker unless it reads the mixer; a mic whose start fails shows its fault in `source_health()` when woken by `set_audio_awake(true)` and by `start_monitor`. `rbs-studio-host`: the grace expires and cancels on the injected clock; a `restart` while resting and during the grace keeps the state; start rests until the window reads levels.
- Specs: recording spec (`StudioSettings::audio_awake`, `set_audio_awake`, the studio's nodes, `stop_output`'s drain line), audio graph spec ("Meter sink"), studio host spec (who wants the branch awake, `AUDIO_REST_AFTER`). Doc comments of `sink.rs` and `meters_ui.rs`. `hot-paths.md`: idle audio-worker wakes before and after.
- P15: no hot-path code changes; the owner's sign-off still covers the measurement (audio worker wakes per second and idle CPU with the Mixer hidden, before and after), because the change decides what the audio thread runs.

## Alternatives considered

- **Keep the branch running and only skip the meters (today):** leaves a thread, device streams and every filter running for nothing. Rejected by the owner (2026-11-22).
- **Keep device streams open and only unschedule the mixer:** the stream callbacks and the input FIFOs keep running and the mic indicator stays on, for a restart that saves a few milliseconds. The input already stops its stream on `release`. Rejected.
- **A per-block check in the mixer that skips work when unread:** a hot-loop flag test that CLAUDE.md rule 10 forbids, and it would keep the worker waking. Rejected.
- **Drop the instances while resting:** a restart would reopen devices and rebuild filters (noise suppression state, latency compensation). Kept instances cost only memory already held. Rejected.
- **No grace:** a tab switch or a minimise would stop and start every device stream. Rejected for a 2 s wake on the control loop.
- **Drop the sink from the studio's own graph too:** every source added while resting would stay unopened and unreported, show as Idle in the Sources panel and the API, and have no meter for the silence check (review finding 4 to 6). Rejected.
- **One flag for meters and the sink:** the host could not take meters off at once and rest later. Rejected.

## Review

- 2026-11-22, independent AI session (ADR-0014; got the proposal, specs, principles and code only). Verdict: changes requested. Findings and what changed:
  1. A plain apply skipped `reconcile`'s bookkeeping (open, offsets, scheduled flags). Now the studio's model keeps the sink and only the engine graph drops it (decision 1).
  2. One flag could not take meters off at once and rest later. Split into `set_meters_on` and `set_audio_awake` (decision 2).
  3. `restart` and host start ignored the host's flag. `StudioSettings::audio_awake` on start and restart, deadline moved, host starts resting (decision 8).
  4. Resting sources would show as Idle in the UI and API. They keep their instance and report as running; no UI or API change (decision 5).
  5. The silence check would warn for a mic added while resting. Fixed by decision 1 (instance and meter made).
  6. The gate meter could not wake the branch. Fixed by decision 1 (filter instance made); the window folds it into `SetMetersUi` (decision 6).
  7. `stop_output` drain path newly empties audio. Named in decision 3 and tested.
  8. Grace value and testability. Named constant in the host spec, reason given, injected clock (decision 7).
  9. Prepare on the control thread and logging. Stated in decision 4.
  10. Doc comments, essential nodes, no closed list. Consequences and decision 6.
- 2026-11-22, same reviewer, re-review of the revision: findings 1 to 10 resolved; changes requested for two new points, now fixed:
  A. Many apply sites, several with trial graphs: one `send_graph` helper, no direct engine apply left in `studio/`, a test per attach and detach path (decision 1).
  B. A device fails in `prepare`, not at instantiation: decision 5 now says a resting source shows its last known health and a start failure appears at the next wake, recorded by `send_graph` on every path; permission prompt moves to the first wake (Consequences).
  C. `scheduled` means the model plan: stated in decision 1.
  The reviewer said they would approve with A and B fixed.
