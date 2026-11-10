# Spec: Recording a canvas

- Status: Draft
- Owner crate(s): `rbs-record`, `rbs-cli`
- Related ADRs: [0004](../adr/0004-single-master-clock.md), [0013](../adr/0013-zero-cost-when-unused.md), [0026](../adr/0026-partitions-and-queues.md), [0027](../adr/0027-engine-loop.md), [0037](../adr/0037-audio-graph.md), [0038](../adr/0038-io-graph-nodes.md), [0039](../adr/0039-scene-source-types.md), [0040](../adr/0040-recording-composition.md), [0041](../adr/0041-studio-recording-on-a-running-engine.md), [0047](../adr/0047-several-outputs-per-canvas.md)
- Builds on: [scene sources](scene-sources.md), [scene model](scene-model.md) (binder), [I/O nodes](io-nodes.md), [audio graph](audio-graph.md), [engine](engine.md), [output ports](output-ports.md)

## Purpose

Records the program scene of one canvas of a scene collection to a file, or streams it live: the sources the scene shows and plays, composited, encoded (video and audio) and muxed (or handed as packets to a transport that frames them, such as RTMP), with every stream on the master clock. `rbs-record` assembles and runs it on the engine; `rbs-cli record` and `rbs-cli stream` are the headless commands that register the platform's adapters and build a fixed scene. A `Studio` keeps the canvas running for a UI (preview, meters, scene edits) and starts and stops outputs (recordings and live streams, up to two at once, ADR-0047) on the running engine (ADR-0041); `Recording` is a studio without a preview plus one recording.

## Scope

In scope: what `Recording::start` builds and how it chooses adapters, the partitions, stream order and timestamps, start and run-time failures, progress counters, the `rbs-cli` commands; the `Studio`: preview, meters, scene edits and outputs started and stopped on a running engine, several at once (recording and streaming), with encoders shared when their settings match.

A live stream is the same graph with a network transport: the output's URL, transport input, secret and reconnect policy differ, nothing else.

Out of scope: more than `MAX_OUTPUTS` outputs, recording and streaming at once from `rbs-cli`, scene documents on disk, the control API, end-of-stream flushing of encoders (engine spec, open question).

## Contract

