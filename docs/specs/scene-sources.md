# Spec: Scene source types

- Status: Draft
- Owner crate(s): `rbs-sources`
- Related ADRs: [0013](../adr/0013-zero-cost-when-unused.md), [0021](../adr/0021-scene-collection-model.md), [0037](../adr/0037-audio-graph.md), [0038](../adr/0038-io-graph-nodes.md), [0039](../adr/0039-scene-source-types.md)
- Builds on: [scene model](scene-model.md) (binder), [capture port](capture-port.md), [audio input port](audio-io-port.md), [I/O nodes](io-nodes.md), [audio graph](audio-graph.md)

## Purpose

Maps the built-in scene source types (display, window, camera, microphone, system audio, application audio) to graph nodes: which types are installed with the registered adapters, the descriptors the scene binder binds with, and the node instance for each bound source.

## Scope

In scope: source type ids and settings keys, the `SourceTypes` registry, default targets, instances, the compositor layer list.

Out of scope: plugin source types (plugin SDK), the settings schema per type, compositor and mixer instances (the composition root makes them with the canvas and audio configuration), encoders and outputs.

## Contract

```rust
// rbs_sources::types
pub const DISPLAY_SOURCE: &str = "org.rbs.source.display";           // capture, TargetKind::Display
pub const WINDOW_SOURCE: &str = "org.rbs.source.window";             // capture, TargetKind::Window
pub const CAMERA_SOURCE: &str = "org.rbs.source.camera";             // capture, TargetKind::Camera
pub const AUDIO_INPUT_SOURCE: &str = "org.rbs.source.audio-input";   // audio, EndpointKind::Input
pub const AUDIO_OUTPUT_SOURCE: &str = "org.rbs.source.audio-output"; // audio, EndpointKind::OutputLoopback
pub const APPLICATION_AUDIO_SOURCE: &str = "org.rbs.source.application-audio"; // EndpointKind::Application
pub const ALL: [&str; 6];
pub mod keys { ADAPTER, TARGET, CURSOR, WIDTH, HEIGHT, FPS }
pub enum SourceClass { Capture(TargetKind), Audio(EndpointKind) }
pub fn class_of(type_id: &NodeTypeId) -> Option<SourceClass>;

// rbs_sources
impl SourceTypes {
    pub fn new() -> Self;
    pub fn register_capture(&mut self, f: Arc<dyn CaptureFactory>) -> Result<(), DuplicateAdapter>;
    pub fn register_audio(&mut self, b: Arc<dyn AudioBackend>) -> Result<(), DuplicateAdapter>;
    pub fn capture_adapters(&mut self) -> &mut Registry<dyn CaptureFactory>; // preference, user choice
    pub fn audio_adapters(&mut self) -> &mut Registry<dyn AudioBackend>;
    pub fn probe(&mut self);
    pub fn is_installed(&self, type_id: &NodeTypeId) -> bool;
    pub fn installed(&self) -> Vec<NodeTypeId>;
    pub fn node_types(&self) -> NodeTypeTable;                 // impl NodeTypes, a snapshot
    pub fn targets(&self, type_id: &NodeTypeId) -> Result<Vec<SourceTarget>, SourceError>;
    pub fn pickers(&self, type_id: &NodeTypeId) -> Result<Vec<SourcePicker>, SourceError>;
    pub fn instantiate(&self, source: &Source, ctx: &InstanceContext)
        -> Result<SourceInstance, SourceError>;
}
pub struct SourceTarget { adapter, target, name, is_default, size: Option<(u32, u32)> }  // size: displays and windows, when known
#[derive(Clone)] pub struct SourcePicker;  // Send; holds the adapter's factory only
impl SourcePicker {
    pub fn adapter(&self) -> AdapterId;
    pub fn adapter_name(&self) -> &str;
    pub fn pick(&self) -> Result<SourceTarget, SourceError>; // blocks while the OS dialog is up
}
pub struct InstanceContext { clock, audio: AudioGraphConfig, max_audio_offset }  // new(), with_max_audio_offset()
pub enum SourceInstance {
    Video(VideoSource { node: CaptureSourceNode, stats, adapter, target }),
    Audio(AudioSource { node: InputNode, offset: ParamSender<InputParams>, stats, adapter, endpoint }),
}
pub fn node_layers(compositor: &BoundCompositor, plan: &SchedulePlan) -> Vec<NodeLayer>;
```

### Settings

| Key | Value | Types | Meaning |
| --- | --- | --- | --- |
| `adapter` | Text | all | Adapter id. Missing: the best probed adapter for the kind (registry order: user choice, preference, hardware, registration) |
| `target` | Text | all | Capture target id or audio endpoint id. Ids are adapter-defined, so a stored target is stored with its adapter |
| `cursor` | Bool | display, window | Draw the cursor; default true |
| `width`, `height` | Int > 0 | display, window, camera | Requested size; both or neither |
| `fps` | Int > 0 | display, window, camera | Requested frame rate |

Unknown keys are ignored. A wrong value type or value is `InvalidSetting`.

### Invariants

