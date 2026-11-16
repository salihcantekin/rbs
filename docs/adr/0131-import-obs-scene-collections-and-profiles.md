# ADR-0131: Import OBS scene collections, profiles and settings

- Status: Proposed
- Date: 2026-11-16
- Deciders: Salih (owner)

## Context

People who try RBS already have their show in OBS Studio: scenes, sources, filters, audio settings, an output profile (stream service and key, recording format, encoder, canvas size) and hotkeys. Rebuilding that by hand is the largest switching cost. The roadmap names an OBS scene collection importer for phase 2 ([roadmap](../roadmap.md)), the vision promises importers instead of loading libobs plugins ([vision](../vision.md)), and the first-run page offers "the OBS import ... when it exists" ([ADR-0113](0113-experience-level-and-first-run.md) decision 5, [design system](../specs/ui-design-system.md)). On 2026-11-16 the owner asked for scenes, profiles and settings to be imported, starting with what RBS can already map (audio filters, text-like sources as they land).

Forces:

- OBS keeps a scene collection as one JSON file (`basic/scenes/<name>.json`) and a profile as a folder (`basic/profiles/<name>/` with `basic.ini`, `service.json`, `streamEncoder.json`, `recordEncoder.json`). Hotkeys live in both: per source and scene in the collection, frontend actions in `basic.ini` `[Hotkeys]`. Global audio devices (`DesktopAudioDevice1`, `AuxAudioDevice1`, ...) are top-level sources of the collection. The format is not versioned as a whole; keys come and go between OBS releases.
- RBS covers part of OBS's source and filter types today. Text, media file, browser, transitions and more microphone filters are being built in parallel threads, so the mapping must grow one type at a time without touching the rest.
- OBS item transforms (position at an alignment anchor, scale, rotation about that anchor, bounds types, crop) differ from RBS's [`Layout`](../specs/scene-model.md) (a box, rotation about its centre). A box size needs the source's native picture size, which OBS does not store.
- Device and window targets are machine-specific ids. RBS already re-finds a lost device or window by name through `Source.hints` (`target_title`, `target_app`, [ADR-0077](0077-source-health-retarget-and-settings-window.md) decision 4).
- Stream keys are secrets; RBS never writes them to a file ([ADR-0051](0051-user-data-profiles-and-scene-collections.md)).
- An import must never change the user's existing RBS data or OBS's files, and it must say plainly what did not come across (owner request: report anything it cannot map).
- Layering (P1): the composition roots own the app's settings types (`RecordProfile`, `StreamProfile` in `rbs-app`); a lower crate cannot produce them.
- Zero cost when unused (P13): an importer is code that runs only when asked.

## Decision

1. **A new crate `rbs-import-obs`** (new layer class `Import`, used only by composition roots) reads OBS files and returns an in-memory **import plan**; it writes nothing. Allowed dependencies: `rbs-core`, `rbs-graph`, `rbs-scene`, `rbs-params`, `rbs-sources` (type ids and settings keys only), `rbs-filters` and `rbs-audio-filters` (catalogue type ids and field schemas, used to clamp values), `rbs-ui` (`shortcut::Chord`), `serde`, `serde_json`. It must not depend on `rbs-app`, `rbs-user-data`, an adapter or a toolkit. `basic.ini` is read by a small INI reader inside the crate (no new dependency). `cargo xtask check-layers` learns the class.

2. **Inputs.** (a) A scene collection JSON file; (b) optionally a profile folder; (c) a `Lookup` trait the caller implements for facts the files lack: `picture_size(path)` (PNG header), `file_exists(path)`, `targets(type_id) -> Vec<(target, name, app, size)>` (one enumeration per type per import, on the control thread, never polled). `scan(config_dir)` lists the collections and profiles of an OBS install; `default_config_dirs()` gives the per-OS OBS folders (macOS `~/Library/Application Support/obs-studio`, Windows `%APPDATA%\obs-studio`, Linux `~/.config/obs-studio` and the Flatpak path). Files larger than 32 MiB are refused. OBS files are opened read-only.

