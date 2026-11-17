# ADR-0138: Split the generated UI code into a base crate and one crate per window group

- Status: Proposed
- Date: 2026-11-17
- Deciders: Salih (owner)

## Context

Slint turns the 21,000 lines under `crates/rbs-app/ui` into one Rust file of about 62 MB, compiled as one crate, `rbs-app-ui`. rustc compiles it on one core. Measured on a 4-core / 15 GB cloud container at main 1b8e325:

| | `rbs-app-ui` today |
| --- | --- |
| Compile (cold) | 327 s, plus 19 s of code generation in `build.rs` (11 s since #496) |
| Rebuild after one changed text in a Settings page | 381 s; rustc's incremental cache does not help |
| Peak memory of the one rustc process | 12.8 GB |
| `clippy --all-targets` | lib and lib-test copies run together and are killed for lack of memory unless `-j1` |

`rbs-app-ui` is on the critical path of every build and test run of `rbs-app`, and of every future CI job. Its memory is close to the 16 GB of a standard CI runner and above what two parallel cloud sessions can use.

Where the 62 MB comes from (generated code attributed to source files): `studio.slint` 29 MB (the Settings pages about 13 MB of it), `app.slint` 9 MB, `components.slint` 4.8 MB, `appearance.slint` 3 MB, the dialogs 6 MB, the test-only probe windows 1.3 MB. 41 MB is 1,320 sub-components that Slint creates for each `if` and `for`, each at least 7 KB and typically 26 KB. The generated menu slots (`menu_bar.rs`, `context_menu.rs`) are about 2.5 MB of that.

Slint shares code between windows of one compilation, but not between crates: a component used by two crates is generated in both. Slint 1.18 has an experimental library mode (`slint-build` feature `experimental-module-builds`, `CompilerConfiguration::as_library`, `import { … } from "@name"`). In it, a library crate's **globals and structs** become Rust types that dependent crates use, but its components are still generated again in each crate. A prototype (two crates, `/mnt/project-files/ui-split/` and a library test) showed:

| Split by window group, built in parallel | Cold | Same one-line Settings change | Peak memory per rustc |
| --- | --- | --- | --- |
| main + floating windows (25 MB) | 136 s | 27 s (output unchanged, incremental cache hit) | 5.4 GB |
| Settings window (26 MB) | 130 s | 62 s | 5.9 GB |
| dialogs (11 MB) | 69 s | 12 s | 2.4 GB |
| probes, test-only (13 MB) | 73 s | 12 s | 2.7 GB |
| **Wall time, all four** | **145 s** (was 346 s) | **73 s** (was 381 s) | sum about 16 GB if all peak at once |

Total generated code grows from 62 to 74 MB because `components.slint` is generated in every crate.

The library test also showed that the base crate must declare `links` in `Cargo.toml` (`slint-build` passes the library's name and source through `DEP_<links>_*`) and must export at least one component, or Slint generates nothing for it. The UI live preview (`cargo xtask ui-dev`, #496) does not support library imports: its generated wrappers do not implement `Global` for imported globals.

## Decision

1. The generated UI code is split into these crates, all in the application layer next to `rbs-app`:
   - `rbs-app-ui-base`: built `as_library("rbsui")`, with `links = "rbsui"`. It holds `tokens.slint` (generated, as today), every `global` and every `struct` that more than one window file uses, and one exported anchor component that has no other use. Its root file exports every global and struct declared in base files.
   - `rbs-app-ui-main`: `MainWindow` and `FloatWindow` (the docked and floating panels).
   - `rbs-app-ui-settings`: `SettingsWindow` and its pages.
   - `rbs-app-ui-dialogs`: `SourceSettingsWindow`, `SyncMeasureWindow`, `ObsImportWindow`, `AttributesWindow`, `ProjectorWindow`.
   - `rbs-app-ui-probes`: the test-only probe windows.
   - `rbs-app-ui-single`: the single-crate build of `app.slint` without library mode, for the UI live preview (decision 5).
   - `rbs-app-ui` stays the one crate that `rbs-app`, its tests and its examples name. It becomes a facade library that re-exports the base and the window crates. It re-exports the probes only behind a `probes` feature, which only `rbs-app`'s dev-dependency turns on, so the `rbs` binary never compiles them. It also holds `tokens_apply`, generated once by one generator module that both build modes use. Only this facade names the split crates.
2. Library identity in Slint 1.18 goes by name, so these rules hold. `cargo xtask check-ui` checks them (a new count that must be 0):
   - Only a window crate's root file imports `@rbsui`, and it imports everything the base root exports.
   - Every global and struct declared in a base file is exported by the base root.
   - No file outside the base declares a global or struct with the name of a base export.
   - Other files (`components.slint`, page files) keep importing `tokens.slint` and the base files by path. That keeps them compiling in the single build and in the base itself.
3. Window crates must import the very `tokens.slint` the base generated. A second copy would be a second `Theme`. The base `build.rs` emits `cargo::metadata=TOKENS_DIR=<OUT_DIR>`. Each window crate depends directly on the base, reads `DEP_RBSUI_TOKENS_DIR` and adds it to its include path.
4. A window crate's root file exports windows only, never components, globals or structs. Globals and structs reach Rust once, through the base, so the facade's re-exports never clash.
5. The UI live preview stays available. `rbs-app`'s `ui-live-preview` turns on the facade's `live-preview` feature, which pulls in `rbs-app-ui-single`. The facade switches to it only when its build script sees both the feature and `SLINT_LIVE_PREVIEW`; it then sets a cfg. With the feature but not the variable (`cargo doc --all-features`), `rbs-app-ui-single`'s build script generates nothing and the split crates are used, so no build compiles the 62 MB crate by accident. Two checks keep the builds from drifting:
   - an `xtask` check that `app.slint`'s exported windows equal the union of the window roots' exports, with no window missing and none extra;
   - `SLINT_LIVE_PREVIEW=1 cargo check -p rbs-app --features ui-live-preview`, run by `cargo xtask affected check` whenever `crates/rbs-app/ui` or a UI crate changes.
6. Global instances stay per window, as today: each window instance creates its own library globals (`rbsuiSharedGlobals::new`). Rust keeps calling `apply_tokens` and setting globals on each window.
7. The `.slint` files stay under `crates/rbs-app/ui`, so the UI review and the baselines are unchanged. Globals and structs move out of `app.slint`, `studio.slint` and the others into base files, with no change to their names, properties or behaviour.
8. A window group is split further only when measured with the same method as the Context table (4 cores, 15 GB, `CARGO_INCREMENTAL=0`, cold `cargo build`, peak RSS of the rustc process from `/usr/bin/time`). The threshold is a cold compile of one crate over 150 s, or a peak over 6 GB.
9. Slint stays pinned (ADR-0022 decision 5). If a Slint upgrade changes or removes the library mode, that upgrade's PR handles it. The fallback is to point the facade at `rbs-app-ui-single` for every build, a change in the facade only.

## Consequences

- Cold UI compile about 2.4 times faster on 4 cores (346 s to 145 s), and a change that touches one window group rebuilds only that group (Settings 73 s instead of 381 s). No rustc process exceeds 6 GB, so cloud sessions and CI runners stop running out of memory. Several groups peaking together can still add up to about 16 GB, so the docs recommend `CARGO_BUILD_JOBS` of 3 or less on 16 GB machines.
- About 20 % more total CPU per cold build (74 MB of code instead of 62). On 1 or 2 cores the cold build is no faster than today; the gain is in rebuilds and memory.
- A change to a base file or to `components.slint` still rebuilds every window crate, in parallel.
- Same change: the `xtask` layer classes (`Class::AppUi`) and `docs/architecture/overview.md` list the new crates. `xtask affected` maps `crates/rbs-app/ui` to all of them. The testing strategy's live-preview section describes the switch.
- Moving globals and structs into base files is a large but mechanical diff in `.slint` files. It changes nothing visible, and every UI test must pass unchanged.
- Window root files that import `@rbsui` do not resolve in the Slint language server or viewer without library paths. Root files stay a few lines long, and every other file keeps path imports, which do resolve.
- We depend on an experimental Slint feature (decision 9).
- Follow-ups, separate from this ADR:
  - Shrink generated code where it costs nothing at run time. First candidates are the generated menu slots (about 2.5 MB) and `if` blocks around single cheap elements, where a `visible:` binding gives the same result. Each change is measured and keeps the zero-cost rule (P13) for anything that holds resources.
  - Recommend `sccache` for local builds across target directories (optional, outside the repository).

## Alternatives considered

- **Keep one crate and wait for rustc's parallel front end.** It is nightly-only (`-Z threads`), and peak memory would not drop.
- **One crate per window group without the library mode.** Globals and structs would become different Rust types per crate, and the Rust side would set every global once per crate type. That doubles the glue code and invites drift.
- **Split by `.slint` file.** Slint components cannot cross crates, so every crate would still generate everything its windows use. The split has to follow windows.
- **Only the live preview (#496).** It removes the rebuild while editing the UI, but tests, screenshots, CI and release builds still compile the 62 MB crate.
- **Lower optimisation or codegen units.** Debug builds are already `opt-level = 0` without debug info for this crate. The time is in the front end (type checking, borrow checking), which these settings do not change.

## Review

### Round 1: independent AI session, 2026-11-17, changes requested

The reviewer checked the proposal against the Slint 1.18.1 sources and the rbs-app code. They confirmed that library-mode globals become one Rust type with a `Global` impl per window. Concerns and how each was resolved:

1. Integration tests and `look.rs`/`window_events.rs` name `rbs_app_ui` directly, so a facade module inside the binary cannot serve them (blocking). Resolved: `rbs-app-ui` becomes the facade library and keeps its name; probes sit behind a dev-only feature (decision 1).
2. Path imports of base files must stay, and library identity goes by name, so a missed export or a same-named local declaration fails without notice (blocking). Resolved: rules restated, plus an `xtask check-ui` count (decision 2).
3. Window crates need the base's own `tokens.slint` (blocking). Resolved: `TOKENS_DIR` metadata through `links` (decision 3).
4. Single and split builds can drift, `tokens_apply` must not be copied, and `--all-features` must not compile the 62 MB crate (should fix). Resolved: one generator, an export-parity check, a live-preview `cargo check` in `affected check`, and the env-gated switch (decisions 1 and 5).
5. Per-window global instances not stated (should fix). Resolved: decision 6.
6. Clashes between re-exported names (should fix). Resolved: window roots export windows only (decision 4).
7. Layer classes, `affected` mapping and docs (should fix). Resolved: listed in Consequences as part of the same change.
8. Measurement method for further splits (nit). Resolved: decision 8.
9. The fallback is a facade change (nit). Resolved: decision 9.
10. Language server cannot resolve `@rbsui` (nit). Resolved: noted in Consequences.