1. A type is installed exactly when a probed adapter declares its kind: `capture.target.kinds` includes the `TargetKind`, or `audio.input.kinds` includes the `EndpointKind`. Nothing is installed before `probe`.
2. `node_types()` holds the compositor (`rbs_gpu::node::compositor_descriptor`), the mixer (`rbs_audio::mixer_descriptor`) and each installed source type: capture types with `rbs_nodes::capture_descriptor`, audio types with `rbs_audio::input_descriptor`. An instance's descriptor equals its type's descriptor in the table, which the controller requires. So graph nodes carry the node crates' type ids (`org.rbs.capture`, `org.rbs.audio.input`), not the scene source type id; the binder maps sources to nodes. `rbs_scene::binder::COMPOSITOR_TYPE` and `MIXER_TYPE` equal the node crates' ids (test).
3. A `target` without `adapter` goes to the first adapter, best first, that lists it, else to the best adapter (picker-only ids are never listed). Default target, when `target` is missing: the first display or camera an adapter lists; the audio endpoint marked default, else the first one; windows have none (`NoTarget`). Without `adapter`, candidates are tried best first until one lists a default; with `adapter`, only that one.
4. A `Video` instance opens nothing until `prepare` (ADR-0038). An `Audio` instance holds a stream opened in Standby: the port guarantees no device activity before `start` (audio input port spec).
5. `instantiate` and `targets` run on the control thread and may enumerate (up to 100 ms per adapter); they never prompt.
6. `pickers` returns, best first, the probed capture adapters whose `capture.target.picker` includes the type's kind (none for audio types). `SourcePicker::pick` calls the port's `pick` and returns the choice as a `SourceTarget` (`is_default` false) to store like a listed one; a closed dialog is `Capture(Cancelled)`. Some systems offer displays and windows only this way (the PipeWire screen-cast portal lists nothing), so hosts show a "choose" entry per picker next to the listed targets, and a headless host with no listed display picks one. `pick` blocks while the dialog is up: hosts call it off the threads that must keep running (a picker is `Send` and holds no resources).
7. `node_layers` returns the bound layers whose producer `plan` schedules, bottom first, each with the index of its link among the compositor's scheduled links (scene model spec, `resolve`), the transform, crop and opacity unchanged.

### How the composition root uses it

1. Register the platform's capture and audio adapters, `probe`.
2. `SceneBinder::new(types.node_types()).with_audio(AUDIO)`; `sync`. `node_types` also installs the descriptor of every built-in video filter type (`rbs_filters::catalogue`, [video-filters.md](video-filters.md)); filter settings convert with `filter_values` (edits, strict) and `filter_values_lenient` (documents).
3. For each `report.created_sources`, `instantiate` and adopt the instance into the node's partition (`Video` into the render partition, `Audio` into the audio partition).
4. Create the compositor (`node_layers`) and mixer instances, attach each canvas's encoders to `compositor(canvas, Program)` and `mixer(canvas)`. Mixer strips are kept per `SourceId` and rebuilt from `report.mixers[canvas].inputs` after every sync.
4a. For each scheduled filter node without an instance, create a `FilterNode` of its type with the source's known picture size and adopt it into the render partition; send new values on `report.changed_filters` (ADR-0062).
5. Apply and collect. Only then take back the instances of `retired_sources` and `retired_filters`: `Controller::take` returns idle instances only.
6. Break-before-make for `report.replaced_sources`: an exclusive device (most cameras) cannot start the new instance while the old one runs. Apply first a copy of the graph with each replacing node moved to Standby, collect (the old instance is released), then apply the graph itself.

## Errors and failure behaviour

`SourceError` (non-exhaustive): `UnknownType`, `NoAdapter`, `AdapterUnavailable(id)`, `NoTarget`, `InvalidSetting { key, expected }`, `Capture(CaptureError)`, `Audio(AudioIoError)`. A capture target that is gone is found by the node's `prepare` (permanent `TargetNotFound`, ADR-0038); an audio endpoint that is gone fails `instantiate` with `Audio`.

## Performance requirements

Control thread only, user-paced; nothing here runs per frame or per block.

## Conformance tests

`crates/rbs-sources/tests/sources.rs`, with the reference adapters `rbs-capture-test` and `rbs-audio-io-test`: installed types follow probed kinds; an empty registry has the compositor and mixer; instance descriptors equal table descriptors for every installed type; default targets; windows need a target; stored targets are used as given; bad settings, adapters and types are refused; targets for pickers; pickers only for kinds an adapter declares, picking on another thread, a cancelled pick, a picked target instantiated by its adapter; a bound scene instantiates into the partitions the binder chose, and `node_layers` keeps the transform, crop, opacity and link order.

Hardware (manual): `cargo run -p rbs-sources --example scene_sources --release -- 15` on Windows (WGC, Media Foundation camera, WASAPI microphone and system audio through the binder, the compositor and the mixer on the engine workers).

Windows result (SC-PC, RTX 4090, 2026-11-03), PASS: every type installed with WGC, Media Foundation and WASAPI (2 displays, 7 windows, 2 cameras, 3 inputs, 2 loopbacks, 6 applications listed). Over 15 s: display 825 frames, all imported zero copy; Cam Link 4K 871 frames, all uploaded; Shure MV7+ microphone and system loopback 722,400 frames each with 0 resyncs and drift correction within -17..+33 ppm; canvas 904 frames, mix 1505 blocks; 0 GPU errors, 0 invalid or unknown mixer inputs.

## Open questions

- A settings schema per type (with the plugin SDK and control API): which keys apply live without a new node.
- Plugin source types: descriptor and factory from the plugin manifest, added to the table.
- Sources with video and audio (cameras with microphones): the binder needs the partition queue's reader as the mixer producer (`AudioOnVideoSource` until then).
- Audio that does not depend on the scene (a global microphone or system audio in every scene).
- Restore tokens for picker-only targets (PipeWire portal): the application stores `CaptureSession::target_to_store` back into the `target` setting, which today makes a new node.