3. **Output.** `ImportPlan { collection: SceneCollection, profile: ObsProfileFacts, shortcuts: Vec<ImportedShortcut>, report: ImportReport }`. The collection is built with `SceneCollection::from_parts`, so it passes the same validation as a loaded document; no scene format change is needed (format stays 18). `ObsProfileFacts` is a neutral record of what the profile says (canvas and output size, frame rate, recording folder, container, codec, hardware-or-software preference, rate control, bitrates, keyframe interval, preset, stream server and service name, the stream key as a `SecretString` that is never `Debug`-printed or serialised, replay buffer length, sample rate, monitoring device name). `rbs-app` maps these facts onto its own `RecordProfile`, `StreamProfile`, replay and audio settings, so app settings types stay in the composition root.

4. **Mapping is a registry of mappers.** `trait SourceMapper { fn obs_ids(&self) -> &'static [&'static str]; fn map(&self, src: &ObsSource, ctx: &mut MapContext) -> SourceOutcome; }` and the same for `FilterMapper` and `TransitionMapper`. `Mappers::builtin()` holds the mappers of this ADR; the caller registers a mapper only if `SourceTypes::is_installed` says its RBS type exists in this build, so a type that is not linked is reported as "not available in this build" rather than imported broken. A new RBS type (text, media, browser, a transition, a microphone filter) adds one mapper file and one fixture test; no other code changes. Every OBS id without a mapper is reported (decision 7).

