# ADR-0112: The Calm look, three densities, an experience level, one screen grammar and a stop rule for UI changes

- Status: Proposed
- Date: 2026-11-13
- Deciders: Salih (owner)
- Spec: [ui-design-system](../specs/ui-design-system.md), [themes](../specs/themes.md)

## Context

The owner asked (2026-11-13) for a look that leaves the OBS style behind, screens a user can read without learning each one, a product that stays usable when it holds many more options, a first experience that suits both an OBS user and a beginner, and rules that AI agents (Claude Code and Cursor) cannot break without asking him.

An audit of `crates/rbs-app/ui` at `8f50b35` found that the rules of [ADR-0076](0076-component-standards-and-panel-spacing.md), [ADR-0078](0078-saved-section-state-and-shared-section-components.md) and [ADR-0094](0094-ui-interaction-patterns-and-tabler-icon-set.md) are only partly applied:

- Three row layouts: `SettingRow` (label left, 76 uses), the private `Field` of `studio.slint` (label above) and the Source settings window. ADR-0094 C2 asks for one.
- Four ways to commit a change: at once (Settings, Properties), Cancel / Apply / OK (Source settings), Save / Cancel (theme editor), Replace / Cancel (Shortcuts). No rule exists.
- Seven private list rows (`SourceRowView`, `SceneRowView`, `FilterRowView`, `ThemeListRow`, `CommandRow`, `LibraryCell`, app menu `ItemRow`); 33 components outside the library use a raw `TouchArea`; `Hint` is defined three times, `Field` and `Problem` twice.
- `Checkbox` is used 8 times for a plain on/off (ADR-0094 A4 says `Toggle`); about 65 help texts are drawn inline (A1 says behind an `InfoHint`).
- `tests/ui_patterns.rs` checks popups and focus scopes only. Nothing checks components, rows or help text. The repository has no `AGENTS.md`, no Cursor rules and no hooks.
- "Add vertical output" exists only as a menu item and the owner could not find it: no rule says where a control must live.
- Rule U2 (three levels) and the first-run guide of U10 are not built.

A Slint-only study (`prototypes/ui-directions`) drew three directions in dark and light for the main window, Settings, first run and a component sheet, rendered by Slint 1.18 with the Skia renderer. The owner chose direction B with the row sizes of A and C as densities (decision card, 2026-11-13).

## Decision

### A. Look: Calm

1. The built-in Dark and Light themes take the Calm values of `prototypes/ui-directions/ui/look.slint` (direction 1). High contrast keeps its colours and takes the structure of items 2 to 5.
2. A docked panel and a Settings card are surfaces without a border, separated from the window by tone (`color.surface.base` against the window colour) and a gap; radius `radius.lg`. In the Light theme a card may carry one soft shadow (new token, at most 12 px blur). No other shadow, no gradient and no blur is added (U12 stands).
3. Fields, dropdowns and segmented tracks are filled with `color.surface.sunken` and have no border at rest; focus draws the `color.border.focus` ring. High contrast keeps a border.
4. Tabs are a segmented track, in panels and in the frame. The underlined `Tabs` bar is removed.
5. A section title is sentence case in `type.label` weight 600 with no rule line. This replaces the capital caption and rule of ADR-0076 decision 3; the rest of ADR-0076 stands.
6. The app ships one UI font, Inter (SIL OFL 1.1), as the first entry of `font.family`, so Windows and macOS render the same shapes and tabular figures (U6). The system font stays the fallback. *Owner to confirm.*

### B. Densities

1. `density` in `appearance.toml` gets a third value: `"roomy"`, `"regular"` (default) or `"compact"`. The appearance format becomes 4; format 3 reads as before.
2. Row height, control height and body size are: roomy 36 / 32 / 14 px, regular 32 / 28 / 13 px, compact 28 / 26 / 12.5 px. Roomy steps `space.*`, `size.control.*` and the panel gaps one token up, the way compact steps them down. `compact_steps` keeps its meaning.
3. Density changes lengths and type size only. It must not add, remove or reorder a control.

### C. Experience level

1. A user setting `experience` has three values: `simple`, `standard`, `pro`. It decides which settings are drawn: `simple` draws settings of level simple; `standard` adds advanced; `pro` adds expert (U2). A row above the user's level is not instantiated (rule 10).
2. First run asks one question with three cards, offers the OBS import and stream setup of U10 and a theme choice, and can be skipped (skip means `standard`). The choice sets, once, the starting workspace (Simple: Scenes, Sources, picture, Go live and Record; Standard: adds Mixer, Properties, Transition; Pro: adds studio mode, Outputs and Stats), the level and the density (simple: roomy, standard: regular, pro: compact).
3. Changing the level later (Settings > General, and a switch in each Settings page header) changes only which settings are drawn. It never moves panels or changes density; those stay the user's own.
4. A setting hidden by the level still applies its stored value, is still found by search (shown with its level), and is still a control API command (U20).

### D. Screen grammar

ADR-0094 A to E stand. Added or made exact:

