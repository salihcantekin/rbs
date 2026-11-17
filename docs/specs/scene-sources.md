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
pub struct InstanceContext { clock, audio: AudioGraphConfig, max_audio_offset }  // new(), with_max_audio_offset()
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
pub fn node_layers(compositor: &BoundCompositor, plan: &SchedulePlan) -> Vec<NodeLayer>;
```

### Settings

| Key | Value | Types | Meaning |
| --- | --- | --- | --- |
| `adapter` | Text | all | Adapter id. Missing: the best probed adapter for the kind (registry order: user choice, preference, hardware, registration) |
| `target` | Text | all | Capture target id or audio endpoint id. Ids are adapter-defined, so a stored target is stored with its adapter |
| `cursor` | Bool | display, window | Draw the cursor; default true |
| `width`, `height` | Int > 0 | display, window, camera, color | Requested size; both or neither. For `color` it is the picture size, default 1920 x 1080, at most 8192 |
| `fps` | Int > 0 | display, window, camera | Requested frame rate |
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
7. `node_layers` returns the bound layers whose producer `plan` schedules, bottom first, each with the index of its link among the compositor's scheduled links (scene model spec, `resolve`), the transform, crop and opacity unchanged.

### How the composition root uses it

1. Register the platform's capture and audio adapters, `probe`.
2. `SceneBinder::new(types.node_types()).with_audio(AUDIO)`; `sync`. `node_types` also installs the descriptor of every built-in video filter type (`rbs_filters::catalogue`, [video-filters.md](video-filters.md)); filter settings convert with `filter_values` (edits, strict) and `filter_values_lenient` (documents).
3. For each `report.created_sources`, `instantiate` and adopt the instance into the node's partition (`Video`, `Still`, `Media` and `Browser` into the render partition, `Audio` into the audio partition; the sound node of `Media` and `Browser` into the audio partition at the binder's sound node).
4. Create the compositor (`node_layers`) and mixer instances, attach each canvas's encoders to `compositor(canvas, Program)` and `mixer(canvas)`. Mixer strips are kept per `SourceId` and rebuilt from `report.mixers[canvas].inputs` after every sync.
4a. For each scheduled filter node without an instance, create a `FilterNode` of its type with the source's known picture size and adopt it into the render partition; send new values on `report.changed_filters` (ADR-0062).
5. Apply and collect. Only then take back the instances of `retired_sources` and `retired_filters`: `Controller::take` returns idle instances only.
6. Break-before-make for `report.replaced_sources`: an exclusive device (most cameras) cannot start the new instance while the old one runs. Apply first a copy of the graph with each replacing node moved to Standby, collect (the old instance is released), then apply the graph itself.

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

## Identity and option keys (ADR-0133 decision 2)

`rbs_sources::option_keys(type_id)` lists the settings Reset source and Paste attributes may change; every other key a type uses is an identity key and is never pasted or reset.

| Class | Identity keys | Option keys |
| --- | --- | --- |
| Camera capture | `adapter`, `target` | `width`, `height`, `fps` |
| Display and window capture | `adapter`, `target` | `cursor`, `width`, `height`, `fps` |
| Output loopback audio | `adapter`, `target` | `exclude_own_process` (never written on a companion) |
| Audio input, application audio | `adapter`, `target` | none |
| Image, colour | all keys | none |
| Media file | `adapter`, `path` | `loop`, `restart_when_shown` |
| Browser | `url` | `width`, `height`, `fps`, `transparent` |
| Plugin types | all keys | none |

Option defaults depend on the canvas and are computed in `rbs-app` (`option_defaults`): a display or window gets the canvas frame rate as `fps`, a camera `1280 x 720` at 30 fps, a browser page the canvas size capped at 1920 x 1080; other option keys are left out (the type's own default). The Add source path uses the same values.

## Source region and display chrome (ADR-0103, ADR-0104)

`region` and `chrome` live on `rbs_scene::Source`, not in settings. The binder compares settings only, so editing them does not replace the capture node. Chrome is refused on every type except `org.rbs.source.display`. Applying chrome writes the four insets into `region`. Cover and fit place the item box; they are not a compositor scale. `work_rect` is copied from `SourceTarget` when Reset runs, and Reset does nothing when that field is absent.

The settings Region section and the add-source region step are not in the desktop UI yet (rule 16). The capture port's `region_grab` (long side at most 960, not `thumbnail`) is what that preview will call.

## Media file (`org.rbs.source.media`, ADR-0108)

Two node types (ADR-0106, ADR-0108 decision 1): the picture node `org.rbs.media` (`rbs_nodes::media_descriptor`, one video output) and the sound node `org.rbs.media.sound` (`rbs_nodes::media_sound_descriptor`, one audio output). `SourceInstance::Media { picture, sound, control, stats, adapter }` and `SourceClass::Media`. Changing `path` replaces both nodes (existing binder break-before-make). `loop` (default true) and `restart_when_shown` (default true) do not require new nodes; the binder still replaces on any settings edit until the live keys of decision 12 are built. The source stores both flags and the media cursor with no decoder while Standby.

`path` must canonicalize to a local regular file before any decoder `pull`. A URL, UNC path, device path, directory, pipe, or symlink, junction, or substituted drive that does not resolve to a local regular file is `InvalidSetting`. A missing or undecodable file is `SourceError::Media(String)`. A side over 8192 or one frame over 128 MiB is `InvalidSetting` before either queue is allocated.

Decode is the port `rbs-media-decode` ([media-decode-port.md](media-decode-port.md): per-stream reads, capability `media.decode.pixel` of `zero-copy` or `cpu-upload`). No real adapter is registered yet; the platform adapters come next (ADR-0108 decision 4). The probe tries each candidate adapter in order and keeps the first that yields a picture. Conformance uses `rbs-media-decode-test`. While Active the node keeps two lock-free queues (video lookahead at most 8 frames and 128 MiB, audio at most four graph blocks). `process` only dequeues. End of file with `loop` false joins the worker on the control path, drops the queues, holds the last picture, and emits silence.

### Playback control handle (ADR-0108 decision 10)

`rbs_nodes::MediaControl`, shared by the picture node, the sound node and the worker; `SourceInstance::Media::control` hands it to the host. Its commands are called on the control path only; they never write the document, are not undo steps, and do not replace the source.

- `play()`, `pause()`, `seek(position)`, `restart()`. `restart` is a seek to the range start (0 until the play range is built) followed by play. Play while playing and pause while paused do nothing. A position past the stated duration is clamped to it.
- Every command writes a **record**: a generation (one more than the last), an instant, a `paused` flag and, for a seek or restart, the position. The handle keeps the last 4 records in a fixed array under a sequence counter (a seqlock). The instant is the master time now plus the picture node's last render period (50 ms before its first two ticks), rounded up to the audio block grid once the sound node has seen a block. Not built yet: the longest render period among several partitions that hold the picture node (today there is one), and merging the seeks of a scrub into one record per render period.
- **Items.** Every picture item and sound block carries the generation it was decoded for. At a tick or block time `t` a node reads the 4 records without retrying: on a torn read it keeps the records of its previous read (counted in `MediaStats::torn_controls`). An item of generation `h` plays when its own stamp is due and, if the record of `h + 1` is present, its stamp is before that record's instant. An item whose generation is older than every record is skipped and returned; an item whose generation is newer than every record read waits.
- **Worker.** Each pass it applies every record newer than the last one it applied, in generation order, so no command is lost when several arrive between two passes (a record that already left the array is skipped). Pause: the cursor becomes the media time at the instant and the worker stops decoding (it keeps the decoder; the pause release of decision 10 comes with the cued state). Seek while paused: it seeks, decodes the first picture that ends after the position, stamps it at the instant, and stops again; the picture node shows that picture and holds it. Play, seek and restart while playing: it seeks to the position (the cursor for play), and anchors again one `LEAD` after the instant (no picture is stamped before that anchor), so the position plays at the anchor with no picture before it and no sample skipped or doubled. A loop pass starts from the position.
- **Silence and underruns.** From a pause's instant the picture node repeats its last texture and the sound node writes silence; neither is an underrun. Silence between a command's instant and the worker's first block of the new generation is not an underrun either.
- **No worker.** In the end hold (`ended`) play and restart start a worker from the range start; seek stores the cursor and starts a worker that decodes one picture at it, paused. In Standby a command is stored: the next start uses the stored cursor and paused flag. Following the decision 10 table, a source that sleeps paused or ended wakes paused or ended (showing the picture at its cursor) only with `restart_when_shown` false; with true it plays from the start. A source that went to sleep from the end hold reports `Ended`.
- **State.** `state()` is `Playing`, `Paused`, `Ended`, `Standby` (asleep with no stored pause or end) or `Unavailable` (a fault). `Cued` arrives with the binder's on-air set. `cursor()` (while paused, where the pause stopped) and `duration()` (the stated duration, `None` until known) are published too. A command reads the state and writes its record under the player lock, so two callers cannot interleave.
- **Cost.** A source nobody controls writes one record when it starts. `process` reads 4 records (about 20 atomic loads) and compares generations; it takes no lock and allocates nothing.

`SourceInstance::Media` holds `picture` (`rbs_nodes::MediaPictureNode`), `sound` (`rbs_nodes::MediaSoundNode`), `stats`, and `adapter`. `node_types()` lists `org.rbs.media` for `org.rbs.source.media`, `org.rbs.media.sound`, and the pair (`NodeTypes::sound_type`). `Studio` in `rbs-record` adopts the picture node into the render partition the way it adopts a still, and the sound node into the audio partition at the binder's sound node once the plan schedules it (it holds the sound node until then, and drops it when audio is not bound, so the worker decodes no sound). The source counts as scheduled when either node is in the plan, so a source that is only heard is instantiated and plays. The instance is recorded as soon as the picture node is adopted, and the sound node is marked adopted only after the engine took it. It stores the decoder adapter id. It does not register a decoder and does not link FFmpeg (ADR-0007). `org.rbs.source.media` is installed only when the host has registered and probed a `MediaDecoderFactory` (the same rule as a capture type that has no adapter). Tests register `rbs-media-decode-test`. The host does not write `adapter` unless the user set it. When `adapter` is absent, pick `zero-copy` before `cpu-upload`, then registration order. The sound node is the strip producer (ADR-0106).

## Browser (`org.rbs.source.browser`, ADR-0109)

Network-capable, including a `file` URL. Settings: `url` (`http`, `https`, or `file`), even `width` and `height` default 1920×1080 and at most 8192, `fps` default 30 and in 1..=60, `transparent` default false (opaque alpha). No CSS, no click forwarding, no JS binding, no device prompt, no audio pin. Straight-alpha sRGB `Rgba8`.

The studio does not link or load CEF. A real CEF build replaces the helper executable and is a process tree. This tree's stand-in is the `rbs-browser-runner` binary (`rbs-browser-ipc` messages). The CEF tree root is `rbs-browser-cef` (ADR-0132 decision 1), built outside the workspace with `cargo build --release --manifest-path crates/rbs-browser-cef/Cargo.toml`; until the runtime download and the sandbox land it is used only when `RBS_BROWSER_HELPER` points at it. It paints in software into the section (converting CEF's premultiplied `Bgra8` to straight `Rgba8` while it copies, so the studio adds no pass), runs headless on Linux (no X11 or Wayland), takes page audio away from the OS output and writes it into the studio's sound section (stereo at the graph's rate in 10 ms packets, each stamped with the OS monotonic clock; dropped when the studio names no section), cancels popups, downloads, file pickers and JavaScript dialogs, denies device permissions, limits a `file` page to its own directory (`rbs_browser_ipc::page`), and keeps its profile in a new private temporary directory removed on exit. Frames cross a studio-allocated shared memory section (`rbs_browser_shm::FrameRing`, ADR-0109 decision 8): a named POSIX shared memory object on macOS and Linux, a pagefile-backed section on Windows, created before the helper starts and named in `configure`. Layout: a 128-byte header (magic `RBSB`, layout version 1, width, height, pixel layout 1 = `Rgba8` straight or 2 = `Bgra8` premultiplied, slot count 3, the triple-buffer exchange word, frames written, heartbeat) and three slots of width × height × 4 bytes. The helper and the studio each own one slot and swap the third through the exchange word, so neither waits and a slot is never written while read. The helper adds one to the heartbeat at least every 500 ms; two seconds without a change is a hang. Before its first beat a helper gets 15 seconds from spawn (`STARTUP_LIMIT`), because a cold engine start blocks before it can beat. A stop closes the helper's stdin and gives it one second (`STOP_GRACE`) to exit on its own, on the supervisor thread, then kills it; the session's `stop` never waits. A studio supervisor thread, which exists only while the source is Active, copies the newest frame into the source's sink at twice the page rate and checks the process and the heartbeat; an exit or a hang marks the source Unavailable and releases its slot. The studio never forms a reference to section bytes the helper may write (raw copy). Page sound crosses a second section (`rbs_browser_shm::SoundRing`, named in `configure` when `sample_rate` is not 0): a 128-byte header (magic `RBSA`, layout version 1, sample and packet capacities, samples and packets written and read, packets dropped), 128 packet entries (first sample, OS time, samples, rate, channels) and a ring of 65 536 `f32` samples. The helper drops a packet that does not fit; the studio checks every entry against its own position before it copies, and skips to the present on a malformed one. The supervisor reads it every 5 ms into the source's `BrowserSoundFeed` (audio graph spec, "Browser sound node"). The test double paints opaque and writes sound only with `--tone` (tests); `transparent` false is applied by the helper, not by a studio pass. `Studio` adopts `SourceInstance::Browser` into the render partition and shares one `BrowserBudget` (`InstanceContext::with_browser`). Before each apply it grants slots to shown pages: program before preview, lower source id inside a class, at most two. Headless crates pass `Studio::start_with_tree` from the composition root (`rbs-app` uses `rbs-browser-helper` via `browser_tree::HelperBrowserTree` when the executable is found). Without a tree, prepare reports that no helper was set. A source without a slot reports `browser_cap` and starts nothing. A helper exit is Unavailable, releases the slot, counts toward back-off, and must not panic the host. The studio allocates the frame buffer; the render path drops it without joining a dead child. Installed and Standby start no process. An agent without the network permission gets `not_permitted` / `network` for any `url`, including `file` (`agent_may_set_browser_url`). The control API kind is not in `rbs-api` yet; the reason string is the contract.


## Text (`org.rbs.source.text`, ADR-0129)

Implementation status: phases (a) to (c). The type, its settings, the node, the text worker, the binder's in-place updates and the host wiring exist, and the control API adds and edits text sources (control-api.md, "Protocol 1.75"). The desktop app registers `rbs-text-parley` on the system's fonts, so the type is installed; no menu adds a text source until phase (f), and a text row in the Sources list uses the `pencil` icon until then. Live kinds (phase (d)) and crisp resizing (phase (e)) follow.

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
| `fields`, `format`, `target`, `duration_ms`, `end_text` | Bool, Text, Text, Int, Text | false, none | live kinds (ADR-0129 decision 11, phase (d)) |

A number key accepts an integer. `validate_text` refuses any other key, a value of the wrong kind or out of range (`InvalidSetting` naming the key), an unknown `kind` and a missing box. `read_text_lenient` never fails: it gives a wrong kind its default, clamps a value out of range (a box over the pixel cap keeps its width and is shortened), reads an unknown `kind` as `text`, and returns every key it changed for `SourceSettingsAdjusted`. Tabular figures are on for every live kind and for `fields`.

**Still or not.** `is_still` answers from the class for every type except text; a text source is still only when `kind` is `text` (or absent) and `fields` is off. `rbs-record`'s picture form (`video_as: auto` writes images only for a still source; `image` on a text that is not still is `ImageOnMovingSource`) and the host's `set_record_separately` `video_as` check ask `is_still`. Recording sound separately is refused for a text source like any picture-only source (`AudioOnPictureOnly`), whatever its settings. Tests: `rbs-sources` `text::tests`, `rbs-record` `iso::tests::a_text_source_is_still_only_as_plain_text`.

**Instantiate.** With no renderer registered the type is not installed and `instantiate` is `NoAdapter`. Otherwise it reads the settings leniently and returns `Text` with a node that shows nothing until the worker sends a picture through `frames`.

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