```rust
// rbs_record
pub const RENDER: PartitionId = PartitionId::RENDER; // capture, compositor, video export
pub const AUDIO: PartitionId;          // 1: audio inputs, mixer (default)
pub const VIDEO_ENCODER: PartitionId;  // 2 (default)
pub const AUDIO_ENCODER: PartitionId;  // 3 (default)
pub const OUTPUT: PartitionId;         // 4: muxer + transport (default)
pub const VIDEO_ENCODER_2: PartitionId; // 5: the second output slot's (ADR-0047)
pub const AUDIO_ENCODER_2: PartitionId; // 6
pub const OUTPUT_2: PartitionId;        // 7

#[non_exhaustive] pub enum Port { VideoEncoder, AudioEncoder, Muxer, Transport } // Display

pub struct RecordAdapters {            // registries filled and probed by the composition root
    pub sources: SourceTypes,
    pub video_encoders: Registry<dyn VideoEncoderFactory>,
    pub audio_encoders: Registry<dyn AudioEncoderFactory>,
    pub muxers: Registry<dyn MuxerFactory>,
    pub transports: Registry<dyn TransportFactory>,
}
impl RecordAdapters { pub fn new() -> Self; pub fn probe(&mut self); }

#[non_exhaustive] pub struct Partitions { // the non-render partition ids; defaults are the constants above
    pub audio: PartitionId, pub video_encoder: PartitionId,
    pub audio_encoder: PartitionId, pub output: PartitionId, // the first output slot
    pub second: OutputPartitions,        // the second slot: VIDEO_ENCODER_2, AUDIO_ENCODER_2, OUTPUT_2
}
#[non_exhaustive] pub struct OutputPartitions {
    pub video_encoder: PartitionId, pub audio_encoder: PartitionId, pub output: PartitionId,
}

#[non_exhaustive] pub struct RecordSettings {
    pub canvas: CanvasId, pub url: String,    // never holds the secret
    pub input: TransportInput,           // Container(c): a muxer's chunks; Packets: no muxer (RTMP)
    pub secret: Option<Secret>,          // stream key or stream id, to the transport only; None
    pub reconnect: ReconnectPolicy,      // Off
    pub transport_options: Vec<(String, String)>, // opaque, passed through (SRT latency_ms); empty
    pub video: VideoSettings,            // codec H.264, CBR 12 Mbit/s, keyframe every 2 s, NV12
    pub audio: Option<AudioSettings>,    // AAC 160 kbit/s; None: video only
    pub audio_graph: AudioGraphConfig,   // default: 48 kHz stereo, 10 ms blocks, 50 ms latency
    pub partitions: Partitions,
    pub audio_offsets: BTreeMap<SourceId, i64>, // sync offset ns per audio source; empty
}
impl RecordSettings {
    pub fn new(canvas: CanvasId, url: impl Into<String>, container: ContainerId) -> Self;
    pub fn packets(canvas: CanvasId, url: impl Into<String>) -> Self;  // input Packets
    pub fn from_output(canvas: CanvasId, out: OutputSettings) -> Self;
}

#[non_exhaustive] pub struct VideoSettings { // every encoder setting that changes quality
    pub codec: CodecId,                  // H.264
    pub rate_control: RateControl,       // CBR 12 Mbit/s; or CBR padded with filler data, VBR target + peak, constant quality 0..=51
    pub keyframe_interval: KeyframeInterval, // 2 s
    pub max_b_frames: u32,               // 0; an encoder with a lower codec.video.b_frames.max uses fewer
    pub profile: Option<String>,         // None: the encoder's default (H.264 High)
    pub preset: EncoderPreset,           // Balanced
    pub format: YuvFormat,               // NV12 (P010 for 10-bit)
}

impl Recording {
    pub fn start(engine: Engine, gpu: Arc<GpuContext>, collection: &SceneCollection,
        adapters: &RecordAdapters, settings: &RecordSettings) -> Result<Recording, RecordError>;
    pub fn started(&self) -> &StartReport;  // chosen adapters, sources (node, adapter, target, failed), problems
    pub fn engine(&self) -> &Engine;
    pub fn node_name(&self, node: NodeId) -> Option<&str>;
    pub fn service(&mut self) -> Serviced;  // the engine's control loop, plus failure tracking
    pub fn failure(&self) -> Option<String>;
    pub fn progress(&self) -> Progress;     // plain counters; peaks since the previous call; packets_muxed()
    pub fn mixer_anomalies(&self) -> u64;
    pub fn end(&mut self);                  // end the video now; keep servicing until tail_done (ADR-0045)
    pub fn tail_done(&self) -> bool;
    pub fn finish(self) -> FinishReport;    // shutdown in pipeline order
}
pub fn scheme_of(url: &str) -> Option<&str>;

// The studio (ADR-0041, ADR-0047)
pub const MAX_LAYERS: usize = 64;      // layers and strips held without allocating
pub const END_TAIL_LIMIT: Duration;    // 500 ms on the master clock (ADR-0045)
pub const MAX_OUTPUTS: usize = 2;      // outputs running at once
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)] pub struct OutputId(/* private */);
#[non_exhaustive] pub struct StudioSettings {
    pub canvas: CanvasId, pub audio: bool, pub audio_graph: AudioGraphConfig,
    pub partitions: Partitions, pub preview: Option<PreviewDesc>,
    pub audio_offsets: BTreeMap<SourceId, i64>, // sync offset ns per audio source; empty
    pub scene_preview: bool, // ADR-0061: in studio mode, the preview scene's own picture; off
    pub extra_canvases: Vec<ExtraCanvas>, // ADR-0093: further canvases beside `canvas`; empty
}
pub const MAX_CANVASES: usize = 2;         // the main canvas included
#[non_exhaustive] pub struct ExtraCanvas { pub canvas: CanvasId, pub preview: Option<PreviewDesc> } // new(canvas).with_preview(desc)
impl StudioSettings { pub fn new(canvas: CanvasId) -> Self; } // audio on, no preview
#[non_exhaustive] pub struct OutputSettings { // a recording or a live stream
    pub url: String, pub input: TransportInput,
    pub secret: Option<Secret>, pub reconnect: ReconnectPolicy,
    pub transport_options: Vec<(String, String)>,
    pub video: VideoSettings, pub audio: Option<AudioSettings>,
}
impl OutputSettings {
    pub fn new(url: impl Into<String>, container: ContainerId) -> Self; // container input
    pub fn packets(url: impl Into<String>) -> Self;                    // packet input
    pub fn container(&self) -> Option<ContainerId>;                   // None for packets
}
impl RecordSettings { pub fn studio(&self) -> StudioSettings; pub fn output(&self) -> OutputSettings; }
#[non_exhaustive] pub struct Updated { pub problems: Vec<BindProblem>, pub failed: Vec<StartFailure> }
#[non_exhaustive] #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Link { pub connected: bool, pub connects: u64, pub reconnect_attempts: u64, pub transport_drops: u64 }

impl Studio {
    pub fn start(engine: Engine, gpu: Arc<GpuContext>, collection: &SceneCollection,
        sources: &SourceTypes, settings: &StudioSettings) -> Result<Studio, RecordError>;
    pub fn update(&mut self, collection: &SceneCollection, sources: &SourceTypes)
        -> Result<Updated, RecordError>;
    pub fn set_mix(&mut self, collection: &SceneCollection) -> bool; // ADR-0059: strips' volume and mute only, no bind; false: no mixer
    pub fn start_output(&mut self, adapters: &RecordAdapters, out: &OutputSettings)
        -> Result<OutputId, RecordError>;
    pub fn end_output(&mut self, id: OutputId) -> bool;  // ADR-0045; false when id does not run
    pub fn output_tail_done(&self, id: OutputId) -> bool; // true when id does not run
    pub fn stop_output(&mut self, id: OutputId) -> Option<FinishReport>;
    pub fn outputs(&self) -> Vec<OutputId>;           // running, in start order
    pub fn output(&self, id: OutputId) -> Option<&StartReport>;
    pub fn output_failure(&self, id: OutputId) -> Option<String>;
    pub fn output_progress(&self, id: OutputId) -> Option<Progress>; // progress() plus its counters
    pub fn output_counters(&self, id: OutputId) -> Option<Progress>; // its counters and late ticks only: no sources, takes no meter peak
    pub fn service(&mut self) -> Serviced;
    pub fn failure(&self) -> Option<String>;          // a studio node failed for good
    pub fn sources(&self) -> Vec<SourceState>;        // scheduled sources, binder order
    pub fn problems(&self) -> &[BindProblem];
    pub fn waiting(&self) -> bool;                  // a replacement waits for its predecessor to close
    pub fn progress(&self) -> Progress;               // sources and workers only
    pub fn link(&self, id: OutputId) -> Option<Link>; // an output's connection; reads no meter
    pub fn preview(&self) -> Option<PreviewEndpoint>; // once the preview node is prepared
    pub fn preview_stats(&self) -> Option<&Arc<PreviewNodeStats>>;
    pub fn scene_preview(&self) -> Option<PreviewEndpoint>; // ADR-0061: the preview scene's picture
    pub fn studio_mode(&self) -> bool; // the canvas has a preview scene
    pub fn canvas_preview(&self, canvas: CanvasId) -> Option<PreviewEndpoint>; // ADR-0093: a further canvas's picture
    pub fn canvas_preview_stats(&self, canvas: CanvasId) -> Option<&Arc<PreviewNodeStats>>;
    pub fn meter(&self, source: SourceId) -> Option<&Arc<LevelMeter>>;
    pub fn set_audio_offset(&mut self, source: SourceId, offset_ns: i64) -> bool; // true: a running input got it
    pub fn master_meter(&self) -> Option<&Arc<LevelMeter>>;
    pub fn mixer_anomalies(&self) -> u64;
    pub fn node_name(&self, node: NodeId) -> Option<&str>;
    pub fn settings(&self) -> &StudioSettings;
    pub fn engine(&self) -> &Engine;
    pub fn start_virtual_camera(&mut self, factory: &dyn VirtualCameraFactory,
        settings: VirtualCameraSettings) -> Result<(), VcamError>; // ADR-0066
    pub fn stop_virtual_camera(&mut self) -> bool;    // false when not sending
    pub fn virtual_camera(&self) -> Option<VirtualCameraState>; // while sending
    pub fn shutdown(&mut self) -> (Vec<(OutputId, FinishReport)>, Vec<String>);
}
pub struct VirtualCameraSettings { pub width: u32, pub height: u32, pub rate: FrameRate } // ::new(w, h, rate)
pub struct VirtualCameraState { pub config: VirtualCameraConfig,
    pub stats: Arc<VirtualCameraNodeStats>, pub failure: Option<String> }
impl Recording { pub fn studio(&self) -> &Studio; }
// RecordError gains TooManyOutputs and OutputFinishing (ADR-0047; they replace ADR-0041's AlreadyRecording).
// StartReport gains video_shared and audio_shared: the encoder ran already for another output.
```

