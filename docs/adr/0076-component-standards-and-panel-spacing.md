# ADR-0076: Component standards, collapsible panel sections and a spacing floor

- Status: Proposed
- Date: 2026-11-08
- Deciders: Salih (owner)
- Spec: [ui-design-system](../specs/ui-design-system.md#component-standards)

## Context

The owner found the Properties panel unreadable (2026-11-08): Transform and Filters (a `SegmentedControl`) do not look like tabs, the gaps are 4 px, the sections (Position and size, Crop, Show) run together, and "Delete source" is a bare purple word that does not look like a button. The same drift can happen in every window because each panel arranges its own Buttons, titles and gaps. The owner asks for standards: buttons, labels, tabs and section headers with fixed shapes, so windows are readable and their categories visible. Spacing comes from `space.*`, which a user theme may set down to 0, so a theme can make a panel unreadable.

The purple is by design: `color.state.error` and `color.action.destructive` differ in hue from the live red (rule U12). It reads as a button only when it is drawn as one.

## Decision

1. **A component standard** is written into the design system spec ("Component standards"): which component is used for which job, and its fixed shape. The main rules: Tabs (an underlined tab bar) switch between views of one thing, `SegmentedControl` picks a value, a `SectionHeader` starts every group of fields, a `PropertyRow` is a label with a field, `HintText` is secondary caption text. A panel does not draw its own titles, tabs or action buttons.
2. **`Tabs`** (new component, full width, one bar, one focus stop; arrows, Home and End choose; underline `size.tab.indicator`, a new token): each tab is a text button on the bar, the chosen one in `text.primary`, bold, with a 2 px `accent` underline; the others `text.secondary`, `surface.overlay` under the pointer. Reached by Tab and the arrow keys, role tab with its selected state. The Properties Transform, Filters and Audio views use it.
3. **`SectionHeader`** (new component): the section title (`type.caption`, uppercase, `text.secondary`) with a `size.border` rule in `border.subtle` filling the rest of the row, and a `chevron-down` / `chevron-right` when the section collapses. A collapsible header is a button (role button, expanded state); a closed section draws nothing and takes no room. An open or closed state is kept per section id while the app runs (saving it in the workspace file comes later); sections start **open** unless the spec says otherwise.
4. **`danger` buttons** are drawn as buttons: a `size.border` outline and ink in `color.state.error` (the same colour as `color.action.destructive`, but with the contrast rules for error text on every surface), with the `trash` icon before the text, never text alone. Dense (`size.control.sm`) in panels.
4a. **Properties structure.** Under the header (source icon, name, name field): `Tabs` (Transform, Filters for a picture source; Audio too when it has sound; a sound-only source has no tab bar), the tab's sections, then two sections for every source: Scenes (Show in every scene) and Remove (Delete source, last). All sections start open.
5. **Spacing floor.** New tokens `size.panel.pad` (12, floor 8), `size.section.gap` (16, floor 12) and `size.field.gap` (8, floor 6) hold the padding of a panel and the gaps between sections and between fields. They are themeable (6 to 64 px at most; `size.field.gap <= size.section.gap` is an across-tokens rule; a test checks the floors match the editor bounds), but a token file or theme below the floor is refused with this ADR named, like `size.focus-ring` (rule U7). Panels use these tokens, not `space.*`, for their own layout.
6. **Scope.** The first change applies the standard to Properties and adds the components. Other windows (Settings, Sources, Mixer) move to the same components in later changes; until then, the standard applies to new and edited UI.

## Consequences

- Properties reads as a tabbed inspector with separated, collapsible sections; a theme cannot squeeze it below the floors.
- A future panel gets structure from components, so the owner's categories stay visible.
- The theme editor lists three more spacing tokens. Themes that set them lower than the floors are refused.
- `SegmentedControl` stays for value choices only (such as Shape: Rectangle / Circle / Ellipse).

## Alternatives considered

- Raise `space.*` bounds instead: changes every window and forbids compact themes everywhere; a floor only for panels is narrower.
- Keep `SegmentedControl` for tabs and restyle it: the control still means "pick a value" to users and tools (accessibility role), so it stays separate.
- Remove Delete source from Properties: the owner chose to keep it, at the bottom, small, with an icon.

## Review

Round 1, independent AI session (2026-11-08): changes requested. Resolved: Tabs are one focus stop with arrows, Home and End and a following focus ring; the tab underline has its own token `size.tab.indicator`; floors and editor bounds are checked to agree and `size.field.gap <= size.section.gap` is an across-tokens rule; the Scenes and Remove sections and the Audio tab are in this ADR; the collapse state scope and the private `SectionTitle` exception are stated in the spec. Not changed: the Remove section starts open like the others (the owner chose the placement); bold selected-tab width shift (minor); no heading role exists in Slint; a further review round and the owner's approval are needed before Accepted.
