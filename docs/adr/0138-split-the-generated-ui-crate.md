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
   - `rbs-app-ui-base`: built `as_library("rbsui")`, `links = "rbsui"`. It holds `tokens.slint` (generated, as today), every `global` and every `struct` the windows share, and one exported anchor component that has no other use. It also holds `tokens_apply` (today in `rbs-app-ui`).
   - `rbs-app-ui-main`: `MainWindow` and `FloatWindow` (the docked and floating panels).
   - `rbs-app-ui-settings`: `SettingsWindow` and its pages.
   - `rbs-app-ui-dialogs`: `SourceSettingsWindow`, `SyncMeasureWindow`, `ObsImportWindow`, `AttributesWindow`, `ProjectorWindow`.
   - `rbs-app-ui-probes`: the test-only probe windows. `rbs-app` has it as a dev-dependency only, so the `rbs` binary never compiles it.
2. Globals and structs must be declared only in base files. A window crate must import them from `@rbsui`, never from the base files by path, so they stay one Rust type. Components (library components in `components.slint` and others) may be imported by path into several window crates; each crate generates its own copy.
3. The `.slint` files stay under `crates/rbs-app/ui`, so `cargo xtask check-ui`, the UI review and the baselines are unchanged. Each window crate's `build.rs` compiles one root file that imports and re-exports its windows. Globals and structs move out of `app.slint`, `studio.slint` and the others into base files (`globals.slint`, `structs.slint` or per area), with no change to their names, properties or behaviour.
4. `rbs-app` keeps one facade module, `ui`, that re-exports all of them (today `pub use rbs_app_ui::*`). Rust code outside the facade must not name a UI crate.
5. The UI live preview stays available: `rbs-app-ui` remains as the single-crate build of `app.slint` (no library mode) and is used only with `rbs-app`'s `ui-live-preview` feature. With the feature, the facade re-exports `rbs-app-ui`; without it, the split crates. Normal builds, tests and builds that ship never compile `rbs-app-ui`.
6. A window group is split further only when measured: when one crate's cold compile exceeds about 150 s on 4 cores, or its peak memory exceeds 6 GB.
7. Slint stays pinned (ADR-0022 decision 5). A Slint upgrade that changes or removes the library mode is handled in that upgrade's PR. If the mode is gone, the fallback is decision 5's single crate, which is today's state.

## Consequences

- Cold UI compile about 2.4 times faster on 4 cores (346 s to 145 s), and a change that touches one window group rebuilds only that group (Settings 73 s instead of 381 s). No rustc process exceeds 6 GB, so cloud sessions and CI runners stop running out of memory. Opening several groups in parallel can still add up to about 16 GB, so the docs recommend `CARGO_BUILD_JOBS` of 3 or less on 16 GB machines.
- About 20 % more total CPU per cold build (74 MB of code instead of 62). On 1 or 2 cores the cold build is no faster than today; the gain is in rebuilds and memory.
- A change to a base file (a global or struct) or to `components.slint` still rebuilds every window crate, in parallel.
- The layer check (`xtask` `Class::AppUi`) and `docs/architecture/overview.md` list the new crates.
- Moving globals and structs into base files is a large but mechanical diff in `.slint` files. It changes nothing visible, and every UI test must pass unchanged.
- We depend on an experimental Slint feature (decision 7).
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

Independent review per ADR-0014. Record: reviewer (human handle, or "independent AI session"), date, verdict (approve / changes requested), main concerns and how each was resolved.