Settings and report types are `#[non_exhaustive]` (ADR-0040 decision 7): callers start from `new`/`default` and set fields.

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
| `partitions.audio` | `()` | `Pacing::stream(audio_graph.cadence(now), 4)` |
| `partitions.video_encoder` | `()` | the canvas frame rate, a quarter frame after render |
| `partitions.audio_encoder` | `()` | doorbell (one tick per queued block), 100 ms heartbeat |
| `partitions.output` | `()` | `Pacing::output()` |

Without audio (`audio: None`) the two audio partitions are not added: three worker threads.

### `Recording::start`

1. The canvas must exist (`UnknownCanvas`) and have a program scene (`NoProgram`). The export is NV12 (or `video.format`) at the canvas size.
2. Adapters are chosen before anything opens, each from its registry's `select`, best first (user choice, preference order, hardware first, registration order):
   - video encoder: `codec.video.encode` includes the codec, `input.memory` includes `system`, `input.pixel_format` includes the export format; the first candidate whose `VideoEncoderNode::open` succeeds with every `VideoSettings` field in its `VideoEncoderConfig` (rate control, keyframe interval, B-frames, profile, `tuning.preset`), so a candidate that refuses a setting (a profile it lacks, `cbr_padded` or constant quality on an encoder without it) is skipped with its reason in `tried`;
   - audio encoder (when `audio` is set): `codec.audio.encode` includes the codec; the first whose session is created for `audio_graph.format` and the bitrate;
   - muxer (container input only; none for packet input, `StartReport::muxer` is `None`): `mux.container` includes the container; the best;
   - transport: `transport.schemes` includes the URL's scheme (lower case) and `transport.input` includes the input's capability value (`packets`, or `container.transport_input()`); the best. It is created with the URL, input, secret, reconnect policy and options of the settings and the default queue limits (output ports spec).
   No candidate: `NoAdapter { port, tried }`, with each rejected or failed adapter and why. A URL without a scheme is `InvalidUrl`.
3. The collection is bound with `SceneBinder::new(sources.node_types())`, `.with_audio(partitions.audio)` only when audio is recorded. A `BindProblem` fails the start (`Problems`) only when it leaves out a source the program scene shows or, with audio, plays: an unknown source type, or a source with no video output when video is what it contributes (an audio-only source without audio is simply not recorded). Problems anywhere else in the collection go to `StartReport::problems` and the recording goes on.
4. Only the sources the output's plan schedules are instantiated (`SourceTypes::instantiate` with the engine's clock and `audio_graph`), in plan order: sources of other scenes or canvases, and audio sources when audio is off, are never opened. An instantiation error fails the start (`Source`). Video sources go to `RENDER`, audio-only sources to `partitions.audio` (the binder chose).
5. The export, the video encoder and the output (streams: video, then audio) are added Active and linked to the program compositor of the canvas; the audio encoder to the canvas's mixer. The compositor's layers come from `node_layers` over the final plan; mixer strips from `report.mixers[canvas].inputs`, each with its source's volume and mute, with a level meter, and a master meter.
6. The partitions are added and every instance adopted, then the graph is applied once. If the compositor, mixer, export, an encoder or the output did not join, or the engine reported errors or links that could not cross partitions, the engine is shut down and the start fails with `Start(Vec<StartFailure { node, reason }>)`, which also lists the shutdown's errors (node `"shutdown"`). The output may have opened the destination by then; removing a file it created is the composition root's job. A source that did not join (a camera another program holds) is recorded in `StartReport::sources[i].failed` and the recording goes on without it.

### Running and stopping

