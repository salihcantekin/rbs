# ADR-0118: Tint buttons

- Status: Accepted
- Date: 2026-11-14
- Deciders: Salih (owner)
- Supersedes in part: [ADR-0112](0112-calm-look-and-roomy-density.md) decision 4 (the secondary button's `color.border.strong` outline only; fields, dropdowns, segmented tracks and switches keep their boundaries) and [ADR-0076](0076-component-standards-and-panel-spacing.md) decision 4 (the danger button's outline; its `trash` icon and ink stay)
- Builds on: [ADR-0057](0057-user-themes-and-appearance-settings.md) (themes and their checks), [ADR-0015](0015-ui-design-rules.md) (U6 tokens, U7 contrast, U12 live colour)
- Spec: [ui-design-system](../specs/ui-design-system.md) (Button, SplitButton, colour and length tables), [themes](../specs/themes.md)

## Context

On 2026-11-14 the owner said the app's buttons look like plain boxes drawn in HTML on an empty canvas, and asked for one rich, standard button for the whole app. Two rounds of mockups were drawn in Slint 1.18 (round 2: `docs/ui/screenshots/buttons/round2-{dark,light}.png`). Round 1 had Soft, Depth and Line. The owner took Soft's corners, and made Line's round icon-only buttons a rule. Round 2, drawn with the Skia renderer the app uses, had Glow, Tint and Key. The owner chose **Tint**: quiet tinted fills, no outline, and the lead icon on a small tinted chip. Glow looked too glassy to the owner. Livelier looks that a theme can choose are a separate decision (draft ADR-0120, not yet accepted).

Today a secondary button is `surface.raised` with a `border.strong` outline (ADR-0112 decision 4). A danger button is outlined (ADR-0076 decision 4). There is no pressed visual. Tint changes all three, and it needs colours that no current token has.

## Decision

1. **Shape.**
   - A button with text has soft corners, `radius.button` (8 px).
   - A button with only an icon is round, and its box is square (a caller that sets the width sets it to the height).
   - The focus ring follows the button's radius.
