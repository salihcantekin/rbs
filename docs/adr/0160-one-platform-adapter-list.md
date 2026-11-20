# ADR-0160: One platform adapter list in `rbs-platform`, shared by the composition roots

- Status: Proposed
- Date: 2026-11-20
- Deciders: salihcantekin

## Context

Only composition roots name adapters (P1, `docs/specs/backend-ports.md` "Registry and selection" item 1). Two roots register built-in adapters today, each with its own hand-written list:

- `crates/rbs-app/src/platform.rs` (224 lines): `adapters() -> (RecordAdapters, Vec<String>)`, passed as a function to `StudioHost::spawn` and used by eight `rbs-app` examples; `virtual_camera(config) -> Result<VirtualCamera, &'static str>` for Settings › Virtual camera.
- `crates/rbs-cli/src/platform.rs` (173 lines): `adapters() -> PlatformAdapters` (the registries, the video encoder factories again as a `Vec` in registration order for `--encoder`, and the errors); `virtual_camera() -> Option<Box<dyn VirtualCameraFactory>>`.

The lists have drifted (quality audit QS-07, `docs/quality/audit-2026-11.md`; plan WP-09, ADR-0144):

- Only the app registers the text renderer and the local text clock (ADR-0129), the media decoders (`rbs-media-anim`, `rbs-media-mf`, `rbs-media-avf`, and the `rbs-media-decode-test` demo pattern as the last one, ADR-0108), and the demo capture and audio sources (feature `demo-sources` plus `RBS_DEMO_SOURCES=1`).
- The same muxers, audio encoders, transports, capture, audio and video encoder adapters appear in both, in the same order per registry; the CLI builds its video encoders in a separate function.
- Both keep the virtual camera's link in `vcam-link.txt` in the settings folder, through two different paths of code (`vcam_settings::link_file` in the app, a literal in the CLI).
- Every new adapter is added in two `platform.rs` files and two `Cargo.toml` files, and the CLI does not exercise what the app ships. `rbs-mcp` registers no adapter (ADR-0052 decision 2) and must stay that way.

ADR-0159 keeps `platform.rs` in `rbs-app` and gives the studio host its adapters only through the `RecordAdapters` closure of `StudioHost::spawn`. The local text clock (`crates/rbs-app/src/text_clock.rs`, `chrono`) is used only by `platform.rs`.

## Decision

