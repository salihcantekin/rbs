# ADR-0144: Code quality rules and how they are enforced

- Status: Accepted
- Date: 2026-11-17
- Deciders: Salih (owner)
- Builds on: ADR-0014 (review policy), ADR-0032 (fast deterministic tests), ADR-0115 (UI stop rule, counted checks), ADR-0138 (UI crate split); companion to ADR-0143 (performance rules, PR #518, merged after this ADR)
- Spec: [testing strategy](../specs/testing-strategy.md); audit: [code quality audit 2026-11](../quality/audit-2026-11.md); plan: [quality plan 2026-11](../quality/plan-2026-11.md)

## Context

The [code quality audit of November 2026](../quality/audit-2026-11.md) measured the tree at `ab90f27`: 437,000 lines of Rust in 90 crates, written mostly by AI agents working in parallel. The rules in CLAUDE.md are good, but several of them are checked only when someone remembers to run a command, and some are not checked at all:

- **Size grows unchecked.** Three files carry most of the desktop app (`studio_host.rs` 16,731 lines with a 1,659-line function and a 97-field struct, `studio_ui.rs` 9,738, `main.rs` 6,367). They are edited by most feature PRs, so parallel work collides there.
- **Logic sits in the toolkit crate.** The studio host never names Slint, but its tests link Slint and Skia because it lives in `rbs-app`.
- **Two command paths.** The window and the control API have separate command enums and dispatchers (147 and 155 variants); principle U20 has no test.
- **No gate between CI runs.** CI runs by hand for now (it becomes automatic again later, owner 2026-11-17) and the toolchain floats on `stable`. Main at `ab90f27` contained merge-conflict markers in two docs, created by a local merge, and new stable lints have turned main red without a code change.
- **Copied test code.** The GPU-or-skip helper exists in 25 files; there are no property tests and no Miri or loom runs for hand-written lock-free code, although the testing strategy spec plans both.
- **Docs drift.** Three Proposed ADRs are implemented, ADR states disagree between files, and the largest specs are too big to read.

ADR-0143 sets rules for runtime cost. This ADR sets the matching rules for readability, change cost and enforcement, so that the structural changes of both audits are done once and stay done.

## Decision

### Governance

Owner approval in writing is needed for a **new** violation: a rise in a counted check, a new file or function over a size limit, a ceiling raised, a new `String` error in a library crate, a new window-only command. Code that already breaks a rule is tracked in the baselines and the audit; a PR that touches it does not need approval to leave it as it is, and the audit's plan brings it in line.

A **library crate** here is every crate whose `check-layers` class is not App or Tool (the classes of `rbs-app`, `rbs-cli`, `rbs-mcp` and `xtask`).

### Q1. Size ceilings only go down

- Limits: 2,000 lines per file and 200 lines per function. Counted: every `.rs` file of the workspace, `src/` and `tests/` alike; excluded: generated code and FFI bindings (`*/src/sys/*`), and `benches/`.
- `cargo xtask check-size` reads `docs/quality/size-baseline.toml`, which lists each existing file and function over a limit with its current length as its **ceiling**. A listed item may not grow beyond its ceiling; an unlisted item may not exceed the limit. Ceilings only fall: `--lower` rewrites them when items shrink and removes items that are back under the limit (a split lowers or removes an entry). Items are keyed by path and function name; `--lower --moved <old> <new>` carries a ceiling to a new path or name when the item does not grow, and `--against <base>` prints every carried ceiling. A move is not a new violation.
- The mechanism is the one of `check-ui` (ADR-0115 decisions 3, 4, 6 and 7): `docs/quality/size-exceptions.toml` is the register of approved rises, written with `--except` after the owner's written approval; `--against <base>` compares with the base commit; a unit test in `xtask` runs the check in `cargo test --workspace`; the edit hook refuses hand edits of both files; both files are in CODEOWNERS. Agents never edit them by hand.

### Q2. Logic belongs where it can be tested without a window (direction)

Composition roots (`rbs-app`, `rbs-cli`, `rbs-mcp`) should not gain new engine or control logic; code that does not draw goes into toolkit-free crates. Moving the existing studio host and sharing the adapter list are decided by their own ADRs (plan WP-09, WP-12).

### Q3. New operations reach the window through the API (direction)

New operations reachable from the window are added as `rbs_api::Command` first and called from the window through it. The U20 parity test (every UI command in the `rbs-ui` command registry maps to an API command, or is in a listed exceptions table with its reason) is added now and runs in the fast tier; adding to its exceptions table is a new violation. Moving existing window commands, and whether validation is shared, are decided in the ADR for one command path (plan WP-20).

### Q4. Typed errors in library crates

Library crates return typed errors; a `String` error is allowed only in App-class crates at the UI and CLI edge. Errors that cross the control API map to an `ApiError` kind by type, not by text.

### Q5. Tests share their support code and test the right risks

This implements the testing strategy spec (its table already plans property tests and Miri) within ADR-0032:

- Test helpers used by more than one test file live in a shared dev-only crate, not in copies.
- Tests of an adapter's behaviour live with the adapter, not in an engine crate's dev-dependencies.
- Pure logic that parses, converts, upgrades formats or undoes edits has property tests. They set a fixed RNG seed and a case count in `ProptestConfig` so each test runs in milliseconds, and turn file persistence off or commit their regression files.
- Hand-written lock-free or `unsafe` concurrency in a pure crate runs under Miri, and loom models (behind `cfg(loom)`) cover the mailbox and rings. Both run in tier T3 on a nightly toolchain pinned by date in `xtask`, not as `#[ignore = "slow"]` tests.
- Tools (screenshot writers, report generators) are examples or `xtask` commands, not ignored tests.
- A skipped hardware test is reported as skipped, and the per-change run requires the hardware the machine is known to have.

### Q6. Lints and toolchain

- The toolchain is pinned to an exact stable version and moved in its own PR that fixes the lints the new version reports. `rust-version` is either tested in T3 or set to the pinned version.
- A crate without `unsafe` has `#![deny(unsafe_code)]` in its crate root (`forbid` where the crate's macros allow it); crates use `[lints] workspace = true`, which cannot carry a per-crate entry.
- An exception to a lint uses `#[expect(lint, reason = "...")]` when the lint fires on every platform, otherwise `#[allow(lint, reason = "...")]` (`clippy::allow_attributes_without_reason`).
- Test code is exempt from `unwrap_used`, `expect_used` and `panic` through `clippy.toml` (keys checked against the pinned clippy); a crate-level allow with a reason remains only where integration-test helpers outside `#[test]` need it.
- Library crates do not print (`print_stdout`, `print_stderr`, `dbg_macro` warn); diagnostics go through the diagnostics facility once its ADR is accepted.

### Q7. One command, run on the merged tree

- `cargo xtask verify` runs the quick part (fmt, `check-layers`, `check-ui`, `check-size`, `check-docs`) and the affected part (clippy, tests and docs of the affected crates; `cargo deny` when `Cargo.lock` changed). `verify --against <base>` adds `check-ui --against <base>` and `check-size --against <base>`.
- Before main is pushed, the quick part and `--against` run **on the merge result**, after any `xtask bump` (Q9), by whoever performs the merge under ADR-0115 decision 7; this ADR does not change who merges. The affected part may run on the branch head. The result line in the PR is a record, not the gate.
- T1 (before a push) gets a time budget in the testing strategy spec (plan WP-08, follow-up); until then the affected part runs in full. Once the budget exists, a change whose affected part would exceed it (for example an `rbs-core` change that selects 69 packages) runs the quick part and the tests of the directly changed crates in T1, and the full reverse closure in T2 (after the batch of merges).
- When CI is automatic again, CI runs the same command. A changed file on a [hot path](../architecture/hot-paths.md) is named in the output with the P15 requirements.

### Q8. Documentation is checked like code

- `cargo xtask check-docs` (also a unit test in `xtask`, so it runs in `cargo test --workspace`) fails on: a broken relative link; a line beginning with `<<<<<<< ` or `>>>>>>> ` in a tracked text file outside a short allow-list of fixtures; an ADR missing from the index or with a different status there; a workspace crate missing from the layer table, or an absent crate in it; a hot path not covered by CODEOWNERS; an ADR whose `Status:` is Proposed and whose implementation state is anything other than `not started`. The link check is turned on after PR #518 (ADR-0143) has merged. An ADR without an entry in `docs/adr/implementation.toml` counts as `not started`; the Proposed-and-built check and a check that every entry names an existing ADR are turned on with that file (plan WP-18).
- The implementation state of every ADR (`not started`, `partial: <what is missing>`, `done`) lives in one file, `docs/adr/implementation.toml`, which the PR building it keeps current. Accepted ADRs stay unchanged (CLAUDE.md rule 9), and one file avoids conflicts across many headers.
- Every spec has an "Implementation status" section; `status.md` links to them instead of repeating them.
- Precondition: the owner decides on ADR-0103, ADR-0104 and ADR-0105 (Proposed but built) before the Proposed-and-built check is turned on.

### Q9. Shared numbers are assigned at merge

Feature branches do not edit the control API protocol number or the scene format number. A branch adds a change fragment (`docs/changes/<kind>/<slug>.md`); `cargo xtask bump` runs as part of the merge, before `verify` on the merged tree, takes the next number, updates the constant and appends the spec section. Branch tests do not assert a version number. The versioning rules in `docs/specs/control-api.md` and the scene-format spec are updated in the same change that introduces `bump`.

### Q10. Recurring procedures get skills; the audit repeats (guidance)

Procedures that recur (a new API method, a new setting, a new source type) get a skill. `cargo xtask quality-report` prints the audit's numbers; the audit is repeated after each large feature phase and compared with the last one.

### How the rules are checked

| Rule | Check | Where |
| --- | --- | --- |
| Governance | baselines, registers, review | `verify`, review |
| Q1 | `cargo xtask check-size` with baseline and register (new) | `xtask` unit test, `verify` |
| Q2 | review; `check-layers` classes | review |
| Q3 | U20 parity test with exceptions table (new) | fast tier |
| Q4 | review; `rbs-api` error mapping tests | review, fast tier |
| Q5 | shared test crate; skip report; T3 Miri and loom | tests, T3 |
| Q6 | `rust-toolchain.toml`, workspace lints, `clippy.toml`, `deny(unsafe_code)` | clippy in `verify` |
| Q7 | `cargo xtask verify` (new) on the merged tree | before main is pushed |
| Q8 | `cargo xtask check-docs` (new) | `xtask` unit test, `verify` |
| Q9 | `cargo xtask bump` (new) | merge |
| Q10 | skills; `cargo xtask quality-report` (new) | review, per phase |

## Consequences

- Rules stop depending on memory: the counted checks also run in `cargo test --workspace`, and the merge runs them on the tree that reaches main.
- Existing oversized files and other existing violations do not block feature work; they cannot get worse, and the plan removes them.
- The structural moves of the audit (studio host crate, shared adapter list, one command path, diagnostics) need their own ADRs and reviews; this ADR sets only their direction.
- Some work gets slower at first: splitting a large file before adding to it, writing a property test, assigning numbers at merge. The plan places these next to the performance waves so code is moved once.
- New `xtask` commands (`verify`, `check-size`, `check-docs`, `bump`, `quality-report`) and skills are follow-up work in the plan. A new `check-ui` count (unmarked visual literals) needs the owner's approval under ADR-0115.

## Alternatives considered

- **Turn CI triggers back on now, with a required status.** The owner keeps CI manual for now because of its cost and will make it automatic later. Agents also need the checks before they push, and the merge happens locally, so a local command is needed in any case; CI will run the same command.
- **Rely on CI alone when it returns.** A PR check does not see the merge result of a local merge, which is how the conflict markers of QD-01 reached main.
- **Run `check-docs` and `check-size` only through `verify`.** Rejected: like `check-ui` (ADR-0115 decision 7), they also run as `xtask` unit tests so that `cargo test --workspace` catches them when nobody calls `verify`.
- **Hard limits that fail on today's tree.** Would block every PR until the god files are split. Per-item ceilings stop growth now and let the splits follow the plan.
- **Count only how many files are over the limit.** Rejected after review: an oversized file could keep growing without changing the count.
- **Leave quality to review.** Reviews are AI sessions that read the diff; drift across files (ADR states, layer table, conflict markers) is what diff review misses.
- **Fold these rules into ADR-0143.** Performance and maintainability are checked by different checklists; two short ADRs share one plan.

## Review

Round 1. Reviewer: independent AI session (given only this ADR, the audit, the plan, the principles, ADR-0014, ADR-0032, ADR-0115, ADR-0138, CLAUDE.md, the testing strategy spec and the code). Date: 2026-11-17. Verdict: changes requested. Fact checks confirmed: file lengths, `ApiTarget::run` 1,659 lines, `Host` at line 3550, 147 and 155 command variants, ADR-0103 to 0105 Proposed, no proptest, quickcheck, loom, `#[expect]` or `forbid(unsafe_code)`, 25 copies of `fn gpu()`, 77 `RBS_REQUIRE_GPU` sites, floating toolchain, `workflow_dispatch` only, no `clippy.toml`, the QD-01 fix, the spec sizes. Corrections: 17 files over 2,000 lines when `tests/` counts (11 in `src/`); `rbs-api-ipc` declares `rust-version` 1.89.

Concerns and resolutions:
1. Q7 conflicted with ADR-0115 decision 7 (an agent's report is not the gate) and left out `check-ui --against`. -> Q7: the merger runs the check; the PR line is a record; `--against` added; ADR-0115 decision 7 applies unchanged.
2. Q7 checked the branch head, but the QD-01 markers came from a merge. -> Q7: the quick part runs on the merge result before main is pushed.
3. Q1 counted oversized files, so an oversized file could grow unchecked. -> Q1: per-item ceilings that only fall.
4. Q1 did not say whether `tests/` counts. -> Q1: `src/` and `tests/` count; the audit numbers are corrected.
5. Q1 had no approval mechanics. -> Q1 uses ADR-0115's baseline, register, `--lower`, `--except`, `--against`, hook and CODEOWNERS.
6. Q8's `Implementation:` line conflicted with CLAUDE.md rule 9. -> Round 1 amended rule 9; round 2 found the amendment missing from CLAUDE.md, so Q8 now keeps the state in `docs/adr/implementation.toml` and rule 9 is unchanged.
7. "A Proposed ADR described as built" could not be checked on free text. -> Q8 checks `Status:` against the implementation file; the owner's decision on ADR-0103 to 0105 is a precondition.
8. A conflict-marker check would match a Markdown `=======` underline. -> Q8 checks only lines beginning with `<<<<<<< ` or `>>>>>>> `, with a fixture allow-list.
9. `unsafe_code = "forbid"` per crate is impossible next to `[lints] workspace = true`, and `forbid` breaks some macros. -> Q6: `#![deny(unsafe_code)]` in the crate root, `forbid` where possible.
10. `#[expect]` is unfulfilled on platforms where a lint does not fire. -> Q6: `expect` only for lints on every platform, otherwise `allow` with a reason.
11. `clippy.toml` test keys do not cover integration-test helpers. -> Q6 keeps a crate-level allow with a reason where needed.
12. Miri needs nightly, contradicting the pinned stable and ADR-0032's slow tier; proptest is random by default. -> Q5: Miri and loom in T3 on a date-pinned nightly; fixed seed and case count; Q5 cites the testing strategy spec.
13. The governance sentence put an owner approval on almost every PR touching existing violations. -> New Governance section: approval only for new violations.
14. Q2 and Q3 decided what their own ADRs should decide. -> Both are now direction only; Q3 keeps the parity test with an exceptions table.
15. Q9 changed the versioning contract without a spec change, and `bump` ran after verify. -> Q9: spec rules updated in the same change; `bump` before verify on the merged tree; branch tests do not assert versions.
16. "Library crate" was undefined. -> Defined in Governance by `check-layers` class.
17. ADR-0143 is not on main yet. -> Header states that PR #518 merges after this ADR; the links resolve then.
18. `affected test` before every push could be slower than CI. -> Q7: a T1 time budget with a fallback to directly changed crates, full closure in T2.
19. Missing alternatives: CI triggers now; checks as `xtask` unit tests. -> Both added; the unit tests are adopted.
20. Q10's "more than two PRs" trigger could not be checked. -> Q10 is guidance, checked by review.

Round 2. Reviewer: the same independent AI session. Date: 2026-11-17. Verdict: changes requested. All 20 round-1 concerns resolved (concern 6 only in part, see N3); five new wording concerns:
- N1. Q7 let an agent merge, against ADR-0115 decision 7 ("Only the owner merges") while saying it applies unchanged. -> Q7 no longer says who merges: the checks run on the merge result by whoever merges under ADR-0115 decision 7.
- N2. Per-item ceilings would make a pure move or rename fail (the plan's first structural step is a move). -> Q1: items keyed by path and function name; `--lower --moved <old> <new>` carries a ceiling; `--against` prints carried ceilings; a move is not a new violation.
- N3. The rule 9 amendment was not in CLAUDE.md. -> Implementation state moved to `docs/adr/implementation.toml`; no rule 9 change.
- N4. Owner approval was recorded before an approving review. -> Recorded below, after the review rounds.
- N5. Q7 referred to a T1 budget the spec does not have; the link check would fail before #518 merges. -> Q7: the budget is follow-up (WP-08), the affected part runs in full until then; Q8: the link check is turned on after #518 merges.

Round 3. Reviewer: the same independent AI session. Date: 2026-11-17. Verdict: approve. N1 to N5 resolved. Two wording points folded in: an ADR without an entry in `docs/adr/implementation.toml` counts as `not started`, and the checks that need the file start with it (WP-18); the plan's WP-02 scope uses the same rule.

Owner approval: Salih, 2026-11-17: "adr ları kabul edelim" ("let's accept the ADRs"), given for acceptance once the independent review approves.