1. **Row.** Every setting on every screen is the library `FormRow`: `[label] [hint] ... [control] [unit]`. Narrow panels may stack label above control through the component's own `stacked` variant, never by hand. Related numbers may share a row as paired fields with the name inside the field (X, Y, W, H).
2. **List row.** Every list uses the library `ListRow`: icon, name, note, trailing state icons; one hover, selection, focus and drag look. A state the user toggles per item (shown, locked, muted) is an icon on the row.
3. **Section header.** One component: chevron, title, changed mark, hint, reset, switch. The changed mark shows while a value in the section differs from its default; reset restores the section and is undoable.
4. **Applying changes.** A change applies at once and is undoable. A window has Cancel and a primary button only when applying reopens a device or restarts an output, and the window says so in a `StatusLine`. Apply / OK pairs must not be used.
5. **Dialog actions.** Library `DialogActions`: right-aligned, Cancel left of the primary, the primary named by its result ("Add source"), at most one `danger`.
6. **Alignment.** Controls in one column share a right edge; a panel uses only `size.panel.pad`, `size.field.gap` and `size.section.gap` between its parts; numbers are right-aligned with tabular figures; a panel never scrolls sideways (the Mixer strip row excepted, ADR-0101).
7. **One home per thing.** Anything that changes what goes out (an output, a canvas, an encoder, a destination) has a row in Settings and a registry command. A menu item or a toolbar button is a shortcut to it and must not be the only way.
8. **Find.** The frame shows a search field that runs registry commands and opens settings by name; Settings and Properties filter their own rows.

### E. Stop rule for agents

1. An AI agent whose change would break a rule in `docs/ui/principles.md`, the design system spec or this ADR must stop and ask the owner in writing before making it. It must not add an exception, raise an allow-list or mark a literal on its own.
2. `docs/ui/exceptions.toml` lists every approved exception: file, rule, reason, date, and the owner's approval. `tests/ui_patterns.rs` fails on a violation that has no entry.
3. `tests/ui_patterns.rs` gains checks, each with a list of existing places that may only shrink: a raw `TouchArea` or `FocusScope` outside `components.slint`; a component that redefines a library name; `Checkbox` outside a multi-select list; a `Text` of more than 90 characters outside `InfoHint` and `StatusLine`; a setting not built from `FormRow`; window buttons not built from `DialogActions`.
4. The same rules are written for both agents: `CLAUDE.md` and the `ui-review` skill; `AGENTS.md` and `.cursor/rules/ui.mdc`. Hooks (`.claude/settings.json`, and `.cursor/hooks.json` where Cursor supports the event) run the UI tests after an edit to a `.slint` or token file and refuse edits to the protected files: the exceptions register, the lists inside the tests, these rule files and the hooks.
5. The pull request template states whether the register changed. Only the owner merges.
6. Screenshots for the owner's approval (rule 16) are dark and light. High contrast is covered by the contrast tests and a screenshot only when its structure differs.

## Consequences

- Every screen changes its look once (token values and a few component shapes). The components stay; user themes keep working because the change is token values plus the structure of A2 to A5.
- Three densities and three levels multiply the states to test. The component sheet is rendered in each density by a test, and level filtering is tested on the settings schema, not per screen.
- Migration is one screen per pull request, each with dark and light screenshots: components (`FormRow`, `ListRow`, `DialogActions`, section header), Settings, Properties, Sources and Scenes, Mixer, Source settings, the remaining windows. `studio.slint` (5,982 lines) is where feature work also lands, so each step must be small.
- Follow-up in the accepting change: design system spec (tokens, component contracts, "Screen grammar"), themes spec (format 4, `roomy`), `principles.md` (U2, U10, U17 wording), `CLAUDE.md` rules 13, 15, 16, the `ui-review` skill, `docs/status.md`.
- Inter adds about 0.9 MB to the installer and a licence notice.
- A hook cannot prove that a person approved something. The register in the pull request and the owner's merge are the gate; the earlier layers bring the question forward.
- Cursor's hook events must be checked against its current documentation before E4 is promised there; if an event is missing, the test and the merge gate still hold.

## Alternatives considered

- **Direction A (Console), today's look refined.** Most rows per screen and least work, but borders multiply as panels do and it looks least new. Its sizes survive as the compact density.
- **Direction C (Stage), large controls everywhere.** Easiest for a beginner, about a fifth fewer rows per panel, against the compact UI rule. Its sizes survive as the roomy density.
- **Three separate looks chosen at first run.** Three sets of structure to test and document; density gives the same benefit with one set.
- **Level as a separate "Simple app".** Two products to maintain; Logic Pro's single switch shows one product can serve both.
- **Warnings instead of a stop rule.** The audit shows accepted rules drift when nothing stops a change.
- **A new toolkit or web UI for a richer look.** Rejected by ADR-0016 and ADR-0022; everything chosen here renders in Slint 1.18.

## Review

Independent review per ADR-0014.

Round 1. Reviewer: independent AI session. Date: 2026-11-13. Verdict: changes requested. All concerns open; this text is not yet revised.

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
