# ADR-0144: Code quality rules and how they are enforced

- Status: Proposed
- Date: 2026-11-17
- Deciders: Salih (owner); independent review pending (ADR-0014)
- Builds on: ADR-0014 (review policy), ADR-0032 (fast deterministic tests), ADR-0115 (UI stop rule, counted checks), ADR-0138 (UI crate split); companion to ADR-0143 (performance rules)

## Context

The [code quality audit of November 2026](../quality/audit-2026-11.md) measured the tree at `ab90f27`: 437,000 lines of Rust in 90 crates, written mostly by AI agents working in parallel. The rules in CLAUDE.md are good, but several of them are checked only when someone remembers to run a command, and some are not checked at all:

- **Size grows unchecked.** Three files carry most of the desktop app (`studio_host.rs` 16,732 lines with a 1,659-line function and a 97-field struct, `studio_ui.rs` 9,739, `main.rs` 6,368). They are edited by most feature PRs, so parallel work collides there.
- **Logic sits in the toolkit crate.** The studio host never names Slint, but its tests link Slint and Skia because it lives in `rbs-app`.
- **Two command paths.** The window and the control API have separate command enums and dispatchers (147 and 155 variants); principle U20 has no test.
- **No gate between CI runs.** CI runs by hand for now (it becomes automatic again later) and the toolchain floats on `stable`. Main at `ab90f27` contains merge-conflict markers in two docs, and new stable lints have turned main red without a code change.
- **Copied test code.** The GPU-or-skip helper exists in 20+ files; there are no property tests and no Miri or loom runs for hand-written lock-free code.
- **Docs drift.** Three Proposed ADRs are implemented, ADR states disagree between files, and the largest specs are too big to read.

ADR-0143 sets rules for runtime cost. This ADR sets the matching rules for readability, change cost and enforcement, so that the structural changes of both audits are done once and stay done.

## Decision

These rules apply to every change. A change that breaks one says why in its PR, and the owner approves it in writing (as for `check-ui`, ADR-0115).

### Q1. Size limits only go down

No new file over 2,000 lines and no new function over 200 lines, excluding generated code and FFI bindings (`*/src/sys/*`). `cargo xtask check-size` counts both against a baseline; the counts may fall (`--lower`) and rise only with the owner's written approval. Existing oversized files are split by the audit's plan; a PR that edits one does not make it larger without a reason.

### Q2. Logic lives where it can be tested without a window

Code that does not draw (control thread, settings files, planning) lives in toolkit-free crates and is tested there. Composition roots (`rbs-app`, `rbs-cli`, `rbs-mcp`) wire adapters and bind the UI; they do not hold engine or control logic. Each root gets its adapter list from one shared place.

### Q3. One command path per operation

Every operation the control API has is reached from the window through the same command and validation. A window-only command is listed in a test with its reason. The U20 parity test (every UI command maps to an API command or is listed) is part of the fast tier.

### Q4. Typed errors in library crates

Library crates return typed errors; a `String` error is allowed only at the UI and CLI edge. Errors that cross the control API map to an `ApiError` kind by type, not by text.

### Q5. Tests share their support code and test the right risks

- Test helpers used by more than one test file live in a shared dev-only crate, not in copies.
- Tests of an adapter's behaviour live with the adapter, not in an engine crate's dev-dependencies.
- Pure logic that parses, converts, upgrades formats or undoes edits has property tests (fixed seed, millisecond cases, ADR-0032).
- Hand-written lock-free or `unsafe` concurrency in a pure crate runs under Miri or loom in the slow tier.
- Tools (screenshot writers, report generators) are examples or `xtask` commands, not ignored tests.
- A skipped hardware test is reported as skipped, and the per-change run requires the hardware the machine is known to have.

### Q6. Lints and toolchain

- The toolchain is pinned to an exact version and moved in its own PR that fixes new lints.
- A crate without `unsafe` forbids it (`unsafe_code = "forbid"`).
- An exception to a lint uses `#[expect(lint, reason = "...")]`; `#[allow]` needs a reason (`clippy::allow_attributes_without_reason`).
- Test code is exempt from `unwrap_used`, `expect_used` and `panic` through `clippy.toml`, not per-module allows.
- Library crates do not print (`print_stdout`, `print_stderr`, `dbg_macro` warn); diagnostics go through the diagnostics facility once its ADR is accepted.