5. **First mapping set.** Each row says whether the result is exact or approximate; approximate rows are marked so in the report.

   | OBS | RBS | Notes |
   | --- | --- | --- |
   | `scene` | scene, scope shared, order from `scene_order`, program from `current_program_scene` | Nested scene items become `ItemContent::Scene` |
   | `group` | items flattened into the parent scene in place, the group transform composed into each child, child visible only if group and child are visible | Reported once per group. A shear (non-uniform group scale with rotated children) is approximated |
   | `monitor_capture`, `display_capture`, `screen_capture` (display mode), `xshm_input`, `pipewire-desktop-capture-source` | `org.rbs.source.display` | Target by display name when one matches, else the default display; `cursor` from `capture_cursor`/`show_cursor` |
   | `window_capture`, `screen_capture` (window mode), `xcomposite_input` | `org.rbs.source.window` | Hints `target_title`, `target_app` from the OBS window string; target resolved when exactly one window matches |
   | `game_capture` | `org.rbs.source.window` (approximate) | Only when it names a window; "any full-screen application" is skipped |
   | `dshow_input`, `av_capture_input`, `av_capture_input_v2`, `macos-avcapture`, `v4l2_input` | `org.rbs.source.camera` | Matched by device name; `width`/`height`/`fps` when OBS stores a custom resolution |
   | `wasapi_input_capture`, `coreaudio_input_capture`, `pulse_input_capture`, `alsa_input_capture` | `org.rbs.source.audio-input` | `default` device is no target (follows the OS default) |
   | `wasapi_output_capture`, `coreaudio_output_capture`, `pulse_output_capture` | `org.rbs.source.audio-output` | |
   | `wasapi_process_output_capture`, `sck_audio_capture` (application) | `org.rbs.source.application-audio` | Hint `target_app` |
   | `image_source` | `org.rbs.source.image` | PNG only; other formats are skipped with the reason |
   | `color_source`, `color_source_v2`, `color_source_v3` | `org.rbs.source.color` | OBS `0xAABBGGRR` to `#rrggbbaa`, `width`, `height` |
   | Top-level `DesktopAudioDevice1..2`, `AuxAudioDevice1..4` | audio sources in the main canvas's global scene ([ADR-0065](0065-global-sources-and-safe-source-removal.md)) | |
   | Source `volume`, `muted`, `sync`, `monitoring_type`, `private_settings.mixer_hidden` | `SourceAudio` volume (clamped to 1.0), muted, `sync_offset_ms` (ns to ms, clamped to ±200, approximate when clamped), monitor `off`/`only`/`also`, `mixer_hidden` | `balance` other than centre, `audio_mixers` other than all tracks, push-to-talk/mute: reported |
   | Item `pos`, `scale`, `rot`, `align`, `bounds_type`, `bounds`, `bounds_align`, `crop_*`, `visible`, `locked` | `Layout` (box from the anchor, native size and scale or bounds; rotation re-expressed about the box centre; negative scale is a flip), `Crop`, visible, locked | Native size from `Lookup` (picture, colour size, target size); when unknown, the canvas size is assumed and the item marked approximate. `blend_type` other than normal, `scale_filter`, show/hide transitions: reported |
   | `gain_filter` | `org.rbs.audio-filter.gain` | `db` clamped to the field's range |
   | `noise_gate_filter` | `org.rbs.audio-filter.noise-gate` | Thresholds, attack, hold, release one to one; `range` default |
   | `compressor_filter` with a `sidechain_source` | `org.rbs.audio-filter.ducker` (approximate) | Threshold, attack, release taken over, `amount` default; without a sidechain the compressor is reported |
   | `chroma_key_filter`, `chroma_key_filter_v2` | `org.rbs.filter.chroma-key` | Similarity, smoothness, spill divided by 1000; key colour |
   | `color_filter`, `color_filter_v2` | `org.rbs.filter.color-correction` (approximate) | Brightness, contrast, saturation (+1), hue; gamma converted; `opacity`, add/multiply colour reported |
   | `sharpness_filter`, `sharpness_filter_v2` | `org.rbs.filter.sharpen` (approximate) | |
   | `clut_filter` | `org.rbs.filter.lut` | When the LUT file exists and its format is supported |
   | `crop_filter` | `Source.region` ([ADR-0103](0103-source-region.md)) | Relative insets one to one; absolute size needs the native size |
   | `scene_order` hotkeys `OBSBasic.SelectScene`, `[Hotkeys]` start/stop streaming and recording, pause, replay save, `libobs.mute`/`unmute` | `scene.select?collection=..&scene=..`, `stream.start`/`stop`, `record.start`/`stop`, `record.pause_toggle`, `replay.save`, `source.muted.toggle` | See decision 9 |

   Everything else (filters on scenes, transitions until `TransitionMapper`s exist, text, media, browser, slideshow, VST, plugin types, the Aitum vertical canvas module, saved projectors, multitrack routing) is reported, never silently dropped.

