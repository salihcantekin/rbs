# Spec: Studio host

- Status: Draft
- Owner crate(s): `rbs-studio-host` (`crates/rbs-studio-host/src/studio_host.rs` and `studio_host/`, ADR-0159 decisions 6 and 8)
- Related ADRs: ADR-0041 (control thread), ADR-0044 (control API), ADR-0055 (caught panics), ADR-0156 (shutdown, `ControlOut`), ADR-0158 (diagnostics log), ADR-0159 (this crate)

## Purpose

The studio host is the control thread that owns the `Studio`, the adapter registries and the scene collection. It answers the window's `Command`s with `Update`s and serves the control API (`rbs_api::Command`) for the remote transports. It is built without a UI toolkit, windowing, OS binding or adapter crate, so its tests link none of them (ADR-0159).

## Scope

In scope: what the host crate may depend on, its public boundary, the seams through which a composition root hands it adapters and OS facts, its thread and its tests.

Out of scope: the commands themselves (each feature's spec), the control API contract ([control API spec](control-api.md)), windows and bindings (`rbs-app`), and running the host in its own process (ADR-0099).

## Contract

### Dependencies

xtask class `StudioHost` (`cargo xtask check-layers`, test `the_studio_host_sees_no_adapter_or_toolkit`):

- Allowed: workspace crates of the classes Core (foundation and ports), Helper, Engine, Control (`rbs-api`), UI model (`rbs-ui`), Settings, User data, Add-ons, Importers, Recording model and Editor projects; the external crates `serde`, `serde_json`, `toml` and `chrono`.
- Denied: adapters, adapter support crates, control transports, `rbs-log`, app UI crates, composition roots, toolkits and OS bindings. `rbs-gpu` is used without its `preview-layer` feature.
- Dev-dependencies: the crate itself (for `fault-injection` in its tests), adapters and anything allowed above; never a composition root, an app UI crate or a toolkit.
- Users: composition roots only (class App).

### Public boundary

Public: `StudioHost` and its methods, `Command`, `Update` and the types they carry, `HostConfig` and its seam traits, the settings modules' file types with their read and write functions, `UserSession`, `ApiTokens`, `SlotFlags`, `levels` and `ui_thread`. Everything else is `pub(crate)` or private, unless a composition root or the crate's own integration tests need it. `Command` and `Update` are an in-process interface, not a versioned contract.

### Seams

A composition root fills these `HostConfig` fields (ADR-0159 decision 4). The host names no adapter itself.

| Field | Type | `None` means |
| --- | --- | --- |
| `main_display` | `Option<fn(&SourceTypes) -> Option<MainDisplay>>` | No main display is known. The root passes `display_from_default_screen` on Windows and Linux, and the ScreenCaptureKit probe on macOS. |
| `chapters` | `Option<Arc<dyn ChapterWriter>>`, where `append(&self, &mut File, &[Chapter]) -> Result<Applied, String>` | No chapters are written; the manifest keeps the markers. |
| `browser` | `Option<Arc<dyn BrowserTrees>>`, where `create(&self, Arc<EngineSlot>, Option<PathBuf>, &Arc<GpuContext>) -> Arc<dyn HostBrowserTree>` | The host's runner-less tree: browser sources are Unavailable with `helper_missing`, `in_use` and `needs_engine` are false. |
| `os_version` | `Option<String>` | `""` in the calibration key. A root always passes it. |
| `system_probe`, `read_free_space` | `Option<SystemProbeFactory>`, `Option<FreeSpaceReader>` | No free-space or memory facts. |
| `keys` | `Arc<dyn KeyStore>` (not optional), where `read`, `write` and `remove` fail with `KeyStoreError` (`Broken`, `Os(reason)`) | The root passes the system key store, tests a `MemoryKeyStore`. |
| `add_ons` | `Option<AddOnsSetup>` | `addon.list` answers that add-ons are not set up. |

`file_url` is the port function `rbs_transport::file_url`. The system implementations (`SystemKeyStore`, `add_ons::system_setup`, `system_probe::factory`, `browser_tree::RunnerTrees`, `chapters::MatroskaChapters`, `platform::host_seams`) stay in `rbs-app`.

### Errors

A refused command or step is a `Refusal` (`NotFound`, `Invalid`, `Conflict`, `Failed`, each with the words shown to the user; quality plan WP-25). `refused` maps it to the control API kind (`not_found`, `invalid_params`, `conflict`, `failed`); a method whose answer kind the protocol already fixes keeps that kind and takes the words. The UI edge (`Update::Error` and the other `Update` results) carries the words. Data types have their own errors: `DestinationError`, `stream_destinations::SectionError`, `ServerError` (`stream_profile::parse_server`), `KeyStoreError`, and `rbs_user_data::DataError` for user data. `UserSession` steps fail with `SessionError` (`Data(DataError)`, `ReadOnly`, `WriterStopped`, `ProfileInUse`, `CollectionInUse`, `NewerSection`, `NoProfileId`); `UserSession::section` returns the `DataError`. A window takes the words (`From<SessionError> for String`, `entry::when_done`).

### Thread

One control thread, `rbs-studio` (ADR-0041 decision 7, [threads](../architecture/threads.md)). Commands and control API requests are handled in order on it; a caught panic answers the waiting command and logs (`studio_host/failures.rs`, ADR-0055). Log lines name a command by its kind only, never its payload (ADR-0158).

### Meters and the audio branch

`studio_host/canvas.rs` `sync_meters`, each control-loop round (F-15, ADR-0169): meters are wanted while the window reads levels (`Command::SetMetersUi`: the Mixer is shown in a window that is not hidden, or a noise gate's bar in Properties reads its levels; the window folds both into one bool, `rbs-app` `meters_ui.rs`) or a control client subscribes to `audio.levels`. Wanted, the host puts meters on the mixer and wakes the audio branch (`Studio::set_audio_awake(true)`) at once. Not wanted, meters come off at once and the branch rests `AUDIO_REST_AFTER` (2 s) later, on the host's injected clock, armed as one control-loop wake (`drive_audio_rest`); a reader that returns before cancels it. The host starts with the branch resting (`StudioSettings::audio_awake = false`) and meters off until the window says otherwise; `restart` carries both states and a pending deadline (moved to the new engine clock) over to the new studio. Failures of waking or resting log one `warn` line in `Area::Audio` with their count.

## Errors and failure behaviour

A refused command answers `Update::Error`; a refused control API request answers its `ApiError` and logs a warning with the method and the error kind. A start failure or a stop answers `Update::Failed` and logs an error.

## Performance requirements

None of its own. The host sends commands to the engine and never runs on a [hot path](../architecture/hot-paths.md).

## Conformance tests

- One test binary, `crates/rbs-studio-host/tests/studio/main.rs` (ADR-0159 decision 7): a module per former `rbs-app` `tests/studio_*.rs` file. Tests that need a real browser runner or a window stay in `rbs-app`.
- `the_main_display_comes_from_the_roots_probe` (seam `main_display`).
- `slow_a_matroska_master_gets_its_markers_as_chapters` (seam `chapters`).
- `studio_host::failures::tests::a_kind_is_the_variant_name_without_its_payload`.

## Modules

`studio_host.rs` keeps the `Host` state, `Command` routing and the shared constants; ADR-0159 decision 8 split the rest by concern under `studio_host/`:

| Concern | Modules |
| --- | --- |
| Thread and seams | `handle` (`StudioHost`, the control API it serves), `config` (`HostConfig`, test faults), `start`, `control_loop`, `seams`, `failures`, `refusal` |
| Window interface | `command`, `updates`, `views`, `rows`, `publish_cost` (tests) |
| Control API | `control_api` (with `control_api/api_*`), `api_answers`, `api_convert` |
| Outputs | `outputs`, `output_state`, `recording`, `recording_markers`, `stream`, `replay`, `projector`, `on_air`, `cues` |
| Scenes and sources | `scenes`, `sources`, `source_settings`, `items`, `targets`, `pickers`, `crop`, `held`, `history`, `canvas`, `filters`, `interact`, `stinger`, `text_live`, `text_measure`, `text_resize`, `scene_cache` |
| Audio and devices | `loudness`, `monitor`, `device_sync`, `sync_measure`, `calibration`, `attributes`, `network`, `protection` |
| Other | `add_ons`, `obs_import_api`, `control_out`, `log_summary`, `support` (shared conversions), `tests` |

State per concern (ADR-0159 decision 8): each is a field of `Host`, defined in its module and built in `start`.

| Field | Struct | Holds |
| --- | --- | --- |
| `recording` | `recording::RecordingState` | Record profiles, separate files, the limit policy, encoders |
| `streaming` | `stream::StreamState` | Stream profiles, destinations, the key store |
| `vcam` | `outputs::CameraState` | The virtual camera |
| `verticals` | `outputs::VerticalHost` | The vertical canvas and its picture |
| `program` | `held::ProgramState` | The held program, transitions, the T-bar |
| `edits` | `history::EditHistory` | Undo history |
| `texts` | `text_live::TextState` | Live text timers and re-renders |
| `loudness_state` | `loudness::LoudnessState` | The loudness meter |
| `pickers` | `pickers::Pickers` | Thumbnail and region grabs, permission prompts |

Modules that already had a state type keep it (`monitor`, `replay`, `protection`, `add_ons`, `calibration`, `sync_measure`, `scene_cache`).

## Open questions

- The scene, source and canvas fields (collection, rows, sizes, canvas outputs) stay on `Host`: nearly every concern reads them.