### Q7. One gate before every merge

`cargo xtask verify` runs format, `check-layers`, `check-ui`, `check-size`, `check-docs`, and clippy, tests and docs of the affected crates (`cargo deny` when the lock file changed). It prints one result line with the commit hash and the selected crates; the PR body carries that line, and the merger checks it matches the head commit. When CI is turned on again, CI runs the same command. A hot-path file in the change is named in the output with the P15 requirements.

### Q8. Documentation is checked like code

`cargo xtask check-docs` fails on: a broken relative link; a merge-conflict marker anywhere in the tree; an ADR missing from the index or with a different status there; a workspace crate missing from the layer table, or an absent crate in it; a hot path not covered by CODEOWNERS; a Proposed ADR described as built. Every ADR header has an `Implementation:` line (`not started`, `partial: <what is missing>`, `done`) that the PR building it keeps current; every spec has an "Implementation status" section; `status.md` links to them instead of repeating them.

### Q9. Shared numbers are assigned at merge

API protocol and scene format numbers are not edited by feature branches. A branch adds a change fragment (`docs/changes/<kind>/<slug>.md`); the merger runs `cargo xtask bump`, which takes the next number, updates the constant and appends the spec section.

### Q10. Repeated procedures become skills, and the audit repeats

A procedure that more than two PRs repeat by hand (a new API method, a new setting, a new source type) gets a skill. `cargo xtask quality-report` prints the audit's numbers; the audit is repeated after each large feature phase and compared with the last one.

### How the rules are checked

| Rule | Check | Where |
| --- | --- | --- |
| Q1 | `cargo xtask check-size` (new) | `verify` |
| Q2 | `check-layers` classes; review; `check-size` | `verify`, review |
| Q3 | U20 parity test | fast tier |
| Q4 | review; `rbs-api` error mapping tests | review, fast tier |
| Q5 | `rbs-test-support`; slow tier Miri and loom; skip report | tests |
| Q6 | `rust-toolchain.toml`, workspace lints, `clippy.toml` | clippy in `verify` |
| Q7 | `cargo xtask verify` (new), opt-in pre-push hook | before merge |
| Q8 | `cargo xtask check-docs` (new) | `verify` |
| Q9 | `cargo xtask bump` (new) | merge |
| Q10 | skills; `cargo xtask quality-report` (new) | per phase |

## Consequences

- Rules stop depending on memory: one command shows they hold, and the PR records it.
- The structural moves of the audit (studio host crate, file splits, one command path) need their own ADRs and reviews where they change layering or contracts; this ADR sets the direction, not those designs.
- Some work gets slower at first: splitting a large file before adding to it, writing a property test, assigning numbers at merge. The audit's plan places these next to the performance waves so code is moved once.
- New `xtask` commands (`verify`, `check-size`, `check-docs`, `bump`, `quality-report`) and skills (`add-api-method`, `add-setting`, `add-source-type`) are follow-up work once this ADR is accepted. Adding a `check-ui` count (unmarked visual literals) needs the owner's approval under ADR-0115.
- Existing code does not follow every rule yet. A change to such code fixes the rule it touches or says why not.

## Alternatives considered

- **Rely on CI alone.** CI runs by hand for now and becomes automatic again later (owner, 2026-11-17). Agents still need the same checks before they push, and one command that CI also runs keeps local and CI results identical.
- **Hard limits that fail on today's tree.** Would block every PR until the god files are split. A ratchet stops growth now and lets the splits follow the plan.
- **Leave quality to review.** Reviews are AI sessions that read only the diff; drift across files (ADR states, layer table, conflict markers) is exactly what diff review misses.
- **Fold these rules into ADR-0143.** Performance and maintainability are reviewed by different checklists and owners of the checks; keeping two ADRs keeps each one short. They share the audit plan.

## Review

Pending: needs an independent review (a human, or a fresh AI session given only this ADR, the audit and the specs; ADR-0014) and the owner's approval before it becomes Accepted.
