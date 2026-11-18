# Spec: Scene source types

- Status: Draft
- Owner crate(s): `rbs-sources`
- Related ADRs: [0013](../adr/0013-zero-cost-when-unused.md), [0021](../adr/0021-scene-collection-model.md), [0037](../adr/0037-audio-graph.md), [0038](../adr/0038-io-graph-nodes.md), [0039](../adr/0039-scene-source-types.md), [0074](../adr/0074-still-image-and-colour-sources.md)
- Builds on: [scene model](scene-model.md) (binder), [capture port](capture-port.md), [audio input port](audio-io-port.md), [I/O nodes](io-nodes.md), [audio graph](audio-graph.md)

## Purpose

Maps the built-in scene source types (display, window, camera, microphone, system audio, application audio, image, colour) to graph nodes: which types are installed with the registered adapters, the descriptors the scene binder binds with, and the node instance for each bound source.

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
pub const IMAGE_SOURCE: &str = "org.rbs.source.image";               // still, a PNG file or a built-in sample (ADR-0074)
pub const COLOR_SOURCE: &str = "org.rbs.source.color";               // still, a solid colour or a gradient (ADR-0074)
pub const ALL: [&str; 8];
pub mod keys { ADAPTER, TARGET, CURSOR, WIDTH, HEIGHT, FPS, PATH, SAMPLE, COLOR, COLOR2, ANGLE, FILLED }
pub enum SourceClass { Capture(TargetKind), Audio(EndpointKind), Still }
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
    // Monitoring (ADR-0097): adapters with `audio.output`, best first.
    pub fn can_monitor(&self) -> bool;
    pub fn monitor_devices(&self) -> Vec<MonitorDevice>; // enumerates: only while a picker is open
    pub fn open_monitor(&self, target: Option<&str>)      // None: the OS default output
        -> Result<(Box<dyn AudioOutput>, AdapterId, EndpointId), SourceError>; // Standby; NoAdapter, NoTarget, Audio(EndpointNotFound)
}
pub struct MonitorDevice { adapter, target, name, is_default, device_key: Option<String> }
pub struct SourceTarget { adapter, target, name, is_default, size: Option<(u32, u32)>, scale: Option<DisplayScale>, app: Option<String> }  // size: displays and windows, when known (a display is the framebuffer in physical pixels); scale: that display's OS pixels per point, when known (ADR-0102; not applied to size); app: owning application of a window, when the OS tells it (grouping and labels only)
#[derive(Clone)] pub struct SourcePicker;  // Send; holds the adapter's factory only
impl SourcePicker {
    pub fn adapter(&self) -> AdapterId;
    pub fn adapter_name(&self) -> &str;
    pub fn pick(&self) -> Result<SourceTarget, SourceError>; // blocks while the OS dialog is up
}
pub struct InstanceContext { clock, audio: AudioGraphConfig, max_audio_offset, browser_budget, browser_tree, canvas: Option<(Size, FrameRate)> }  // new(), with_max_audio_offset(), with_browser(), with_canvas()
pub enum SourceInstance {
    Video(VideoSource { node: CaptureSourceNode, stats, adapter, target }),
    Audio(AudioSource { node: InputNode, offset: ParamSender<InputParams>, stats, adapter, endpoint }),
    Still(StillSource { node: StillSourceNode, stats }),   // render partition, no adapter (ADR-0074)
}

// rbs_sources::still: pixels of still sources, control thread only
pub fn parse_color(text: &str) -> Option<[u8; 4]>;                  // #rrggbb or #rrggbbaa
pub fn color_picture(w, h, first, second: Option<[u8; 4]>, angle: f32) -> Vec<u8>;
pub fn decode_png(path: &Path) -> Result<(u32, u32, Vec<u8>), String>;
pub const SAMPLES: [Sample; 8];                                      // { id, name, kind: Background | Frame }
pub fn sample_picture(id: &str, w: u32, h: u32) -> Option<Vec<u8>>;
pub fn node_layers(
    compositor: &BoundCompositor,
    plan: &SchedulePlan,
    runs: &ScrollRuns,
) -> Vec<NodeLayer>;
```

### Settings

| Key | Value | Types | Meaning |
| --- | --- | --- | --- |
| `adapter` | Text | all | Adapter id. Missing: the best probed adapter for the kind (registry order: user choice, preference, hardware, registration) |
| `target` | Text | all | Capture target id or audio endpoint id. Ids are adapter-defined, so a stored target is stored with its adapter |
| `cursor` | Bool | display, window | Draw the cursor; default true |
| `width`, `height` | Int > 0 | display, window, camera, color | Requested size; both or neither. For `color` it is the picture size, default 1920 x 1080, at most 8192. A camera without them asks for the context's canvas size (the nearest mode, not the largest; audit C-21); a display or window keeps its own size |
| `fps` | Int > 0 | display, window, camera | Requested frame rate. With a canvas in the `InstanceContext` (the studio passes the main canvas), a capture never runs faster than the canvas rate, and without `fps` it runs at the canvas rate (audit C-02) |
| `path` | Text | image | A PNG file (straight alpha, any colour type; a side over 8192 pixels or a decoder buffer over 128 MiB is refused; gamma and colour profiles are ignored, pixels are sRGB). Wins over `sample` |
| `sample` | Text | image | A built-in sample picture id (below) at 1920 x 1080 |
| `color` | Text | color | `#rrggbb` or `#rrggbbaa`; default `#000000` |
| `color2` | Text | color | Second colour; present makes a linear gradient from `color` |
| `angle` | Int | color | Gradient direction in degrees, 0 = top to bottom, 90 = left to right; default 0 |
| `filled` | Bool | color | Draw the fill. Absent or true fills the shape. False leaves the inside empty: the stored `color` stays and its alpha is not drawn, so turning the fill back on restores it |

Built-in samples (generated in code, original to this repository, ADR-0074 decision 6): backgrounds `aurora`, `sunset`, `slate`, `dots`; frames (transparent in the middle) `frame-thin`, `frame-thick`, `frame-neon`, `lower-third`. Adding `frame-thin` or `frame-thick` from the Library or `source.add_sample` creates a transparent colour source with that border (width 4 or 28, corner radius 6 or 40) instead of the generated picture, so the thickness stays the item's border. The pictures are still generated, for the Library's thumbnails and for an image source whose `sample` is that id. The Library does not list `lower-third`: the words are part of the picture. `source.add_sample` still accepts it, and an image source whose `sample` is that id still draws it.

Unknown keys are ignored. A wrong value type or value is `InvalidSetting`.

### Invariants

0. Still types (`image`, `color`) are installed always, with no adapter and no probe. `node_types()` holds `rbs_nodes::still_descriptor` for them. `instantiate` decodes or generates the pixels on the control thread and returns `Still`; a missing, unreadable or oversized picture is `Image`, a missing `path` and `sample`, an unknown sample or a bad colour is `InvalidSetting`. A settings edit replaces the node. The node uploads once and costs nothing per frame.