2. **Fills by kind.**
   - `primary`: `accent`, with `accent.hover` under the pointer and `accent.pressed` while pressed. A one-pixel `accent.hover` lip along the top while it rests, inset past the corners.
   - `secondary`: `button.fill`, with `button.hover` under the pointer and `button.pressed` while pressed.
   - `ghost`: no fill until the pointer brings `button.hover`, then `button.pressed`. A `framed` ghost (the mixer mute) rests on `button.fill`.
   - `danger`: `action.destructive.subtle`, with `.subtle.hover` under the pointer and while pressed. Ink and the `trash` icon stay in `state.error` (ADR-0076). An icon-only danger button (a row's delete) rests bare like a ghost and takes the fill only under the pointer, so a list does not carry a tinted disc on every row.
   - `live`: filled with the fixed live colour and no lip (U12).
   - checked: `accent.subtle`, with `accent.subtle.hover` under the pointer and while pressed, and an `accent` outline.
3. **Outline only where the fill does not separate.** A secondary, danger (with text) or framed button draws a `border.subtle` outline while `Theme.button-outline` is on. That boolean is true when either resting fill, `button.fill` or `action.destructive.subtle` (`BUTTON_FILLS`), is closer than 1.15:1 to any of `surface.base`, `.raised`, `.sunken` or `.overlay`. It uses the same threshold as a panel (ADR-0112 decision 3), and is computed once when the theme slot is filled (`ResolvedTheme::button_outline`).

   | `button.fill` against | base | raised | sunken | overlay | Outline |
   | --- | --- | --- | --- | --- | --- |
   | Dark `#33353b` | 1.60 | 1.37 | 1.55 | 1.20 | no |
   | Light `#eff0f3` | 1.03 | 1.14 | 1.01 | 1.15 | yes |
   | High contrast `#1a1a1a` | 1.21 | 1.14 | 1.21 | 1.06 | yes |

   | `action.destructive.subtle` against | base | raised | sunken | overlay |
   | --- | --- | --- | --- | --- |
   | Dark `#38303f` | 1.56 | 1.33 | 1.51 | 1.16 |
   | Light `#f4ecf8` | 1.02 | 1.15 | 1.02 | 1.13 |

   Danger buttons with text sit in callouts on `surface.overlay`, so the Dark danger fill is tuned to clear 1.15:1 there too.

   High contrast also outlines every unfilled kind except a plain ghost and an icon-only danger button in `border.strong`, as before.
4. **Lead icon chip.**
   - With text, the lead icon of an unfilled button sits on a chip, `size.button.chip` square with `radius.md`. The icon is `accent` on `button.chip`. On a danger button it is `state.error` on `surface.raised`.
   - The chip lets a row of commands scan by icon.
   - On a filled button the icon is bare, in the button's ink.
5. **Signal icons sit in a well.** A caller that colours its lead icon as a signal sets `lead-signal`. Examples are Record's dot and a running output's stop square. The icon then sits in a `surface.sunken` well, round on an icon-only button.
   - The pair `state.live` on `surface.sunken` has its 3:1 rule: Dark 3.32, Light 5.08, High contrast 3.66.
   - No live pair is added to `drawn.rs`'s accepted list. That list stays at the three pairs it had before this ADR.
6. **New tokens** (all themeable, in all three built-in themes, with their checks):
   - Colours:
     - `color.button.fill`, `.hover`, `.pressed` and `.chip`;
     - `color.accent.pressed`;
     - `color.accent.subtle.hover`;
     - `color.action.destructive.subtle` and `.subtle.hover`.
   - Lengths: `radius.button` (8; 0 to 32), `size.button.chip` (20; 12 to 32).
   - Contrast rules, each 4.5:1 (7:1 in high contrast) unless marked 3:1:
     - `text.primary` and `text.secondary` on the four button colours;
     - `text.primary` on `accent.subtle.hover`;
     - `text.inverse` on `accent.pressed`;
     - `state.error` on both danger fills;
     - 3:1: `accent` on `button.chip` and `accent.subtle.hover`;
     - 3:1: `border.focus` on `button.fill`, `.hover`, `.pressed`, `accent.subtle.hover` and both danger fills (the focus ring is drawn inside the button, over its own fill).
   - Rules across tokens:
     - `size.icon.box` ≤ `size.button.chip` ≤ `size.control.sm`;
     - `radius.button` ≤ `size.control.sm` / 2, so the lip never gets a negative width.
   - The token file format stays 2: adding tokens with defaults is compatible (ADR-0057).
7. **SplitButton** takes the same look: radius, fills, pressed, outline rule, chip and well.
8. **Settings › Shortcuts.** Each command row's Reset becomes a round `IconButton` (`reset`, "Reset <command>"), the reset mark of every setting (ADR-0114 review). With the text Reset and the Add chip, the reset and add column no longer fit and Add moved out of line. The owner saw this in the screenshots and approved it on 2026-11-14.
9. **One component.** Every button-like control is `Button`, `IconButton` (a ghost, dense, round `Button`) or `SplitButton`. Home-made button parts move onto them in later changes: caption buttons, the app menu button, monitor, add filter and number steppers.

## Consequences

- Every button in the app changes its look once.
- Light and high contrast keep a soft outline on secondary buttons. Dark has none.
- **User themes:** a theme that recolours `color.surface.*` but not the button colours keeps the base theme's button fills.
  - The outline rule still adapts: a fill too close to the new surfaces gets the outline.
  - A fill that fails a new contrast rule shows a colour warning (ADR-0072), and the theme is still used. The editor's Fix button moves the colour to the nearest passing value.
  - The theme editor lists the new colours in its Colours group.
- Screenshots in dark, light and high contrast are the evidence for the owner's approval (rule 16): `docs/ui/screenshots/buttons/tint-{dark,light,high-contrast}.png`; the round 2 comparison is `round2-{dark,light}.png` beside them.
- **Follow-up:**
  - icons on every command button (owner, 2026-11-14): a command shows its icon with its text; dialog answers (Save, Cancel, Keep, Discard) stay text only; remove and delete always show `trash`; a missing icon is added to the set;
  - migrating the home-made button parts;
  - busy, badge, picture and menu forms of `Button`.

## Alternatives considered

- **Glow** (sheen, a glow under the main action, soft shadows): the owner found it too glassy. It needs alpha and blur, which the opaque-colour token model does not have. Livelier looks a theme can choose go to a separate ADR (draft ADR-0120).
- **Key** (a darker bottom edge and a press): heavier than the calm look wants as the default.
- **Keep the outline on every secondary button**: draws a box around every command, which is what the owner asked to leave behind. The outline stays only where the fill alone does not show the edge.
- **Accept the live dot on the button fills**: Dark gives 2.0 to 2.4:1. These are new tokens chosen freely, so the "not a theme's to fix" reason does not hold. Rejected for the well.

## Review

- 2026-11-14, independent AI session (architecture-review, ui-review), round 1 on the implementation (PR #349): changes requested. 3 major, 7 minor, 6 nits. Resolutions:
  - Major: this ADR was written (1); the outline flag (3); the signal well for the live dot, with the accepted list back to three pairs (5).
  - Minor: cross-token rules (6); spec lines; SplitButton (7); danger fills renamed to `action.destructive.subtle`; icon-only danger rests bare; user themes in Consequences; focus ring rules (6).
  - Nits: chip in high contrast retuned; `lead-ink` comment; comments; square icon-only toolbar buttons; pressed for danger and checked; test sample comment.
- 2026-11-14, independent AI session, round 2: changes requested. 1 major, 3 minor, 4 nits. Resolutions:
  - N1: the outline flag covers the danger fill, and the Dark danger fill clears 1.15:1 on `surface.overlay` (3).
  - N2: the stale accepted-list comment in `drawn.rs` was corrected.
  - N3: an icon-only danger button has no high-contrast ring at rest (3).
  - N4: the Shortcuts change is recorded (8).
  - Nits: the ADR-0120 reference marked as a draft; focus rules listed (6); forward notes added to ADR-0112 and ADR-0076. Dark `text.secondary` on `button.pressed` stays at 4.54:1, above the 4.5:1 rule.
  - Left open (nit, round 1 #11): the icon chip is close to `button.hover` under the pointer. The icon keeps its accent colour, so the button still reads.
- 2026-11-14: owner approval ("onaylıyorum bekleyenleri", 17:07) of the screenshots and this ADR. Status Accepted.
