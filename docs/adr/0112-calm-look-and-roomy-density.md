# ADR-0112: The Calm look for the built-in themes and a roomy density

- Status: Proposed
- Date: 2026-11-13
- Deciders: Salih (owner)
- Amends: [ADR-0076](0076-component-standards-and-panel-spacing.md) decision 3 (section title style only); rule U11 (wording on 24 px targets)
- Spec: [ui-design-system](../specs/ui-design-system.md#tokens), [themes](../specs/themes.md)

## Context

The owner asked (2026-11-13) for a look that leaves the OBS style behind and stays calm as options multiply. A Slint-only study (`prototypes/ui-directions`, rendered by Slint 1.18 with the Skia renderer) drew three directions in dark and light. The owner chose direction B ("Calm") with the row sizes of the other two as densities, and then its revised form B2 (direction 3 in `ui/look.slint`), which keeps today's accent and control boundaries after a first review found that B broke the contrast rules.

Today a docked panel has a `size.border` outline in `color.border.subtle`, so a window with eight panels draws eight boxes. The app draws no shadow anywhere (`grep drop-shadow crates/rbs-app/ui` finds none). Density is `regular` or `compact` (`rbs_ui::tokens::apply_density`): compact steps `space.*`, `size.control.*` and the panel gaps down; type sizes are not part of it.

## Decision

### Look

1. **One new colour token**, `color.surface.window`: the ground behind docked panels and behind Settings cards, and the fill of the frame row and the status bar. Dark `#0b0b0d`, Light `#ececf0`, High contrast `#000000`. It is themeable; a theme that does not set it gets its base's value. Contrast rules are added for `color.text.primary` and `color.text.secondary` on it (4.5:1) and for `color.accent` and `color.border.strong` on it (3:1).
2. **Changed values** in the built-in themes; every other token keeps its value, including `color.accent`, `color.border.strong`, the state colours and the fixed `color.state.live`:

   | Token | Dark | Light |
   | --- | --- | --- |
   | `color.surface.base` | `#17181b` | `#ffffff` |
   | `color.surface.raised` | `#222327` | `#ffffff` |
   | `color.surface.overlay` | `#2a2b30` | `#e9e9ee` |
   | `color.surface.sunken` | `#0f1012` | `#f1f1f4` |
   | `color.border.subtle` | `#2c2d32` | `#dde0e5` (unchanged) |
   | `radius.lg` | 12 | 12 |

   With these values `built_in_tokens_pass_every_check` passes (run on 2026-11-13 against the surface values; the `checks_fail_loudly` fixture needs a new sample colour because it relies on the old surfaces).
3. **Panels and cards have no outline** in Dark and Light. A docked panel and a Settings card are `color.surface.base` with radius `radius.lg` on `color.surface.window`; the gap between panels is the splitter handle. **High contrast keeps the outline** in `color.border.subtle` on panels and cards, because its surfaces are all black.
4. **Controls keep their boundaries.** A field, a dropdown and a segmented track keep the `color.border.subtle` line they have today on a `color.surface.sunken` fill; a secondary button keeps its `color.border.strong` outline; a switch that is off draws its track in `color.border.strong`. Nothing a control needs to be recognised becomes weaker than it is today (resting field line on its panel: 1.24:1 dark and 1.32:1 light, today and after).
5. **Section titles** are sentence case in a new text style `type.section` (13 / 20 / 600) in `color.text.primary`. This replaces the capital `type.caption` title of ADR-0076 decision 3. `SectionHeader` keeps everything else.
6. **Three shadows, no others.** New tokens `shadow.card` (Light theme only, under a panel or card), `shadow.knob` (slider and fader knob) and `shadow.plate` (the chosen segment of a segmented track and of `Tabs`): blur at most 12 px, offset at most 2 px, colour from a new `color.shadow` per theme. They are not themeable beyond the colour. U12's "no heavy shadows" stands; gradients and blur stay out.
7. `Tabs` and the frame's tab stacks keep their components, roles and `size.tab.indicator`. Nothing in the tab model changes.

### Density

8. `density` in `appearance.toml` gets a third value, `"roomy"`. The appearance format becomes 4; formats 1 to 3 read as before. A build older than this change refuses a format 4 file and starts with its defaults, as it does for any unknown format.
9. A new length token `size.control.xl` (36, floor 32) is added as the last entry of the control steps. Roomy steps `space.*` and `size.control.*` one entry up and `size.panel.pad`, `size.section.gap` and `size.field.gap` to the next larger `space.*` value; the largest entry of each list is unchanged. Regular control height is 28 px, compact 24 px (as today), roomy 32 px. `compact_steps` keeps its meaning and applies to compact only.
10. Density changes lengths only. It must not change a text style, and it must not add, remove or reorder a control. Text size stays a separate, later preference (themes spec, out of scope).
11. Density steps the resolved theme, as compact does today, so a user theme's own lengths are stepped the same way; roomy only enlarges, so no cross-token rule can newly fail.
12. U11's sentence "24 px is only used in dense Advanced and Expert views" becomes: "24 px is used only in the compact density, which the user chooses".

## Consequences

- Every screen changes its look once. No component is added or removed by this ADR.
- User themes: a theme inherits the new built-in values for tokens it does not override. A new warning rule (ADR-0072 style, never a refusal) reports a theme whose `color.surface.base` is within 1.05:1 of its `color.surface.window`, because its panels would merge with the window.
- The pictures the owner chose differ from this text in two ways, both from the review: compact controls are 24 px (the study drew 26) and text does not change size with density (the study drew 12.5 / 13 / 14 px).
- Follow-up in the implementing change: `tokens.toml`, `tokens.rs` (new tokens, contrast rules, roomy step, fixture), themes spec (format 4, `roomy`, the new tokens' themeability), design system spec tables, `drawn.rs` pairs, screenshots in dark, light and high contrast for this change.
- Reversal: the look is token values and four structure rules, undone by one change. Format 4 and `roomy` are permanent once a release has written them.

## Alternatives considered

- **Direction A (bordered, dense)** and **direction C (flat, large)**: survive as the compact and roomy densities; see the study.
- **Direction B as first drawn** (borderless filled fields, weaker outlines, an indigo accent): fails the 3:1 rules in `tokens.rs` and sits about 37 degrees of hue from the error colour.
- **A wider `compact_steps` scale with negative steps** instead of a named value: one number with two meanings is harder to explain in the Appearance page than three names.
- **Absolute sizes per density**: would not fit `apply_density`, which steps the resolved theme, and would be undefined for user themes.
- **Bundling Inter as the UI font**: not decided here; the study uses it, the app keeps the system font until the owner decides.

## Review

Independent review per ADR-0014. Round 1 (2026-11-13, independent AI session, changes requested) reviewed a larger text that also held ADR-0113, 0114 and 0115; its concerns 2 to 7, 12 (shadows, type token), 14 and 15 (first three) are answered in this text. The full round 1 record is in ADR-0115.
