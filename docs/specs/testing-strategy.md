# Spec: Testing strategy

- Status: Draft
- Related ADRs: [0003](../adr/0003-ports-and-adapters.md), [0009](../adr/0009-performance-budgets-in-ci.md), [0012](../adr/0012-ai-assisted-development.md), [0032](../adr/0032-fast-deterministic-tests.md)

## Layers

| Layer | What | Where it runs |
| --- | --- | --- |
| Unit tests | Pure logic in every crate | Hosted CI, all platforms |
| Property tests | Clock conversion, capability matching, mux output, protocol framing (`proptest`) | Hosted CI |
| Conformance suites | One per port; every adapter runs it | Hosted CI (software adapters), self-hosted (hardware) |
| Fuzzing | Parsers and protocol handlers (`cargo fuzz`) | Nightly |
| Miri / sanitizers | Crates with `unsafe` | Hosted CI where supported |
| Golden image tests | Compositor and filters render reference scenes; compared with tolerance | Hosted CI with software rendering (WARP / lavapipe) |
| Scenario benchmarks | End-to-end headless scenes with budgets | Self-hosted GPU runners, nightly |
| Soak tests | 4-hour stream + record per tier-1 platform | Before each release |

## Rules

Binding ([ADR-0032](../adr/0032-fast-deterministic-tests.md); procedure: `test-reliability` skill).

1. **No flaky, no hanging test.** A test that fails once without a code change is fixed (root cause, in the test or the code) before any other work merges. No retries, no `#[ignore]` or skip to get green, no "re-run CI".
2. **Fake time.** Time-dependent logic reads an injected clock (`rbs_core::time::Clock`; `TestClock` in tests) or takes `now` as an argument. Tests advance fake time and never sleep for timing.
3. **Wait for conditions, not durations.** Another thread is awaited by polling the condition (`eventually`, a channel, a join). A tick is awaited by its index, not by a counter bumped after it. Thread order that matters is set by the test.
4. **Timeouts that must fire** hold the other side until the timeout was observed (a gate opened on an observable step such as `WorkerStats::stops`), never for a fixed time.
5. **Real-time bounds are hang guards**: at least 10x the expected time and never below 250 ms (prefer 1 s or more when a passing run does not wait for it), with a comment. Performance budgets are checked by benchmarks, not by wall-clock asserts in pass/fail tests.
6. **Tiers.** Fast tier: everything run by default; each test well under 1 s, a crate's fast tier in seconds; a binary over 1 s says why in a comment. Slow tier: deterministic tests that need seconds of real time, `#[ignore = "slow: <why>"]` and named `slow_*`, run with `cargo nextest run --run-ignored only -E 'test(/^slow_/)'` (other ignored tests are child-process helpers or need a tool). Hardware: skip unless `RBS_REQUIRE_<THING>=1`. External tools: `#[ignore = "needs <tool>"]` in `*_interop` binaries.
7. **CI** runs both tiers on every platform with `cargo nextest run --profile ci` (`.config/nextest.toml`): no retries, a test at 15 s is reported, at 60 s it is killed; job timeouts; the slowest tests are listed in the job summary (reported, not gated). Tests and lints (clippy, rustdoc) are separate jobs that run side by side. A pull request that changes only documentation (`docs/`, `.claude/`, Markdown files at the repository root) skips the build and test steps; pushes to main always run everything.
8. **A flake fix is proven**: root cause in the PR, and the test run repeatedly under CPU load before and after.
9. No test depends on network access except explicitly marked integration tests against local servers started by the test.
10. Hardware-dependent tests skip with a clear message when hardware is absent.

## Running tests

| Goal | Command |
| --- | --- |
| The crates a change touches | `cargo test -p <crate>` (or `cargo nextest run -p <crate>`) |
| Everything a branch can break | `cargo xtask affected test` (also `check`, `clippy`, `list`; `--base <rev>`, extra arguments after `--`) |
| Whole fast tier | `cargo nextest run --workspace` (or `cargo test --workspace`) |
| Slow tier | `cargo nextest run --workspace --run-ignored only -E 'test(/^slow_/)'` |
| Stress one test binary | run it 50 times pinned to two cores (`taskset -c 0,1`) with busy loops on the others |

### Which tests to run

`cargo xtask affected` diffs the working tree, untracked files included, against the merge base with `origin/main` and selects the workspace packages the change can break:

- the package that owns each changed file under `crates/` or `xtask/`;
- every package whose Rust sources name a changed file or its directory (the `build.rs` of each `rbs-app-ui*` crate compiles `../rbs-app/ui`, `rbs-import-obs` includes `docs/specs/obs-import.md`, `xtask` reads `docs/ui`);
- for a `Cargo.lock` change, the users of each lock entry that changed;
- then every package that depends on a selected one (normal, build and dev dependencies, from `Cargo.lock`).

