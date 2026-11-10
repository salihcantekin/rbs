# ADR-0094: UI interaction patterns (info hints, feature sections, one menu system) and the Tabler icon set

- Status: Accepted
- Date: 2026-11-10
- Deciders: Salih (owner)
- Spec: [ui-design-system](../specs/ui-design-system.md#component-standards)

## Context

The owner asked (2026-11-10) for foundational UI rules so that a user who learns one screen knows every screen: components chosen by how many settings a window holds, short labels with an info hint instead of long explanations, icons and dropdowns instead of rows of big buttons, grouped menus, and a more modern icon set.

A review of the code found drift:

- Help is mostly inline text: `grep -c word-wrap crates/rbs-app/ui/*.slint` finds 51 wrapped texts (studio 21, settings 8, appearance 8, components 4, app 3, shortcuts 3, vcam 3, source_settings 1), several over 100 characters. Only the filter fields have an info button (`studio.slint`, `FilterFieldView`: it toggles an inline `Hint`, documented in the spec's icon list). U2 promises an explanation "on hover or focus", but no tooltip component exists (`IconButton` has a "tooltip-to-be" comment).
- Four notice styles: `Hint` (private copies in `studio.slint`, `appearance.slint` and `shortcuts.slint`), `SettingsNotice`/`SettingsCallout`/`SettingsStatus`, and bare `Text`. `Field` is also copied in `studio.slint` and `appearance.slint`.
- Two section systems (`SectionHeader` with the private `Section`, `SectionTitle`, `Field`; and `SettingsCard`). The "Advanced settings" of `EncoderFields` is its own button.
- Nine `PopupWindow`s (`grep -c 'PopupWindow {'`: app 3, studio 2, components 2, app_menu 1, appearance 1), most of them menus, next to the registry-based `CommandMenu` (ADR-0050).
- No rule for choosing `Toggle`, `SegmentedControl` or `Select`.
- Hand-drawn icons that read poorly at 16 px (flip, rotate, fit, center).

[ADR-0015](0015-ui-design-rules.md) (rules U1-U21), [ADR-0076](0076-component-standards-and-panel-spacing.md) and [ADR-0078](0078-saved-section-state-and-shared-section-components.md) already set structure and spacing. This ADR adds the interaction patterns and the icon set. It does not change an accepted ADR. It makes U2, U6 and U13 concrete and reads U2's "on hover or focus" as follows: the `Tooltip` (hover and focus) gives the one-line explanation and default; the `InfoHint` popover (press) carries longer text. `principles.md` U2 gets this wording when the ADR is accepted.

## Decision

### A. Density and disclosure

1. **Short label, optional info hint.** A setting has a label of one to three words and, only when it needs one, an `InfoHint`: a small `info` icon button after the label (it replaces the inline-toggled help of the filter fields). Pressing it (Return or Space while focused) opens a non-modal popover with the explanation, the default value and the range; Escape or a click outside closes it. One hint is open at a time. An `InfoHint` has a hit target of at least 24 x 24 px (U11) around its 16 px icon, an accessible name ("Help: <label>") and its popover text is the accessible description of the field; closing it returns focus to the button. Hint text, tooltips and `StatusLine` text go through `@tr` (U8) and are never truncated. No explanation paragraph is drawn inline. Inline text is allowed only for a state the user must see now (warning, error, status) and then in a `StatusLine` (D1).
2. **Many settings means sections.** A group of two or more related settings is a `SectionHeader` section with a stable id saved in the workspace file (ADR-0078).
3. **A feature with parameters is a feature section.** New component `FeatureSection`: one row with the feature's name flush left, then on the right an `InfoHint`, the fold chevron and a `Toggle`. Switching it on expands its settings below and shows the chevron (right side, so no name ever moves); switching it off collapses them and hides the chevron, so the parameters of a feature that is off are not drawn. The parameters are kept while it is off and applied when it is switched on again. Enabling a feature while live applies its stored parameters at once and shows an undo toast; the section moves other controls only by the user's own action. Filters, the ducker, the gate and Settings cards such as Auto-record use it. Its open or closed state is a section id (ADR-0078); the on/off state is the feature's own setting. Advanced settings of a feature are a nested standard "Advanced" section (U2), not a private button.
4. **Control by option count.** Boolean: `Toggle`. Two to four short options that should stay visible: `SegmentedControl`. Five or more options, or long labels: `Select`. A number with a range: `NumberField`, plus `Slider` when drag feedback matters. Free text: `TextField`. A value is never picked from a row of independent buttons; `SegmentedControl` is the one control that shows several options side by side.
5. **Buttons are for actions.** One `primary` button per window or section. Row actions are `ghost` `IconButton`s with a tooltip; three or more row actions that do not form an icon group (A7) collapse into one `more` menu. Destructive actions stay `danger` with the `trash` icon and a confirmation or undo toast (U4).
6. **Icon-only controls have a name.** Every icon-only control has a tooltip on hover and focus (new `Tooltip`, built into `IconButton` and `Button`) and an accessible name; the tooltip text is the name for an icon-only control. A tooltip shows on hover and keyboard focus only (touch uses the `InfoHint`), stays visible while the pointer is over it and is dismissed with Escape (WCAG 1.4.13). There is one tooltip host per window, driven by callbacks from `IconButton` and `Button`; a button does not own a popup or a timer. The `InfoHint` popover and the tooltip host are in-window overlay layers in the window root, not `PopupWindow`s (a `PopupWindow` cannot take focus). The window keeps a stack of open overlays, routes Escape in the E1 order, returns focus to the button when the popover closes, and opening one hint closes the other. A spike on Slint 1.18 proves this (focus, Escape, hover-over-tooltip, screen reader) before the components merge.
7. **Icon instead of a button when the meaning is universal.** An action whose icon everyone reads (flip, rotate, delete, show or hide, lock, mute, add) is an `IconButton`, not a text button. Related actions form one compact icon group in a row (flip horizontal, flip vertical, rotate left, rotate right). Today only one rotate command exists (`item.rotate`, clockwise); a counter-clockwise command is added with the group, not assumed. A rare or ambiguous action keeps icon and text. Every icon has a glossary term (U13).

### B. Menus

1. **One menu system.** Every context menu, `more` menu and add menu is a `CommandMenu` filled from the command registry (ADR-0050). No component builds its own `PopupWindow` menu. One `Context::Source` menu serves the preview, the Sources list and the Properties header.
2. **Shape.** At most seven top-level items; related items go into submenus (Transform, Order, Audio, Show or hide). Most used first, destructive last after a separator. An item has an icon, a name and its shortcut. A command that can become available is shown disabled, not hidden.
3. **Parity.** Everything in a context menu is also a registry command, so it is in the menu bar or the command palette and in the control API (U20).

### C. Structure

1. **One shell per surface.** Panels use `SectionHeader` sections; Settings cards are `SectionHeader` sections underneath. The private `Section`, `SectionTitle` and `Field` of `studio.slint` move into `components.slint`.
2. **One row anatomy.** `[label] [info hint] ...... [control] [unit]`. The label is on the left (it wraps, never truncates), the control in a right column that keeps its minimum width (U11), the unit a suffix in `text.secondary`. Same in Properties, Settings and the Mixer filters. Everything in a row is vertically centred on the label. The parameters of an open `FeatureSection` start at the section's left edge, flush with the other rows, not indented under the feature's name.
3. **Where settings live** stays as in the design system spec. A search field appears in Settings and Properties once they have more than six sections (U17).

### D. Feedback and states

1. **One status component.** `StatusLine` has four kinds: info, success, warning, error. It shows an icon, one line and an optional action button ("Fix", "Open settings"). Where a line can appear while the user is working live, its space is reserved or it overlays; it never moves controls. It replaces `SettingsNotice`, `SettingsCallout`, `SettingsStatus` and the per-page "A problem with..." strings. Detail goes behind an info hint.
2. **Inline validation.** An invalid field shows an error border, an icon and a one-line reason under it, and the value is not applied; Escape restores the last good value. Valid changes apply at once (U17) without a confirmation.
3. **Empty states** show one sentence and the next action as a button (U10).
4. **Disabled with a reason.** A disabled control says why in its tooltip and stays focusable so the tooltip shows on keyboard focus: its `FocusScope` stays enabled, the action is gated, and `accessible-description` carries the reason. In native menus (`CommandMenu`, no tooltips) the reason is the item's accessible description and the status line. It is not hidden, except for a feature that does not exist on this platform.
5. Toasts and dialogs stay as in U18; a reversible action uses an undo toast rather than a confirmation dialog.

### E. Interaction

1. **Keyboard parity.** Every row, hint and menu is reachable with Tab and the arrows; Return activates; Escape acts on one thing at a time, in this order: a tooltip, then the top popover, then a field edit; the window handles Shift+F10 and the Menu key; Shift+F10 or the Menu key opens a context menu. The focus ring shows on keyboard focus only.
2. **Number fields.** Arrows step by 1 (Shift: 10), typing and Return apply, Escape restores, double-click on a slider resets to the default; the same reset is a Reset item in the field's context menu and Ctrl+Backspace, so it works without a pointer.
3. **Copy.** Sentence case, glossary terms, verbs for actions, nouns for settings, no full stop at the end of a label, hints of at most two sentences.

### F. Icons

1. **The set is Tabler Icons (outline)**, MIT licensed, version 3.26.0, replacing the hand-drawn paths. 62 of the 70 icons in `rbs_ui::icon::ICONS` take the Tabler drawing of the same meaning (mapping in `tools/icons/convert_tabler.py`: for example `close` is Tabler `x`, `gear` is `settings`, `eye-off` is `eye-off`). Counter-clockwise rotate is the mirrored clockwise icon, because Tabler's own counter-clockwise rotate icons are dashed or have the arrow head in the lower corner; `reset` is Tabler `restore`, so Reset transform and Rotate differ in the Transform submenu. `flip-h` (left-right mirror) is Tabler `flip-vertical` and `flip-v` is Tabler `flip-horizontal`, because Tabler names the axis the other way round. Each mapping is chosen by the use of the icon in `command.rs` and the panels; the script holds the table.
2. **Eight icons stay RBS drawings** because they are state marks or pixel-aligned window parts, not general pictograms: `dot`, `ring`, `stop` (live state, filled), `rail-between`, `rail-below` (drawn like the docking guides), `window-minimize`, `window-maximize`, `window-restore` (Windows caption buttons).
3. **How they are used.** The converter turns each Tabler SVG into absolute `M`, `L`, `C`, `Z` path commands scaled from the 24 grid to the 16 x 16 view box, so `Icon`, `IconName`, `Icons.named`, the menu image path and the 16 x 16 view box test do not change. Colour still comes from the caller's `ink` and line width from `size.icon.stroke`, so every theme, including high contrast, applies. `Icon` draws round caps and joins (the `more` icon relies on them). Tabler is designed with a 2 px line on 24 px (1.33 px at 16 px); `size.icon.stroke` stays 1.5 and can be set by a user theme. Icons are verified at 16 px (`size.icon.box` default); smaller sizes are not promised. Menu images are rasterised at the menu stroke in grey as before; they are checked in a screenshot on Windows and macOS before merge.
4. **Gaps** are filled by drawing in Tabler's style (round caps and joins, 2 px at 24 px, keyline 24 x 24 scaled), never by mixing in another set. Each new icon needs a glossary term (A7).
5. **Licence.** The MIT notice is kept in `crates/rbs-ui/ICONS-LICENSE.txt`. The licence is compatible with GPL-3.0-or-later. Icons are data in `rbs-ui`; no new crate dependency. A test checks the file exists. The notice ships in the installer and the About window (`deny.toml` does not cover artwork). The converter fetches Tabler 3.26.0 from a registry; the pinned SHA-256 of each source SVG is committed next to the script and the script refuses a mismatch (done before the next icon is added).

### Enforcement

- New library components: `InfoHint`, `Tooltip`, `StatusLine`, `FeatureSection`, and the existing `SettingRow` moved from `settings.slint` to `components.slint`, each with states, keyboard behaviour, accessible names and light, dark and high-contrast screenshots (the spec's component contract). Zero cost (rule 10): popovers, tooltips and collapsed `FeatureSection` bodies are not instantiated, and no timer runs, until opened or hovered (hover delay is a one-shot, never polling); a collapsed body stays out of the scheduled layout, not just hidden.
- A test `ui_patterns` next to `no_hard_coded_visual_values` fails on a `PopupWindow` outside the component library, on a literal over 90 characters in a `Text` that is not in `InfoHint` or `StatusLine`, and on an icon-only `Button` without a tooltip (static instances only). Rules D4, the seven-item limit and B3 are checked in review and by the registry tests.
- The `ui-review` skill gets one checklist line per rule group A to F.

### Migration

Each step is its own pull request with screenshots in the three themes for the owner's approval (CLAUDE.md rule 16):

1. The Tabler icons (implemented in the proposing pull request, not merged: screenshots in three themes await the owner's approval, rule 16).
2. `InfoHint`, `Tooltip` (a `tooltip` property on `IconButton` and `Button`, replacing the "tooltip-to-be" comment), `StatusLine` components and spec text, then move the long inline texts behind hints screen by screen (Settings, Properties, Mixer filters).
3. `FeatureSection` for filters and the Settings cards.
4. Fold the nine `PopupWindow`s into `CommandMenu`; the shared `Context::Source` menu.
5. The `ui_patterns` test and the `ui-review` checklist. The test starts earlier with an allowlist of existing violations that only shrinks.

## Consequences

- The design system spec gets the `InfoHint`, `Tooltip`, `StatusLine` and `FeatureSection` contracts in the same change as each component (rule 2).
- A user meets one pattern for help, for features with parameters, for menus and for status. Screens get shorter.
- Icons read better at 16 px, especially flip, rotate and fit.
- The wrapped texts and menus counted above need migration; the order above keeps each change small and reviewable.
- The icon set is third-party artwork; its licence notice must ship with the app (installer and About window).
- Tabler's stroke is lighter and its shapes rounder than the old icons, so every screen changes slightly.

## Alternatives considered

- **Lucide or Phosphor.** Lucide is similar; Phosphor is filled-looking at small sizes. The owner compared all three on a sheet and chose Tabler (2026-11-10).
- **Keep the hand-drawn set and fix the weak icons.** Costs ongoing drawing and design work for a set that will keep growing.
- **An icon font.** Rejected by the design system: a missing glyph shows as an empty box and fonts cannot follow `size.icon.stroke`.
- **Tooltips only, no info button.** Hover does not exist on touch screens and is hard on keyboards; a pressed hint works everywhere.
- **Inline help with a "show help" setting.** Keeps the drift and costs space for everyone.

## Review

Round 1, independent AI session (2026-11-10): changes requested. Resolved in this revision: wrong counts (50 wrapped texts, 9 popups, four notice styles) and the existing filter info button are stated; U2 wording made explicit; ADR-0070 citation replaced; InfoHint and Tooltip accessibility, touch and i18n added; zero-cost wording added; `reset` and `rotate-ccw` now differ; flip axis naming explained; licence notice shipping and the test added; rule 16 status corrected; enforcement test gets an allowlist. Round 2, independent AI session (2026-11-10): changes requested. Resolved in this revision: the wrapped-text count (51) and nine popups; A4 and A5 no longer contradict `SegmentedControl` and the icon group; one tooltip host per window instead of a popup and timer per button, and focus handling for popovers stated; disabled controls stay focusable so their reason can show; the feature section toggle and chevron rule defined; an Escape order; a keyboard and context-menu way to reset a value; status lines never move controls while live; labels wrap; a single rotate command exists today. Round 3, independent AI session (2026-11-10): changes requested. Resolved in this revision: overlay layers instead of `PopupWindow`s with a spike as a merge precondition for the components; feature section keeps parameters while off and enabling while live shows an undo toast; disabled reasons in native menus; how a focusable disabled control is built; Tabler hash pinning scheduled; toolkit statements reworded; touch long press dropped. Slint feasibility spike (2026-11-10, throwaway test on the 1.18.1 testing backend, not merged): an in-window overlay takes focus, handles Escape and returns focus to its button; a tooltip stays visible while the pointer is over it; a `PopupWindow`'s `FocusScope` did not receive Escape. An element inside an `if` cannot be referenced from outside, so each overlay component owns its `if` and exposes `open()`. Screen reader and real Windows and macOS windows are verified in the component pull requests. The ADR stays `Proposed` until round 3 approves and the owner approves.

Owner input, 2026-11-10: approved info hint on press and tooltip on hover; the seven-item menu limit and the "four options or fewer: segmented control" rule; the feature section for Settings cards; icons instead of buttons where the meaning is universal; Tabler as the icon set, replacing all current icons.

Owner decision, 2026-11-10: Salih reviewed the proposal screenshots (info hint popover, icon group with tooltip, status line, feature section), asked for two alignment corrections (row icons vertically centred on the label; parameters of an open feature section flush left), which are now in rule C2, and accepted the ADR once they were made. Status `Accepted`.
