# ADR-0040: A recording of a canvas is assembled by an engine crate; `rbs-cli record` is the first composition root that runs it

- Status: Accepted
- Date: 2026-11-03
- Deciders: Salih (owner)

## Context

Every piece of a local recording exists and has passed on Windows hardware on its own: capture nodes, the video export, the encoder and output nodes (ADR-0038), the audio input nodes, mixer and audio encoder node (ADR-0037), the scene source registry and the binder's audio routing (ADR-0039), NVENC, AMF and Media Foundation AAC, the Matroska and fMP4 muxers and the file transport. They are only joined in hardware-check examples, each wiring a different subset by hand. The owner's Windows-first plan (2026-11-03) asks for a headless `record` command next (screen, camera in a corner, microphone and system audio into an MKV or MP4 file, audio and video in sync), and then the same recording driven from `rbs-app`.

Forces:

- **P1:** engine code never names an adapter; the composition roots (`rbs-app`, `rbs-cli`) register them. Both roots need the same recording graph, and `rbs-cli` must not build Slint (ADR-0027).
- **P3:** adapters are chosen by capability, not by vendor name.
- **P4 exception (ADR-0038):** NVENC and AMF take system memory, so the export reads the canvas back to the CPU; that exception ends when the export picks native or readback by the memory kinds the encoder declares.
- **P13:** a recording that is not running costs nothing; an engine worker exists only while its partition schedules nodes (ADR-0027).
- **ADR-0026:** encoders and outputs run on their own partitions so a slow file write or encoder never stalls the render or audio thread.
- **ADR-0004:** one master clock; every stream's timestamps are master time, so sync is a property of the timestamps, not of arrival order.
- The roadmap's Phase 1 has headless recording; scene documents on disk and the control API come later.

## Decision

