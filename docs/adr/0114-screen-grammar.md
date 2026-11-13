# ADR-0114: One screen grammar: row, list row, section marks, held edits, one home

- Status: Proposed
- Date: 2026-11-13
- Deciders: Salih (owner)
- Amends: [ADR-0094](0094-ui-interaction-patterns-and-tabler-icon-set.md) rule C2 (adds two named row variants)
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

1. **The row is `SettingRow`**, moved to `components.slint` as ADR-0094 ordered, used in Settings, Properties, Source settings and the Mixer filters. Its anatomy is ADR-0094 C2. Two variants are added to C2 and are the only other row shapes: `stacked` (label above the control), which the component chooses itself when the row is narrower than a new token `size.row.stack-below`, never the author; and `PairRow`, two number fields side by side with the name inside each field (X and Y, W and H). The private `Field` components are removed.
2. **The list row is `ListRow`**, a new library component: optional icon, name, optional note, then a slot for trailing state icons and a `more` button. It owns hover, selection, focus ring, drag feedback and the context-menu key. A state the user toggles per item (shown, locked, muted, enabled) is an icon or a `Checkbox` in the trailing slot. The seven private rows move onto it; what is special to one list stays in its slots.
3. **`Checkbox`** is for choosing several items of a set and for a per-item state in a list row. A setting in a `SettingRow` is a `Toggle` (ADR-0094 A4).
4. **Section marks.** `SectionHeader` and `FeatureSection` keep the order of their parts (ADR-0094 A3). Two optional parts are added: a changed mark directly after the title, and a reset button before the info hint. A page shows them only for a section whose defaults it knows; reset is one undo step.
5. **Held edits are the exception and are named.** A change applies at once (U17). A window may hold edits behind buttons only when applying reopens a device or restarts an output, or when it creates or saves a named thing (a source, a theme, a shortcut). The spec lists each such window; today: Source settings (ADR-0077), the theme editor (ADR-0057), the shortcut recorder, and the add and name prompts. A new one is added to the list in the same change.
6. **`DialogActions`**, a new library component, draws the buttons of every window in that list: right-aligned, Cancel to the left of the primary, at most one `danger`, the primary named by its result ("Add source", "Save theme"). Existing accepted labels (Apply and OK in Source settings) stay until their own ADR changes; new windows do not use "OK".
7. **Alignment.** Controls in one column share a right edge; a panel uses only `size.panel.pad`, `size.field.gap` and `size.section.gap` between its parts; numbers are right-aligned in tabular figures; a panel does not scroll sideways (the Mixer strip row excepted, ADR-0101).
8. **One home per thing.** Anything that changes what goes out (an output, a canvas, an encoder, a destination) has a row or card in Settings and a registry command. A menu item or toolbar button is a shortcut to it and must not be the only way to reach it.
9. **Find.** The frame shows a search field that runs registry commands and opens settings by name; Settings and Properties filter their own rows (U17, ADR-0094 C3).

## Consequences

- New library components: `ListRow`, `DialogActions`, `PairRow`; `SettingRow` moves. Each gets the spec's component contract (states, keyboard, accessible names).
- Migration is one screen per pull request with dark and light screenshots for the owner: the components, Settings, Properties, Sources and Scenes, Mixer, Source settings, the remaining windows. These steps replace steps 2, 3 and 5 of ADR-0094's migration list where they overlap. `studio.slint` (5,982 lines) is where feature work also lands, so each step stays small.
- Rules 1 to 3, 5 and 6 are checked by counts in `cargo xtask check-ui` (ADR-0115). Rules 4, 7, 8 and 9 are checked in review with the `ui-review` skill; rule 8 can later become a registry test.
- Reversal: the components can stay even if a rule is withdrawn; a withdrawn rule costs only its check.

## Alternatives considered

- **Finish ADR-0094 only, add nothing.** Leaves lists, held edits and place without a rule, which is where the owner's findability problem came from.
- **A new `FormRow` beside `SettingRow`:** two rows again; the existing one already has the anatomy.
- **Forbid held edits everywhere:** contradicts ADR-0077 decision 6, where one apply is one device restart and one undo step.
- **Label always above the control** (as Properties does today): costs a line per setting, against the compact UI rule.

## Review

Independent review per ADR-0014. Round 1 (2026-11-13, independent AI session, changes requested) reviewed a larger text; its concerns 1 (a to d), 10 (components), 12 (D4 against D5) and 13 are answered here. The full round 1 record is in ADR-0115.