1. A type is installed exactly when a probed adapter declares its kind: `capture.target.kinds` includes the `TargetKind`, or `audio.input.kinds` includes the `EndpointKind`. Nothing is installed before `probe`.
2. `node_types()` holds the compositor (`rbs_gpu::node::compositor_descriptor`), the mixer (`rbs_audio::mixer_descriptor`) and each installed source type: capture types with `rbs_nodes::capture_descriptor`, audio types with `rbs_audio::input_descriptor`. An instance's descriptor equals its type's descriptor in the table, which the controller requires. So graph nodes carry the node crates' type ids (`org.rbs.capture`, `org.rbs.audio.input`), not the scene source type id; the binder maps sources to nodes. `rbs_scene::binder::COMPOSITOR_TYPE` and `MIXER_TYPE` equal the node crates' ids (test).
3. A `target` without `adapter` goes to the first adapter, best first, that lists it, else to the best adapter (picker-only ids are never listed). Default target, when `target` is missing: the first display or camera an adapter lists; the audio endpoint marked default, else the first one; windows have none (`NoTarget`). Without `adapter`, candidates are tried best first until one lists a default; with `adapter`, only that one.
4. A `Video` instance opens nothing until `prepare` (ADR-0038). An `Audio` instance holds a stream opened in Standby: the port guarantees no device activity before `start` (audio input port spec).
5. `instantiate` and `targets` run on the control thread and may enumerate (up to 100 ms per adapter); they never prompt.
6. `pickers` returns, best first, the probed capture adapters whose `capture.target.picker` includes the type's kind (none for audio types). `SourcePicker::pick` calls the port's `pick` and returns the choice as a `SourceTarget` (`is_default` false) to store like a listed one; a closed dialog is `Capture(Cancelled)`. Some systems offer displays and windows only this way (the PipeWire screen-cast portal lists nothing), so hosts show a "choose" entry per picker next to the listed targets, and a headless host with no listed display picks one. `pick` blocks while the dialog is up: hosts call it off the threads that must keep running (a picker is `Send` and holds no resources).
7. `node_layers` returns the bound layers whose producer `plan` schedules, bottom first, each with the index of its link among the compositor's scheduled links (scene model spec, `resolve`), the transform, crop and opacity unchanged. A layer with `scroll` gets `NodeLayer::scroll` from its `Scroll` and the run of its `scroll_key` in `runs` (ADR-0150); a scrolling layer whose key has no run is left out (a host bug: a debug assertion). `glide_layers` takes `node_layers`' output, so its layers keep their scroll.

### How the composition root uses it

1. Register the platform's capture and audio adapters, `probe`.
2. `SceneBinder::new(types.node_types()).with_audio(AUDIO)`; `sync`. `node_types` also installs the descriptor of every built-in video filter type (`rbs_filters::catalogue`, [video-filters.md](video-filters.md)); filter settings convert with `filter_values` (edits, strict) and `filter_values_lenient` (documents).
3. For each `report.created_sources`, `instantiate` and adopt the instance into the node's partition (`Video`, `Still`, `Media` and `Browser` into the render partition, `Audio` into the audio partition; the sound node of `Media` and `Browser` into the audio partition at the binder's sound node).
4. Create the compositor (`node_layers`) and mixer instances, attach each canvas's encoders to `compositor(canvas, Program)` and `mixer(canvas)`. Mixer strips are kept per `SourceId` and rebuilt from `report.mixers[canvas].inputs` after every sync.
4a. For each scheduled filter node without an instance, create a `FilterNode` of its type with the source's known picture size and adopt it into the render partition; send new values on `report.changed_filters` (ADR-0062).
5. Apply and collect. Only then take back the instances of `retired_sources` and `retired_filters`: `Controller::take` returns idle instances only.
6. Break-before-make for `report.replaced_sources`: an exclusive device (most cameras) cannot start the new instance while the old one runs. Apply first a copy of the graph with each replacing node moved to Standby, collect (the old instance is released), then apply the graph itself.

### OS permissions (ADR-0141)

`rbs_sources::permissions` maps the four kinds a user gives permission for (Camera, Microphone, Screen and windows, System and app sound) to port kinds and folds every serving adapter's `permission` answer and permission capabilities into one `PermissionInfo` (status, whether **Allow** can prompt, whether a grant applies after a restart). The fold is pure; its precedence is Managed, Blocked, Not asked, Unknown, Asked each time, Allowed, No permission needed, and no serving adapter is Unavailable.

- `SourceTypes::permission(kind)`: calls `permission` only; fast, never prompts.
- `SourceTypes::request_permission(kind)`: calls `request_permission` on each serving adapter that says `NotDetermined` and declares the kind's `.prompt` capability, then reads again. It blocks while a prompt is shown, so the composition root calls it from a worker thread, one request per kind at a time.
- `PermissionKind::of_type(type_id)`: the kind a source type needs, if any.

Nothing is cached, polled or started.

## Errors and failure behaviour

`SourceError` (non-exhaustive): `UnknownType`, `Image(String)` (a still's picture), `NoAdapter`, `AdapterUnavailable(id)`, `NoTarget`, `InvalidSetting { key, expected }`, `Capture(CaptureError)`, `Audio(AudioIoError)`. A capture target that is gone is found by the node's `prepare` (permanent `TargetNotFound`, ADR-0038); an audio endpoint that is gone fails `instantiate` with `Audio`.

## Performance requirements

Control thread only, user-paced; nothing here runs per frame or per block.

## Conformance tests

`crates/rbs-sources/tests/sources.rs` (still types need no adapter, make a still node and refuse bad settings; the unit tests in `src/still.rs` cover colours, gradients, every sample and PNG decoding), with the reference adapters `rbs-capture-test` and `rbs-audio-io-test`: installed types follow probed kinds; an empty registry has the compositor and mixer; instance descriptors equal table descriptors for every installed type; default targets; windows need a target; stored targets are used as given; bad settings, adapters and types are refused; targets for pickers; pickers only for kinds an adapter declares, picking on another thread, a cancelled pick, a picked target instantiated by its adapter; a bound scene instantiates into the partitions the binder chose, and `node_layers` keeps the transform, crop, opacity and link order.

Hardware (manual): `cargo run -p rbs-sources --example scene_sources --release -- 15` on Windows (WGC, Media Foundation camera, WASAPI microphone and system audio through the binder, the compositor and the mixer on the engine workers).

Windows result (SC-PC, RTX 4090, 2026-11-03), PASS: every type installed with WGC, Media Foundation and WASAPI (2 displays, 7 windows, 2 cameras, 3 inputs, 2 loopbacks, 6 applications listed). Over 15 s: display 825 frames, all imported zero copy; Cam Link 4K 871 frames, all uploaded; Shure MV7+ microphone and system loopback 722,400 frames each with 0 resyncs and drift correction within -17..+33 ppm; canvas 904 frames, mix 1505 blocks; 0 GPU errors, 0 invalid or unknown mixer inputs.

## Open questions

- A settings schema per type (with the plugin SDK and control API): which keys apply live without a new node.
- Plugin source types: descriptor and factory from the plugin manifest, added to the table.
- Sources with picture and sound are two nodes ([ADR-0106](../adr/0106-picture-and-sound-on-one-source.md), implemented for media files and browser pages): `NodeTypeTable::insert_sound` names the sound node type, and the binder links the mixer from the sound node. A camera's microphone stays an [ADR-0096](../adr/0096-capture-audio-companions-and-camera-microphone.md) companion. A browser page's sound node is `org.rbs.browser.sound` (audio graph spec, "Browser sound node").
- Audio that does not depend on the scene (a global microphone or system audio in every scene).
- Restore tokens for picker-only targets (PipeWire portal): the application stores `CaptureSession::target_to_store` back into the `target` setting, which today makes a new node.

## Scrolling layers (ADR-0150)

`rbs_sources::scroll` holds the control side of scrolling. `ScrollRuns` maps each `rbs_scene::ScrollKey` to an `rbs_gpu::scroll::ScrollRun`:

- `keep(keys, now)` keeps exactly the runs of `keys`: a new key starts a run at `now`, at the key's speed, from its start (`ScrollRun::start`, held until its anchor); a run whose key is not listed ends, and a key seen again starts over. Returns whether a run started or ended.
- `moved(old_now, new_now)` moves every run to another clock (a studio restart; either sign).
- `change_speed(key, now, speed, ramp)` and `restart(key, now)` act on one run (the live calls of ADR-0150 decision 11 build on them). Both return false for a key without a run.
- `layer_scroll(scroll, run)` is the node's `LayerScroll` for a layer.

`rbs-record`'s `Studio` owns the runs. Before it sends lists it calls `keep` with the keys of every list it draws (program and further canvases, the studio-mode preview, hold scenes, a transition's outgoing picture and incoming compositor) and `scroll_keys_cued` of each canvas's program scene and, in studio mode, the preview scene, so an item faded to 0 keeps its run. Every compositor the studio builds has a scroll channel (`CompositorNode::with_scroll`); its port sends a `ScrollBinding` (which builds the device's wrap pipelines on first use) before the first list with a scrolling layer, and never otherwise, so a device on which nothing scrolls builds nothing (P13). `Studio::scroll_runs` reads the runs; `Studio::scroll_handover` takes them with the engine time, and `StudioSettings::scroll_runs` gives them to the studio that replaces it, which moves them to its clock before its first lists, so a ticker goes on where it was. A restart that changes the canvas size changes the keys (lengths are canvas pixels), so those runs start again.

