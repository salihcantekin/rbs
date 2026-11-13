# ADR-0115: A stop rule for UI changes by AI agents, with counted checks and an exceptions register

- Status: Proposed
- Date: 2026-11-13
- Deciders: Salih (owner)
- Amends: `CLAUDE.md` rule 16 and the Migration paragraph of [ADR-0094](0094-ui-interaction-patterns-and-tabler-icon-set.md) (which themes are shown for approval)
- Spec: [ui-design-system](../specs/ui-design-system.md), [testing-strategy](../specs/testing-strategy.md)

## Context

The owner works with two kinds of AI agents, Claude Code and Cursor, and asked (2026-11-13) that a change which breaks a UI rule stops and asks him, with exceptions possible only by his approval.

Today two tests of `rbs-app` guard the UI: `no_hard_coded_visual_values` and `every_drawn_pair_has_a_contrast_rule`; and `tests/ui_patterns.rs` holds two checks (no new `PopupWindow`, focus scopes). The other checks ADR-0094 planned were not built. The accepted rules have drifted (see the Context of ADR-0114). The repository has no `AGENTS.md`, no `.cursor` folder, no `.claude/settings.json` and no git hooks; an agent that does not read `CLAUDE.md` meets no UI rule. `.github/CODEOWNERS` does not cover `crates/rbs-app/ui/`, `docs/ui/`, `CLAUDE.md` or the agent folders. GitHub CI triggers are off; tests run locally before a merge, and only the owner merges.

## Decision

1. **The rule.** An AI agent whose change would break a rule of `docs/ui/principles.md`, the design system spec or an accepted UI ADR must stop and ask the owner in writing before making the change. It must not add an exception, raise a count or mark a literal to get past a check.
2. **Counted checks.** A new command `cargo xtask check-ui` reads the `.slint` files as text (no UI build, well under a second) and counts, per file:
   - `touch`: `TouchArea {` outside `components.slint`;
   - `private`: non-exported `component` declarations outside `components.slint`;
   - `shadowed`: a `component` whose name is exported by `components.slint`;
   - `help`: a string literal longer than 90 characters given to a `help`, `description`, `note` or `text` property outside `InfoHint`, `StatusLine` and tooltips;
   - `checkbox`: `Checkbox {` that is a child of a `SettingRow`;
   - `cancel`: `@tr("Cancel")` outside `components.slint` (window buttons belong to `DialogActions`).
3. **Baseline.** `docs/ui/baseline.toml` holds the allowed count per file and check, written once from the tree at acceptance. `check-ui` fails when a count is above its baseline. `cargo xtask check-ui --lower` rewrites the baseline with the current counts where they are lower and refuses to raise any; a migration runs it, so the baseline only shrinks. `shadowed` starts with the existing copies and ends at zero; `touch` and `private` end at the count of legitimately special parts (splitters, meters, the picture).
4. **Exceptions register.** `docs/ui/exceptions.toml` lists each approved increase or broken rule: file, check or rule id, count, reason, date, and a link to the owner's message that approved it. `check-ui` adds a file's register entries to its baseline. The register is the only way a count can rise.
5. **The same rules for both agents.** `CLAUDE.md` and the `ui-review` skill; `AGENTS.md` at the root and `.cursor/rules/ui.mdc` scoped to `crates/rbs-app/ui/**`, `crates/rbs-ui/tokens.toml` and `docs/ui/**`, with a Cursor command that carries the `ui-review` checklist. All four state decision 1 in the same words and point to the same spec.
6. **Hooks, as early warning.** A Claude Code hook runs `check-ui` after an edit to a `.slint` or token file and refuses an edit to `docs/ui/exceptions.toml`, telling the agent to ask the owner. The same is set up for Cursor where its hook events allow it; if they do not, layers 2 to 4 and 7 still hold. Hooks are not the gate: a shell write goes round them.
7. **The gate.** `check-ui` runs in `cargo test --workspace` (a test in `xtask` calls it) and in the pre-merge local run. It also compares `docs/ui/exceptions.toml` with the merge base and prints every added entry; the pull request template has a line "UI exceptions added: none | list". `CODEOWNERS` gains `/crates/rbs-app/ui/`, `/crates/rbs-ui/tokens.toml`, `/docs/ui/`, `/CLAUDE.md`, `/AGENTS.md`, `/.claude/`, `/.cursor/` and `/xtask/`. Only the owner merges.
8. **Screenshots for approval** (rule 16) are dark and light. High contrast is added when the change touches a surface or border token, an outline, or a branch on the high-contrast theme.

## Consequences

