# ADR-0094: UI interaction patterns (info hints, feature sections, one menu system) and the Tabler icon set

- Status: Proposed
- Date: 2026-11-10
- Deciders: Salih (owner)
- Spec: [ui-design-system](../specs/ui-design-system.md#component-standards)

## Context

The owner asked (2026-11-10) for foundational UI rules so that a user who learns one screen knows every screen: components chosen by how many settings a window holds, short labels with an info hint instead of long explanations, icons and dropdowns instead of rows of big buttons, grouped menus, and a more modern icon set.

A review of the code found drift:

- Help is inline text. There are 29 `word-wrap` texts across `studio.slint`, `settings.slint`, `appearance.slint` and `vcam.slint`, several over 100 characters. U2 promises an explanation "on hover or focus", but no tooltip or hint component exists (`IconButton` has a "tooltip-to-be" comment).
- Three notice systems (`Hint` in `studio.slint`, `SettingsNotice`/`SettingsCallout`/`SettingsStatus`, bare `Text`).
- Two section systems (`SectionHeader` with the private `Section`, `SectionTitle`, `Field`; and `SettingsCard`). The "Advanced settings" of `EncoderFields` is its own button.
- Ten separate `PopupWindow` menus next to the registry-based `CommandMenu` (ADR-0050).
- No rule for choosing `Toggle`, `SegmentedControl` or `Select`.
- Hand-drawn icons that read poorly at 16 px (flip, rotate, fit, center).

[ADR-0015](0015-ui-design-rules.md) (rules U1-U21), [ADR-0076](0076-component-standards-and-panel-spacing.md) and [ADR-0078](0078-saved-section-state-and-shared-section-components.md) already set structure and spacing. This ADR adds the interaction patterns and the icon set. It does not change an accepted ADR; U2, U6 and U13 stay as they are and are made concrete.

## Decision

### A. Density and disclosure

1. **Short label, optional info hint.** A setting has a label of one to three words and, only when it needs one, an `InfoHint`: a small `info` icon button after the label. Pressing it (or Return or Space while focused) opens a popover with the explanation, the default value and the range; Escape or a click outside closes it. One hint is open at a time. An `InfoHint` has an accessible name ("Help: <label>") and its text is also the accessible description of the field. No explanation paragraph is drawn inline. Inline text is allowed only for a state the user must see now (warning, error, status) and then in a `StatusLine` (D1).
2. **Many settings means sections.** A group of two or more related settings is a `SectionHeader` section with a stable id saved in the workspace file (ADR-0078).
3. **A feature with parameters is a feature section.** New component `FeatureSection`: one row with the feature's name, a `Toggle` and a chevron. Switching it on opens its settings below; switching it off collapses them. Parameters of a feature that is off are not drawn. Filters, the ducker, the gate and Settings cards such as Auto-record use it. Its open or closed state is a section id (ADR-0078); the on/off state is the feature's own setting. Advanced settings of a feature are a nested standard "Advanced" section (U2), not a private button.
4. **Control by option count.** Boolean: `Toggle`. Two to four short options that should stay visible: `SegmentedControl`. Five or more options, or long labels: `Select`. A number with a range: `NumberField`, plus `Slider` when drag feedback matters. Free text: `TextField`. A value is never picked from a row of buttons.
5. **Buttons are for actions.** One `primary` button per window or section. Row actions are `ghost` `IconButton`s with a tooltip; three or more row actions collapse into one `more` menu. Destructive actions stay `danger` with the `trash` icon and a confirmation or undo toast (U4).
6. **Icon-only controls have a name.** Every icon-only control has a tooltip on hover and focus (new `Tooltip`, built into `IconButton` and `Button`) and an accessible name.
7. **Icon instead of a button when the meaning is universal.** An action whose icon everyone reads (flip, rotate, delete, show or hide, lock, mute, add) is an `IconButton`, not a text button. Related actions form one compact icon group in a row (flip horizontal, flip vertical, rotate left, rotate right). A rare or ambiguous action keeps icon and text. Every icon has a glossary term (U13).

### B. Menus

1. **One menu system.** Every context menu, `more` menu and add menu is a `CommandMenu` filled from the command registry (ADR-0050). No component builds its own `PopupWindow` menu. One `Context::Source` menu serves the preview, the Sources list and the Properties header.
2. **Shape.** At most seven top-level items; related items go into submenus (Transform, Order, Audio, Show or hide). Most used first, destructive last after a separator. An item has an icon, a name and its shortcut. A command that can become available is shown disabled, not hidden.
3. **Parity.** Everything in a context menu is also a registry command, so it is in the menu bar or the command palette and in the control API (U20).

### C. Structure

1. **One shell per surface.** Panels use `SectionHeader` sections; Settings cards are `SectionHeader` sections underneath. The private `Section`, `SectionTitle` and `Field` of `studio.slint` move into `components.slint`.
2. **One row anatomy.** `[label] [info hint] ...... [control] [unit]`. The label is on the left, the control in a fixed right column, the unit a suffix in `text.secondary`. Same in Properties, Settings and the Mixer filters.
3. **Where settings live** stays as in the design system spec. A search field appears in Settings and Properties once they have more than six sections (U17).

### D. Feedback and states

1. **One status component.** `StatusLine` has four kinds: info, success, warning, error. It shows an icon, one line and an optional action button ("Fix", "Open settings"). It replaces `SettingsNotice`, `SettingsCallout`, `SettingsStatus` and the per-page "A problem with..." strings. Detail goes behind an info hint.
2. **Inline validation.** An invalid field shows an error border, an icon and a one-line reason under it, and the value is not applied; Escape restores the last good value. Valid changes apply at once (U17) without a confirmation.
3. **Empty states** show one sentence and the next action as a button (U10).
4. **Disabled with a reason.** A disabled control says why in its tooltip. It is not hidden, except for a feature that does not exist on this platform.
5. Toasts and dialogs stay as in U18; a reversible action uses an undo toast rather than a confirmation dialog.

### E. Interaction

1. **Keyboard parity.** Every row, hint and menu is reachable with Tab and the arrows; Return activates; Escape closes the top popover; Shift+F10 or the Menu key opens a context menu. The focus ring shows on keyboard focus only.
2. **Number fields.** Arrows step by 1 (Shift: 10), typing and Return apply, Escape restores, double-click on a slider resets to the default.
3. **Copy.** Sentence case, glossary terms, verbs for actions, nouns for settings, no full stop at the end of a label, hints of at most two sentences.

### F. Icons

1. **The set is Tabler Icons (outline)**, MIT licensed, version 3.26.0, replacing the hand-drawn paths. 62 of the 70 icons in `rbs_ui::icon::ICONS` take the Tabler drawing of the same meaning (mapping in `tools/icons/convert_tabler.py`: for example `close` is Tabler `x`, `gear` is `settings`, `eye-off` is `eye-off`). Counter-clockwise rotate and `reset` are the mirrored clockwise icon, because Tabler's own counter-clockwise icons are dashed or have the arrow head in the lower corner.
2. **Eight icons stay RBS drawings** because they are state marks or pixel-aligned window parts, not general pictograms: `dot`, `ring`, `stop` (live state, filled), `rail-between`, `rail-below` (drawn like the docking guides), `window-minimize`, `window-maximize`, `window-restore` (Windows caption buttons).
3. **How they are used.** The converter turns each Tabler SVG into absolute `M`, `L`, `C`, `Z` path commands scaled from the 24 grid to the 16 x 16 view box, so `Icon`, `IconName`, `Icons.named`, the menu image path and the 16 x 16 view box test do not change. Colour still comes from the caller's `ink` and line width from `size.icon.stroke`, so every theme, including high contrast, applies. Tabler is designed with a 2 px line on 24 px (1.33 px at 16 px); `size.icon.stroke` stays 1.5 and can be set by a user theme.
4. **Gaps** are filled by drawing in Tabler's style (round caps and joins, 2 px at 24 px, keyline 24 x 24 scaled), never by mixing in another set. Each new icon needs a glossary term (A7).
5. **Licence.** The MIT notice is kept in `crates/rbs-ui/ICONS-LICENSE.txt`. The licence is compatible with GPL-3.0-or-later. Icons are data in `rbs-ui`; no new crate dependency.

### Enforcement

- New library components: `InfoHint`, `Tooltip`, `StatusLine`, `FeatureSection`, and a shared `SettingRow` promoted from `settings.slint`, each with states, keyboard behaviour, accessible names and light, dark and high-contrast screenshots (the spec's component contract).
- A test `ui_patterns` next to `no_hard_coded_visual_values` fails on a `PopupWindow` outside the component library, on a literal over 90 characters in a `Text` that is not in `InfoHint` or `StatusLine`, and on an icon-only `Button` without a tooltip.
- The `ui-review` skill gets one checklist line per rule group A to F.

### Migration

Each step is its own pull request with screenshots in the three themes for the owner's approval (CLAUDE.md rule 16):

1. The Tabler icons (done in the proposing change; no layout change).
2. `InfoHint`, `Tooltip`, `StatusLine` components and spec text, then move the long inline texts behind hints screen by screen (Settings, Properties, Mixer filters).
3. `FeatureSection` for filters and the Settings cards.
4. Fold the ten `PopupWindow` menus into `CommandMenu`; the shared `Context::Source` menu.
5. The `ui_patterns` test and the `ui-review` checklist.

## Consequences

- A user meets one pattern for help, for features with parameters, for menus and for status. Screens get shorter.
- Icons read better at 16 px, especially flip, rotate and fit.
- About 70 existing texts and ten menus need migration; the order above keeps each change small and reviewable.
- The icon set is third-party artwork; its licence notice must ship with the app (installer notices, ADR-0070).
- Tabler's stroke is lighter and its shapes rounder than the old icons, so every screen changes slightly.

## Alternatives considered

- **Lucide or Phosphor.** Lucide is similar; Phosphor is filled-looking at small sizes. The owner compared all three on a sheet and chose Tabler (2026-11-10).
- **Keep the hand-drawn set and fix the weak icons.** Costs ongoing drawing and design work for a set that will keep growing.
- **An icon font.** Rejected by the design system: a missing glyph shows as an empty box and fonts cannot follow `size.icon.stroke`.
- **Tooltips only, no info button.** Hover does not exist on touch screens and is hard on keyboards; a pressed hint works everywhere.
- **Inline help with a "show help" setting.** Keeps the drift and costs space for everyone.

## Review

Independent review per [ADR-0014](0014-review-policy.md) and rule 11 is pending: an independent AI session or a human gets only this ADR, the design system spec and the code. The ADR stays `Proposed` until the review is recorded and the owner approves.

Owner input, 2026-11-10: approved info hint on press and tooltip on hover; the seven-item menu limit and the "four options or fewer: segmented control" rule; the feature section for Settings cards; icons instead of buttons where the meaning is universal; Tabler as the icon set, replacing all current icons.