Tests: `src/scroll.rs` (runs start and end with their keys, a pause is continuous and a restart starts over, runs move to a new clock either way), `tests/sources.rs` (a scrolling layer moves by its key's run; one without a run is left out), `rbs-record/tests/scroll.rs` (one run across a scene change, kept while faded to 0, ended when no list draws it, carried across a restart; no wrap pipelines until something scrolls).

## Identity and option keys (ADR-0133 decision 2)

`rbs_sources::option_keys(type_id)` lists the settings Reset source and Paste attributes may change; every other key a type uses is an identity key and is never pasted or reset.

| Class | Identity keys | Option keys |
| --- | --- | --- |
| Camera capture | `adapter`, `target` | `width`, `height`, `fps` |
| Display and window capture | `adapter`, `target` | `cursor`, `width`, `height`, `fps` |
| Output loopback audio | `adapter`, `target` | `exclude_own_process` (never written on a companion) |
| Audio input, application audio | `adapter`, `target` | none |
| Image, colour | all keys | none |
| Media file | `adapter`, `path` | `loop`, `restart_when_shown`, `start_ms`, `end_ms` |
| Browser | `url` | `width`, `height`, `fps`, `transparent` |
| Plugin types | all keys | none |

Option defaults depend on the canvas and are computed in `rbs-app` (`option_defaults`): a display or window gets the canvas frame rate as `fps`, a camera `1280 x 720` at 30 fps, a browser page the canvas size capped at 1920 x 1080; other option keys are left out (the type's own default). The Add source path uses the same values.

## Source region and display chrome (ADR-0103, ADR-0104)

`region` and `chrome` live on `rbs_scene::Source`, not in settings. The binder compares settings only, so editing them does not replace the capture node. Chrome is refused on every type except `org.rbs.source.display`. Applying chrome writes the four insets into `region`. Cover and fit place the item box; they are not a compositor scale. `work_rect` is copied from `SourceTarget` when Reset runs, and Reset does nothing when that field is absent.

The settings Region section and the add-source region step are not in the desktop UI yet (rule 16). The capture port's `region_grab` (long side at most 960, not `thumbnail`) is what that preview will call.

## Media file (`org.rbs.source.media`, ADR-0108)

Two node types (ADR-0106, ADR-0108 decision 1): the picture node `org.rbs.media` (`rbs_nodes::media_descriptor`, one video output) and the sound node `org.rbs.media.sound` (`rbs_nodes::media_sound_descriptor`, one audio output). `SourceInstance::Media { picture, sound, control, stats, adapter }` and `SourceClass::Media`. Changing `path` replaces both nodes (existing binder break-before-make). `loop` (default true), `restart_when_shown` (default true), `start_ms` and `end_ms` are live keys (ADR-0108 decision 12, `NodeTypes::live_keys`): an edit that changes only these keeps both nodes, and the binder lists the source and the changed keys in `BindReport::live_changed`; an edit that also changes `path` or `adapter` replaces the source. The source stores both flags and the media cursor with no decoder while Standby.

**Play range (ADR-0108 decision 11).** `start_ms` (integer milliseconds of media time, at least 0, default 0) and `end_ms` (at least 0, absent for the end of the file) trim the file without re-encoding. `end_ms` must be greater than `start_ms` when both are set, or the setting is `InvalidSetting` (`end_ms`, "greater than start_ms"); a negative or non-integer value is `InvalidSetting` on its own key. `instantiate` hands the range to the control handle (`MediaControl::set_range`) before the first wake; a later edit reaches it through the live keys below. Values past the end of the file are clamped at open: a start at or past the stated duration shows the last picture. The worker:

- starts every run, play and seek inside the range: a position before the start, or at or after the end, becomes the start; `seek` clamps a position past the end to just before it (the last picture of the range);
- ends a pass at the first picture stamped at or after the end (the first picture of a pass is always shown) and at the first sound sample at the end, padding with silence to it;
- with `loop`, seeks back to the start and maps the next pass to begin where this one's range ended (the pass length is the earlier of the range end and the later end of its pictures and sound, minus the start), so a loop leaves no timestamp gap; without `loop`, the end of the range is the end hold;
- looks back for the picture that covers the start when the adapter gives the first picture after it (the port allows that) and that one is past the end, or gives none: it seeks back 100 ms, doubling, and keeps the last picture at or before the start, so the range shows a picture of its own;
- holds the one picture and seeks no more when a looping range is shorter than one picture, or holds no picture past the start; the state stays `Playing` until a command.

**Live keys (ADR-0108 decision 12).** For each source in `BindReport::live_changed` the studio calls `rbs_sources::apply_media_live(control, settings, keys)` on the control path, which sends each changed key to the control handle: `restart_when_shown` to `set_restart_when_shown` (read by the next transition of the on-air table and the next wake), `start_ms` or `end_ms` to `set_range`, then `loop` to `set_loop`. A live `loop` takes effect at the next end of the range while playing (the worker reads it there), and at once otherwise: an end hold whose `loop` becomes true restarts from the range start (cued when not on air). The host checks the settings (`validate_settings`) before it writes the document, so an invalid pair never reaches the handle; `apply_media_live` still reports a refused value and applies the others. The document write is the usual one undo step per edit.

`MediaControl::set_range(start, end)` refuses `end <= start` and changes nothing; otherwise a cursor outside the new range moves to its start (a seek while playing, paused or cued; stored when no worker runs), and a new end takes effect when the worker reaches it.

`path` must canonicalize to a local regular file before any decoder `pull`. A URL, UNC path, device path, directory, pipe, or symlink, junction, or substituted drive that does not resolve to a local regular file is `InvalidSetting`. A missing or undecodable file is `SourceError::Media(String)`. A side over 8192 or one frame over 128 MiB is `InvalidSetting` before either queue is allocated.

Decode is the port `rbs-media-decode` ([media-decode-port.md](media-decode-port.md): per-stream reads, capability `media.decode.pixel` of `zero-copy` or `cpu-upload`). No real adapter is registered yet; the platform adapters come next (ADR-0108 decision 4). The probe tries each candidate adapter in order and keeps the first that yields a picture. Conformance uses `rbs-media-decode-test`. While Active the node keeps two lock-free queues (video lookahead at most 8 frames and 128 MiB, audio at most four graph blocks). `process` only dequeues. End of file with `loop` false joins the worker on the control path, drops the queues, holds the last picture, and emits silence.

### Playback control handle (ADR-0108 decision 10)

`rbs_nodes::MediaControl`, shared by the picture node, the sound node and the worker; `SourceInstance::Media::control` hands it to the host. Its commands are called on the control path only; they never write the document, are not undo steps, and do not replace the source.

- `play()`, `pause()`, `seek(position)`, `restart()`. `restart` is a seek to the range start followed by play. Play while playing and pause while paused do nothing. A position past the stated duration is clamped to it, and every position into the play range.
- Every command writes a **record**: a generation (one more than the last), an instant, a `paused` flag and, for a seek or restart, the position. The handle keeps the last 4 records in a fixed array under a sequence counter (a seqlock). The instant is the master time now plus the picture node's last render period (50 ms before its first two ticks), rounded up to the audio block grid once the sound node has seen a block. Not built yet: the longest render period among several partitions that hold the picture node (today there is one), and merging the seeks of a scrub into one record per render period.
- **Items.** Every picture item and sound block carries the generation it was decoded for. At a tick or block time `t` a node reads the 4 records without retrying: on a torn read it keeps the records of its previous read (counted in `MediaStats::torn_controls`). An item of generation `h` plays when its own stamp is due and, if the record of `h + 1` is present, its stamp is before that record's instant. An item whose generation is older than every record is skipped and returned; an item whose generation is newer than every record read waits.
- **Worker.** Each pass it applies every record newer than the last one it applied, in generation order, so no command is lost when several arrive between two passes (a record that already left the array is skipped). Pause: the cursor becomes the media time at the instant and the worker stops decoding (it keeps the decoder until the pause release below). Seek while paused: it seeks, decodes the first picture that ends after the position, stamps it at the instant, and stops again; the picture node shows that picture and holds it. Play, seek and restart while playing: it seeks to the position (the cursor for play), and anchors again one `LEAD` after the instant (no picture is stamped before that anchor), so the position plays at the anchor with no picture before it and no sample skipped or doubled. A loop pass starts from the position.
- **Silence and underruns.** From a pause's instant the picture node repeats its last texture and the sound node writes silence; neither is an underrun. Silence between a command's instant and the worker's first block of the new generation is not an underrun either.
- **No worker.** In the end hold (`ended`) play and restart start a worker from the range start; seek stores the cursor and starts a worker that decodes one picture at it, paused. In Standby a command is stored: the next start uses the stored cursor and paused flag. Following the decision 10 table, a source that sleeps paused or ended wakes paused or ended (showing the picture at its cursor) only with `restart_when_shown` false; with true it plays from the start. A source that went to sleep from the end hold reports `Ended`.
- **On air and cued.** `set_on_air(bool)` is the source's membership in the binder's on-air set (a program or hold scene on some canvas draws it, ADR-0107 decision 6); the host writes it in the same apply as the Active set. It is true until the host first writes it. A source that is Active but not on air is `Cued`: it shows its picture, writes silence (not an underrun) and does not advance. The transitions follow the decision 10 table, and each writes the handle as the matching command: Standby to preview only is cued at the start (`restart_when_shown` true) or at the stored cursor; cued to on air plays, motion and sound one `LEAD` after the instant; playing to preview only is a pause, plus a seek to the start with `restart_when_shown` true; paused or ended stay so in a preview and, on air again, stay so with `restart_when_shown` false or play from the start with true. A source that sleeps cued wakes by the Standby rows. While not on air, `play` and `restart` do not play: they cue (restart cues at the start), and the source plays on the take; `pause` of a cued source makes it paused, so it no longer plays on the take; `seek` moves the cued picture, and the take plays from there with `restart_when_shown` false or from the range start with true (the table's row, the same whether or not the source slept).
- **Pause release.** `service()` is called by the host on the control path about once per frame. When the newest record is a pause (paused or cued) and its instant is `pause_release` ago (`set_pause_release`, the `pause_release_s` app setting, default 30 s), it joins the worker, which frees the decoder, the file handle and the worker's buffers. The state, the cursor and the picture node's last picture stay; `is_released()` reports it. The next command that needs a worker starts one: play opens the file again and anchors one `LEAD` after its own start; a seek starts a worker for the one picture at the new position, shown from the seek's instant, and the timer releases it again. A picture node that is prepared while released starts a worker for its picture too. The timer counts from the later of the newest record's instant and the worker's start, so a source that wakes paused or cued keeps its worker for the full time. Until the async open is built, a command that starts a worker after the release waits on the control path for the open and the first picture. When the file cannot be opened again (it moved, its drive left), the command returns the error, the source reports `Unavailable` and keeps its last picture, and the next command tries again. A worker that already reached the end of the file is not released; it goes to the end hold. Not built yet: the picture ring and the one picture the node holds stay until the next start (the rings still belong to a worker run).
- **State.** `state()` is `Playing`, `Paused`, `Cued`, `Ended`, `Standby` (asleep with no stored pause or end; a source asleep from cued is `Standby`) or `Unavailable` (a fault). `cursor()` (while paused, where the pause stopped) and `duration()` (the stated duration, `None` until known) are published too. A command reads the state and writes its record under the player lock, so two callers cannot interleave.
- **Stinger** (ADR-0140 decisions 2, 4 and 5). `arm() -> Result<u32, MediaNodeError>` is a runtime override for one transition, never written to the document: it saves the `loop` and `restart_when_shown` keys and sets both false, clears `cued` and the stored end, sets the shared `keep_alpha` flag (a worker that runs without it is released, so the next one opens keeping alpha), and writes a paused record at the start of the play range (from the end hold, a paused start there). It returns that record's generation. While armed, `set_loop` and `set_restart_when_shown` change the saved keys only. `picture_ready(g)` is true once the worker queued the first picture of generation `g` (the worker stores the generation of every picture it pushes, plus one, in an atomic; `process` is unchanged). `play_now() -> Timestamp` writes a play record and returns the anchor the worker uses, the record's instant plus `LEAD`. `disarm()` gives the saved keys back and clears `keep_alpha`; `is_armed()` tells whether it is armed. `set_alpha_alternates(factories)` names the adapters tried in order after the source's own answers `AlphaUnsupported` (the registry passes the candidates after the chosen one). Not yet: keeping the primed picture when the play position equals the paused one (the worker seeks again).
- **Cost.** A source nobody controls writes one record when it starts. `process` reads 4 records (about 20 atomic loads) and compares generations; it takes no lock and allocates nothing.

`SourceInstance::Media` holds `picture` (`rbs_nodes::MediaPictureNode`), `sound` (`rbs_nodes::MediaSoundNode`), `stats`, and `adapter`. `node_types()` lists `org.rbs.media` for `org.rbs.source.media`, `org.rbs.media.sound`, and the pair (`NodeTypes::sound_type`). The studio keeps each media instance's `MediaControl` (`Studio::media_control`), writes its on-air membership from `BindReport::on_air_sources` before each apply, and calls `service` from `Studio::service`; the control API's `media.*` methods (protocol 1.83) run its commands. `Studio` in `rbs-record` adopts the picture node into the render partition the way it adopts a still, and the sound node into the audio partition at the binder's sound node once the plan schedules it (it holds the sound node until then, and drops it when audio is not bound, so the worker decodes no sound). The source counts as scheduled when either node is in the plan, so a source that is only heard is instantiated and plays. The instance is recorded as soon as the picture node is adopted, and the sound node is marked adopted only after the engine took it. It stores the decoder adapter id. It does not register a decoder and does not link FFmpeg (ADR-0007). `org.rbs.source.media` is installed only when the host has registered and probed a `MediaDecoderFactory` (the same rule as a capture type that has no adapter). Tests register `rbs-media-decode-test`. The host does not write `adapter` unless the user set it. When `adapter` is absent, pick `zero-copy` before `cpu-upload`, then registration order. The sound node is the strip producer (ADR-0106).

## Browser (`org.rbs.source.browser`, ADR-0109)

Network-capable, including a `file` URL. No click forwarding yet, no JS binding, no device prompt. Page sound is a second node (audio graph spec, "Browser sound node"). Straight-alpha sRGB `Rgba8`.

Settings (keys in `types::keys`, `rbs_sources::BROWSER_KEYS`; checked by `validate_browser`, read with defaults by `browser_settings`; scene format 22 when any key after `transparent` is stored, [user data](user-data.md)):

| Key | Kind | Default | Range or values | ADR |
| --- | --- | --- | --- | --- |
| `url` | Text | absent | `http`, `https` or `file`; absent: no address yet | 0109 |
| `width`, `height` | Int | 1920, 1080 | even, at most 8192 | 0109 |
| `fps` | Int | 30 | 1..=60 | 0109 |
| `transparent` | Bool | false | opaque alpha when false | 0109 |
| `css` | Text | empty | at most 65 536 bytes (`MAX_CSS_BYTES`) | 0132 d4 |
| `reload_on_file_change` | Bool | false | a `file` page only | 0132 d5 |
| `keep_signed_in`, `profile_id` | Bool, Text | false, absent | `profile_id`: 32 lowercase hex digits, set only by the server (a client write is `invalid_params`); counts only while `keep_signed_in` is on | 0132 d10 |
| `reload_every_s` | Int | 0 | 0 or 10..=86400 | 0149 d2 |
| `script` | Text | empty | at most 32 768 bytes (`MAX_SCRIPT_BYTES`) | 0149 d3 |
| `script_on_load` | Bool | false | | 0149 d3 |
| `script_every_s` | Int | 0 | 0..=86400 | 0149 d3 |
| `zoom_percent` | Int | 100 | 25..=500 | 0149 d4 |
| `keep_running` | Bool | false | stored; the background slot class is not built yet | 0149 d5 |
| `retry_on_error` | Bool | true | | 0149 d6 |

A source with no `url` (Add Source › Browser makes one, and the Page tab opens with the address field focused) is Unavailable with `browser_no_address`: it holds no slot, starts no process and does not back off. Writing `url`, `css`, `script`, `script_on_load`, `script_every_s`, or `keep_running` true needs the network permission (`BROWSER_NETWORK_KEYS`, `rbs-app` `studio_host/network.rs`). A token (any token) is refused with `not_permitted` and reason `local_only` when a write of `script`, `script_on_load`, `script_every_s`, `keep_signed_in` or `url` would leave a non-empty script with a run setting on in a `keep_signed_in` source, and for `browser.run_script` with a `script` argument on such a source (ADR-0149 decision 3). When `keep_signed_in` turns on the server stores a new `profile_id`; the composition root gives the runner the directory `browser-profiles/<profile_id>` in the local data directory (`BrowserTree::profile_dir`, only for a well-formed id).

**Page automation (ADR-0132, ADR-0149), as built.** `Configure` carries `PageOptions` (CSS, zoom, script, run settings, timers, retry, file-change reload, profile directory) as a tail after `sample_rate`; an older message without the tail decodes as the defaults. After `configure` the studio sends control messages on the same pipe (`rbs_browser_ipc::control`: a 4-byte length, a kind byte, a body; `2` reload with a bypass-cache byte, `3` run script with the script's UTF-8 bytes, empty for the saved one; an unknown kind is skipped, a closed pipe or a length out of bounds stops). `MAX_PAYLOAD` is 128 KiB. The studio sends them through `BrowserControlSidecar::send` → `BrowserSession::send` (`rbs-app` `RunnerSession` → `Supervisor::send`, which writes to the helper's stdin under a lock on the control path); false when no page runs. The CEF runner reads them on a stdin thread and applies them on CEF's UI thread. Its main-frame load cycle is `rbs_browser_ipc::automation::Cycle`, a pure state machine fed with the runner's own clock: a new main-frame document is held (no frame sent; the studio keeps the last good picture) until its response status is known and, when CSS or a zoom other than 100% applies, 50 ms (`SETTLE`) after the commit, bounded by 500 ms after its first paint on a first load or retry and 2 s from the start of a reload; a load that failed (a network error, or 408, 429, 502, 504 with `retry_on_error`) is never sent and is retried after 2, 4, 8, 16, 32, then every 60 s (`Retry-After` of 1..=600 s replaces a step; a good load resets). At each main-frame commit the runner sets the zoom (`zoom_level`: ln(p/100)/ln(1.2)) and applies the CSS with the one fixed script `css_script` (a constructed stylesheet; the CSS is only an escaped string value). After a finished load it runs the script when `script_on_load`, then every `script_every_s`, and reloads after `reload_every_s`; timers pause while loading or waiting for a retry. The script runs only while the main frame is on the saved URL's exact origin, or for a `file` page under the saved file's directory (`script_may_run`); the result is discarded. With `reload_on_file_change` on a `file` page the runner checks the page's directory (not recursive) every 500 ms and reloads on any change. A kept profile directory is created private to the user and not removed on exit; otherwise the profile is temporary. Not yet: the status channel back (`page`, `script`, `css_failed`), a running page taking setting changes without a restart (the binder restarts the source on a settings edit), `keep_running`, the Interact window and `browser.input`, Forget sign-in and the profile sweep. Tests: `rbs-browser-ipc` (`automation`, `control`, configure round trip), `rbs-browser-runner` `tests/helper.rs` (`control_messages_reach_the_running_helper_in_order`; the test double shows the count and kind of the last message in its first pixel), `rbs-sources` `tests/browser.rs` (`control_messages_reach_only_a_running_page_with_its_profile`, `page_settings_have_their_bounds`, `page_settings_take_their_defaults`), `rbs-app` `studio_host::network` and `browser_tree` unit tests.

The studio does not link or load CEF. A real CEF build replaces the helper executable and is a process tree. This tree's stand-in is the `rbs-browser-runner` binary (`rbs-browser-ipc` messages). The CEF tree root is `rbs-browser-cef` (ADR-0132 decision 1), built outside the workspace with `cargo build --release --manifest-path crates/rbs-browser-cef/Cargo.toml`; it ships next to the app and loads the CEF runtime from the browser engine add-on (ADR-0145, [add-ons spec](add-ons.md#browser-engine-consumer)), which the studio names in `RBS_CEF_RUNTIME`. Without an installed engine a browser source is Unavailable with `browser_runtime_missing`, holds no slot and does not back off; an engine of another version is `browser_runtime_mismatch`; no runner at all is `helper_missing`; any other runner exit is `helper_exit`. It paints in software into the section (converting CEF's premultiplied `Bgra8` to straight `Rgba8` while it copies, so the studio adds no pass), runs headless on Linux (no X11 or Wayland), takes page audio away from the OS output and writes it into the studio's sound section (stereo at the graph's rate in 10 ms packets, each stamped with the OS monotonic clock; dropped when the studio names no section), cancels popups, downloads, file pickers and JavaScript dialogs, denies device permissions, limits a `file` page to its own directory (`rbs_browser_ipc::page`), and keeps its profile in a new private temporary directory removed on exit (or in the kept directory of a `keep_signed_in` source). Frames cross a studio-allocated shared memory section (`rbs_browser_shm::FrameRing`, ADR-0109 decision 8): a named POSIX shared memory object on macOS and Linux, a pagefile-backed section on Windows, created before the helper starts and named in `configure`. Layout: a 128-byte header (magic `RBSB`, layout version 1, width, height, pixel layout 1 = `Rgba8` straight or 2 = `Bgra8` premultiplied, slot count 3, the triple-buffer exchange word, frames written, heartbeat, then at offset 48 the paint time of each slot in OS monotonic nanoseconds, 0 when unknown) and three slots of width × height × 4 bytes. The helper and the studio each own one slot and swap the third through the exchange word, so neither waits and a slot is never written while read. The helper adds one to the heartbeat at least every 500 ms; two seconds without a change is a hang. Before its first beat a helper gets 15 seconds from spawn (`STARTUP_LIMIT`), because a cold engine start blocks before it can beat. A stop closes the helper's stdin and gives it one second (`STOP_GRACE`) to exit on its own, on the supervisor thread, then kills it; the session's `stop` never waits. A studio supervisor thread, which exists only while the source is Active, copies the newest frame into the source's sink at twice the page rate and checks the process and the heartbeat. It stamps each picture with master time at that edge, by the rule page sound follows (`BrowserPictureEdge`: an `EdgeClock` fed with OS and master times read back to back; a paint time of 0 or after now counts as now), and makes the system frame there (`BrowserFrameSink::publish_at`; it keeps one reference to the bytes and reuses them when the render thread has let go, so the render thread never frees them). The picture node takes the newest picture whose stamp is due on each render tick (`BrowserFrameSink::take_due`: `try_lock`, never waits, never allocates; a picture not taken is replaced by a newer one), uploads it and sets its output, which keeps it until the next one. So a picture is on air at the master time of the sound painted with it (ADR-0106 decision 4, ADR-0109 decision 8); a source without a sound node has no clock and its pictures are due at once. Test: `rbs-sources` `browser_sound::tests::a_picture_and_its_sound_reach_the_program_at_one_master_time`, `tests/browser_gpu.rs`; an exit or a hang marks the source Unavailable and releases its slot. The studio never forms a reference to section bytes the helper may write (raw copy). Page sound crosses a second section (`rbs_browser_shm::SoundRing`, named in `configure` when `sample_rate` is not 0): a 128-byte header (magic `RBSA`, layout version 1, sample and packet capacities, samples and packets written and read, packets dropped), 128 packet entries (first sample, OS time, samples, rate, channels) and a ring of 65 536 `f32` samples. The helper drops a packet that does not fit; the studio checks every entry against its own position before it copies, and skips to the present on a malformed one. The supervisor reads it every 5 ms into the source's `BrowserSoundFeed` (audio graph spec, "Browser sound node"). The test double paints opaque and writes sound only with `--tone` (tests); `transparent` false is applied by the helper, not by a studio pass. `Studio` adopts `SourceInstance::Browser` into the render partition and shares one `BrowserBudget` (`InstanceContext::with_browser`). Before each apply it grants slots to shown pages: program before preview, lower source id inside a class, at most two. Headless crates pass `Studio::start_with_tree` from the composition root (`rbs-app` starts `rbs-browser-cef` through `browser_tree::RunnerBrowserTree`; tests use `rbs-browser-helper`). Without a tree, prepare reports that no helper was set. A source without a slot reports `browser_cap` and starts nothing. A helper exit is Unavailable, releases the slot, counts toward back-off, and must not panic the host. The studio allocates the frame buffer; the render path drops it without joining a dead child. Installed and Standby start no process. An agent without the network permission gets `not_permitted` / `network` for any `url`, including `file` (`agent_may_set_browser_url`). The control API kind is not in `rbs-api` yet; the reason string is the contract.


## Text (`org.rbs.source.text`, ADR-0129)

Implementation status: phases (a) to (f). The type, its settings, the node, the text worker, the binder's in-place updates and the host wiring exist, the control API adds and edits text sources (control-api.md, "Protocol 1.75") and runs timers (control-api.md, "Protocol 1.84"), and live kinds are drawn ("Live kinds" below). The desktop app registers `rbs-text-parley` on the system's fonts, so the type is installed; Add source lists Text, a text row uses the `text` icon, and Properties edits it on its Text tab (ui-design-system.md, "Text tab"; host commands `AddText`, `SetSourceSettings`, `FitTextBox`, `ListTextFonts`, `TextTimer`; rbs-app `studio_ui::text_props`). Crisp resizing is in (phase (e), "Crisp resizing" below). Hold start and take start do not wait for entering texts; a program switch does.

```rust
// rbs_sources::types
pub const TEXT_SOURCE: &str = "org.rbs.source.text";   // in ALL (11 types)
pub enum SourceClass { ..., Text }                     // video pin only; a picture-only source
// rbs_sources
pub fn is_still(type_id: &NodeTypeId, settings: &Settings) -> bool; // class, except text: its settings
impl SourceTypes {
    pub fn register_text(&mut self, renderer: Arc<dyn TextRenderer>); // installs the type
    pub fn text_renderer(&self, block: &TextBlock) -> Option<Arc<dyn TextRenderer>>; // colour glyphs first when the text has emoji
}
pub enum SourceInstance { ..., Text(Box<TextSource { node: TextSourceNode, frames: ParamSender<Option<TextFrame>>, stats, settings: TextSettings }>) }
// rbs_sources::text
pub const DEFAULT_BOX: [u32; 2] = [640, 160];
pub const TEXT_KEYS: &[&str];
pub enum TextKind { Text, Clock, Countdown, Stopwatch, RecordingTime, StreamTime } // as_str, parse
pub struct TextSettings { kind, block: TextBlock, file, file_lines, fields, format, target, duration_ms, end_text } // is_still()
pub fn text_is_still(settings: &Settings) -> bool;
pub fn validate_text(settings: &Settings) -> Result<TextSettings, SourceError>;      // edits: strict
pub fn read_text_lenient(settings: &Settings) -> (TextSettings, Vec<&'static str>); // documents: clamp, report keys
pub fn has_color_glyphs(text: &str) -> bool;
```

Settings (keys in `types::keys`; ranges are the `rbs_text` constants):

| Key | Kind | Default | Range or values |
| --- | --- | --- | --- |
| `kind` | Text | `text` | `text`, `clock`, `countdown`, `stopwatch`, `recording_time`, `stream_time` |
| `text` | Text | empty | at most 10 000 characters; `\n` breaks a line |
| `file`, `file_lines` | Text, Int | none, 0 | a local file; 0 = whole file, N = last N lines |
| `font` | Text | system UI font | a family name |
| `size` | Float | 72 | 1 to 1024 px |
| `weight` | Int | 400 | 100 to 900 |
| `italic`, `wrap` | Bool | false, true | |
| `color`, `color2`, `angle` | Text, Text, Int | `#ffffff`, none, 0 | `#rrggbb[aa]`; `color2` makes a gradient; -360 to 360 degrees |
| `width`, `height` | Int | 640 x 160 (documents only) | 1 to 4096 each, at most 8 388 608 pixels; required for an edit |
| `align`, `valign`, `overflow` | Text | `center`, `middle`, `fit` | `left`/`center`/`right`, `top`/`middle`/`bottom`, `fit`/`clip`/`ellipsis` |
| `line_spacing`, `letter_spacing` | Float | 1, 0 | 0.5 to 3, -50 to 200 px |
| `outline_width`, `outline_color` | Float, Text | 0 (none), `#000000` | 0 to 64 px |
| `shadow_color`, `shadow_x`, `shadow_y`, `shadow_blur` | Text, Float | none (no shadow), 0 | ±256 px, 0 to 64 px |
| `background`, `padding` | Text, Float | none, 0 | 0 to 1024 px |
| `fields` | Bool | false | `{...}` fields in `text` ("Live kinds") |
| `format` | Text | per kind | a clock or timer pattern ("Live kinds") |
| `target` | Text | none | a countdown's time of day, `HH:mm` or `HH:mm:ss` |
| `duration_ms` | Int | none (0) | a countdown's length, 0 or more |
| `end_text` | Text | `00:00` for a countdown, else empty | shown when a countdown reaches zero and while a recording or stream is stopped |

A number key accepts an integer. `validate_text` refuses any other key, a value of the wrong kind or out of range (`InvalidSetting` naming the key), an unknown `kind` and a missing box. `read_text_lenient` never fails: it gives a wrong kind its default, clamps a value out of range (a box over the pixel cap keeps its width and is shortened), reads an unknown `kind` as `text`, and returns every key it changed for `SourceSettingsAdjusted`. Tabular figures are on for every live kind and for `fields`.

**Still or not.** `is_still` answers from the class for every type except text; a text source is still only when `kind` is `text` (or absent) and `fields` is off. `rbs-record`'s picture form (`video_as: auto` writes images only for a still source; `image` on a text that is not still is `ImageOnMovingSource`) and the host's `set_record_separately` `video_as` check ask `is_still`. Recording sound separately is refused for a text source like any picture-only source (`AudioOnPictureOnly`), whatever its settings. Tests: `rbs-sources` `text::tests`, `rbs-record` `iso::tests::a_text_source_is_still_only_as_plain_text`.

**Instantiate.** With no renderer registered the type is not installed and `instantiate` is `NoAdapter`. Otherwise it reads the settings leniently and returns `Text` with a node that shows nothing until the worker sends a picture through `frames`.

### Live kinds (ADR-0129 decision 11)

`rbs_sources::text_live` holds the pure parts; the worker calls them on the control thread with its injected clocks.

```rust
pub struct LocalTime { year, month, day, hour, minute, second, weekday /* 0 = Monday */, nanos } // LocalTime::utc(SystemTime)
pub enum Language { English, Turkish }                   // weekday names; the app's UI language
pub fn parse_target(&str) -> Option<u32>;                // HH:mm[:ss] -> seconds into the day
pub fn format_clock(&LocalTime, pattern, Language) -> String;
pub fn format_elapsed(seconds: u64, pattern) -> String;
pub fn default_timer_pattern(seconds) -> &'static str;   // mm:ss, H:mm:ss from one hour
pub struct RunTime { value: Duration, since: Option<Timestamp> } // at(now), paused(now); master clock
pub struct LiveInputs { timers: BTreeMap<SourceId, RunTime>, recording, stream: Option<RunTime>, scene: String, language }
pub fn shown(source, &TextSettings, &LiveInputs, &LiveClocks) -> Option<Shown { text, next: Option<Duration> }>;
pub fn expand_fields(text, &LiveInputs, &LiveClocks) -> Shown;
// rbs_sources::text_worker
pub trait TextClock { fn now(&self) -> SystemTime; fn local(&self) -> LocalTime /* UTC */; fn until_local(&self, seconds: u32) -> Duration /* UTC */; }
// rbs_sources::SourceTypes
pub fn set_text_clock(&mut self, Arc<dyn TextClock>); pub fn text_clock(&self) -> Option<Arc<dyn TextClock>>;
```

- **Patterns.** `HH` (two-digit hour), `H`, `mm`, `ss`, `dd`, `MM`, `yyyy`, `ddd` (weekday name) and text in single or double quotes (an unclosed quote runs to the end); any other character is itself. A clock uses `HH:mm:ss` without `format`. A timer uses `mm:ss`, with `H:` from one hour. In a timer pattern the largest unit present takes the whole (`mm:ss` shows 75:00 for 75 minutes), `dd` is days, and `MM`, `yyyy` and `ddd` show nothing. A countdown rounds up, so it shows `00:01` until it reaches zero; elapsed times round down.
- **Kinds.** `clock` shows `TextClock::local`. `countdown` with a valid `target` counts to its next occurrence (`TextClock::until_local`, zero at that second, tomorrow's once passed); otherwise it counts down `duration_ms` (0 when absent) with its timer. At zero it shows `end_text` (`00:00` by default). `stopwatch` counts its timer up. `recording_time` and `stream_time` show the main canvas's main recording and stream (`end_text`, default empty, while stopped). A live kind ignores `file`; `fields` applies to `text` only, so a file source shows its file as it is.
- **Fields.** With `fields` on, `{time}` (`HH:mm:ss`), `{date}` (`yyyy-MM-dd`), `{time:PATTERN}`, `{scene}`, `{recording_time}` and `{stream_time}` (empty while stopped) are replaced; any other brace text stays as it is.
- **Weekdays.** English and Turkish tables in `Language`; the app gives `Language::English` until the UI has languages.
- **Updates.** The worker keeps each live source's last text and when it can change next: the next whole second of the wall clock for a wall value, of the value itself for a running timer or elapsed time. `tick` draws an on-air live source when its time has come and its text changed; a value that stands still (a paused or unstarted timer) is not drawn again. `attach` and `set_settings` carry the text in the same command, so a new live source is drawn once, never empty; `refresh` draws it up front with what it shows now. `next_due` tells the host when to service again; `Studio::service` lowers its `wait` to it. Inputs come through `set_live(&LiveInputs)` (or `set_timer`, `set_elapsed`, `set_scene`, `set_language`); a change marks the affected sources for the next tick.
- **Clocks.** The composition root sets the local clock (`rbs-app` `text_clock::LocalTextClock`, `chrono::Local`). A local time in a DST gap resolves to the first valid minute after it; one that occurs twice resolves to the first. Without a clock set, `TextClock` reads UTC.

Studio (`rbs-record`):

```rust
pub enum TimerAction { Start, Pause, Reset }   // Start on a finished countdown starts it over
pub enum TimerRefused { NotFound, NotATimer }  // a timer is a countdown without a target, or a stopwatch
pub struct TextTimer { source, kind: TextKind, running: bool, value: Duration } // value: time counted
impl Studio {
    pub fn text_timer(&mut self, source, action: TimerAction) -> Result<TextTimer, TimerRefused>;
    pub fn text_timer_kind(&self, source) -> Option<TextKind>;
    pub fn text_timers(&self) -> Vec<TextTimer>;            // timers that are not zero and stopped
    pub fn set_text_timers(&mut self, timers: &[TextTimer]); // a running one runs on from now
    pub fn set_text_outputs(&mut self, scene: &str, recording: Option<RunTime>, stream: Option<RunTime>);
    pub fn set_text_language(&mut self, language: Language);
}
```

The host (`rbs-app`) calls `set_text_outputs` before each service: the program scene's name (the scene a held switch is going to), and how long the main canvas's main recording (not while ending; paused time left out) and stream have run, on the studio's master clock. A studio restart hands the timers and elapsed times to the new studio with the running part folded in, so they run on; a collection switch saves the old collection's timers and restores the new one's. Timers are saved in `text-timers.toml` (user-data spec, "Text timers") on every `text.timer` and at exit.

Tests: `text_live::tests` (UTC dates and weekdays, every pattern token and quoted text, timer units, targets, a clock, both countdowns, stopwatch and elapsed times, fields), `tests/text_worker.rs` (a clock on air is drawn each second and not off air; a stopwatch follows its run state and draws only on a change, once at attach; fields show the scene and the recording time), `rbs-record` `tests/text_source.rs` `timers_start_pause_reset_and_carry_across_a_restart`, `rbs-app` `text_timers::tests`, `studio_host::text_live::tests`, `text_clock::tests` and `tests/studio_api.rs` `a_control_api_client_adds_and_edits_a_text_source` (`text.timer`).

### Crisp resizing (ADR-0129 decision 12)

```rust
// rbs_sources::text_resize
pub struct Resized { settings: Settings, crop: Crop, size: [f32; 2] }
pub fn resized(settings: &Settings, crop: Crop, size: [f32; 2]) -> Option<Resized>;
// rbs_scene
impl SceneCollection { pub fn resize_text(&mut self, item, settings, layout) -> Result<Undo, SceneError>; } // Op::Batch [ReplaceSource, ReplaceItem]
```

- **Rule.** The item shows `crop` of the box at `size` canvas pixels; the factors are `size / (box - crop)` per axis. The new box is the old box times each factor, rounded to whole pixels; the crop scales per axis. `size`, `outline_width`, `shadow_x`, `shadow_y`, `shadow_blur`, `padding` and `letter_spacing` scale by the vertical factor, or by the smaller factor when the two are within 1 % (aspect kept); keys that are absent stay absent (`size` is always written), values are clamped to their ranges and kept to two decimals. The item's new size is the new box less the new crop, so it draws one picture pixel per canvas pixel; its sign (a flip), position and rotation stay.
- **Caps.** A side of 4096, 8 388 608 pixels and `size` 1024: the factors are lowered together until all hold, the box stops there and the item keeps the size asked for, so the rest is stretched. `None` (stretch only) when the box would not change, the picture shows nothing, or a crop would cover the new box.
- **Host** (`rbs-app` `studio_host/text_resize.rs`). A box edit on the main canvas (`item.set_box`, the preview, Transform's size) of a text item is crisp when the source is placed once in the whole collection, the item has no layout of its own on any canvas, and no running take writes its separate picture track. A single edit (`gesture` 0) changes the box in the same undo step at once. A drag (a `gesture`) is stretched while it moves; once it rests for `CRISP_SETTLE` (250 ms, master clock) the box changes, recorded under the drag's gesture so drag and box undo as one step. Anything else stretches as before. The vertical canvas and Transform's Fit and Reset stretch for now.

Tests: `rbs-sources` `text_resize::tests` (doubling, wider and taller boxes, crop per axis and rounding, caps, nothing to change), `rbs-scene` `tests/collection.rs` `a_text_resize_changes_settings_and_layout_as_one_undo_step`, `rbs-app` `tests/studio_api.rs` `a_control_api_client_adds_and_edits_a_text_source` (a single box edit and a rested drag, each undone in one step).

### Text worker (ADR-0129 decisions 6 and 7)

`rbs_sources::text_worker::TextWorker` draws every text source on one render thread and reads files on one probe thread. Control thread only; no call blocks it.

```rust
impl TextWorker {
    pub fn new(renderers: Vec<Arc<dyn TextRenderer>>, files: Arc<dyn FileProbe>,
        clock: Arc<dyn Clock>, wall: Arc<dyn TextClock>) -> Self;   // starts no thread
    pub fn attach(&mut self, source, frames: ParamSender<Option<TextFrame>>, settings: TextSettings) -> u64;
    pub fn set_settings(&mut self, source, settings: TextSettings) -> Option<u64>; // None: unchanged
    pub fn refresh(&mut self, source) -> Option<u64>;   // entering the on-air set: front of the queue, file read now
    pub fn detach(&mut self, source);                   // the node was retired; its last picture stays cached
    pub fn set_on_air(&mut self, sources: BTreeSet<SourceId>);
    pub fn families(&mut self) -> Families;             // Ready(cache) or NotReady; refreshed at most once a minute
    pub fn take_problems(&mut self) -> Vec<(SourceId, ProblemSlot, Option<TextProblem>)>;
    pub fn tick(&mut self);                             // control tick: polls, hang checks, idle stop
    pub fn stop(&mut self);                             // never waits on a hung call
    pub fn is_running(&self) -> bool;
    pub fn detached_probes(&self) -> usize;
}
pub trait TextClock: Send + Sync { fn now(&self) -> SystemTime; }   // SystemTextClock
// rbs_sources::text_file
pub trait FileProbe: Send + Sync { canonicalize, metadata -> Option<FileMeta> (None: not a regular file), read(path, head, tail) }
pub struct RealFileProbe;
pub fn decode(head, tail, whole) -> Result<String, FileTextError>;  // UTF-8 with or without BOM, UTF-16 LE/BE with BOM
pub fn last_lines(text, n) -> String;
```

- **Threads.** None until the first `attach` or `families`. Both stop on `tick` when no source is attached and nothing was asked for `IDLE_STOP_AFTER` (30 s, master clock); the renderers are trimmed when the render thread was joined. `tick` while stopped does nothing.
- **Order and numbers.** Each `attach`, `set_settings`, `refresh` and accepted file change carries a request number (one counter); the node stores the one it uploaded (`TextNodeStats::uploaded_request`). Settings get a new generation only when they differ from the last sent ones, so a preview equal to the later commit draws once. A file result for an older generation is dropped. Commands that arrive together are coalesced to the newest request per source, `refresh`es first.
- **Cache.** The render thread keeps each source's last `TextFrame`; an `attach` whose settings match the cached generation sends it at once without drawing (a re-created node, undo of a delete).
- **Files.** Only attached sources in the on-air set are polled, every `POLL_INTERVAL` (500 ms, master clock), and at once on `attach`, a `file` or `file_lines` change and `refresh`. The probe canonicalises the path before every read and refuses anything but a regular file. It reads at most `MAX_FILE_BYTES` (1 MiB) from the end plus the first bytes for the byte order mark. A file whose size and modification time are unchanged and older than `RECENT_WINDOW` (2 s, `TextClock`) is not read. Otherwise the content hash decides: a new hash is accepted when the next poll agrees, except the first content of a file older than the window, which is accepted at once; an empty file always needs the agreeing poll. A missing file shows an empty picture with `FileNotFound` and is watched until it appears; bytes that are not text keep the last picture with `NotText`.
- **Hangs.** A probe call longer than `HANG_AFTER` (5 s) gives its source `FileNotAnswering`; the path is not polled until the call returns (then the problem clears), and while fewer than `MAX_DETACHED_PROBES` (2) are left behind, the probe is detached and a new one serves the others. At the cap no thread is made. `stop` detaches a busy probe under the cap and keeps it for the next start at the cap. A render longer than 5 s gives `CouldNotDraw`; `stop` does not join a busy render thread.
- **Problems.** `ProblemSlot::File` and `ProblemSlot::Render` are set and cleared separately. Renderer errors and panics (caught; the renderers are trimmed before the next draw) are `RenderFailed` with the renderer's message; `font_missing` is the note `FontMissing`. `TextProblem::message` never contains a path or file content; `fault()` maps `FileNotFound` to `TargetGone`, notes to none, the rest to `Other`.

Tests: `crates/rbs-sources/tests/text_worker.rs` (nothing runs before an attach; edits redraw, equal settings do not; a re-created node gets the cached picture without a render; last N lines of a file; a change needs two agreeing polls; an unchanged old file is not read again; off-air files are not polled; a missing file shows nothing until it appears; bytes that are not text keep the last picture; a hung call stops only its own source and clears when it returns; stuck probes never pass the cap and `stop` does not block; idle stop trims; a renderer panic is a problem and the next draw works; the font list is cached), `text_file::tests` (encodings, tails, lines, the real probe).
