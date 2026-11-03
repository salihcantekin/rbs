# Spec: Recording a canvas

- Status: Draft
- Owner crate(s): `rbs-record`, `rbs-cli`
- Related ADRs: [0004](../adr/0004-single-master-clock.md), [0013](../adr/0013-zero-cost-when-unused.md), [0026](../adr/0026-partitions-and-queues.md), [0027](../adr/0027-engine-loop.md), [0037](../adr/0037-audio-graph.md), [0038](../adr/0038-io-graph-nodes.md), [0039](../adr/0039-scene-source-types.md), [0040](../adr/0040-recording-composition.md)
- Builds on: [scene sources](scene-sources.md), [scene model](scene-model.md) (binder), [I/O nodes](io-nodes.md), [audio graph](audio-graph.md), [engine](engine.md), [output ports](output-ports.md)

## Purpose

Records the program scene of one canvas of a scene collection to a file: the sources the scene shows and plays, composited, encoded (video and audio) and muxed, with every stream on the master clock. `rbs-record` assembles and runs it on the engine; `rbs-cli record` is the headless command that registers the platform's adapters and builds a fixed scene.

## Scope

In scope: what `Recording::start` builds and how it chooses adapters, the partitions, stream order and timestamps, start and run-time failures, progress counters, the `rbs-cli` commands.

