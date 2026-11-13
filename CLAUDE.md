# CLAUDE.md

Instructions for AI coding agents working in the RBS repository. Read this file fully before any change.

## What RBS is

A headless live production engine in Rust (capture, compose, encode, stream, record) with swappable backends and a plugin system. Start with [docs/architecture/overview.md](docs/architecture/overview.md) and [docs/architecture/principles.md](docs/architecture/principles.md).

## Non-negotiable rules

1. **English only.** Code, identifiers, comments, docs, commit messages and PR text are in English, even when the conversation is in another language ([ADR-0011](docs/adr/0011-english-only-repository.md)).
2. **Spec first.** Before implementing a public API, port, node type or crate, find its spec in `docs/specs`. If it is missing or wrong, update the spec in the same change and say so. Do not invent contracts in code alone.
3. **Respect the layers.** Core and port crates never depend on adapters, vendor SDKs, FFmpeg, OS media APIs or UI toolkits (principle P1). Only the composition roots `rbs-app`, `rbs-cli` and `rbs-mcp` (planned, ADR-0052) wire adapters together.
4. **Capabilities, not vendors.** Never branch on vendor or device model names in engine code. Add a capability instead (P3).
5. **Real-time paths.** No allocation, blocking, I/O or contended locks in audio callbacks or `Node::process` (P6). Use pre-allocated buffers and lock-free queues.
6. **Zero-copy by default.** Pass GPU texture handles; never read frames back to the CPU on the default path (P4).
7. **`unsafe` discipline.** Only in adapter crates or marked low-level modules, every block with a `// SAFETY:` comment explaining the invariant. Run the `risky-review` skill.
8. **No panics across boundaries.** Port and plugin calls return typed errors. No `unwrap()`/`expect()` in library code except for proven invariants with a comment.
9. **Do not change accepted ADRs.** Write a new ADR that supersedes it (appending a `## Review` record is allowed).
10. **Zero cost when unused.** Design every feature with the Installed / Standby / Active lifecycle (P13, [ADR-0013](docs/adr/0013-zero-cost-when-unused.md)). Installed items add no threads, polling, GPU resources, device handles or per-frame work. Never add per-iteration checks in hot loops that test whether a feature is enabled; keep inactive nodes out of the scheduled graph instead.
11. **Never review your own architecture.** ADRs, port changes, public contract changes (plugin SDK, control API, scene format), crate layering changes and new core/port dependencies need an independent review: a human, or a fresh AI session that gets only the proposal, specs and code (P14, [ADR-0014](docs/adr/0014-review-policy.md), `architecture-review` skill). An ADR stays `Proposed` until that review is recorded and the owner approves.
12. **Hot paths need numbers and a human.** Code edited inside, or called directly from, the [hot paths](docs/architecture/hot-paths.md) needs before/after measurements, the `hot-path-review` checklist and a human maintainer's approval (P15). Your own checklist is never the only review.
13. **UI follows binding rules.** Any UI work follows [UI principles](docs/ui/principles.md) (U1-U21), uses only design tokens and library components from the [design system](docs/specs/ui-design-system.md), and uses [glossary](docs/ui/glossary.md) terms ([ADR-0015](docs/adr/0015-ui-design-rules.md)). Run the `ui-review` skill.
14. **Tests are fast and deterministic.** No flaky or hanging test: one that fails without a code change blocks all other work until its root cause is fixed; never retry, ignore or re-run to get green. Time-dependent logic uses an injected clock (`TestClock`), tests never sleep for timing, wait for conditions or tick indices instead of durations, and bound real time only as hang guards. Unit tests run in milliseconds; slow ones are `#[ignore = "slow: ..."]` and named `slow_*`. Run the tests of the crates you touch, not the whole suite each time ([ADR-0032](docs/adr/0032-fast-deterministic-tests.md), `test-reliability` skill).