1. **A new crate `rbs-platform`** (library `rbs_platform`) holds the one list of built-in adapters for every platform. It holds no state, starts no thread and opens nothing: building the list constructs factories only, and a factory's constructor must not probe, load a library or open a device (P13; probing stays with the registries, backend-ports item 2).
2. **Its public API:**
   - `pub struct Options { pub demo_sources: bool }` with `Default` (`false`). `demo_sources` registers `rbs-capture-test` and `rbs-audio-io-test`; it exists only with the crate's feature `demo-sources`, which `rbs-app`'s feature of the same name forwards. The crate reads no environment variable; `rbs-app` keeps reading `RBS_DEMO_SOURCES` and fills `Options`.
   - `pub fn adapters(options: &Options) -> PlatformAdapters`, where `PlatformAdapters { adapters: RecordAdapters, video_encoders: Vec<Arc<dyn VideoEncoderFactory>>, errors: Vec<String> }`. `video_encoders` holds the same `Arc`s as `adapters.video_encoders`, in registration order (the CLI's `--encoder` installs one of them alone). `errors` are duplicate-id registrations, a bug in the list.
   - `pub struct VirtualCamera` (today's `rbs-app` type: factory plus `remove_completely`) and `pub fn virtual_camera(config: &Path) -> Result<VirtualCamera, &'static str>`, with the reasons of today's app ("Not available on macOS yet", "Not available on Linux yet"). `pub fn vcam_link_file(config: &Path) -> PathBuf` names `vcam-link.txt`; `rbs-app`'s `vcam_settings::link_file` calls it.
   - `LocalTextClock` moves from `rbs-app` into the crate (a private module); `adapters` sets it as the text clock.
3. **One list, the app's.** The list is today's `rbs-app` list, in its order: muxers, audio encoders, transports, text renderer and clock, then per platform capture, audio, video encoders and audio encoders, then demo sources, then media decoders (animation, platform decoder, demo pattern last). Registration order within each registry is unchanged for both roots, so no root picks a different adapter than today. The CLI gains the text renderer, the clock and the media decoders; it uses none of them today (`record` captures a display or camera), and they cost nothing until a source asks (decision 1). Moving `rbs-media-decode-test` out of the product (QS-09) is a separate change and edits only this crate.
4. **Both roots use it, and only roots may.** `rbs-app` and `rbs-cli` call `rbs_platform::adapters` and `rbs_platform::virtual_camera`; the CLI passes `rbs_remote_settings::config_dir()`. `rbs-app/src/platform.rs` stays as a thin wrapper with today's signatures (`adapters() -> (RecordAdapters, Vec<String>)`, `virtual_camera`, `VirtualCamera` re-exported), so `StudioHost::spawn`, the examples, `vcam_page.rs` and ADR-0159's move do not change. `rbs-cli/src/platform.rs` is deleted. Each root drops the adapter dependencies only `platform.rs` used; dependencies its other code names directly stay.
5. **A layer class of its own, enforced.** `rbs-platform` gets the xtask class `Platform` and a row "Platform adapter list" in the overview's layer table. It may depend on workspace crates of the classes Core, Helper, Support, Engine and Adapter, and on external crates except UI toolkits (`is_ui_toolkit`); it may not depend on Control, Transport, UI, App UI, Settings, User data, Add-ons, Importers, Diagnostics, Plugin, Recording model, Editor projects, App or Tool crates. Only App-class crates may depend on it; `check-layers` fails any other crate that does, and `rbs-mcp` must not (ADR-0052). xtask tests cover both directions.
6. **Tests.** In `rbs-platform`: `adapters(&Options::default())` returns no errors; `video_encoders` lists exactly the registry's ids in order; the text clock is set; the media decoders end with the demo pattern; with `demo-sources`, `demo_sources: true` adds the two test adapters and `false` does not. The CLI's existing `--encoder` tests stay and keep passing.
7. **Order of work.** One commit adds the crate, the class and its tests; one switches `rbs-cli`; one switches `rbs-app` (wrapper, moved text clock, `vcam_settings::link_file`, `Cargo.toml`), with `check-size --lower` for the removed lines. The only files shared with ADR-0159's work are the workspace `Cargo.toml` members, `rbs-app/Cargo.toml` and the xtask class table; each push fetches `main` first.

## Consequences

- One place to add an adapter: a line in `rbs-platform` and its `Cargo.toml`. The CLI registers and can test what the app ships, and a later root (a studio process, ADR-0099) gets the same list with one call.
- `rbs-cli` links the text and media decoder crates it did not link before: a larger binary and some more build work for the CLI alone; no run-time cost until used.
- Spec and docs updates in the first commit: `docs/specs/backend-ports.md` item 1 names `rbs-platform` as where built-in adapters are listed; the overview's layer table gets the row; `docs/status.md`; the ADR index.
- Not changed: the registries, the preference order and selection (backend-ports items 2 to 5), `StudioHost::spawn` and `HostConfig` (ADR-0159), the virtual camera's behaviour and file, the control API, the scene format, hot paths (registration runs once at start-up, outside `hot-paths.md`).
- Not covered: adapters `rbs-app` uses outside the registries (the add-ons system setup, the system probe, the browser runner, the key store, and the seams of ADR-0159 decision 4). They stay in `rbs-app`; moving any of them here needs its own change.

## Alternatives considered

- **A shared source file (`include!` or a `#[path]` module) in both roots.** One list, but both `Cargo.toml` files still name every adapter, and `check-layers` cannot see or guard it.
- **Keep two lists and add a test that compares them.** Still two edits per adapter; the test would need a third list of allowed differences.
- **Put the list in `rbs-record` or another engine crate.** Engine crates may not depend on adapters (P1, `check-layers`).
- **Put it in `rbs-studio-host`.** The CLI would link the whole studio host, and ADR-0159 keeps adapters out of that crate.
- **A data file of adapter ids with a lookup.** Code must still name each factory to construct it, so the lookup is a second list.
- **Options per difference (text, media, demo pattern) instead of one list.** Keeps today's drift as configuration; the CLI gains nothing by leaving them out, since unused factories cost nothing.

## Review

Independent review per ADR-0014. Pending.