- `service` runs `Engine::service` and reads its events, matched to sources by `SourceState::node`: a `Failed` event of a source sets that source's `failed`, `Restarted` or `Recovered` clears it; a permanent failure (`retry_at: None`) of an essential node sets `failure`. `failure()` also returns the output's first permanent failure cause (`OutputNodeStats::failure`). The composition root stops when `failure()` is set.
- `progress` has the output's connection state: `connects` (times the transport reached its destination, reconnections included; 0: nothing was ever delivered), `reconnect_attempts`, `connected` (now) and `transport_drops` (items a network transport dropped by its drop policy), from `OutputNodeStats`.
- `progress` counts video frames along the chain: `frames_exported` (canvas frames the export read back), `export_drops` (dropped at the export), `encoder_skipped` (exported frames a newer frame replaced in the one-frame queue into the video encoder's partition before the encoder took them, or that were still queued at stop: `QueueStats::video_drops`), `frames_encoded` and `encoder_drops` (busy, refused or lost in the carry-over). The queues into the encoders' partitions are found after the apply that schedules them and kept with the recording, so the final `Progress` still has their counters after the draining apply removed them (`encoder_skipped`, `audio_queue_drops`).
- `finish` shuts the engine down (sources first, the output last, which writes the container's trailer and finishes the transport) and returns the final `Progress`, `failure`, shutdown errors, `flush_lost` (packets the video encoder still held after its final-tick flush, ADR-0043; normally 0) and `finish_lost` (output items not delivered). Call `end` first and service until `tail_done` to keep the frames still read back (ADR-0045). `Progress::start_drops` counts audio packets dropped by the output's aligned start.

### Studio (ADR-0041)

1. `Studio::start` checks the canvas (`UnknownCanvas`, `NoProgram`), binds the collection (`with_audio(partitions.audio)` when `settings.audio`), adds `RENDER` and the video encoder and output partitions, and with audio the audio and audio encoder partitions (pacing as in the table above), and adopts a compositor (`MAX_LAYERS` layers) and, with audio, the canvas's mixer (`MAX_LAYERS` strips, a master meter). With `settings.preview` a `PreviewNode` (I/O nodes spec) is linked to the compositor's `canvas` on `RENDER`; with audio a `MeterSinkNode` (audio graph spec) to the mixer's `main` on the audio partition. These are the studio's nodes: if one does not start, the engine is shut down and the start fails with `Start`.
2. Sources: the sources the plan schedules are instantiated and adopted (video to `RENDER`, audio-only to the audio partition); a source that cannot be instantiated or does not start is reported in `sources()[i].failed` and the studio runs without it. Binder problems are reported in `problems()`, never fatal. Without a preview the compositor has no sink, so video sources are opened only while a recording runs; without audio no audio source is opened (P13). The compositor gets `node_layers` of the plan and the mixer one strip per input of `report.mixers[canvas]`, with the gain and mute of its source's `audio` (ADR-0059), each with a level meter (`meter(source)`), through their parameter channels.
3. `update(collection, sources)`: the canvas must keep its size and rate (`NotBound`) and have a program scene (`NoProgram`), both checked before anything changes. The binder syncs; new sources get entries, retired ones are taken back from their partitions after the apply; a replaced source (settings changed) is applied break-before-make (one apply without the new node first). An instance whose take fails (its partition's swap still in flight) is kept and `service` retries it; a replacement is not instantiated while the instance it replaces is not taken back (`sources()[i].failed` says it waits, `waiting()` is true; an `update` with the same collection opens it once `waiting()` is false). Scheduled sources without an instance are instantiated (a source's first failure reason is kept), the layers are sent, and the graph is applied once. Strips: before the apply the mixer gets the strips of the old and the new inputs, after it only the new ones, so neither schedule feeds an input without a strip (`unknown_inputs` stays 0). Layers are sent before the apply: for up to one tick the old schedule may draw without a layer for a source that just left (skipped) and the new one without a layer for a source that just joined (not drawn yet); a producer is never drawn in the wrong place. Returns problems and the nodes that did not join.
2a. Filters (ADR-0062, [video-filters.md](video-filters.md)): every scheduled filter node gets a `FilterNode` in `RENDER`, created with its source's picture size when the studio has seen it (every filter of a chain gets it), its values from `filter_values_lenient` (adjustments in `filter_problems()`); `changed_filters` of an update send the new values through the node's channel without rebinding; retired filter instances are taken back like sources. `preview_filter(filter, values)` sends values to a running filter without changing the collection (a slider drag) and returns whether one got them; `restore_filter(filter)` sends the collection's values again (a cancelled drag); `filter_stats(filter)` gives a running instance's counters.
3a. `set_mix(collection)` (ADR-0059): for a change of a source's volume or mute alone. It reads each source's `audio` from the collection, rebuilds the mixer parameters from the strips the studio already holds and sends them through the mixer's parameter channel; nothing is bound, instantiated or applied. Returns false when the studio has no mixer. The mixer ramps a gain change over 10 ms, so a fader drag does not click.
4. `start_output(adapters, out)` (ADR-0047): `TooManyOutputs` while `MAX_OUTPUTS` run, before anything is chosen; `NotBound("mixer")` for audio on a studio without audio. Adapters are chosen as in `Recording::start` step 2 before anything is added, except that a running encoder is reused (see "Several outputs"). Then the export (`RENDER`) and video encoder, the audio encoder (with `out.audio`), each unless shared, and the output are added Active and linked (output streams: video, then audio), sources only now scheduled are opened, layers are sent and the graph is applied once. The new nodes are added to a copy of the graph and adopted all or nothing, so a failure there leaves the studio as it was. If one of the new nodes did not join, or the engine reported errors or links that could not cross partitions (in any partition), the new nodes are removed again (plain apply) and taken back, and the call fails with `Start` (removal errors listed with node `"rollback"`); the studio and the other output run on. The output may have created the destination.
5. `stop_output(id)`: the output's node, and every encoder chain no other running output uses, are removed and the graph applied with `Engine::apply_draining`: the partitions this empties run their final ticks in pipeline order (an unshared encoder its final tick and flush, the output drains its queue and writes the trailer in `release`) and stop; a shared encoder keeps running for the other output untouched (its frames in flight at that moment do not reach the stopped output; `flush_lost` is 0). Render and audio keep running when the preview, the meter sink or the other output keeps them scheduled. Instances are taken back. Returns a `FinishReport` (its `shutdown_errors` are the apply's errors), or `None` when `id` does not run.
5a. **Keeping the tail** (ADR-0045, per output). `end_output(id)` ends the output's video at the master clock's now. When its video chain is its alone, the export ends (`ExportEnd::end_at`): it takes no later canvas and passes on every frame it still reads back, and `output_tail_done(id)` is true once the export reports `ended` and the video encoder accepted the export's last frame (`last_frame` of both). A shared export keeps running for the other output, and `output_tail_done(id)` is true once the encoder accepted a frame at or after the end. Either way it is also true `END_TAIL_LIMIT` after the end on the master clock, and when `id` does not run; false while the output runs and was not ended. An export asked to end is not shared with an output started after that. The composition root keeps calling `service` until then and calls `stop_output`; an unshared video encoder's final tick takes the last frame from the queue, and an unshared audio encoder flushes on its last final tick (audio graph spec). Audio keeps flowing until the drain, so the file's audio may end a few frames after its last picture. `rbs-cli` (when the recording did not fail) and the desktop app's Stop and close do this for the recording; `stop_output` alone loses the frames still read back.
6. `service()` maps engine events as `Recording::service` does; a permanent failure of a studio node sets `failure()`; of an output node, that output's `output_failure(id)` only; of an encoder chain's node (export, video encoder, audio encoder), `output_failure` of every output using it. `output_failure` also returns the output's first permanent failure (`OutputNodeStats::failure`). The composition root stops an output when its `output_failure` is set.
7. `shutdown()` stops every output in one draining apply (as `stop_output` does for one, so a shared encoder is drained and flushed for all of them), then shuts the engine down; the reports are in start order. Every call runs on the control thread.
8. Sync offsets: `settings.audio_offsets` holds a sync offset in ns per audio source (positive delays it, negative advances it; audio graph spec, "Offset", clamps it). An audio source's offset is sent on its input's offset channel before the node is adopted, so its first block already has it; this also holds for a source opened later or rebound after a settings change. `set_audio_offset(source, ns)` updates the setting (0 removes it) and sends it to a running input of that source (a change below 5 ms slews, a larger one resyncs once), returning whether one got it. `RecordSettings::audio_offsets` is passed to the studio by `studio()`. The desktop app fills `settings.audio_offsets` from the collection's `sync_offset_ms` (ADR-0062 decision 10) and calls `set_audio_offset` when one changes, so the collection is its one source of truth.

### Further canvases (ADR-0093, first step)

- **What runs.** Each `StudioSettings::extra_canvases` entry adds one compositor on `RENDER` at the canvas's own size and one picture (`canvas_preview`). Source nodes and their filter chains are built once by the binder and feed every compositor, so a device is opened once whatever the number of canvases. No entry: nothing of the others is scheduled (Installed, P13).
- **Checks at start.** `Studio::start` fails with `TooManyCanvases` (more than `MAX_CANVASES`, a canvas listed twice or equal to the main one), `UnknownCanvas`, `NoProgram`, or `CanvasRate` (the frame rate differs from the main canvas, ADR-0093 decision 2). `update` rejects a size or rate change of any running canvas.
- **Outputs.** `OutputSettings::canvas` (`None`: main) picks the canvas an output records or streams; an unknown one fails with `UnknownCanvas`. An encoder is shared only between outputs of the same canvas (and equal settings). `MAX_OUTPUTS` is still 2 in total; the per-canvas limit of ADR-0093 decision 6 follows with the partition change.
- **Not yet.** Follow modes, audio per canvas, Auto layout and the control API are the next steps of ADR-0093 decision 11; a further canvas shows its own program scene until the follow modes exist. The render tick is a hot path: measurements and the maintainer's sign-off (P15) are pending before this merges.

### Several outputs (ADR-0047)

- **Partitions by role.** Each output's node gets its own output partition, so a transport that stalls or reconnects stalls only its own queue. Each video encoder chain (an export on `RENDER` plus a video encoder) gets a video encoder partition, each audio encoder an audio encoder partition. Partitions of each role are taken separately: the first of `partitions` (`video_encoder`, `audio_encoder`, `output`) or `partitions.second` that is free and has no instance whose take is still pending (its swap or drain in flight), so a stop followed at once by a start never cancels the stopped output's drain. With `MAX_OUTPUTS` outputs, each using at most one chain of each kind, a free partition of each role exists while fewer outputs run, once pending takes are done; until then `start_output` fails with `OutputFinishing` (try again after `service`). `Studio::start` adds every slot's partitions; they get a worker only while something is scheduled on them (P13).
- **Sharing.** A running video encoder is reused when the new output's `VideoSettings` equal those it was opened with and its adapter is a candidate of the new output's selection (the output's registry holds it and it meets the codec, memory and pixel-format requirement; a registry narrowed to another encoder, the user's explicit choice, gets its own). The audio encoder likewise with `AudioSettings`. Video and audio are decided independently. A shared encoder's `packets` pin feeds both outputs, each link with its own packet queue; a shared encoder takes no request that would change one output's stream only (any such future feature, such as a per-output bitrate change, ends the sharing first). Different settings get a chain of their own: a second export (another readback) and a second encoder session (selection moves on to the next candidate when a hardware encoder refuses another session).
- **Joining.** An output that joins a running encoder starts at its next keyframe (the packet queue endpoint's rule), at most a keyframe interval after `start_output`; the encoder is not asked for an early keyframe. Its audio before the first video packet is held and trimmed by the output's aligned start (I/O nodes spec, `START_HOLD`); one stopped before its first keyframe has no video (and possibly no media at all).
- **Counters.** `output_progress(id)` counts from the output's start request: the shared chain's counters at that moment are its base. Packets the join's resync dropped (up to one GOP) are counted by the queue, not as loss.

`Recording::start` is a studio without a preview, in a strict mode, with the recording's nodes added before the first apply: a binder problem that leaves out a source of the program scene fails the start (`Problems`) and a source the recording schedules (video and audio) that cannot be instantiated fails it (`Source`), both before any device opens (test `a_video_source_that_cannot_be_instantiated_fails_the_start_before_any_device_opens`). `finish` is `Studio::shutdown`; its `shutdown_errors` hold the draining apply's errors and the engine shutdown's.

### Replay buffer (ADR-0088)

`rbs_nodes::replay::PacketRing` is the buffer's data structure; the ring node and the `Studio` output kind that feed it follow in later changes.

- Holds **encoded packets** (`RingEntry`: stream index plus `EncodedPacket`, payload shared as `Bytes`), in arrival order. Limits: `length`, `memory_limit` and `capacity` (packets, reserved in `PacketRing::new`).
- `push` is hot: it never grows storage and evicts the **oldest** entries while a limit is exceeded; the newest packet is never dropped. Evicted entries go to a caller-reserved vector for a reaper thread; if it has no room the entry is freed in place and counted (`inline_frees`).
- `held()` reports the usable past (oldest video keyframe to newest packet), bytes, packets and `limited_by: Some(Memory)` when memory is why it is shorter than `length` ("Holds the last 3:12, limit: memory").
- `cut(seconds, out)` starts at the latest video keyframe at or before `newest - seconds` (the oldest keyframe if the ring is shorter; the whole ring for `None`), so a clip is never shorter than asked unless the ring is. Audio is cut at the same instant. Without a video keyframe it fails with `ClipError::NoKeyframe`. `Clip::rebased` shifts timestamps so the earliest sample is 0; it runs on the writer task.

### Virtual camera (ADR-0066)

- **Not an output.** The camera is none of the `MAX_OUTPUTS` outputs: it has no encoder or output partition and runs alongside recordings and streams without touching them. It shows the program compositor's canvas, whatever studio mode shows (ADR-0066 decision 5).
- **Start.** `start_virtual_camera` fails with `InvalidConfig` while the camera already sends. It opens the session (`factory.open` with NV12, system memory, the settings' size and rate) on the calling control thread, so `NotInstalled` and the adapter's other refusals come back before anything is added. Then a `VirtualCameraNode` for the canvas size is added Active on `RENDER`, linked from the program compositor's "canvas", adopted and applied on a copy of the graph. An adoption or apply refusal is `Os { code: 0, .. }`, and the node is taken back (the session drops with it, on this thread).
- **Running.** The node keeps the program compositor scheduled, so the render worker and the program's video sources run while the camera sends, even with no preview and no output. The node is not essential: its failure is kept in `VirtualCameraState::failure` (cleared when it runs again), never in `failure()` or an output's failure.
- **Stop.** `stop_virtual_camera` removes the node with a plain apply, then takes it back. If the render partition still runs it, the take is queued in the pending takes, and `service` completes it. Either way the node, and with it the session whose drop joins the adapter's sender thread, is dropped on the control thread, never on `RENDER`. `shutdown` stops the camera first. Stopping never uninstalls the camera.

`Recording` passes `start_virtual_camera`, `stop_virtual_camera` and `virtual_camera` through to its studio; `finish` stops the camera with the rest.

### Invariants

1. Engine code names no adapter: `rbs-record` depends on ports, node crates, the scene and the engine only (P1), and selects by capability (P3).
2. Nothing opens before `start` has chosen every adapter: a missing encoder, muxer or transport fails before any device or file is touched.
3. Video packet `pts` is the canvas tick time that produced the frame (on the canvas cadence from time 0); audio packet `pts` starts at the first mixed block's time (tick time minus the graph latency, the time of its first sample) and advances by exactly one codec frame. Both are master-clock time, so streams are in sync by timestamp; the muxer maps them to one origin.
4. Output stream 0 is video and stream 1 audio, in the order the output declares them to the muxer.
5. A recording that is not started holds no thread, device or file; after `finish` every worker has stopped and the transport is finished (P13). After `stop_output` the partitions only that output used have no worker and its transport is finished; video sources are closed unless a preview shows them.
6. `start` and `finish` run on the control thread. Nothing in `rbs-record` runs per frame or per block; the per-frame work is in the nodes ([I/O nodes](io-nodes.md), [audio graph](audio-graph.md)).

### `rbs-cli`

- `rbs-cli record <file.mkv|file.mp4> [options]`: `.mkv` is Matroska, `.mp4` fragmented MP4. Options: `--seconds N` (else until Enter; required when stdin is not a terminal, so a script cannot record forever), `--overwrite` (replace an existing file; without it an existing file is refused), `--size WxH` (even, default 1920x1080), `--fps N` (default 60), `--rate-control cbr|vbr|cq` (default cbr), `--bitrate KBPS` (the CBR rate or VBR target, default 12000; refused with cq), `--max-bitrate KBPS` (VBR peak, at least the target, default 1.5 x the target; vbr only), `--quality N` (cq level 0 best to 51, default 23; cq only), `--keyframe-interval S` (whole seconds, default 2), `--b-frames N` (at most, 0 to 4, default 0), `--profile NAME` (codec profile, default the encoder's choice), `--preset quality|balanced|speed` (default balanced), `--cbr-padding` (with cbr: the `cbr_padded` mode, filler data to the full bitrate), `--audio-bitrate KBPS` (default 160), `--encoder ID` (a video encoder adapter id or its last dotted part: only that encoder is used, no fallback; an unknown id is an error), `--display N`, `--camera N` (indices from `devices`), `--no-camera`, `--no-mic`, `--no-system-audio`, `--no-audio`, `--mic-offset MS` and `--system-audio-offset MS` (sync offsets in whole ms, within ±200: positive delays that source, negative plays it earlier; the input clamps an advance to the audio graph's latency minus a period minus 5 ms, 35 ms by default). Bad arguments exit with code 2 and the usage text. The video settings are printed in one line before the start. While recording, one line per second shows frames exported and encoded, drops and skips, each source and the packets muxed; the summary at the end splits video frames into exported, encoded, dropped at export, skipped before the encoder and dropped at the encoder.
- The scene: one canvas of the given size and rate, one scene on air with the chosen display filling the canvas (cursor on, at the canvas rate), the camera at 1280x720 30 fps in a 16:9 rectangle 4/15 of the canvas wide, a margin of 1/40 of the width from the bottom-right corner, the default microphone and the default system audio. A type with no target is left out and said so; no display is an error. Before the scene is built the camera's and microphone's adapters are asked for capture permission (`request_permission`, which may show the OS prompt and waits for the answer); a refused or unanswered one is left out and said so, like an absent one, so a missing permission never fails the start (an audio source opens at instantiation, which is strict) and never adds a source that cannot run.
- Adapters: Matroska, fMP4 and file on every platform; Windows adds WGC and Media Foundation capture, WASAPI, NVENC and AMF (in that order), Media Foundation AAC; macOS adds ScreenCaptureKit and AVFoundation capture, Core Audio, VideoToolbox (hardware, then Apple's software encoders) and AudioToolbox AAC; Linux adds PipeWire capture (cameras, and displays and windows through the screen-cast portal's picker: `record` shows the portal dialog when no display is listed), PipeWire audio, NVENC, AMF, QSV and VA-API (in that order) and FFmpeg's AAC encoder (`rbs-codec-ffmpeg`, the system's libavcodec loaded at run time). A platform without video or audio encoders says recording is not available there yet.
- The engine gets `OsThreadPolicy` (ADR-0030). The device is `GpuRequest::default()` (high performance, no software adapter). The file path is made absolute and turned into a `file://` URL from its components: a drive path is `file:///C:/...`, a UNC path `file://server/share/...` (verbatim `\\?\` prefixes included), and `%`, space, `#`, `?` and a `\` inside a name are escaped. When the start fails after the file was created, `rbs-cli` removes it.
- Exit code 0 when the recording ran and stopped cleanly with packets muxed (`Progress::packets_muxed`); 1 when start failed, the recording failed while running, the engine did not stop cleanly, or no packet was muxed.
- `rbs-cli stream <rtmp://host[:port]/app | srt://host:port> [options]`: the same scene and options as `record` except `--overwrite`, with `--bitrate` defaulting to 6000 kbit/s. `rtmp://` sends H.264 and AAC packets to the RTMP transport (no muxer); `srt://` muxes MPEG-TS for the SRT transport; any other scheme (`rtmps` included, not supported yet) is refused when the arguments are parsed. The stream key (RTMP, required) or stream id (SRT, optional) is read from the environment variable `RBS_STREAM_KEY`, or the one `--key-env NAME` names, never from the command line, and is trimmed; a missing RTMP key fails before any device is opened or permission asked; it goes into `RecordSettings::secret` only, so it is in no URL, log line or report. `--srt-latency MS` (0 to 65535, SRT only) becomes the `latency_ms` transport option. Reconnect is on by default (`Backoff`, 1 s doubling to 30 s, 10 attempts; with the transports' rule that attempts restart after a connection that reached publish), `--no-reconnect` turns it off; a server that rejects the key is not retried and stops the stream with the error. Lines and summary as for `record`, with the URL instead of the file; each connection change is printed once when it happens (`connected to <url>`, `connected to <url> again (n times)`, `not connected, trying again (attempt n)`), the per-second line ends with `live` or `not connected`, and the summary adds connections, reconnect attempts and transport drops. Exit code 1 also when no packet reached the stream, or when the stream never connected (`never connected to <url>`, followed by the cause when the output failed, such as the connection error with `--no-reconnect`).
- Stream adapters on every platform: the MPEG-TS muxer, the RTMP and SRT transports (pure Rust, no FFmpeg).
- `rbs-cli vcam status|add|format|remove` (ADR-0066, ADR-0070; Windows 11, elsewhere it exits with code 1 and "not available on this platform"): `status` prints the camera's state in words (`added`, `not added`, `update needed: <reason>`, `not available on this computer: <reason>`); `add [--size WxH] [--fps N]` adds the camera (`install`: one administrator prompt when the source class is not registered yet) and then sets its format; `format [--size WxH] [--fps N]` changes the format (`set_format`) and says when it applies only once no app uses the camera; `remove` removes the camera (`uninstall`; the class registration stays). Sizes are 1920x1080, 1280x720 (default) or 640x360, rates 15, 24, 30 (default) or 60 (ADR-0066 decision 11); anything else exits with code 2. The adapter keeps the camera's symbolic link in `vcam-link.txt` in the RBS settings folder (the `LinkStore`). `rbs-vcam-setup.exe` and `rbs_vcam_mf_source.dll` must be next to `rbs-cli.exe` (build `-p rbs-cli -p rbs-vcam-mf -p rbs-vcam-mf-source`); outside Program Files, `add` needs a development build with `--features rbs-vcam-mf/dev-any-location`.
- `record` and `stream` take `--vcam` (with `--vcam-size WxH` and `--vcam-fps N`, same values and defaults as `vcam format`): after the start, the camera's format is set and `Recording::start_virtual_camera` sends the program to the camera for the whole run; a camera that is not added fails the command before it records (`--vcam: not installed`). The per-second line adds `camera <n> delivered, <n> with no app reading, <n> waiting for a format change; <n> handed over, <n> skipped (app behind), <n> skipped (readback busy), <n> lost, <n> refused, <n> failed` (the first part is the session's counters, `VirtualCameraState::session`, when the adapter shares them; the rest is the node's), and the summary repeats it with the node's failure, if any. This is how the P15 measurements (camera off and on next to a recording) and the hardware test run without the desktop app.
- `rbs-cli devices` lists installed source types with numbered targets, and every registered adapter as available or unavailable.

## Errors and failure behaviour

`RecordError` (non-exhaustive): `UnknownCanvas`, `NoProgram`, `InvalidUrl`, `Bind`, `Problems`, `NotBound`, `Source { name, error }`, `UnsupportedSource`, `NoAdapter { port, tried }`, `Graph`, `Engine` (for example a partition id already in use), `Start(Vec<StartFailure>)`. Run-time failures do not panic or end the process: they are `failure()` and `StartReport::sources[i].failed`; the nodes' own restart rules apply (a source with a recoverable error restarts with back-off, ADR-0024; an output that wrote media never restarts, ADR-0038).

## Performance requirements

Control-thread only; the graph's hot paths are the nodes' and keep their budgets. Five worker threads while recording (render, audio, two encoders, the output), three without audio. A second output adds one (its output) when it shares both encoders, up to three when it has its own. Every cadence partition gets the `Periodic` thread class from the engine's policy; the doorbell partitions get `Responsive`.

## Conformance tests

- `crates/rbs-record/tests/vcam.rs` (one test, with `rbs-vcam-test`): a camera that is not installed fails the start and leaves nothing; while not sending, no node, render worker or camera thread; a second start fails; the camera is not an output and schedules the program on its own; a recording starts and stops while it sends and it keeps sending, frames in order; after stop no render worker and no camera thread (Linux lists threads by name).
- `crates/rbs-record/tests/studio.rs` (same rig and clock driver as `record.rs`, shared in `tests/support`):
  - `recordings_start_and_stop_while_the_studio_keeps_running`: without a preview no render worker and no capture open while the meters run; two recordings in a row on one engine, each with its trailer and finished transport, the recording partitions stopped and video sources closed afterwards.
  - `a_recording_that_cannot_start_leaves_the_studio_running`: `NoAdapter` for a container without a muxer; an output that cannot open its destination fails with `Start` naming the output, nothing records, the studio runs, the next recording works.
  - `scene_edits_open_and_close_sources_while_it_runs`: a microphone and a camera added while recording are opened, metered and delivered; removed again, they are closed and the recording goes on.
  - `audio_sync_offsets_apply_from_the_start_and_while_it_runs`: a microphone offset in the settings is in force from its first block (sending the same value again does not resync it); a 10 ms change of the system audio while running resyncs that input exactly once and not the microphone; 0 removes a setting; a source without an audio input only keeps the setting.
  - `a_preview_where_the_platform_has_none_fails_the_start` (Linux): the preview node's permanent `prepare` failure fails `Studio::start` with nothing left open.
- `crates/rbs-record/tests/outputs.rs` (same rig; recordings go to the logging muxer, streams to the packet transport's own log):
  - `a_recording_and_a_stream_with_different_settings_run_on_their_own_encoders`: two video sessions, the AAC encoder shared; workers of both slots; a third output refused (`TooManyOutputs`) before any session opens; the recording stops with its trailer while the stream keeps delivering with no drops; then the stream stops and no encoder or output worker is left.
  - `outputs_with_the_same_settings_share_the_encoders`: a recording joining a running stream with the same settings opens no session (`video_shared`, `audio_shared`), its first video packet is a keyframe no older than the join less 100 ms, it counts fewer frames than the stream; the stream stops first (no flush, `flush_lost` 0) and the recording goes on; a new stream takes the free output partition and shares the recording's chain; shutdown stops both.
  - `an_output_that_cannot_start_leaves_the_running_one_alone`: a recording that would share the stream's encoders but cannot open its file fails with `Start` naming the output; the stream's partitions and delivery are unchanged.
- `crates/rbs-graph/tests/partition.rs` `a_consumer_added_to_a_running_producer_starts_at_a_keyframe`: a second consumer link added mid-GOP to a running packet producer gets its own queue, starts at the next keyframe, the first consumer sees no gap, and the ticks after the swap allocate nothing.

- `crates/rbs-record/tests/record.rs` (any GPU adapter, lavapipe on CI): a scene with the synthetic display, camera, microphone and loopback records through a fake H.264 encoder (after a candidate that fails to open), the Media Foundation AAC session over its fake engine and a logging muxer and transport, on a `TestClock` started off the frame grid (3.217 s) and moved in 5 ms steps, every cadence worker waited for by tick count and the GPU waited idle after each step, so readbacks complete before the next render tick on software adapters too (hang guard 20 s). Tests:
  - `a_scene_records_video_and_audio_in_sync_on_the_master_clock`: chosen adapters; every source delivers; streams are H.264 then AAC; header, trailer and transport finish once; video `pts` exactly on the canvas cadence, the first within four frames of the first tick; the first audio `pts` plus the graph latency exactly on the audio cadence at the start (index 0 or 1), every step one AAC frame (±1 µs); no muxer, queue or encoder drops; no late ticks.
  - `without_audio_only_video_is_recorded_and_audio_sources_stay_closed`: one stream, no audio stream opened, no audio worker.
  - `problems_outside_the_recorded_scene_do_not_stop_it`: an unknown source type in another scene is a reported problem; put on air, it fails the start with `Problems` before anything opens.
  - `a_stream_sends_packets_without_a_muxer_with_its_secret_and_reconnect_policy`: packet input chooses no muxer and the packet transport; the transport is created with the URL, `Packets`, the secret, the reconnect policy and the options; it gets the H.264 and AAC streams and packets, no chunk, and is finished once; the secret is in no `Debug` output of the settings or the report.
  - `a_transport_that_does_not_take_the_input_is_not_chosen`: packets to a transport that takes only Matroska chunks, and Matroska to one that takes only packets, fail with `NoAdapter { port: Transport }` before anything opens.
  - `an_unknown_canvas_or_a_missing_encoder_fails_before_anything_runs`: `NoAdapter` for the video encoder and the muxer, `UnknownCanvas`; no capture buffer or audio stream was opened.
- `crates/rbs-cli` unit tests: argument parsing and refusals (`record` and `stream`: URL schemes, stream-only options, the stream bitrate default), file URLs (drive, UNC, verbatim on Windows; a write through the file transport to the URL's path), the scene (sources present, left out on request, bad indices refused, camera rectangle, sync offsets per audio source in ns, none for a source left out; offsets beyond ±200 ms refused).
- `crates/rbs-cli` unit tests for `vcam` and `--vcam`: parsing and refusals (sizes and rates outside ADR-0066 decision 11, `--vcam-size` without `--vcam`), and `vcam` against the test adapter (`rbs-vcam-test`): add, format pending while an app reads, remove.
- Hardware (manual, Windows 11): `rbs-cli vcam add` (one administrator prompt), the Windows Camera app and a meeting app list "RBS Virtual Camera" and show the still picture; `rbs-cli record <file> --seconds 60 --vcam` shows the program in those apps, then `vcam format --size 1920x1080` while an app reads says it applies later, and the app keeps the camera after it reopens it (same symbolic link); `vcam remove` takes it out of the apps' lists; `vcam add` again asks for no approval.
- Hardware (manual, Windows and macOS): `rbs-cli stream rtmp://127.0.0.1/live --seconds 30` with `RBS_STREAM_KEY` set, to `ffmpeg -listen 1 -i rtmp://127.0.0.1/live/<key> -c copy out.flv` (FLV with H.264 and AAC, a decode pass, video and audio durations within a frame of the run); the same with `srt://127.0.0.1:<port>` to `ffmpeg -i "srt://:<port>?mode=listener" -c copy out.ts`; a listener started after the stream (reconnect) and a wrong key (the stream stops with the server's error).
- Hardware (manual, Windows): `rbs-cli record <file> --seconds 20` to `.mkv` and `.mp4`, with NVENC and with `--encoder amf`; `ffprobe` streams, packet steps and a decode pass; an A/V sync measurement with a clip that flashes the screen white and beeps at the same time every second, played full screen while recording display and system audio.

Windows result (SC-PC, RTX 4090 + AMD iGPU, 2026-11-03), PASS: `rbs-cli record` with WGC display, Media Foundation camera (Cam Link 4K), WASAPI microphone (Shure MV7+) and loopback, NVENC (default) and AMF (`--encoder amf`), Media Foundation AAC. 20 s MKV: 1210 frames exported, 1202 encoded, 0 dropped, 0 resyncs, 0 late render or audio ticks, 0 queue or muxer drops; `ffprobe`: H.264 High 1920x1080 60/1 and AAC LC 48 kHz stereo, video steps 16.667 ms and audio steps 21.333 ms throughout, a full decode without errors. MP4 and AMF runs the same. The first packets: audio at 1.000 s, video 83 to 138 ms later (the first canvas frame waits for the display's first capture). The last 1 to 8 frames the encoder still held at shutdown are lost (open question below). Sync, measured end to end with a clip that flashes white and beeps every second played full screen by `ffplay` on the recorded display: audio leads the picture by 44 ms on average (29 to 63 ms over 28 flashes, after subtracting the analysis filter's delay measured on the clip itself); this includes `ffplay`'s output, DWM presentation and WGC capture latency on the video side. FFmpeg drops the first second of audio of the fragmented MP4 file: it reads the audio track's edit list (media time = the muxer's 1 s lead) as encoder priming (`rbs-mux-fmp4`). Fixed: media time now starts at the file's earliest sample and the muxer writes no edit list, so FFmpeg presents every audio sample from 0 (`ffmpeg_interop` test).

## Open questions

- After `end_output`, audio keeps flowing until the drain, so the file's audio may end a few frames (at most `END_TAIL_LIMIT` plus the caller's loop latency) after its last picture; trimming audio at the end time is not done (ADR-0045).
- A/V sync, measured on SC-PC 2026-11-03 (ADR-0042, Rejected): with Windows Media Player the system audio leads the picture by 26 to 30 ms (`ffplay` 34 ms; the 44 ms above included `ffplay`'s frame drops). Both capture timestamps are honest: a click test showed WASAPI loopback timestamps equal the device's play time (`IAudioClock`) within 0.2 ms, and the display's frame age at render ticks is 0.5 to 4 ms. About 8 ms is the 60 fps sampling; the rest is likely the player's own uncompensated present latency (not measured). Per-source manual sync offsets are available for device latencies that software cannot see (see "Sync offsets").
- Starting and stopping a recording while the engine keeps running (preview in the desktop app): the `attach`/`detach` entry point of ADR-0040 decision 6.
- Several outputs per canvas: one export for encoders with different settings (saves a readback, needs a larger readback buffer set in `rbs-gpu`), an early keyframe when an output joins a shared encoder, more than two outputs, and `rbs-cli` record and stream at once are follow-ups (ADR-0047).
- `rtmps` (TLS) and enhanced RTMP (HEVC, AV1) wait for the RTMP transport.
- Ctrl+C in `rbs-cli` ends the process without a trailer (Matroska stays playable, without duration and cues); a console control handler that stops cleanly is a follow-up.