When a selected package is one of the UI crates, `affected test` also runs `cargo check -p rbs-app --features ui-live-preview` with `SLINT_LIVE_PREVIEW=1` in `target/live-preview`, so a change cannot break the live preview's build unnoticed.

A change to the root `Cargo.toml`, `.cargo/`, `.config/`, a toolchain file or an unknown root file selects the whole workspace. Documentation, `.claude/`, `.github/` and `prototypes/` select nothing unless a source names them. `test` runs `cargo nextest run` (`cargo test` when nextest is missing), `clippy` adds `--all-targets -- -D warnings`, and `check` adds `--all-targets`.

When the selection holds `rbs-app`, `test` builds the whole workspace and runs only the selected packages' tests (`cargo nextest run --workspace -E 'package(=a) | package(=b)'`). A `-p` subset resolves features over the subset alone, so it compiles third-party crates and about 46 workspace crates a second time with other features: `-p rbs-app` beside a workspace build took 248 s and 7 GB of disk, while the workspace build after a change in `rbs-app` takes about 70 s (4-core cloud container, 2026-11-17). A filter of the caller's own (`-- -E ...`) keeps `-p`. Every `cargo` that xtask starts drops the `CARGO_MANIFEST_DIR` and `CARGO_PKG_*` variables `cargo run` set for xtask; passed on, they made `ring`'s build script rerun, and every switch between `cargo xtask affected test` and a plain `cargo test` rebuilt `ring`, `rustls` and everything above them (about 200 s).

Before a pull request is pushed, `cargo fmt --all -- --check`, `cargo xtask affected clippy` and `cargo xtask affected test` are enough. The whole workspace runs once after a batch of merges, not before each one.

### UI live preview

A UI edit does not need a Rust build. `cargo xtask ui-dev` builds and runs `rbs` with the `ui-live-preview` feature (`rbs-app`, forwarding `rbs-app-ui`'s `live-preview`) and `SLINT_LIVE_PREVIEW=1`, in `target/live-preview` (under `CARGO_TARGET_DIR` when it is set) so that switching between it and a normal build never rebuilds either. The facade `rbs-app-ui` then re-exports `rbs-app-ui-single`, which compiles every window from `ui/roots/single.slint` as small wrappers, and the split crates skip their code generation (ADR-0138 decision 5; Slint's live-preview wrappers cannot import a Slint library). The app interprets `crates/rbs-app/ui` at run time. Saving a `.slint` file reloads the open windows and keeps the state the Rust side set. An editor that saves by writing a new file and renaming it over the old one may not trigger a reload; saving in place does. Arguments after `ui-dev` go to `cargo run` (`cargo xtask ui-dev --features demo-sources`).

Measured on a 4-core cloud container (2026-11-17): `rbs-app-ui` builds in 8 s instead of about 330 s, a cold `rbs` build takes 3 min 17 s instead of 8 min 26 s, and a saved file reloads in about 1.5 s. The Slint compiler is optimised in dev builds (`[profile.dev.package.i-slint-compiler]`); unoptimised, a reload took about 20 s, and the normal build's code generation in `rbs-app-ui`'s build script takes 19 s instead of 11 s.

The live preview is for looking at and adjusting the UI only. The interpreter is slower than the generated code, so performance measurements, tests and builds that ship use the normal build. A change made this way is still checked with `cargo xtask check-ui`, the UI review and the normal test run before it is pushed. `SLINT_LIVE_PREVIEW` without the feature fails in `rbs-app-ui`'s build script; the feature without the variable uses the split crates and only compiles the unused interpreter (`cargo doc --all-features`).

### UI crates (ADR-0138)

