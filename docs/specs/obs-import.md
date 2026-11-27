# Spec: OBS import

- Status: Draft (ADR-0131 Accepted; step 1 in progress)
- Owner crate(s): `rbs-import-obs`, `rbs-app`, `rbs-cli`
- Related ADRs: [0131](../adr/0131-import-obs-scene-collections-and-profiles.md), [0188](../adr/0188-defer-new-nested-scenes.md), [0051](../adr/0051-user-data-profiles-and-scene-collections.md), [0065](../adr/0065-global-sources-and-safe-source-removal.md), [0077](../adr/0077-source-health-retarget-and-settings-window.md), [0103](../adr/0103-source-region.md), [0113](../adr/0113-experience-level-and-first-run.md)

## Purpose

Turn an OBS Studio scene collection, profile and hotkeys into a new RBS scene collection, output profile and shortcuts, and report every OBS object that did not come across exactly.

## Scope

In scope: reading OBS files, the import plan, the mapper registry, the transform conversion, the report, the entry points (`rbs-cli`, desktop, control API).

Out of scope: writing OBS files; importing from other apps (Streamlabs, XSplit); loading OBS plugins; adding skipped objects later (ADR-0131 decision 11 keeps allow-listed data for it).

## Contract

```rust
// rbs_import_obs
pub fn default_config_dirs() -> Vec<PathBuf>;                 // existing OBS folders on this OS
pub fn scan(config_dir: &Path) -> Result<ObsInstall, ImportError>; // collections and profiles by name
pub struct ObsInstall { collections: Vec<Found>, profiles: Vec<Found> }
pub struct Found { name: String, path: PathBuf }

pub trait Catalogue {                                          // implemented by rbs-cli (step 1) and rbs-app
    fn has(&self, type_id: &NodeTypeId) -> bool;               // sources, filters, transitions
    fn fields(&self, type_id: &NodeTypeId) -> Option<&[rbs_params::Field]>; // ranges used to clamp
}
pub trait Lookup {
    fn picture_size(&self, path: &Path) -> Option<(u32, u32)>;
    fn file_exists(&self, path: &Path) -> bool;
    fn targets(&self, type_id: &NodeTypeId) -> Vec<LookupTarget>; // asked at most once per type per import
    fn platform(&self) -> Platform { Platform::current() }   // reserved shortcuts; modifier names
                                                            // when no path in the files tells
}
pub struct LookupTarget { target: String, name: String, app: Option<String>, size: Option<(u32, u32)> }

pub struct Options {                                           // the wizard's choices; all default on
    scenes: SceneChoice,               // All scenes, or exactly Only(names); no implicit additions
    global_audio: bool, audio_filters: bool, video_filters: bool, hotkeys: bool,
    canvas: bool, recording: bool, stream_server: bool, stream_key: bool,
    replay: bool, audio_settings: bool,
}
// Left-out parts are not in the plan; each is one report entry, outcome NotChosen.

pub fn plan(collection_json: &[u8], profile: Option<&ProfileFiles>, mappers: &Mappers,
            catalogue: &dyn Catalogue, lookup: &dyn Lookup, options: &Options) -> Result<ImportPlan, ImportError>;
pub struct ProfileFiles { basic_ini: Vec<u8>, service_json: Option<Vec<u8>>,
                          stream_encoder: Option<Vec<u8>>, record_encoder: Option<Vec<u8>> }

pub struct ImportPlan {
    pub name: String,                          // the OBS collection name
    pub collection: SceneCollection,           // passed SceneCollection::from_parts
    pub profile: Option<ObsProfileFacts>,     // { name, video: VideoFacts, output: OutputFacts }
    pub secrets: ImportedSecrets,              // { stream_key: Option<StreamKey> }; StreamKey's Debug
                                               // prints <redacted>; no Serialize, no Display
    pub shortcuts: Vec<ImportedShortcut>,      // { command, scene: Option<SceneId>, chord, obs_action }
                                               // command_for(collection_id) gives the id with its
                                               // parameters once the collection has its id
    pub report: ImportReport,
}

pub struct OutputFacts {                       // neutral; the composition root maps them
    mode: OutputMode,                          // Simple | Advanced
    output_size: Option<(u32, u32)>,           // [Video] OutputCX/CY when it differs from the canvas
    recording: Option<RecordingFacts>,         // { folder, format: Mkv|Mp4|Mov, encoder: EncoderFacts }
    stream: Option<StreamFacts>,               // { server, service, encoder: EncoderFacts }
    replay: Option<ReplayFacts>,               // { enabled, seconds }
    audio: Option<AudioFacts>,                 // { sample_rate, monitoring_device }
}                                              // each None when not chosen (NotChosen entry)
pub struct EncoderFacts { encoder: Option<(Codec, EncoderKind)>, rate_control, video_kbps,
                          max_video_kbps, quality, keyframe_seconds, preset, b_frames, audio_kbps }

pub fn merge_shortcuts(doc: &mut ShortcutsDoc, shortcuts: &[ImportedShortcut],
                       collection_id: &str, platform: Platform) -> Vec<ReportEntry>;
// Adds each binding in app scope unless Keymap::check says Taken or Refused, or a limit
// (MAX_ENTRIES, MAX_PER_COMMAND) is reached; returns an entry for each one left out.

pub struct Mappers;                            // builtin(), register_source/filter/transition
pub trait SourceMapper { fn obs_id(&self) -> &'static str;          // one mapper per OBS id
                         fn rbs_types(&self) -> &'static [&'static str]; // all must be in the Catalogue
                         fn map(&self, src: &ObsSource, ctx: &mut MapContext) -> SourceOutcome; }
pub trait FilterMapper { /* same shape, FilterOutcome */ }
pub trait TransitionMapper { /* same shape, TransitionOutcome */ }
```

