# ADR-0040: A recording of a canvas is assembled by an engine crate; `rbs-cli record` is the first composition root that runs it

- Status: Proposed
- Date: 2026-11-03
- Deciders: Salih (owner)

## Context

Every piece of a local recording exists and has passed on Windows hardware on its own: capture nodes, the video export, the encoder and output nodes (ADR-0038), the audio input nodes, mixer and audio encoder node (ADR-0037), the scene source registry and the binder's audio routing (ADR-0039), NVENC, AMF and Media Foundation AAC, the Matroska and fMP4 muxers and the file transport. They are only joined in hardware-check examples, each wiring a different subset by hand. The owner's Windows-first plan (2026-11-03) asks for a headless `record` command next (screen, camera in a corner, microphone and system audio into an MKV or MP4 file, audio and video in sync), and then the same recording driven from `rbs-app`.

Forces:

- **P1:** engine code never names an adapter; the composition roots (`rbs-app`, `rbs-cli`) register them. Both roots need the same recording graph, and `rbs-cli` must not build Slint (ADR-0027).
- **P3:** adapters are chosen by capability, not by vendor name.
- **P13:** a recording that is not running costs nothing; an engine worker exists only while its partition schedules nodes (ADR-0027).
- **ADR-0026:** encoders and outputs run on their own partitions so a slow file write or encoder never stalls the render or audio thread.
- **ADR-0004:** one master clock; every stream's timestamps are master time, so sync is a property of the timestamps, not of arrival order.
- The roadmap's Phase 1 has headless recording; scene documents on disk and the control API come later.

## Decision

1. **A new engine-layer crate `rbs-record`** assembles and runs the recording of one canvas of a `SceneCollection` (spec [recording](../specs/recording.md)). It depends on `rbs-core`, `rbs-graph`, `rbs-engine`, `rbs-scene`, `rbs-sources`, `rbs-gpu`, `rbs-audio`, `rbs-nodes` and the codec, mux and transport ports; never on an adapter. `Recording::start` binds the scene (`SceneBinder` with audio, `SourceTypes::instantiate`), creates the compositor and the mixer, attaches the encoders and the output, adds the partitions to the engine it is given and applies; `Recording::finish` shuts the engine down in pipeline order and reports. The crate owns the engine for the recording's lifetime.
2. **Adapters arrive as port registries.** The caller hands over a `SourceTypes` and one `rbs_core::registry::Registry` per port (video encoders, audio encoders, muxers, transports), probed. `rbs-record` selects by capability: video encoders that list the codec, system-memory input and the export's pixel format; audio encoders that list the codec; the muxer that lists the container; the transport that lists the URL scheme and the container input. Candidates are tried best first (user choice, preference order, hardware first) until one opens. A composition root sets preference and user choice; it never branches on a vendor in `rbs-record`.
3. **Five partitions, fixed ids:** render (the canvas cadence: capture, compositor, video export), audio (block cadence with catch-up: inputs, mixer), video encoder (the canvas cadence a quarter frame later), audio encoder (doorbell: one tick per queued block) and output (doorbell). Five worker threads while recording, none before `start` or after `finish`.
4. **Sync by timestamps.** Output stream 0 is video, stream 1 audio. Video packets carry the canvas tick time, audio packets the block time (tick time minus the graph latency, the time of the block's first sample); both are master time, and the muxer places them on one origin. What the canvas shows at tick `t` is each source's newest frame, so video can lag audio by a source's capture latency (about a frame for screen capture, more for some cameras); per-source offsets are the follow-up, not a reason to re-time packets.
5. **`rbs-cli` is a new application crate** with a `rbs-cli` binary and a `record` subcommand. It registers the platform's adapters (Windows: WGC, Media Foundation camera, WASAPI, NVENC, AMF, Media Foundation AAC; Matroska, fMP4 and file everywhere), builds the scene in code (display full canvas, first camera in the bottom-right corner, default microphone, system audio) until scene documents can be loaded, and records until `--seconds` pass or Enter is pressed. It passes `OsThreadPolicy` to the engine (ADR-0030). Platforms without an encoder pair print that recording is not available there yet.
6. **One-shot for now.** A `Recording` starts with the engine and ends it. Starting and stopping a recording on an engine that keeps running (preview in `rbs-app`) adds a detach step to this crate when the app needs it; the partition layout and the selection rules stay.

## Consequences

- `rbs-app` reuses the crate: it registers the same adapters and calls `Recording::start` with the user's scene and settings.
- macOS and Linux need only adapter registration in the composition roots (ScreenCaptureKit, AVFoundation, Core Audio, VideoToolbox, AudioToolbox; PipeWire, VA-API and an AAC encoder on Linux).
- The end-of-stream gaps of the nodes remain: frames and audio the encoders still hold at shutdown are lost (about a frame of video and two AAC frames), until the engine has a drain hook (engine spec, open question).
- A source that fails to start (a camera in use) does not stop the recording; the report lists it. A failed encoder, export or output does.
- Tests run the whole graph on the reference adapters (synthetic capture, synthetic audio, fake encoders, a recording muxer) on a `TestClock`, and check the sync of the packets the output receives.

## Alternatives considered

- **Assemble the graph in `rbs-cli`:** `rbs-app` would duplicate it, and a library function in a binary crate cannot be shared without making `rbs-app` depend on `rbs-cli`.
- **Assemble it in `rbs-app` and run it headless with a flag:** pulls Slint and its MSRV into the headless path (rejected for the engine loop in ADR-0027 for the same reason).
- **Put it in `rbs-sources` or `rbs-nodes`:** those crates map one concept to nodes each; a recording joins all of them plus the engine, and would make them depend on `rbs-engine` and every port.
- **Name encoders in `rbs-record` (try NVENC, then AMF):** a vendor branch in engine code (P3); the registry's preference order already says the same thing as data.
- **Re-time audio to the first video packet:** hides real offsets instead of measuring them, and breaks when a stream starts late.

## Review

Pending: independent review per ADR-0014.