- A migration lowers counts with one command and never edits a guarded file by hand, so the guard does not block the work it exists to protect.
- A new private component or raw `TouchArea` in a feature needs the owner's approval once, through the register. That is the intended stop; it costs a question per new special part.
- The checks count text, so they can be wrong in both directions (a `TouchArea` in a comment, a long text built from two literals). They catch drift, not intent; review with the `ui-review` skill stays.
- Nothing proves that a person wrote a register entry. The entry is visible in the diff and in the pull request line, and the owner merges; the link to his message makes a false entry easy to see.
- Cursor's hook events must be read from its current documentation before decision 6 is promised there.
- Follow-up in the implementing change: `xtask` command and test, baseline and register files, `CLAUDE.md` rules 13, 15 and 16, `AGENTS.md`, the Cursor rule and command, hooks, `CODEOWNERS`, pull request template, `ui-review` skill, testing-strategy spec.
- Reversal: delete the command, the two files and the hooks; no product code depends on them.

## Alternatives considered

- **Gate only, no hooks or rule files:** the agent learns of a violation at the end of its work instead of at the edit.
- **Hooks that refuse edits to the baseline and rule files:** every migration must change the baseline, so agents could never finish one; replaced by the lower-only command.
- **More checks in `tests/ui_patterns.rs`:** that test builds the whole Slint UI; a text check should not wait for it.
- **Warnings instead of a stop:** the audit shows accepted rules drift when nothing stops a change.
- **Semantic checks through the Slint compiler's syntax tree:** more exact, more work; possible later behind the same command and files.

## Review

Independent review per ADR-0014.

This record covers ADR-0112 to ADR-0115, which were one text in round 1. The texts were split and rewritten after it; where each concern is answered is noted in the Review section of each ADR. Concerns 1 (e, f), 10 (checks), 11 and 15 (d) are answered in this ADR.

Round 1. Reviewer: independent AI session. Date: 2026-11-13. Verdict: changes requested. Concerns as given:

1. Changes to accepted decisions are not declared: ADR-0077 decision 6 (Apply / OK in Source settings) and the theme editor's Save / Cancel (ADR-0057 decision 6, ADR-0072 decision 5) against D4; ADR-0094 A3 (feature section order) against D3; ADR-0094 C2 against D1's stacked variant and paired fields; U2 and ADR-0094 A3 (Advanced in place) against the global level of C1; CLAUDE.md rule 16 and ADR-0094 Migration (three themes) against E6. Each needs an explicit "amends" line with the new wording.
2. A4 and A5 describe as new what the code already has: `Tabs` is already a sunken pill (`components.slint` 1353-1364) and `SectionHeader` already has no rule. A4 must keep the `Tabs` component and its role, and say what happens to `size.tab.indicator`.
3. A1 is not a token table. The study has `window`, `page`, `card` and `hover` colours that no token holds, and no values for several existing tokens. Its `border-strong` (1.45:1 dark, 1.53:1 light) fails the 3:1 contrast rules in `tokens.rs`; its dark live red is not the fixed `#c4262e`; its indigo accent sits close to the error purple and the brand colour.
4. Borderless fields and panels have no accessibility argument (U7, 3:1): field on panel is 1.07:1 dark and 1.13:1 light. The fate of button outlines, switch tracks and checkboxes is unspecified.
5. High contrast is contradictory: its surfaces are all black, so panels cannot be separated by tone, and E6 drops its screenshot for a migration that changes its structure everywhere.
6. "User themes keep working" is asserted, not shown: a theme whose sunken colour equals its panel colour gets invisible fields with no warning.
7. Density numbers do not fit the mechanism: text size is an integer (12.5 px cannot be stored), `apply_density` does not touch type styles, compact today gives a 24 px control (not 26), no row-height token exists, roomy has no step above the largest token, and results for user themes are undefined.
8. The experience level relies on a settings schema with levels that does not exist; storage, the control API command and the scope (Properties, Mixer) are not stated.
9. Hiding by level can strand a user (imported non-default advanced values act unseen), and the Simple starting workspace has no Mixer, Properties or Transition, which breaks U1 and U16 for the first-time streamer.
10. Several `ui_patterns` checks are not definable over `.slint` text ("a setting", "window buttons", "multi-select list", help passed as a property). The raw `TouchArea` list will never reach zero and its relation to the exceptions register is undefined. `FormRow`, `ListRow` and `DialogActions` do not exist and their relation to `SettingRow` and `PropertyRow` is not stated.
11. Protected files deadlock the migration: every migration must lower a list inside a protected test. Hooks miss shell writes; CODEOWNERS does not cover the UI and rule paths; the text checks should not need a UI build.
12. D4 contradicts D5; "undoable" is unproven for all settings; the changed mark needs defaults (the missing schema); A5 needs a new type token; A2 "no other shadow" contradicts the study's shadows on the slider knob and the chosen segment; A6 is undecided inside a decision.
13. Context: "`Checkbox` is used 8 times for a plain on/off" is overstated (at most 5 or 6 are plain on/off); "about 65 help texts" has no stated method. The other counts are correct.
14. Five decisions with different reversal costs in one ADR. Split into look and densities; experience level and first run; screen grammar; stop rule.
15. Missing alternatives: a resting hairline on fields; Advanced in place as U2 defines it; a wider `compact_steps` scale; enforcement in CI only; finishing the migrations of ADR-0076, 0078 and 0094 first.