15. **No hard-coded visual values in UI.** Every size, spacing, padding, margin, radius, line width, font size, font weight, duration and colour in new or edited UI comes from a design token (rule U6, [design system](docs/specs/ui-design-system.md#tokens), user themes in [ADR-0057](docs/adr/0057-user-themes-and-appearance-settings.md) and the [themes spec](docs/specs/themes.md)). In `.slint` files use `Tokens.*` and `Theme.*`; in Rust, read the resolved theme (`rbs-app` `look`), never a literal `px` value or `#rrggbb`. If no token fits, add one in the same change (how: `ui-review` skill, "Adding a token"); do not reuse an unrelated token because its value happens to match. The only literals allowed are listed in the `ui-review` skill.
16. **Visual UI changes are approved before merge.** Any change a user can see (layout, sizes, colours, icons, text placement, new or restyled windows) follows three steps: first propose what will change and how to the owner, then implement it and share screenshots in dark and light (also high-contrast when the change touches a surface or border token, an outline, or a branch on the high-contrast theme; [ADR-0115](docs/adr/0115-ui-stop-rule-for-agents.md)), or ask the owner to test locally, and start CI and merge only after the owner approves the result. Docs-only and non-visual changes skip this.
17. **Lean and beyond parity.** RBS must be easy on memory, CPU and GPU, and should exceed the features of OBS and similar tools, not only match them. Do not copy a reference tool's design by default: find what it cannot do or does badly because of its technology, and design for that when technically possible. Hot-path changes that serve this are welcome when done very well and measured (rules 10 and 12). Details: [vision](docs/vision.md#product-expectations-owner-2026-11-08).
18. **UI stop rule.** An AI agent whose change would break a rule of `docs/ui/principles.md`, the design system spec (`docs/specs/ui-design-system.md`) or an accepted UI ADR must stop and ask the owner in writing before making the change. It must not add an exception, raise a count or mark a literal to get past a check. ([ADR-0115](docs/adr/0115-ui-stop-rule-for-agents.md))
    - `cargo xtask check-ui` counts three things in `crates/rbs-app/ui` (raw `TouchArea` outside the library, component names declared in more than one file, `help:` texts over 90 characters). Each count must equal `docs/ui/baseline.toml` plus `docs/ui/exceptions.toml`. It runs in `cargo test --workspace`.
    - Never edit those two files by hand. When your change lowers a count, run `cargo xtask check-ui --lower`. When a count would rise, stop and ask the owner; only after written approval run `cargo xtask check-ui --except <check> <count> <file> <reason> <owner's approval> <date>`.
    - The checks see only what they count. Every other rule (tokens, library components, row anatomy, `Toggle` for a setting, `DialogActions` for window buttons, glossary terms) is checked with the UI review checklist.
    - Screenshots for the owner's approval of a visual change are dark and light; add high contrast when the change touches a surface or border token, an outline, or a branch on the high-contrast theme.

## Workspace layout

- `crates/rbs-core`: time and clocks, capability model, adapter registry. Foundation for everything.
- Planned crates and layers: see the table in [overview.md](docs/architecture/overview.md#layers).
- `docs/adr`: decisions. `docs/specs`: contracts. `docs/ui`: binding UI rules and glossary. `docs/roadmap.md`: phases.
- `.claude/skills`: repeatable procedures. Use them.

## Worktrees and `target/`

A git worktree is a second checkout. Cargo puts `target/` in that checkout unless `CARGO_TARGET_DIR` is set, so each worktree grows its own copy. One debug `target/` used to reach about 55 GB, and several worktrees together filled the disk.

- Before the first `cargo` command in a new worktree, set `CARGO_TARGET_DIR` to the primary checkout's `target` (`git worktree list` shows that checkout). Do not compile into the worktree's own `target/`.
- `[profile.dev]` keeps line tables only, and `[profile.dev.package."*"]` drops debug info for every dependency (`Cargo.toml`). Do not turn full debug info back on for dependencies. A crate you are stepping through may set its own `debug` in a package override.
- When the branch is merged or abandoned, remove the worktree with `git worktree remove`. Do not leave the directory, and do not delete the shared `target/` while another checkout is using it.

## Commands

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace            # or: cargo nextest run --workspace (CI uses nextest)
cargo doc --workspace --no-deps
cargo xtask check-ui              # after any change under crates/rbs-app/ui (rule 18)
```

A task is done when all of them pass, the acceptance criteria are met, and specs are in sync. Saying it works is not evidence; show the command output.

## Skills

| Skill | Use when |
| --- | --- |
| `add-backend` | Adding an adapter for any port (encoder, capture, transport, ...) |
| `add-graph-node` | Adding a source, filter, transition or other node type |
| `write-adr` | A decision is hard to reverse or crosses crates |
| `measure-performance` | Touching a hot path or claiming a performance change |
| `risky-review` | Before finishing any change with `unsafe`, FFI, GPU interop, threads or public API changes |
| `hot-path-review` | Any change in or called from a hot path |
| `architecture-review` | Reviewing someone else's ADR, port or contract change (never your own) |
| `ui-review` | Any change to UI code, tokens, components, UI text or plugin UI (includes adding a missing token and the approval steps) |
| `test-reliability` | Writing or changing tests, or a test that failed, hung or got slow |

## How to work

- Keep each change to one crate and one concern where possible. Prefer several small PRs over one large one.
- Read the relevant spec and ADRs before writing code; cite them in the PR.
- Write tests first for contracts: conformance tests for ports, property tests for pure logic.
- Use the controllable `TestClock` from `rbs-core` in tests; never sleep for timing. While iterating, run `cargo test -p <crate>` for the crates you change; run the whole workspace before you push.
- When a requirement is unclear or a spec seems wrong, stop and ask the human instead of guessing.
- Read [docs/status.md](docs/status.md) before trusting the roadmap or a "planned" line in an index. If a spec's implementation status and an index disagree, the spec section written beside the code wins, and the index is fixed in the same change.
- Do not implement a Proposed ADR. An ADR stays Proposed until an independent review is recorded and the owner approves (ADR-0014).
- The default encode path still reads frames back to the CPU (`YuvDownload`, ADR-0038). Do not add another per-frame CPU copy, and do not change that path outside ADR-0038's exit criteria. UI mutations go through `studio_host::Command` until they are also `rbs_api::Command`.
- Prefer well-maintained crates already listed in specs. Adding a new dependency to a core or port crate needs a justification in the PR.
