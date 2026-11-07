---
name: ui-review
description: Checklist for any RBS change to user interface code, tokens, components, UI text or plugin UI. Use before finishing a UI change and when reviewing one.
---

# UI review

Rules: `docs/ui/principles.md` (U1-U21). Design system: `docs/specs/ui-design-system.md`. Decision: ADR-0015.

1. **Scope.** If the change alters a core flow (U1), adds a component or changes a token name, it is architecture scope: an independent review is required (`architecture-review`, ADR-0014).
2. **Tokens and components (U6, CLAUDE.md rule 15).** No hard-coded colours, sizes, spacing, padding, radii, line widths, font sizes or weights, or durations in new or edited `.slint` or Rust UI code: every value is a `Tokens.*` / `Theme.*` property (Rust: the resolved theme from `rbs-app` `look`). Only the literals in "Allowed literals" below. A missing token is added as in "Adding a token". Only library components; a new component is added to the library with all states documented.
3. **Disclosure (U2).** New options are placed at the right level (Simple, Advanced, Expert) with a default and a one-line explanation.
4. **Core tasks (U1, U3).** No core task gets more interactions than before. Direct manipulation still works.
5. **Live safety (U4).** Undo works for the new actions. Nothing restarts outputs silently.
6. **Responsiveness (U5).** No work over 8 ms on the UI thread; hidden panels do not render or poll.
7. **Accessibility (U7).** Keyboard-only walkthrough done; focus visible; accessible names set; colour is never the only signal; contrast checked; reduce-motion respected.
8. **Text (U8, U13).** All strings in Fluent files; glossary terms; sentence case; layout survives 40 % longer strings.
9. **Status and errors (U9).** New failure modes have a plain-language message with a fixing action.
10. **Panels and workspaces (U21).** A new panel has a stable id (with parameters if it can exist per canvas), works at any size, can be closed, floated, reopened and moved with the keyboard alone, and does nothing while hidden. Layout state stays in the UI crate and uses `ui.*` commands, never the engine command log. The live indicator keeps state, duration and outputs in every workspace.
11. **Streaming specifics (U15-U20).** Canvas being edited is clear; mixer, settings, notifications, projectors and control-surface rules hold; no automatic modal while live.
12. **Evidence in the PR.** Screenshots in dark, light and high-contrast themes; short screen recording for interaction changes; checklist answers (OK / Issue / N/A, one line each).
13. **Owner approval for visual changes (CLAUDE.md rule 16).** Before writing code, the owner saw a short proposal of what changes and how. After implementing, the owner saw the screenshots from item 12 (or tested locally) and approved them. CI and merge come only after that approval.

The author fills this in; a different reviewer verifies the screenshots and the keyboard walkthrough.

## Allowed literals

A literal is allowed only where it is not a visual design choice:

- `0px`, `0ms` and `0deg` (nothing), and `100%` where it means the full parent size;
- unit conversions such as `self.width / 1px` (turning a length into a number for Rust);
- a "no limit" sentinel such as `max-width: 100000px` in a conditional;
- colours and gradients that are content, not chrome: the colour picker's hue and saturation fields, a swatch showing a user's colour, a source's own pixels;
- generated files (`tokens.slint`, `tokens_apply.rs`) and the token file itself.

Anything else (`gap: 6px`, `letter-spacing: 0.5px`, `interval: 400ms` for a hover delay, `#808080`) needs a token. When unsure, add the token.

The test `no_hard_coded_visual_values` in `rbs-app` (`src/look/literals.rs`) reads every `.slint` file and fails on a literal length, duration or colour. Zero, `/ 1px` conversions and sentinels of 10000 px or more pass on their own. A content colour or other non-design value passes only with a reason the reviewer can judge: `// literal: <why>` at the end of its line or on the line above, or `// literal-begin: <why>` ... `// literal-end` around a block (preset colours). Never mark a design value to silence the test; add the token. The test does not read Rust: colours and sizes set from Rust are checked in review.

## Adding a token

If no existing token fits, add one in the same change. Never reuse a token of a different meaning because its value matches today; themes can change either one.

1. **Name it** by meaning, not value: lowercase words joined by `.` or `-` (`size.panel.gap`, not `size.6`). Check the [design system tables](../../../docs/specs/ui-design-system.md#tokens) first; a new token name is a contract (item 1: independent review if it is a new token group or changes a name).
2. **Add the value** to `crates/rbs-ui/tokens.toml` in its section: `[length]` (logical px), `[duration]` (ms), `[type]`, or for a colour in **each** of `[themes.dark]`, `[themes.light]` and `[themes.high-contrast]` (opaque `#rrggbb`, tuned per theme, not copied).
3. **Register it** in `crates/rbs-ui/src/tokens.rs`: `LENGTHS`, `DURATIONS`, `TYPE_STYLES` or `THEME_COLORS`. The loader fails on an unknown or missing token, so a step you skip shows up as a failing test.
4. **Decide if users may theme it** ([themes spec](../../../docs/specs/themes.md#themeable-tokens-rbs_uitokens)). If yes, add it to `THEMEABLE` in `crates/rbs-ui/src/tokens/theme.rs` with a label in words and bounds, and add the row to the themes spec table. If no (window sizes, keyboard steps), add it to the "Not themeable" list there.
5. **Colours only:** add a contrast rule for every pair the UI draws with it (`CONTRAST_RULES` / `THEME_CONTRAST_RULES`); the `every_drawn_pair_has_a_contrast_rule` test in `rbs-app` fails otherwise. It must also stay at least ΔE2000 15 from the live red.
6. **Document it** in the design system spec table ("Space, size, shape, motion" or "Colour"), in the same change.
7. **Use it**: `build.rs` generates the Slint property (dots and dashes become `-`, for example `size.settings.w` is `Tokens.size-settings-w`, `color.text.primary` is `Theme.text-primary`). Run `cargo test -p rbs-ui -p rbs-app`.