`ImportError`: `TooLarge`, `NotJson`, `NotACollection` (no `sources` array), `Internal(String)` (the built collection failed `from_parts` although the importer enforces its invariants first: a bug). A single bad source, filter or item, a name clash or a scene past `MAX_LAYERS` is never an error: it is a report entry.

**ADR-0188 import contract.** `SceneChoice::All`
imports every OBS scene separately; `Only(names)` imports exactly the selected
scenes and does not add their referenced scenes. For each nested-scene
placement in an imported scene, including one inside an OBS group, omit the
placement rather than flattening it or importing the referenced scene
implicitly. Report each omission as `Outcome::Skipped`, `Area::Scene`, with
the referenced scene name as `obs_name`, the containing scene in `scenes`, and
the reason "Scenes cannot be placed inside other scenes." The same report and
selection rules apply to `import.obs.check` and `import.obs.run` for all clients.

### Report

```rust
pub struct ImportReport { pub exact: Counts, pub entries: Vec<ReportEntry> }
pub struct ReportEntry { outcome: Outcome /* Approximated | Skipped | NotChosen */, area: Area,
                         obs_name: String, obs_type: String, scenes: Vec<String>,
                         reason: String, hint: Option<String> }
```

Reasons are one sentence of plain English ("Only PNG pictures can be shown for now."). No stream key, file content or full path appears in a report; a path is shown by its file name.

`rbs-app` saves the report as `imports/<collection id>.report.toml` (`kind = "obs-import-report"`, format 1), deleted with its collection and not exported with it. Skipped objects keep only allow-listed keys under `[[skipped]]` (ADR-0131 decision 11): URLs without query or fragment, no key containing `token`, `key`, `password`, `secret`, `auth` or `cookie`, paths cut to the file name, no opaque plugin state. The allow-list per OBS type is added here with the mapper that will use it.

