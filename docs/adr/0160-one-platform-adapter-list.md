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

1. **A new crate `rbs-platform`** (library `rbs_platform`) holds the one list of built-in adapters for every platform. It holds no state and starts no thread. `adapters` opens nothing and touches no file: building the list constructs factories only, and a factory's constructor must not probe, load a library or open a device (P13; probing stays with the registries, backend-ports item 2).
2. **Its public API:**
   - `#[non_exhaustive] pub struct Options { pub demo_sources: bool }` with `Default` (`false`), so a later option does not break callers. `demo_sources` registers `rbs-capture-test` and `rbs-audio-io-test` when the crate's feature `demo-sources` is on (`rbs-app`'s feature of the same name forwards to it); without the feature the field is still there and is ignored, as its doc comment says. The crate reads no environment variable; `rbs-app` keeps reading `RBS_DEMO_SOURCES` and fills `Options`.
   - `pub fn adapters(options: &Options) -> PlatformAdapters`, where `PlatformAdapters { adapters: RecordAdapters, video_encoders: Vec<Arc<dyn VideoEncoderFactory>>, errors: Vec<String> }`. `video_encoders` holds the same `Arc`s as `adapters.video_encoders`, in registration order (the CLI's `--encoder` installs one of them alone). `errors` are duplicate-id registrations, a bug in the list.
   - `pub struct VirtualCamera` (today's `rbs-app` type: an `Arc<dyn VirtualCameraFactory>` plus `remove_completely`) and `pub fn virtual_camera(config: &Path) -> Result<VirtualCamera, VcamUnavailable>`, where `VcamUnavailable` is a typed reason (`#[non_exhaustive] enum { NotOnThisPlatform }`, rule 8). The words a user sees stay in the roots: `rbs-app` maps it to today's "Not available on macOS yet" and "Not available on Linux yet" (Settings › Virtual camera), and the CLI keeps today's messages ("--vcam: the virtual camera is not available on this platform", and `vcam`'s no-camera path). `pub fn vcam_link_file(config: &Path) -> PathBuf` names `vcam-link.txt`; `rbs-app`'s `vcam_settings::link_file` calls it.
   - `LocalTextClock` moves from `rbs-app` into the crate (a private module); `adapters` sets it as the text clock.
3. **One list, the app's.** The list is today's `rbs-app` list, in its order: muxers, audio encoders, transports, text renderer and clock, then per platform capture, audio, video encoders and audio encoders, then demo sources, then media decoders (animation, platform decoder, demo pattern last). Registration order within each registry is unchanged for both roots, so no root picks a different adapter than today. The CLI gains the text renderer, the clock and the media decoders; it uses none of them today (`record` captures a display or camera). Its start-up probe (`probe_io`) now also probes the media decoders; those probes are static (`cfg!` checks, no I/O) and open nothing, and registration adds no thread or device (decision 1). Moving `rbs-media-decode-test` out of the product (QS-09) is a separate change and edits only this crate.
4. **Both roots use it, and only roots may.** `rbs-app` and `rbs-cli` call `rbs_platform::adapters` and `rbs_platform::virtual_camera`; the CLI passes `rbs_remote_settings::config_dir()`. `rbs-app/src/platform.rs` stays as a thin wrapper with today's signatures (`adapters() -> (RecordAdapters, Vec<String>)`, `virtual_camera`, `VirtualCamera` re-exported), so `StudioHost::spawn`, the examples, `vcam_page.rs` and ADR-0159's move do not change. `rbs-cli/src/platform.rs` is deleted; the CLI's `start_camera` returns the `Arc` from `VirtualCamera::factory` instead of today's `Box`, and `vcam::run` (it takes `Option<&dyn VirtualCameraFactory>`) is unchanged (internal to `rbs-cli`). `rbs-app`'s wrapper maps `VcamUnavailable` back to today's `&'static str`, so `vcam_page.rs` does not change. ADR-0159 is unchanged and needs no `## Review` edit: `platform.rs` stays in `rbs-app`, and `StudioHost::spawn` still gets the same function. Each root drops the adapter dependencies only `platform.rs` used; dependencies its other code names directly stay.
5. **A layer class of its own, enforced.** `rbs-platform` gets the xtask class `Platform` and a row "Platform adapter list" in the overview's layer table.
   - What it uses is an allow-list (`deny_platform`): workspace crates of the classes Core, Helper, Support, Engine and Adapter, except the three Support crates that carry OS bindings (`rbs-vcam-mf-source`, `rbs-browser-shm`, `rbs-system-probe`, excluded as in `deny_engine`), and the external crate `chrono` (the text clock). Everything else is denied, Diagnostics included. FFmpeg, OS bindings and vendor SDKs come only through an adapter crate, never directly.
   - Who uses it: `to == "rbs-platform"` is denied unless `from` is `rbs-app` or `rbs-cli`, for normal and dev-dependencies alike: the rule sits in `denied` before its early return for dev edges (otherwise an engine crate could pull every adapter in through a dev-dependency), and in `deny` before the class dispatch, like the `rbs-log` rule. This is needed because App-class crates may otherwise depend on anything, and `rbs-mcp` (App class) must not use it (ADR-0052 decision 2).
   - `Class::Platform` is added to the denied arm of `deny_engine` and `deny_adapter`, whose matches list every class (unreachable after the rule above, kept for exhaustiveness).
   - xtask tests: `rbs-app` and `rbs-cli` may depend on it; `rbs-mcp` and `rbs-record` may not, as a normal or a dev-dependency; `rbs-platform` may depend on an adapter, `rbs-core` and `chrono`, and not on `rbs-api`, `rbs-log`, `rbs-system-probe`, `slint` or a `windows` crate.
6. **Tests.** In `rbs-platform`: a golden test lists, per registry readable through today's public API (muxers, audio and video encoders, transports, and the source types `SourceTypes` reports), the expected adapter ids in order for the build platform (Linux, Windows and macOS each a `cfg` block; any other target expects the common part only), so "order unchanged" is checked and later drift fails a test; `adapters(&Options::default())` returns no errors; `video_encoders` lists exactly the registry's ids in order; the text clock is set. The media decoders are built by a private function that returns them in registration order, and a unit test checks that list (animation first, the demo pattern last), so no new reader is added to `rbs-sources`; with `demo-sources`, `demo_sources: true` adds the two test adapters and `false` does not. The CLI's existing `--encoder` tests stay and keep passing.
7. **Order of work.** One commit adds the crate, the class and its tests; one switches `rbs-cli`; one switches `rbs-app` (wrapper, moved text clock, `vcam_settings::link_file`, `Cargo.toml`), The list is written as one function per platform block (common, Windows, Linux, macOS, demo, media), so no function nears `check-size`'s 200-line limit and the new items need no baseline entry; after the switch, `check-size --lower` records the shorter `rbs-app/src/platform.rs` and the deleted `rbs-cli/src/platform.rs` (no `--moved`, since the items are rewritten, not moved). The only files shared with ADR-0159's work are the workspace `Cargo.toml` members, `rbs-app/Cargo.toml` and the xtask class table; each push fetches `main` first.

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

Independent review per ADR-0014.

### Round 1

- Reviewer: independent AI session (fresh context: the ADR, CLAUDE.md, principles, overview, backend-ports spec, ADR-0052, ADR-0144, ADR-0159 and the code), 2026-11-20. Verdict: changes requested.
- Verified against the code: the same order per registry in both roots; factory constructors do not probe; `LocalTextClock` is used only by `platform.rs`; the same link file name; the test adapters are used in `rbs-app` only by `platform.rs` and tests.
- Concerns and resolution:
  1. (Blocking) `check-layers` cannot stop `rbs-mcp` (App-class crates may depend on anything). Resolved: decision 5 names a rule in `deny` that allows `rbs-platform` only from `rbs-app` and `rbs-cli`, with tests.
  2. (Blocking) The class's allowed set was incomplete and external crates were open. Resolved: decision 5 is an allow-list (Core, Helper, Support, Engine, Adapter, `chrono`).
  3. The CLI's start-up probe now also probes media decoders. Resolved: decision 3 says so (static probes, no I/O).
  4. A feature-gated field on a public struct. Resolved: the field is always present, ignored without the feature; `Options` is `#[non_exhaustive]`.
  5. User-visible text and `&'static str` in a non-UI crate. Resolved: typed `VcamUnavailable`; the roots keep their words, CLI messages unchanged.
  6. The CLI moves from `Box` to `Arc`. Resolved: stated in decision 4.
  7. No test guards the order claim. Resolved: golden per-registry order test in decision 6.
  8. `check-size` handling. Resolved: per-platform functions, `--lower` after the switch, stated in decision 7.
  9. ADR-0159 tie. Resolved: decision 4 says ADR-0159 is unchanged.

### Round 2

- Reviewer: a second independent AI session (fresh context: the ADR with the round 1 record, CLAUDE.md, the architecture-review skill, `xtask/src/main.rs`, both `platform.rs` files, `rbs-cli` `main.rs` and `vcam.rs`, `vcam_page.rs`, `vcam_settings.rs`, the `rbs-sources` registry, the media decoder probes), 2026-11-20. Verdict: changes requested.
- Verified: round 1 resolutions 2 to 9 are in the decision text and match the code; no caller of either `platform` module outside the two roots and the eight `rbs-app` examples; the same order per registry; static media decoder probes, the text renderer not probed; `VcamUnavailable` is enough; the golden test is deterministic (no environment read, `Options` explicit, so feature unification does not change it).
- Concerns and resolution:
  1. (Blocking) Dev-dependency edges skipped the rule (`denied` returns early for most dev edges). Resolved: decision 5 applies the rule to dev edges too, before that early return, with tests.
  2. Support crates with OS bindings were allowed. Resolved: the three are excluded.
  3. `Class::Platform` needs arms in the exhaustive matches of `deny_engine` and `deny_adapter`. Resolved: stated.
  4. The decoder order test needed a reader `rbs-sources` does not have. Resolved: the test checks the crate's own ordered list; no engine API change.
  5. `vcam::run` already takes `&dyn`. Resolved: only `start_camera` changes.
  6. Other targets in the golden test. Resolved: they expect the common part only.
  7. `virtual_camera` creates the settings folder on Windows. Resolved: "opens nothing" is scoped to `adapters`.