1. **A new engine-layer crate `rbs-record`** assembles and runs the recording of one canvas of a `SceneCollection` (spec [recording](../specs/recording.md)). It depends on `rbs-core`, `rbs-graph`, `rbs-engine`, `rbs-scene`, `rbs-sources`, `rbs-gpu`, `rbs-audio`, `rbs-nodes` and the codec, mux and transport ports; never on an adapter (its tests use the reference adapters and, as a dev-dependency only, the Media Foundation AAC crate's `testing` feature: the real AAC session logic over a fake engine, the only AAC encoder that runs on every platform). `Recording::start` binds the scene (`SceneBinder`, with audio when audio is recorded), attaches the encoders and the output to the canvas's program compositor and mixer, instantiates only the sources the output's plan schedules (what the program scene shows and plays), adds the partitions to the engine it is given and applies; `Recording::finish` shuts the engine down in pipeline order and reports. Binder problems fail the start only when they leave a recorded source out; problems elsewhere in the collection are reported. The crate owns the engine for the recording's lifetime.
2. **Adapters arrive as port registries.** The caller hands over a `SourceTypes` and one `rbs_core::registry::Registry` per port (video encoders, audio encoders, muxers, transports), probed. `rbs-record` selects by capability: video encoders that list the codec, system-memory input and the export's pixel format; audio encoders that list the codec; the muxer that lists the container; the transport that lists the URL scheme and the container input. Candidates are tried best first (user choice, preference order, hardware first) until one opens. A composition root sets preference and user choice; it never branches on a vendor in `rbs-record`.
3. **Five partitions:** render (`PartitionId::RENDER`, where the binder puts video sources; the canvas cadence: capture, compositor, video export), audio (block cadence with catch-up: inputs, mixer), video encoder (the canvas cadence a quarter frame later), audio encoder (doorbell: one tick per queued block) and output (doorbell). The four non-render ids are settings (defaults 1 to 4), so a caller with other partitions can choose free ids. Five worker threads while recording (three without audio: the audio partitions are not added), none before `start` or after `finish`.
4. **Sync by timestamps.** Output stream 0 is video, stream 1 audio. Video packets carry the canvas tick time, audio packets the block time (tick time minus the graph latency, the time of the block's first sample); both are master time, and the muxer places them on one origin. Two systematic offsets remain and are measured, not hidden: what the canvas shows at tick `t` is each source's newest frame, so video lags audio by the source's capture latency (about a frame for screen capture, more for some cameras); and the AAC encoder's priming is not signalled in the container (output ports spec), so a decoder that does not trim it plays audio late by the priming (1024 to 2112 samples, 21 to 44 ms at 48 kHz). Per-source offsets and priming signalling are the follow-ups, not a reason to re-time packets.
5. **`rbs-cli` is a new application crate** with a `rbs-cli` binary and a `record` subcommand. It registers the platform's adapters (Windows: WGC, Media Foundation camera, WASAPI, NVENC, AMF, Media Foundation AAC; Matroska, fMP4 and file everywhere), builds the scene in code (display full canvas, first camera in the bottom-right corner, default microphone, system audio) until scene documents can be loaded, and records until `--seconds` pass or Enter is pressed. It passes `OsThreadPolicy` to the engine (ADR-0030). Platforms without an encoder pair print that recording is not available there yet.
6. **One-shot for now.** A `Recording` takes an engine with none of its partitions, owns it, and ends it. `rbs-app` keeps an engine running for the preview and records on it, so its step adds an `attach` entry point to this crate that adds a recording's nodes to a graph, binder and engine the caller owns (partition ids passed in, as here) and a `detach` that finishes the output and removes them; `start` stays as the one-shot convenience on top. The partition layout, the selection rules, the stream order and the failure rules stay.
7. **Semver.** The workspace is pre-1.0 and unpublished; `rbs-record`'s settings and reports are `#[non_exhaustive]` so counters and options can be added without breaking callers, and the `attach` step may change `start`'s signature, which only the composition roots call.

## Consequences

- `rbs-app` reuses the crate's assembly and selection through the `attach` step of decision 6 (its signature is new; today's `start` does not fit an engine that keeps running).
- macOS and Linux need only adapter registration in the composition roots (ScreenCaptureKit, AVFoundation, Core Audio, VideoToolbox, AudioToolbox; PipeWire, VA-API and an AAC encoder on Linux).
- The end-of-stream gaps of the nodes remain until the engine has a drain hook (engine spec, open question): frames the video encoder holds (counted as `flush_lost`; more with encoder look-ahead), up to three frames in the export's readback ring and the AAC encoder's last frames are lost at shutdown (measured on Windows: 1 to 8 video frames).
- A source that fails to start (a camera in use) does not stop the recording; the report lists it. A failed encoder, export or output does; the output may already have opened the destination by then, so the composition root removes a file it created (`rbs-cli` does, and refuses to replace an existing file without `--overwrite`).
- Tests run the whole graph on the reference adapters (synthetic capture, synthetic audio, fake encoders, a recording muxer) on a `TestClock`, and check the sync of the packets the output receives.

## Alternatives considered

- **Assemble the graph in `rbs-cli`:** `rbs-app` would duplicate it, and a library function in a binary crate cannot be shared without making `rbs-app` depend on `rbs-cli`.
- **Assemble it in `rbs-app` and run it headless with a flag:** pulls Slint and its MSRV into the headless path (rejected for the engine loop in ADR-0027 for the same reason).
- **Put it in `rbs-sources` or `rbs-nodes`:** those crates map one concept to nodes each; a recording joins all of them plus the engine, and would make them depend on `rbs-engine` and every port.
- **Name encoders in `rbs-record` (try NVENC, then AMF):** a vendor branch in engine code (P3); the registry's preference order already says the same thing as data.
- **Re-time audio to the first video packet:** hides real offsets instead of measuring them, and breaks when a stream starts late.

## Review

- 2026-11-03, independent AI session (given only this ADR, the specs and the code, per ADR-0014): **changes requested**, 18 findings, all resolved in the same change:
  1. The sync test checked a window, not the timestamps: it now starts the clock off the frame grid and checks video `pts` exactly on the canvas cadence and the first audio `pts` plus latency exactly on the audio cadence.
  2. GPU tests were outside the nextest `gpu` group: `rbs-record` added.
  3. Fixed partition ids would clash with an engine the caller owns: the non-render ids are settings (`Partitions`); decision 6 now names the `attach`/`detach` step `rbs-app` needs.
  4. AAC priming was not stated as a residual offset: decision 4.
  5. `audio: None` still bound audio and added audio partitions: audio routing and partitions are added only with audio.
  6. Every source in the collection was opened and any binder problem was fatal: only planned sources are instantiated, and problems are fatal only for recorded sources (`StartReport::problems`).
  7. A failed start could leave a created file: `rbs-cli` removes it, refuses an existing file without `--overwrite`, and `Start` lists shutdown errors.
  8. Source state was matched by position and `failed` never cleared: keyed by `NodeId`, cleared on `Restarted`/`Recovered`.
  9. Stringly typed errors and exhaustive public types: `Port` enum, `StartFailure`, `#[non_exhaustive]`, decision 7.
  10. File URLs were built by string replacement: built from path components (drive, UNC, verbatim), with a round trip through the file transport in tests.
  11. The CPU readback was not tied to its exception: the P4 force cites ADR-0038.
  12. Tail loss was understated: consequences give the measured 1 to 8 frames.
  13. The test hang guard was long and the polling was unclear: 20 s guard; `yield_now` stays as condition polling (ADR-0032).
  14. The early-failure test did not prove nothing opened: it asserts no capture buffer and no audio stream.
  15. The `rbs-codec-mf` dev-dependency was unexplained: justified in decision 1.
  16. `--encoder` fell back to other encoders silently: only that encoder is used.
  17. A script without `--seconds` recorded forever: refused when stdin is not a terminal.
  18. Doc fixes; `packets_written` counted packets the muxer dropped: `packets_taken` and `packets_muxed()`.
- 2026-11-03: marked Accepted by the implementing thread under the owner's standing ADR policy; Windows hardware pass recorded in the spec.
