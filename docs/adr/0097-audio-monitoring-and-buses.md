# ADR-0097: Audio monitoring and buses

- Status: Proposed
- Date: 2026-11-10
- Deciders: Salih (owner)
- Spec: [audio-graph](../specs/audio-graph.md), [audio-io-port](../specs/audio-io-port.md), [scene-model](../specs/scene-model.md), [control-api](../specs/control-api.md)
- Builds on: [ADR-0037](0037-audio-graph.md), [ADR-0081](0081-master-strip-in-the-mixer.md), [ADR-0082](0082-audio-filter-chains-on-sources.md), [ADR-0095](0095-audio-ducker-and-key-tap.md), [ADR-0013](0013-zero-cost-when-unused.md)

## Context

Every audio source goes to one mix, `main`, whose master strip feeds the encoders. Two everyday needs are missing: hearing a source or the mix in headphones without sending it to the stream (monitoring), and sending different sources to different outputs, for example a stream mix and a recording or "VOD safe" mix without music (buses). The audio-graph spec lists both as out of scope; audio-io-port.md reserves `AudioBackend::open_output` as a later minor version. The owner approved the proposal (2026-11-10): monitoring first, send levels (not only on/off), and an output picks one bus for now.

## Decision

### Part 1: monitoring (F1)

1. **Audio output in the port.** `AudioBackend::open_output(OutputRequest) -> Result<Box<dyn AudioOutput>, AudioError>` with a default returning `Unsupported`; capability `audio.output` (Flag) and `audio.output.latency_ns` (Int, the adapter's best estimate). `OutputRequest` names an `EndpointId` of a new `EndpointKind::Output` (render endpoints for playback; `OutputLoopback` stays the capture of a render endpoint), a format and a block size. `AudioOutput` is pull-driven: the adapter's render callback takes interleaved f32 blocks from a lock-free ring the engine fills; underrun plays silence and is counted. No allocation, blocking or locks in the callback (P6). Adapters: WASAPI render (shared mode) and CoreAudio output; `rbs-audio-io-test` gets a reference output with a controllable clock. Conformance tests cover listing, open, underrun, device loss and close.
2. **Monitor device.** One monitor device is chosen in Settings (default: the system default output). If it is the endpoint a `Display` companion records from, the UI warns about feedback; the engine does not block it.
3. **Per-strip monitor mode.** A source has `monitor: Off | Only | Also` (default Off): `Only` is heard in the monitor and leaves the mix; `Also` is heard in both. Solo (listen to one strip; the mix is unchanged) is a session state, not saved. The monitor mix is the strips' post-fader signal, summed by a monitor sum node fed by the same blocks as the mix, so it cannot drift from the stream. Monitor volume is one setting.
4. **Zero cost (P13).** With every strip Off and no solo, no monitor device is opened, no ring or thread exists and the scheduled graph has no monitor node. The first monitored strip builds the node and opens the device (Standby to Active); the last one tears them down. Enumeration of output devices runs only while the Settings picker is open.

### Part 2: buses (F2)

5. **Buses in the scene model.** The collection keeps an ordered list of buses; `main` always exists and cannot be removed. A bus has a name, volume, mute and a chain of audio filters (ADR-0082 `Filter` model, unchanged). A source has sends: per bus a level (linear gain, 0 = no send), default `main` at 1.0 and every other bus at 0. Sends are post-fader and pre the bus's own fader. The ducker key stays pre-fader per source (ADR-0095) and does not depend on buses. Removing a bus drops its sends and any output reference to it in one undo record. Scene format number is bumped for the new data (older readers would lose sends); the migration is a loader step that creates `main` with the old master volume and mute (ADR-0081), and the number is decided and recorded with the implementation PR.
6. **Graph.** The binder builds one summing node per bus that has at least one non-zero send or is used by an output (rule 10: a bus with neither costs nothing). `main` is unchanged for a project that never adds a bus. The mixer (hot path, P15) changes from one sum to N sums over the same input blocks; per-block work for a project with only `main` must not grow, and the numbers prove it.
7. **Outputs choose a bus.** A recording or stream output takes the audio of one bus (default `main`). Multi-track containers (one bus per track) are a later change tied to the container work, not decided here.
8. **UI and API.** Monitoring first, then buses; each has a visual proposal and screenshots in the three themes before merge (rule 16). Control API: `monitor.*` and `bus.*` calls, `source.set_sends`, `source.set_monitor`, each the next free minor at merge, with the audio-graph, audio-io-port, scene-model and control-api specs updated in the same changes.
9. **Staging.** (a) this ADR, reviewed; (b) F1: port `open_output`, test adapter, CoreAudio and WASAPI output adapters (device tests on the Mac and the Windows machine, one device slot, numbered steps), `rbs-app` monitor node and setting, monitor UI; (c) F2: scene model and loader migration, binder and mixer (bench and P15 sign-off), API, bus UI.

## Consequences

- Headphone monitoring and per-output mixes arrive without changing how a single-mix project behaves or costs.
- The port gains audio output once; it also serves a later virtual microphone output.
- The mixer hot path changes in F2: needs before/after measurements and a human maintainer's approval.
- The scene format and control API grow. Round-trip and loader migration tests are required.
- Monitoring through a device that is also captured can echo; the UI warns, the engine does not block.

## Alternatives considered

- **Monitoring as a bus.** Couples headphone latency and device handling to bus mixing; the monitor has a different consumer (a device callback) and must stay absent when unused.
- **Send on/off only (like OBS tracks).** Cannot sit a strip lower in one mix; the level costs one multiply per send and nothing when the send is 0.
- **Per-output mixes without buses (a mask per output).** No bus fader or filters, so "limiter only on the stream" is impossible.
- **Doing both in one change.** The port and device work (F1) and the mixer hot-path work (F2) have different risks and reviewers.

## Review

Pending: needs an independent review (new port method and capabilities, scene format, control API, mixer hot path) before it can be Accepted.
