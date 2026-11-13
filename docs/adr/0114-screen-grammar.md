# ADR-0114: One screen grammar: row, list row, section marks, held edits, one home

- Status: Accepted
- Date: 2026-11-13
- Deciders: Salih (owner)
- Amends: [ADR-0094](0094-ui-interaction-patterns-and-tabler-icon-set.md) rule C1 (`Field` is not moved to the library; the stacked `SettingRow` replaces it), rule C2 (two named row variants), rule A3 (two optional header parts) and its Migration steps 2 and 3 (replaced by the list in Consequences; step 5 and the Enforcement checks stand); [ADR-0076](0076-component-standards-and-panel-spacing.md) decision 1 (the row's name is `SettingRow`; `PropertyRow` was never built and the name is retired) and decision 3 (the same two header parts)
- Spec: [ui-design-system](../specs/ui-design-system.md#component-standards)

## Context


The owner wants a user who learns one screen to know every screen (2026-11-13). ADR-0076, ADR-0078 and ADR-0094 decided most of the grammar; an audit of `crates/rbs-app/ui` at `8f50b35` shows the migration stopped part-way and four things have no rule:

- Rows: `SettingRow` (76 uses, still in `settings.slint` although ADR-0094 moves it to the library), the private `Field` of `studio.slint` and `appearance.slint` (label above), and the Source settings window.
- Lists: seven private row components (`SourceRowView`, `SceneRowView`, `FilterRowView`, `ThemeListRow`, `CommandRow`, `LibraryCell`, app menu `ItemRow`), each with its own hover, selection and focus drawing.
- Committing: Settings and Properties apply at once; Source settings holds edits behind Apply / OK / Cancel (ADR-0077 decision 6); the theme editor has Save / Cancel (ADR-0057, ADR-0072); the shortcut recorder Replace / Cancel. All four are decided somewhere, but no rule tells the author of a new window which to use.
- Place: "Add vertical output" exists only as a menu item and the owner could not find it.
- Leftovers of ADR-0094: `Hint` is defined three times, `Problem` twice; `help:` is set 77 times as inline text against 13 `InfoHint` uses; of 8 `Checkbox` instances, 5 or 6 are a plain on/off setting.

## Decision

ADR-0076, 0078 and 0094 stand. Their unfinished migrations are finished as part of this one. Added:

1. **The row is `SettingRow`**, moved to `components.slint` as ADR-0094 ordered, used in Settings, Properties, Source settings and the Mixer filters. Its anatomy is ADR-0094 C2. Two variants are added to C2 and are the only other row shapes:
   - **Stacked** (label above the control). The row stacks itself when it is narrower than a new length token `size.row.stack-below` (280); the author never chooses. In the side-label shape the control column is the smaller of its preferred width and half the row, but never below the control's minimum width (C2, U11); a row whose control would not fit stacks. A 320 px Properties panel so keeps side labels for ordinary controls. Slint cannot put `@children` under an `if`, so the row is one component that places its one control box by geometry; the spike `prototypes/ui-directions/spike/row.slint` renders both shapes this way in Slint 1.18 with no binding loop.
   - **`PairRow`**, two number fields side by side with the name inside each field (X and Y, W and H).

   The private `Field` components are removed.
2. **The list row is `ListRow`**, a new library component. Icon, name and note are properties; its `@children` are the trailing content (state icons and a `more` button). It owns hover, selection, focus ring, drag feedback and the context-menu key. A state the user toggles per item (shown, locked, muted, enabled) is an icon or a `Checkbox` in the trailing content. The seven private rows move onto it.
3. **`Checkbox`** is for choosing several items of a set and for a per-item state in a list row. A setting in a `SettingRow` is a `Toggle` (ADR-0094 A4).
4. **Section marks.** `SectionHeader` and `FeatureSection` keep the order of their parts (ADR-0094 A3, ADR-0076 decision 3). Two optional parts are added: a changed mark directly after the title, and a reset button at the trailing end of the header, before the info hint where the header has one. A page shows them only for a section whose defaults it knows; reset is one undo step.
5. **Three ways to commit, each named.**
   - *At once* is the rule (U17).
   - *Held edits*: a window may hold edits behind buttons only when applying reopens a device or restarts an output, or when it creates or saves a named thing (a source, a theme, a shortcut). The spec lists each such window; today: Source settings (ADR-0077), the theme editor (ADR-0057), the shortcut recorder, and the add and name prompts. A new one is added to the list in the same change.
   - *Confirmation*: a question before an act that removes something or cannot be undone, in a window or in place (delete source, remove global source, make a new key, delete theme, "Keep this look?", end stream). It has the act and a way back, nothing else.
6. **`DialogActions`**, a new library component, draws the buttons of every held-edit window and every confirmation: right-aligned, the way back (Cancel, or words such as "Keep streaming") to the left of the primary, at most one `danger`, the primary named by its result ("Add source", "Save theme"). Existing accepted labels (Apply and OK in Source settings) stay until their own ADR changes; new windows do not use "OK". An inline rename is not a confirmation: Escape and its cancel icon stay as they are.
7. **Alignment.** Controls in one column share a right edge; a panel uses only `size.panel.pad`, `size.field.gap` and `size.section.gap` between its parts; numbers are right-aligned in tabular figures; a panel does not scroll sideways (the Mixer strip row excepted, ADR-0101).
8. **One home per thing.** Anything that changes what goes out (an output, a canvas, an encoder, a destination) has a row or card in Settings and a registry command. A menu item or toolbar button is a shortcut to it and must not be the only way to reach it.

Find is not decided here. The RBS menu search (ADR-0069) and the page filter of ADR-0094 C3 stay as they are; one search that also opens settings by name is a later decision.

## Consequences

- New library components: `ListRow`, `DialogActions`, `PairRow`; `SettingRow` moves and gains the stacked shape; one new length token. Each component gets the spec's contract (states, keyboard, accessible names).
- Migration is one screen per pull request with the screenshots rule 16 asks for: the components, Settings, Properties, Sources and Scenes, Mixer, Source settings, the remaining windows. These steps replace steps 2 and 3 of ADR-0094's Migration. Its step 5 and its Enforcement checks (the `ui_patterns` test for long text in a `Text` and icon-only buttons without a tooltip) stand as planned. `studio.slint` (5,982 lines) is where feature work also lands, so each step stays small.
- Checking: all eight rules are review rules in the `ui-review` skill. Where ADR-0115 is accepted, its `touch` and `duplicate` counts also guard rules 1 and 2, and its `help` count guards ADR-0094's inline help. Rule 8 can later become a registry test. This ADR does not depend on ADR-0115.
- Reversal: the components can stay even if a rule is withdrawn.

## Alternatives considered

- **Finish ADR-0094 only, add nothing.** Leaves lists, held edits and place without a rule, which is where the owner's findability problem came from.
- **A new `FormRow` beside `SettingRow`:** two rows again; the existing one already has the anatomy.
- **Two row components chosen by the author or the container:** the author chooses again, which is how three row shapes came to exist. The spike shows one component can do both.
- **Forbid held edits everywhere:** contradicts ADR-0077 decision 6, where one apply is one device restart and one undo step.
- **Label always above the control** (as Properties does today): costs a line per setting, against the compact UI rule.
- **A search field in the window frame** (round 2 text): a second search beside the RBS menu search of ADR-0069, undeclared; left for its own decision.

## Review

Independent review per ADR-0014. Round 1 (2026-11-13, independent AI session, changes requested) reviewed a larger text; its concerns 1 (a to d), 10 (components), 12 (D4 against D5) and 13 are answered here. The full round 1 record is in ADR-0115.

Round 2 (2026-11-13, independent AI session, changes requested on all four texts). The full record is in ADR-0115. Concerns 14, 15 and 16 and the rest of round 1 concern 10 are answered here: the Amends header names A3, ADR-0076 decisions 1 and 3 and the Migration steps, and the frame search is withdrawn (14); the stacked row has a value, a control-column rule and a Slint spike, and `ListRow` uses one `@children` (15); confirmations are a third named kind drawn by `DialogActions`, and the `cancel` count is gone (16); `PropertyRow` is retired in favour of `SettingRow` (round 1, 10).

Round 3 (2026-11-13, independent AI session; this text: approve, with two non-blocking concerns). The full record is in ADR-0115. Both are fixed here: C1 is named in the Amends header and ADR-0094's step 5 and Enforcement checks stand (7); the control column never goes below the control's minimum width (8).

- 2026-11-13: Owner (Salih) accepts this ADR after the independent review recorded above (rounds 1 to 3 and the follow-up check; reviewer verdict: approve). Status: Accepted.
- 2026-11-13: Owner (Salih) decision on the screenshots of decision 2 (PR #324): a filter's on/off `Checkbox` stays before its name, as before, not in the trailing part. In the 200 px Properties panel the trailing checkbox cut long filter names, and the on/off state is what the filter list is about. `ListRow` gets an optional leading checkbox for this case; other per-item states (shown, locked, muted) stay trailing icons.
- 2026-11-13: Owner decision after a side-by-side drawing: the RBS menu's item rows move onto `ListRow` too (the current row takes `accent.subtle` on the menu's overlay surface). The Library samples wait for the owner's choice between the grid and `ListRow` rows with a picture; `ListRow` gains an optional picture at its start for that.
- 2026-11-13: Owner decision after a drawing of both: the Library's Background and Frame samples move from the two-column grid (`LibraryCell`) onto `ListRow` rows with a medium picture (`size.row.picture.md`) and a trailing Add button. `LibraryCell` and the `size.library.thumb-w`/`thumb-h` tokens are removed. A list container with an optional row limit and a visible scrollbar for long lists is the next, separate step.