The file: `format = 1`, `kind = "obs-import-report"`, `obs_collection` (the OBS collection's name), `[exact]` (`scenes`, `sources`, `items`, `filters`), `[[entries]]` (`outcome` `approximated`, `skipped` or `not-chosen`; `area`; `obs_name`; `obs_type` when there is one; `scenes` when there are any; `reason`; `hint` when there is one) holding the plan's entries and then what writing left out, and `[[skipped]]` (`obs_type`, `obs_name` and, once a mapper defines it, the allow-listed data of that type). No allow-list is defined yet, so `[[skipped]]` holds only the type and name. The report is written last; when it cannot be written, the import keeps what it wrote and says so. `UserSession::delete_collection` removes it with its collection.

### Stream server

A server from `service.json` is kept only if `parse_server` accepts it. Otherwise the report names its scheme only; the server text and the check's error text (which can contain the full URL) are never shown, logged or written.

### Transform conversion

ADR-0131 decision 8 is normative: absolute `pos`, `scale`, `bounds` are read; an item with only OBS 31 relative keys is reported until their conversion is checked against a real OBS 31 file; `bounds_crop`; crop filter as region plus item crop. OBS places an item at `pos` with the anchor `align` (bits: left 1, right 2, top 4, bottom 8; none is centre), size `native × scale` or, with a bounds type, the bounds box (stretch, scale inner, scale outer, to width, to height, max only) aligned by `bounds_align`, rotated by `rot` degrees about `pos`, after `crop_*`. RBS stores the box (`Layout.position`, `Layout.size`, negative for a flip) rotated about its centre. The converter computes the four corners of the OBS item and returns the RBS box with the same corners in texture order (within 0.01 px). The native size comes from `Lookup` or settings; when unknown, the canvas size is used and the item is marked approximate.

### Mapping table

ADR-0131 decision 7 is the first mapping table. Each mapper keeps its own row in this section when it is implemented.

- `group` (ADR-0182 decision 13, replacing ADR-0131's flattening row) → an RBS group of the scene. The group's surviving source items have the group's transform composed into each child and a child is shown only when group and child are; they get the group's tag and sit next to each other, in OBS order. The name is the OBS group's, made unique in the scene ("Name 2"). Count members after all omissions, including nested-scene placements: a group with fewer than two surviving items becomes separate items and one `Approximated` group entry says so, in addition to each skipped placement entry. A group inside a group does not occur in OBS. A crop on the group item, which clipped its children, is not kept and is reported. An import with no group adds no group list and no entry.
- `image_source` → `org.rbs.source.image` for a `.png`, `.jpg` or `.jpeg` file (JPEG since ADR-0171); other formats are skipped. The picture size comes from the PNG header or the JPEG headers (upright by EXIF orientation).
- `slideshow`, `slideshow_v2` → `org.rbs.source.slideshow` (ADR-0171 decision 11). The folder is the first `files` entry's folder (an entry that is not a picture name is a folder); entries in other folders, listed files (RBS shows every PNG and JPEG of the folder), and other picture formats are reported. `slide_time` → `interval_s` (nearest half second, 1 to 3600), `slide_mode: mode_manual` → `advance: manual`, `randomize` → `order: shuffle`, `loop`, `transition` (`cut`, `fade`, `slide`, `swipe`), `transition_speed` → `transition_ms` (down to a 50 ms step, at most 5000), `fit: contain`. `use_custom_size` `WxH` → `width`/`height`, else the canvas (an aspect ratio is reported). `playback_behavior` `always_play` (the default) → run `placed`; `pause_unpause` keeps the type default; `stop_restart` is reported (a slide show continues on return). `hide` is reported.

### Degrading on scene validation (ADR-0131 decision 4)

| `SceneError` | Action |
| --- | --- |
| `DuplicateName` | "Name 2", "Name 3", ... |
| `EmptyName` | the OBS type's readable name plus a number |
| `TooManyLayers` | item left out, Skipped |
| `NestingUnsupported` | each scene placement is omitted and reported as specified above before model construction; reaching `from_parts` with one is an importer bug (`Internal`) |
| `NestingTooDeep`, `NestingCycle`, `CropOnScene`, `StyleOnScene`, `ScrollOnScene` | cannot arise from valid imported collections: scene placements are skipped before construction |
| `ItemAlreadyGrouped`, `AmbiguousCompanion`, `PictureNotPlaced`, `CompanionExists` | cannot arise: the importer constructs groups directly and does not run regroup or companion-edit operations |
| `InvalidLayout`, `InvalidVolume`, `InvalidSyncOffset`, `InvalidSetting` | value clamped or the key dropped, Approximated |
| `CanvasTooLarge`, `EmptyCanvas`, `FrameRateTooHigh` (also a zero rate or `FPSDen` 0) | 1920×1080 at 30, Profile entry |
| `ScopeViolation`, `GlobalScene` | cannot arise: user scenes are shared; the global scene is scoped to its canvas and made only by the importer |
| `UnknownCanvas`, `UnknownSource`, `UnknownScene` | cannot arise: supported references are resolved before construction; an item or audio level whose target was skipped is left out and reported on the referring object. Scene placements are skipped and reported under `NestingUnsupported` |
| `UnknownItem`, `UnknownFilter`, `IndexOutOfRange`, `CanvasHasScenes`, `UndoConflict` | cannot arise: edit-time errors, not raised by `from_parts` |
| `DuplicateId`, `DuplicateFilter`, `IdsExhausted` | cannot arise: the importer issues ids from one counter per kind |
| `BadTypeId` | cannot arise: mapper type ids are checked by the catalogue test in `rbs-cli` (`obs_import::tests`) |
| `InvalidFollow`, `SharedLevels` | cannot arise: one canvas, no follow, no scene levels imported |
| `InvalidGlide`, `InvalidTransition`, `InvalidShowFor`, `InvalidActivity` | cannot arise: default glide and transition (ADR-0128), no timed show imported, no activity policy imported yet (ADR-0152) |
| `ChromeNotDisplay` | cannot arise: chrome is never imported |
| `StingerPlaced` | cannot arise: no stinger is imported, so the transition never names a source (ADR-0140 decision 1) |
| `UnknownGroup`, `GroupTooSmall`, `GroupNotContiguous`, `GroupScope` | cannot arise: a group is made only from two or more items of one scene that sit next to each other; a group with fewer than two imported items becomes separate items and is reported (ADR-0182 decision 13) |
| `UnknownBus`, `TooManyBuses` | cannot arise: the import adds no bus (ADR-0170); OBS track flags are mapped later |

A test (`crates/rbs-import-obs/tests/scene_errors.rs`) reads the variants of `SceneError` from its source and fails when one has no row here, so a new variant fails until it has an action. `SceneError` is `#[non_exhaustive]`, so an exhaustive `match` outside `rbs-scene` is not possible.

## Errors and failure behaviour

A failed plan writes nothing. A plan is written in order: collection, profile, credentials, shortcuts (ADR-0131 decision 9), each atomic; a failed later step leaves the earlier ones and is named in the report. Names clash-free by the free-name helper ("Name 2"). Unknown keys are ignored; a missing key takes OBS's default.

## Performance requirements

Runs on the control thread on demand only. A 200-source collection plans in under 100 ms on the reference machines, excluding `Lookup` enumeration. No threads, timers or resources while no import runs.

## Conformance tests

`crates/rbs-import-obs/tests`: golden fixtures (hand-written for this repository) for each mapper, groups, global audio devices, cross-platform ids, unsupported types; the property test of the transform conversion; a report and every `Debug` contain no stream key, password or user name; simple- and advanced-mode profile goldens (`tests/profile.rs`); `from_parts` accepts every fixture's collection. ADR-0188 tests cover `All` and exact `Only` selection, every skipped placement's containing and referenced names and reason, nested placements inside groups, and group counts after omissions with the additional `Approximated` entry.

## Open questions

- Converting non-PNG images (needs a decoder; part of the media work).
- Mapping the Aitum vertical canvas plugin's data to the RBS vertical canvas (ADR-0093).
- Saved projectors to projector windows (ADR-0125).

## Implementation status

ADR-0188's scene-placement omission, per-placement report and exact scene
selection semantics are implemented in `rbs-import-obs`; `import.obs.check` and
`import.obs.run` both use this plan and report.

- Step 1 (done on this branch): `rbs-import-obs` with the collection planner, the source and filter mappers in the mapping table, the report and its golden fixtures; `rbs-cli obs-import scan|check|run` (`check` prints the report and writes nothing; `run` writes a new scene collection under the writer lock with a free name). The CLI's `Lookup` reads files only (PNG sizes, file presence) and enumerates no devices, so device targets keep their OBS ids and are reported. The catalogue test (every mapper type and field key exists in this build) lives in `rbs-cli`, a composition root that links the source and filter registries; `rbs-app` gets the same test with step 2.
- Step 2a (done on this branch): the profile's output facts for both output modes (encoder table, rate control, bitrates, quality, keyframes, preset, B-frames, recording folder and format, replay buffer, audio), the stream server (kept only as `rtmp://` or `srt://` without `?`, `#`, `@` or spaces; the composition root still runs `parse_server` before writing it), the stream key in `ImportedSecrets`, and hotkeys as shortcuts. `rbs-cli obs-import run` adds the hotkeys to `shortcuts.toml` with `merge_shortcuts` after writing the collection; it does not write an output profile or a credential, and says the app imports those.
- Step 2b (done on this branch): `rbs-app` `obs_import`: `AppCatalogue` and the catalogue test; `output_profile(facts, has_key, folder_exists)` maps `OutputFacts` onto `RecordProfile` (`[recording]`) and `StreamProfile` (`[stream]`): codec, rate control, bitrates clamped to 500..200000 kbit/s, quality, keyframes clamped to 1..60 s, preset, B-frames up to `MAX_B_FRAMES`, audio bitrate to the nearest of `AUDIO_KBPS`, output size when `output_size_problem` accepts it, the recording folder only when it is a full path that exists here, the server only when `parse_server` accepts it. A section that still fails its `problem()` check keeps the RBS encoder defaults (reported). A software OBS encoder is reported: RBS picks its best encoder (P3: no adapter is chosen by vendor). `write(data, lock, plan, keys, platform, folder_exists)` writes in the order of decision 9: the collection, the output profile (OBS profile name, free name), the stream key as `stream-key/<profile id>` (only with the stream settings), the shortcuts; a later step that fails is a report entry and the earlier ones stay.
- Replay buffer and monitoring device: these are app-wide settings in RBS (`replay.toml`, the monitor settings), not part of an output profile. Decision 9 lets an import change no existing file but `shortcuts.toml`, so they are reported with what OBS had and where to set it, never written. The sample rate is not reported (RBS mixes at its own rate).
- Step 3 (done on this branch): the desktop wizard. Settings › General ends with the card "Bring from another app" (key `settings.general.import`, row `general.import-obs`; at the bottom because it is rarely used, owner 2026-11-16) whose button opens the Import from OBS window (`ui/obs_import.slint`, `obs_import_page.rs`). Choose: the collections and profiles `find_obs` finds in OBS's settings folders, "No profile", or a collection file from the system file dialog; then what to bring (scenes, with a per-scene pick, global audio, audio filters, video filters, canvas, recording, stream server, stream key, hotkeys), which become `Options` (replay and audio settings stay on and are reported, decision 9). Check: `plan` with `AppCatalogue` and `AppLookup`, three counts (exact scenes, sources and items; not exactly; not imported) and the two lists, three rows each until "N more". Import: `UserSession::import_obs` runs `write` on the writer thread; the stream key goes to the control thread's key store with `Command::SaveImportedKey`; the libraries and the shortcuts reload; the Done page names the new collection and profile and lists what writing left out. Nothing is switched to: the user picks the new collection and profile in their menus.
- The saved report: `imports/<collection id>.report.toml` (`rbs_app::obs_import::report_path`, `report_text`), written by `write` after the shortcuts and removed by `delete_collection`. Test: the file is typed, its `[[skipped]]` rows hold only `obs_type` and `obs_name`, and it holds no key and no full path.
- Step 4 (done): the control API methods `import.obs.scan`, `import.obs.check` and `import.obs.run` (protocol 1.82, control API spec): sources named only as `scan` found them, files resolved and kept inside OBS's folder, `run` `admin` and `destructive`.
- Step 5 (done on this branch): Import from OBS... in the Scene collection menu of the status bar, under Import scene collection... (`ui.collection.import_obs`, in the command search too); it opens the same window as Settings › General.

### Hotkey actions

| OBS | RBS command | Notes |
| --- | --- | --- |
| `OBSBasic.StartStreaming` | `stream.toggle` | `OBSBasic.StopStreaming` reported: RBS has one start-and-stop command |
| `OBSBasic.StartRecording` | `record.toggle` | `OBSBasic.StopRecording` reported |
| `OBSBasic.PauseRecording` | `record.pause_toggle` | `OBSBasic.UnpauseRecording` reported |
| `OBSBasic.StartReplayBuffer` | `replay.toggle` | `OBSBasic.StopReplayBuffer` reported |
| `ReplayBuffer.Save` (inside `ReplayBuffer`) | `replay.save` | |
| scene `OBSBasic.SelectScene` | `scene.select?collection=<id>&scene=<id>` | |
| any other frontend action, source hotkeys | none | reported when it has a binding |

ADR-0131's table names `stream.start` and `record.start`; the shortcut registry binds the toggles `stream.toggle` and `record.toggle` (whose halves those are), so the pair rule of decision 10 maps the start action to the toggle. Keys: `OBS_KEY_A`..`Z`, `0`..`9`, `F1`..`F24` and the named keys of `rbs_ui::shortcut::Named`; number pad keys and mouse buttons are reported. The platform whose modifier names apply is read from the paths in the profile's output sections, then the collection's, then `Lookup::platform`.