6. **Nothing existing is changed by default.** `rbs-app` writes the plan as a **new** scene collection and a **new** output profile through `rbs-user-data`'s `Library::create`, with the existing name-clash rule ("Name 2"), under the writer lock, after the plan is complete. Nothing is written if building the plan fails. The import does not switch to the new collection while recording or streaming (ADR-0051's switching rule applies). Shortcuts are added to `shortcuts.toml` only when the user keeps that option on, and a chord that is already bound is not added (reported, the user's binding kept). OBS's files are never written.

7. **The report is part of the result.** `ImportReport` lists one entry per OBS object that was not imported exactly: `{ outcome: Approximated | Skipped, area: Scene | Source | Filter | Item | Transition | Hotkey | Profile | Setting, obs_name, obs_type, scenes: Vec<String>, reason, hint: Option<String> }` plus counts of what was imported exactly. Reasons are one plain sentence ("Text sources are not in RBS yet."). The report never contains file contents, stream keys or full paths beyond the file name. `rbs-app` saves it as `imports/<collection id>.report.toml` next to the user data, so it can be read again; the skipped OBS objects' JSON is kept in the same file under `[[skipped]]` so a later RBS can offer to add what it learned to map. That offer is not part of this ADR.

8. **Secrets.** A stream key read from `service.json` is stored only in the OS credential store under the new profile's id, the way the app stores a typed key, and only when the user keeps "Bring the stream key" on (on by default). It is never written to the plan's serialised forms, the report, logs or the control API.

9. **Hotkeys.** OBS key names (`OBS_KEY_*` plus shift, control, alt, command) are converted to `Chord`. A chord RBS reserves, a mouse button, or an action without an RBS command is reported. Bindings take the global scope, as OBS hotkeys work while OBS is not focused, unless the chord is not allowed globally on the platform, then app scope (reported).

10. **Entry points.**
    - `rbs-cli obs-import check <collection.json> [--profile <dir>]` prints the report; `rbs-cli obs-import run ...` writes the new collection and profile. These come first and are how the mapping is tested by hand.
    - The desktop app: an *Import from OBS...* item in the RBS menu's scene collection submenu and on the first-run page. It lists the detected OBS collections and profiles (and a file picker), shows the report before anything is written, and imports on confirmation. Its layout is proposed to the owner with screenshots before it is built (rule 16, ADR-0115).
    - Control API: `import.obs.scan`, `import.obs.check`, `import.obs.run` (next free protocol version at merge). Over the API a collection and profile are named only by what `import.obs.scan` found in the detected OBS folders; arbitrary paths are refused, so a remote client cannot make RBS read other files. Same permission as `scene.*` edits.

11. **Testing.** Fixtures are hand-written OBS-shaped JSON and INI files written for this repository (no user's real files). Golden tests compare the plan (collection document and report) with checked-in TOML. Transform conversion has property tests: the four corners of an OBS item and of the RBS box agree within 0.01 px for random positions, scales, rotations, alignments and bounds types. Tests are pure, no file system except fixture reads, no sleeps.

## Consequences

- A user can bring an OBS setup across in one step and sees exactly what did not come across, also cross-platform (a Windows collection on a Mac finds cameras by name), which OBS itself does not do.
- Each parallel feature thread (text, media, browser, transitions, microphone filters) adds an OBS mapper and fixture with its type; the coordinator tells those threads.
- Approximate mappings can surprise. They are always in the report, never silent.
- OBS file keys change between releases; unknown keys are ignored, a missing key takes OBS's default, and fixtures cover OBS 29 to 31 shapes.
- No cost while unused: no threads, no polling, no resources until an import runs. `default_config_dirs` and `scan` touch the disk only when the dialog, first-run page, CLI or `import.obs.scan` asks.
- Follow-up: spec [`obs-import.md`](../specs/obs-import.md); `docs/architecture/overview.md` layer row; `docs/status.md` row; control-api spec entries with the API step; first-run page wiring with the UI step; glossary term check for the dialog text.

Implementation steps, each its own PR: (1) crate, scene collection mapping, report, `rbs-cli obs-import`; (2) profile and hotkeys, `rbs-app` mapping of the facts and the credential store; (3) desktop dialog and first-run entry (visual approval first); (4) control API.

## Alternatives considered

- **Load OBS's own code (libobs) to read collections.** Rejected: GPL C dependency with its own graphics stack, against P1 and the vision's "importers instead of libobs".
- **Write the importer inside `rbs-app`.** Rejected: it would not be testable without the app, and `rbs-cli` could not share it.
- **Produce `RecordProfile` and `StreamProfile` in the importer.** Rejected: those types belong to the composition root; neutral facts keep the crate below it.
- **Groups as nested scenes.** Rejected for the first version: it adds scenes the OBS user never saw in their scene list, and an extra composite pass per group. Flattening gives the same picture. A group can still be made a nested scene later if users ask to keep groups.
- **Skip what cannot be mapped without a record.** Rejected: the owner asked for a clear report, and keeping the skipped JSON lets a later version finish the import.
- **Import into the current collection.** Rejected: it would mix two shows and need an undo of a large change; a new collection is reversible by deleting it.
- **Placeholder sources for unsupported types.** Rejected: a source with no node type breaks binding and the Sources panel; the report and the kept JSON serve the same purpose.

## Review

Pending: independent review per ADR-0014.