Out of scope: streaming (same graph, a network transport; later), several outputs per canvas, starting and stopping a recording on an engine that keeps running (the desktop app's step), scene documents on disk, the control API, end-of-stream flushing of encoders (engine spec, open question).

## Contract

```rust
// rbs_record
pub const RENDER: PartitionId = PartitionId::RENDER; // capture, compositor, video export
pub const AUDIO: PartitionId;          // 1: audio inputs, mixer
pub const VIDEO_ENCODER: PartitionId;  // 2
pub const AUDIO_ENCODER: PartitionId;  // 3
pub const OUTPUT: PartitionId;         // 4: muxer + transport

pub struct RecordAdapters {            // registries filled and probed by the composition root
    pub sources: SourceTypes,
    pub video_encoders: Registry<dyn VideoEncoderFactory>,
    pub audio_encoders: Registry<dyn AudioEncoderFactory>,
    pub muxers: Registry<dyn MuxerFactory>,
    pub transports: Registry<dyn TransportFactory>,
}
impl RecordAdapters { pub fn new() -> Self; pub fn probe(&mut self); }

pub struct RecordSettings {
    pub canvas: CanvasId, pub url: String, pub container: ContainerId,
    pub video: VideoSettings,            // codec H.264, CBR 12 Mbit/s, keyframe every 2 s, NV12
    pub audio: Option<AudioSettings>,    // AAC 160 kbit/s; None: video only
    pub audio_graph: AudioGraphConfig,   // default: 48 kHz stereo, 10 ms blocks, 50 ms latency
}
impl RecordSettings { pub fn new(canvas: CanvasId, url: impl Into<String>, container: ContainerId) -> Self; }

impl Recording {
    pub fn start(engine: Engine, gpu: Arc<GpuContext>, collection: &SceneCollection,
        adapters: &RecordAdapters, settings: &RecordSettings) -> Result<Recording, RecordError>;
    pub fn started(&self) -> &StartReport;  // chosen adapters, sources (adapter, target, failed)
    pub fn engine(&self) -> &Engine;
    pub fn node_name(&self, node: NodeId) -> Option<&str>;
    pub fn service(&mut self) -> Serviced;  // the engine's control loop, plus failure tracking
    pub fn failure(&self) -> Option<String>;
    pub fn progress(&self) -> Progress;     // plain counters; peaks since the previous call
    pub fn mixer_anomalies(&self) -> u64;
    pub fn finish(self) -> FinishReport;    // shutdown in pipeline order
}
pub fn scheme_of(url: &str) -> Option<&str>;
```

### The graph

```
sources --video--> compositor --canvas--> video export ==video==> video encoder ==packets==> output s0
 (render)           (render)              (render)                (video encoder)            (output)
audio inputs --audio--> mixer ==audio==> audio encoder ==packets==> output s1
 (audio)                (audio)          (audio encoder)
```

| Partition | Runtime | Pacing |
| --- | --- | --- |
| `RENDER` | `GpuRuntime` on the given device | the canvas frame rate from time 0 |
| `AUDIO` | `()` | `Pacing::stream(audio_graph.cadence(now), 4)` |
| `VIDEO_ENCODER` | `()` | the canvas frame rate, a quarter frame after render |
| `AUDIO_ENCODER` | `()` | doorbell (one tick per queued block), 100 ms heartbeat |
| `OUTPUT` | `()` | `Pacing::output()` |

### `Recording::start`

1. The canvas must exist (`UnknownCanvas`). The export is NV12 (or `video.format`) at the canvas size.
2. Adapters are chosen before anything opens, each from its registry's `select`, best first (user choice, preference order, hardware first, registration order):
   - video encoder: `codec.video.encode` includes the codec, `input.memory` includes `system`, `input.pixel_format` includes the export format; the first candidate whose `VideoEncoderNode::open` succeeds;
   - audio encoder (when `audio` is set): `codec.audio.encode` includes the codec; the first whose session is created for `audio_graph.format` and the bitrate;
   - muxer: `mux.container` includes the container; the best;
   - transport: `transport.schemes` includes the URL's scheme (lower case) and `transport.input` includes `container.transport_input()`; the best.
   No candidate: `NoAdapter { port, tried }`, with each rejected or failed adapter and why. A URL without a scheme is `InvalidUrl`.
3. The collection is bound with `SceneBinder::new(sources.node_types()).with_audio(AUDIO)`. Any `BindProblem` fails the start (`Problems`); the composition root builds scenes from installed types only.
4. Every created source is instantiated (`SourceTypes::instantiate` with the engine's clock and `audio_graph`); an instantiation error fails the start (`Source`). Video sources go to `RENDER`, audio-only sources to `AUDIO` (the binder chose).
5. The export, the video encoder and the output (streams: video, then audio) are added Active and linked to the program compositor of the canvas; the audio encoder to the canvas's mixer. The compositor's layers come from `node_layers` over the final plan; mixer strips from `report.mixers[canvas].inputs`, gain 1, each with a level meter, and a master meter.
6. The partitions are added and every instance adopted, then the graph is applied once. If the compositor, mixer, export, an encoder or the output did not join, or the engine reported errors or links that could not cross partitions, the engine is shut down and the start fails with `Start([(node, reason)])`. A source that did not join (a camera another program holds) is recorded in `StartReport::sources[i].failed` and the recording goes on without it.

### Running and stopping

- `service` runs `Engine::service` and reads its events: a `Failed` event of a source updates that source's `failed`; a permanent failure (`retry_at: None`) of an essential node sets `failure`. `failure()` also returns the output's first permanent failure cause (`OutputNodeStats::failure`). The composition root stops when `failure()` is set.
- `finish` shuts the engine down (sources first, the output last, which writes the container's trailer and finishes the transport) and returns the final `Progress`, `failure`, shutdown errors, `flush_lost` (packets the video encoder still held) and `finish_lost` (output items not delivered).

### Invariants

1. Engine code names no adapter: `rbs-record` depends on ports, node crates, the scene and the engine only (P1), and selects by capability (P3).
2. Nothing opens before `start` has chosen every adapter: a missing encoder, muxer or transport fails before any device or file is touched.
3. Video packet `pts` is the canvas tick time that produced the frame (on the canvas cadence from time 0); audio packet `pts` starts at the first mixed block's time (tick time minus the graph latency, the time of its first sample) and advances by exactly one codec frame. Both are master-clock time, so streams are in sync by timestamp; the muxer maps them to one origin.
4. Output stream 0 is video and stream 1 audio, in the order the output declares them to the muxer.
5. A recording that is not started holds no thread, device or file; after `finish` every worker has stopped and the transport is finished (P13).
6. `start` and `finish` run on the control thread. Nothing in `rbs-record` runs per frame or per block; the per-frame work is in the nodes ([I/O nodes](io-nodes.md), [audio graph](audio-graph.md)).

### `rbs-cli`

- `rbs-cli record <file.mkv|file.mp4> [options]`: `.mkv` is Matroska, `.mp4` fragmented MP4. Options: `--seconds N` (else until Enter), `--size WxH` (even, default 1920x1080), `--fps N` (default 60), `--bitrate KBPS` (default 12000), `--audio-bitrate KBPS` (default 160), `--encoder ID` (a video encoder adapter id or its last dotted part, set as the registry's user choice), `--display N`, `--camera N` (indices from `devices`), `--no-camera`, `--no-mic`, `--no-system-audio`, `--no-audio`. Bad arguments exit with code 2 and the usage text.
- The scene: one canvas of the given size and rate, one scene on air with the chosen display filling the canvas (cursor on, at the canvas rate), the camera at 1280x720 30 fps in a 16:9 rectangle 4/15 of the canvas wide, a margin of 1/40 of the width from the bottom-right corner, the default microphone and the default system audio. A type with no target is left out and said so; no display is an error.
- Adapters: Matroska, fMP4 and file on every platform; Windows adds WGC and Media Foundation capture, WASAPI, NVENC and AMF (in that order), Media Foundation AAC. A platform without video or audio encoders says recording is not available there yet.
- The engine gets `OsThreadPolicy` (ADR-0030). The device is `GpuRequest::default()` (high performance, no software adapter). The file path is made absolute and turned into a `file://` URL (`%`, space, `#`, `?` escaped).
- Exit code 0 when the recording ran and stopped cleanly with packets written; 1 when start failed, the recording failed while running, the engine did not stop cleanly, or no packet was written.
- `rbs-cli devices` lists installed source types with numbered targets, and every registered adapter as available or unavailable.

## Errors and failure behaviour

`RecordError` (non-exhaustive): `UnknownCanvas`, `InvalidUrl`, `Bind`, `Problems`, `NotBound`, `Source { name, error }`, `UnsupportedSource`, `NoAdapter { port, tried }`, `Graph`, `Engine`, `Start(nodes)`. Run-time failures do not panic or end the process: they are `failure()` and `StartReport::sources[i].failed`; the nodes' own restart rules apply (a source with a recoverable error restarts with back-off, ADR-0024; an output that wrote media never restarts, ADR-0038).

## Performance requirements

Control-thread only; the graph's hot paths are the nodes' and keep their budgets. Five worker threads while recording (`RENDER`, `AUDIO` with a raised class, two encoders, the output).

## Conformance tests

- `crates/rbs-record/tests/record.rs` (any GPU adapter, lavapipe on CI): a scene with the synthetic display, camera, microphone and loopback records through a fake H.264 encoder (after a candidate that fails to open), the Media Foundation AAC session over its fake engine and a logging muxer and transport, on a `TestClock` moved in 5 ms steps with every cadence worker waited for by tick count. Checks: chosen adapters; every source delivers; streams are H.264 then AAC; header, trailer and transport finish once; video `pts` strictly increasing and on the canvas cadence; audio `pts` one AAC frame apart with no gap; both streams start within the audio latency plus two frames of each other and end before the clock; no muxer, queue or encoder drops; no late ticks. A missing video encoder, an unlisted container and an unknown canvas fail before anything opens.
- `crates/rbs-cli` unit tests: argument parsing and refusals, file URLs, the scene (sources present, left out on request, bad indices refused, camera rectangle).
- Hardware (manual, Windows): `rbs-cli record <file> --seconds 20` to `.mkv` and `.mp4`, with NVENC and with `--encoder amf`; `ffprobe` streams, packet steps and a decode pass; an A/V sync measurement with a clip that flashes the screen white and beeps at the same time every second, played full screen while recording display and system audio.

## Open questions

- End of stream: the video encoder's held frames and the AAC encoder's last frames are lost at shutdown until the engine has a drain hook (engine spec).
- Capture latency: video content at tick `t` is each source's newest frame, so video lags audio by the source's latency; per-source sync offsets (a video offset in the scene model, the audio input's offset parameter) are the follow-up.
- Starting and stopping a recording while the engine keeps running (preview in the desktop app): detach the recording's nodes, finish the output, keep the scene bound.
- Several outputs per canvas (record and stream with one encoder) share the encoders through many-output packet pins.
- Ctrl+C in `rbs-cli` ends the process without a trailer (Matroska stays playable, without duration and cues); a console control handler that stops cleanly is a follow-up.
