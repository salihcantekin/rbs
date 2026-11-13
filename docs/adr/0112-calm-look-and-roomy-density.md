# ADR-0112: The Calm look for the built-in themes and a roomy density

- Status: Proposed
- Date: 2026-11-13
- Deciders: Salih (owner)
- Amends: [ADR-0076](0076-component-standards-and-panel-spacing.md) decision 3 (section title style only)
- Spec: [ui-design-system](../specs/ui-design-system.md#tokens), [themes](../specs/themes.md)

## Context

The owner asked (2026-11-13) for a look that leaves the OBS style behind and stays calm as options multiply. A Slint-only study (`prototypes/ui-directions`, rendered by Slint 1.18 with the Skia renderer) drew three directions in dark and light. The owner chose direction B ("Calm") with the row sizes of the other two as densities, and then its revised form B2 (direction 3 in `ui/look.slint`), which keeps today's accent and control boundaries after a first review found that B broke the contrast rules.

Today the window ground is `color.surface.base` and a docked panel or Settings card is `color.surface.raised` with a `size.border` outline in `color.border.subtle`, so a window with eight panels draws eight boxes. In the built-in themes the panel differs from the ground by 1.10:1 (Dark) and 1.08:1 (Light). The app draws no shadow anywhere, and the token model has no shadow kind. Density is `regular` or `compact` (`rbs_ui::tokens::apply_density`): compact steps `space.*`, `size.control.*` and the three panel gaps down and never changes the smallest step; type sizes are not part of it.

## Decision

### Look

1. **No token is added and no token changes its role.** `color.surface.base` stays the window ground (frame row, status bar, the ground behind panels and Settings cards), `color.surface.raised` stays the panel and card, `color.surface.overlay` stays popups and hover plates, `color.surface.sunken` stays the field fill. A user theme therefore keeps colouring the same things.
2. **Changed values** in the built-in Dark and Light themes. Every other token keeps its value, including `color.accent`, `color.border.strong`, the state colours and the fixed `color.state.live`. High contrast is unchanged.

   | Token | Dark today | Dark | Light today | Light |
   | --- | --- | --- | --- | --- |
   | `color.surface.base` | `#121417` | `#0b0b0d` | `#f5f6f8` | `#ececf0` |
   | `color.surface.raised` | `#1b1e23` | `#17181b` | `#ffffff` | `#ffffff` |
   | `color.surface.overlay` | `#252931` | `#222327` | `#e8ebf0` | `#e9e9ee` |
   | `color.surface.sunken` | `#0c0e10` | `#0f1012` | `#e9ebef` | `#f1f1f4` |
   | `color.border.subtle` | `#2a2f37` | `#2c2d32` | `#dde0e5` | `#dde0e5` |
   | `radius.lg` | | 12 | | 12 |

   With these values every check of `cargo test -p rbs-ui` passes except the fixture test `checks_fail_loudly`, which builds its failing sample from the old surfaces and gets a new sample colour (run on 2026-11-13, 165 passed, that one failed).
3. **Panels and cards lose their outline when their colour separates them.** When the resolved theme's `color.surface.raised` differs from its `color.surface.base` by at least 1.10:1, a docked panel and a Settings card draw no outline and use radius `radius.lg`; the gap between panels is the splitter handle. Below 1.10:1 the outline in `color.border.subtle` is drawn as today. The answer is one boolean computed when the theme is resolved (`rbs-app` `look`), not per frame. Built-in Dark gives 1.11 and Light 1.18, so no outline; High contrast gives 1.06, so it keeps its outline; a user theme with close surfaces keeps the outline it has today, with no new warning and no change to ADR-0072.
4. **Controls keep their boundaries.** A field, a dropdown and a segmented track keep their `color.border.subtle` line on a `color.surface.sunken` fill; a secondary button keeps its `color.border.strong` outline; a switch that is off draws its track in `color.border.strong`. The line is the boundary and does not get weaker; the fill gets closer to the panel, which this decision accepts:

   | Pair | Dark today | Dark | Light today | Light |
   | --- | --- | --- | --- | --- |
   | Field line on panel | 1.24 | 1.29 | 1.32 | 1.32 |
   | Field line on overlay | 1.08 | 1.14 | 1.11 | 1.09 |
   | Field fill against panel | 1.16 | 1.07 | 1.19 | 1.13 |
   | Panel against ground | 1.10 with outline | 1.11, no outline | 1.08 with outline | 1.18, no outline |
5. **Section titles** are sentence case in the existing text style `type.label` (13 / 20 / 500) in `color.text.primary`. This replaces the capital `type.caption` title of ADR-0076 decision 3. `SectionHeader` keeps everything else, including its `size.control.sm` height.
6. **No shadows.** The study drew a soft shadow under Light panels and under knobs. They are not adopted: the token model has no shadow kind and a colour token must be opaque. A shadow kind is a separate decision if the owner wants it after seeing the built screens.
7. `Tabs` and the frame's tab stacks keep their components, roles and `size.tab.indicator`. Nothing in the tab model changes.

### Density

8. `density` in `appearance.toml` gets a third value, `"roomy"`. The appearance format becomes 4; formats 1 to 3 read as before. A build older than this change refuses a format 4 file and starts with its defaults, as it does for any unknown format. The token file format stays 2, because no token is added.
9. **What roomy changes, and nothing else:** `size.control.md` takes the resolved value of `size.control.lg`, and `size.panel.pad`, `size.section.gap` and `size.field.gap` each take the next larger resolved `space.*` value (unchanged when none is larger). With the built-in lengths the default control is 32 px (28 regular, 24 compact as today), the panel pad 16, the section gap 24 and the field gap 12. `space.*`, `size.control.sm` and `size.control.lg` are not touched, so the fixed sizes computed from the smallest steps (`size.mixer.*`, `size.field.compact`, `size.transition.*`) and the dense parts (`IconButton`, `SectionHeader`) are the same in every density. `compact_steps` keeps its meaning and applies to compact only.
10. Density changes lengths only. It must not change a text style, and it must not add, remove or reorder a control. Text size stays a separate, later preference (themes spec, out of scope).
11. Density steps the resolved theme, as compact does today, so a user theme's own lengths are stepped the same way. The cross-token rules are "must not be smaller than" (`tokens/theme.rs`, `ascending`): `size.control.md` equal to `size.control.lg` holds, and the three gaps move up the same ordered list so `size.field.gap` stays at or below `size.section.gap`. A property test over valid themes proves this in the implementing change.
12. **The 1280 x 720 minimum (U11) holds in roomy.** The implementing change adds a test that the built-in Edit workspace's minimum panel sizes fit 1280 x 720 in each density; roomy is not offered until it passes.

## Consequences

- Every screen changes its look once. No component and no token is added by this ADR.
- User themes: a theme inherits the new built-in values for tokens it does not override; a token it sets keeps its role. A theme that sets only one of `color.surface.base` and `color.surface.raised` may end below 1.10:1 and then keeps the outline.
- The table in decision 2 is the authority, not the study's pictures. The pictures differ from it: compact controls are 24 px (drawn 26), text does not change size with density (drawn 12.5 / 13 / 14 px), section titles are weight 500 (drawn 600), there are no shadows (drawn under Light panels and knobs), and hairlines, hover plates, text greys, the control radius and the panel padding keep today's token values (drawn slightly different). The owner approves the built screens from screenshots (rule 16), which settles each of these.
- Follow-up in the implementing change: `tokens.toml`, `tokens.rs` (roomy, fixture), `look` (the outline boolean), themes spec (format 4, `roomy`, the outline rule), design system spec tables, `drawn.rs` pairs if a drawn pair changes, screenshots in dark, light and high contrast.
- Reversal: the look is token values and two structure rules, undone by one change. Format 4 and `roomy` are permanent once a release has written them.
- Order: this ADR depends on no other. ADR-0113 needs `roomy` from it.

## Alternatives considered

- **Direction A (bordered, dense)** and **direction C (flat, large)**: survive as the compact and roomy densities; see the study.
- **Direction B as first drawn** (borderless filled fields, weaker outlines, an indigo accent): fails the 3:1 rules in `tokens.rs` and sits about 37 degrees of hue from the error colour.
- **A new `color.surface.window` token with panels moved to `color.surface.base`** (round 2 text): moves a user theme's panel colour to another role without a migration and puts the frame row on a colour the theme never chose.
- **A `size.control.xl` step and stepping every `space.*` and `size.control.*` up** (round 2 text): breaks themes that set a large `size.control.lg`, and changes the fixed sizes that are sums of the smallest steps.
- **Shadow tokens**: needs a new token kind and an alpha colour; left out until asked for.
- **A wider `compact_steps` scale with negative steps** instead of a named value: one number with two meanings is harder to explain in the Appearance page than three names.
- **Bundling Inter as the UI font**: not decided here; the study uses it, the app keeps the system font until the owner decides.

## Review

Independent review per ADR-0014. Round 1 (2026-11-13, independent AI session, changes requested) reviewed a larger text that also held ADR-0113, 0114 and 0115; its concerns 2 to 7, 12 (shadows, type token), 14 and 15 (first three) are answered in this text. The full round 1 record is in ADR-0115.

Round 2 (2026-11-13, independent AI session, changes requested on all four texts). The full record is in ADR-0115. Concerns 4 to 9 and 19 are answered here: no role change and no new token (4, 8, 19); no `size.control.xl` and roomy leaves the smallest steps alone (5, 6); the figures of decision 4 are recomputed and the weaker fill is stated (7); U11 is not amended (9); the differences from the pictures are listed in full (19).
