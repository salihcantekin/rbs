# Spec: Studio host

- Status: Draft
- Owner crate(s): `rbs-studio-host` (today still `crates/rbs-studio-host/src/studio_host.rs` and `studio_host/`; the move of ADR-0159 decision 6 is next)
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
| `keys` | `Arc<dyn KeyStore>` (not optional) | The root passes the system key store, tests a `MemoryKeyStore`. |
| `add_ons` | `Option<AddOnsSetup>` | `addon.list` answers that add-ons are not set up. |

`file_url` is the port function `rbs_transport::file_url`. The system implementations (`SystemKeyStore`, `add_ons::system_setup`, `system_probe::factory`, `browser_tree::RunnerTrees`, `chapters::MatroskaChapters`, `platform::host_seams`) stay in `rbs-app`.

### Thread

One control thread, `rbs-studio` (ADR-0041 decision 7, [threads](../architecture/threads.md)). Commands and control API requests are handled in order on it; a caught panic answers the waiting command and logs (`studio_host/failures.rs`, ADR-0055). Log lines name a command by its kind only, never its payload (ADR-0158).

## Errors and failure behaviour

A refused command answers `Update::Error`; a refused control API request answers its `ApiError` and logs a warning with the method and the error kind. A start failure or a stop answers `Update::Failed` and logs an error.

## Performance requirements

None of its own. The host sends commands to the engine and never runs on a [hot path](../architecture/hot-paths.md).

## Conformance tests

- One test binary, `crates/rbs-studio-host/tests/studio/main.rs` (ADR-0159 decision 7): a module per former `rbs-app` `tests/studio_*.rs` file. Tests that need a real browser runner or a window stay in `rbs-app`.
- `the_main_display_comes_from_the_roots_probe` (seam `main_display`).
- `slow_a_matroska_master_gets_its_markers_as_chapters` (seam `chapters`).
- `studio_host::failures::tests::a_kind_is_the_variant_name_without_its_payload`.

## Open questions

- The split by concern (ADR-0159 decision 8) adds modules `recording`, `stream`, `scenes`, `sources`, `outputs`, `projector` and `api`. This spec lists them when they land.