The Rust code Slint generates from `crates/rbs-app/ui` is split by window group: `rbs-app-ui-base` (shared globals, structs, enums and tokens, built as the Slint library `@rbsui`), `rbs-app-ui-main` (main and floating windows), `rbs-app-ui-settings`, `rbs-app-ui-dialogs` and `rbs-app-ui-probes` (test-only windows, built for `rbs-app`'s tests). `rbs-app` names only the facade `rbs-app-ui`. Each window group has its own root (`ui/roots/<group>.slint`) that imports its windows from their own files (`main_window.slint`, `settings_window.slint`, the dialog files, `overlay_probe.slint`), never through `app.slint`, so Cargo reruns only the groups whose files changed. The Settings pages live in `studio_settings.slint`, apart from the panels in `studio.slint`. A change to a file every group uses (`components.slint`, `settings.slint`, `base/`) still rebuilds every group. `cargo xtask check-ui` enforces the name and import rules of the split.

Measured on a 4-core / 15 GB cloud container (2026-11-17, `CARGO_BUILD_JOBS=4`): the UI crates build cold in about 146 s instead of 346 s; one changed text in a Settings page rebuilds only `rbs-app-ui-settings`, in 99 s with incremental compilation (138 s without) instead of 381 s; the largest rustc process peaks at 6.5 GB instead of 12.8 GB. Several groups peaking at once can still add up to about 16 GB, so use `CARGO_BUILD_JOBS=3` or less on a 16 GB machine.

### Cloud containers

Cloud sessions run on about 4 vCPUs, 16 GB of memory and 30 GB of disk. The cloud environment's setup script is a copy of `tools/cloud/setup.sh`: it installs the system packages the build needs (Vulkan through lavapipe, PipeWire, ALSA, D-Bus, xkbcommon, fontconfig, Wayland, udev, clang), `cargo-nextest`, and builds every third-party crate of the workspace into `rbs/target` with `cargo chef cook`, which compiles the dependencies against a stub of the workspace. The script takes about 3.5 minutes; the environment caches the file system when setup finishes in under about 5 minutes and rebuilds it about every 7 days or when the script changes, so a session starts with the dependencies already compiled. A `Cargo.lock` change since the cache only rebuilds the crates it changed.

The environment also sets `CARGO_INCREMENTAL=0` (the incremental directory alone reached 10 GB), `CARGO_BUILD_JOBS=3` (the UI crates can peak at about 16 GB together) and `CARGO_PROFILE_DEV_DEBUG=0` (no line tables in the cloud; panic messages still name the file and line). Local builds keep the profile in `Cargo.toml`.

Measured on a 4-core cloud container (main c4974cb, 2026-11-17, `cargo test --workspace --no-run`, `CARGO_BUILD_JOBS=3`, incremental off):

| Setup | Time | Disk | Peak memory |
| --- | --- | --- | --- |
| Cold, line tables (before) | 473 s | 17 GB | about 12 GB |
| Cold, `mold` linker | 484 s | 19 GB | about 12 GB |
| Cold, no debug info | 450 s | 12 GB | about 12.6 GB |
| Dependencies cooked by the setup script, no debug info | 284 s | 14 GB | about 12.7 GB |
| Then a change in `rbs-app` (`cargo xtask affected test -- --no-run`) | about 70 s | | about 3.8 GB |

The `mold` linker is not used: Rust links with `rust-lld` on this target already, and it was not faster. Of the 17 GB of the cold build, 11.7 GB are test executables (213 integration-test binaries); merging each crate's integration tests into one binary is the largest remaining saving. When the disk fills, `cargo clean` and the setup's cooked dependencies are gone too; `rm -rf target/debug/deps/*-????????????????` without an extension removes only the test executables, which the next run builds again.

## Counted UI checks (ADR-0115)

`cargo xtask check-ui` reads the `.slint` files under `crates/rbs-app/ui` as text, with comments and string contents removed, and counts over the whole tree:

| Check | Counts | Outside the library only |
| --- | --- | --- |
| `touch` | `TouchArea {` elements and components that `inherits TouchArea` | yes |
| `duplicate` | component declarations whose name is declared in more than one file | no |
| `help` | `help:` statements, up to their `;`, that hold a string literal over 90 characters | yes |

The library is the `library` list in `docs/ui/baseline.toml`. Each count must equal its number in that file plus the entries for that check in `docs/ui/exceptions.toml`; a count above or below fails. The unit test `the_tree_matches_the_baseline_and_the_register` in `xtask` runs the check, so `cargo test --workspace` includes it; it reads only the tree and takes milliseconds.

| Command | Does |
| --- | --- |
| `cargo xtask check-ui` | Fails unless every count matches |
| `cargo xtask check-ui --lower` | Writes lower counts down: register entries of that check first (oldest first), then the baseline; refuses to raise |
| `cargo xtask check-ui --except <check> <count> <file> <reason> <owner's approval> <date>` | Appends a register entry after the owner approved in writing; `<check>` is a check name or `library` for a new library file |
| `cargo xtask check-ui --against <commit>` | The pre-merge gate, run by the owner: also fails when a baseline number is above the one at `<commit>` or a library file was added without a `library` entry, and prints the register entries added since |
| `cargo xtask check-ui --hook pre\|post` | An agent's edit hook (`.claude/settings.json`): `pre` refuses a direct edit to the two files, `post` runs the check after an edit to a `.slint` file or `tokens.toml`; exit code 2 on refusal |

Neither file is edited by hand. The checks see only what they name; the `ui-review` skill lists what stays a review rule.

