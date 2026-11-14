# ADR-0122: Bold theme effects: Glow, Glass and a deeper Tactile

- Status: Proposed
- Date: 2026-11-14
- Deciders: Salih (owner)
- Builds on: [ADR-0120](0120-theme-effects.md) (theme effects), [ADR-0118](0118-tint-buttons.md) (Tint buttons), [ADR-0057](0057-user-themes-and-appearance-settings.md), [ADR-0072](0072-theme-colour-rules-warn.md), [ADR-0013](0013-zero-cost-when-unused.md), [ADR-0015](0015-ui-design-rules.md)
- Supersedes in part:
  - **ADR-0120:**
    - **Decision 1:** the setting names. Every mention of "Vivid" in ADR-0120 (decisions 1, 2, 3, 4, 5, 12 and 13) reads as Glow.
    - **Decision 5:** the editor labels.
    - **Decision 6:** the token table and its bounds, and the parse rule, which becomes decision 3's.
    - **Decision 7:** derived colours.
    - **Decision 8:** effects by kind.
    - **Decision 9:**
      - "text and icons never sit on an effect … no text pair changes";
      - "checks cover only the pairs an effect changes".
    - **Decision 10:** the face structure: a gradient face and more children.
    - **Decision 11:**
      - the extent is no longer the only limit, because the halo has its own (decision 8);
      - "button rows in the library use at least `space.1`";
      - the sheen clamp.
    - **Decision 16:** the U12 wording.
  - **ADR-0118, decision 3:** "an outline only where the fill does not separate", for Glow only (the rim, decision 7).

  Everything else in ADR-0120 stays.

## Context

ADR-0120 shipped Calm, Vivid and Tactile. At its tightest bounds the owner found the settings too alike. Those bounds are:

- 4 px reach past the box;
- a 2 px sheen band;
- no face gradient.

The owner's words (2026-11-14 21:07): "the differences are not visible without looking closely; options like Glowy or Glassy that are clearly different would be better". The evidence:

- `docs/ui/screenshots/effects/max-{dark,light}.png`: ADR-0120 at its bounds;
- the mock-up `docs/ui/screenshots/effects/bold-{dark,light}.png` (Calm, Glow, Glass, Tactile), approved by the owner as the direction ("uygun", 21:26).

The mock-up was drawn freehand. It uses free values, wider gaps than `space.1` and no contrast rules. Two independent reviews found three ways it breaks the rules:

- **The halo covers neighbours.** At the 4 px gap, the primary's outer glow (the halo) would cover the neighbour's focus ring. Slint paints a child's shadow in tree order, so the halo also lies over the ink of earlier siblings, including the fixed live colour. Under the rules it scales down to about Vivid.
- **The gradients are limited by text contrast.** Text contrast allows almost no gradient across the band where the text sits.
- **The highlight line shows as a stray white line.** The mock-up's line inside the top edge looks like a drawing fault. The owner pointed it out: "butonların üstünde beyaz çizgiler kalmış" ("white lines were left on top of the buttons").

`docs/ui/screenshots/effects/bold-vs-rules-{dark,light}.png` shows the mock-up and the strict result side by side. The owner then decided (2026-11-14 21:50): "Glow üstteki gibi olabilir, glow dışarı doğru olsun. Gerekiyorsa mock için kuralımızı güncelleyebiliriz." ("Glow can be like the top one; let the glow go outward. If needed, we can update our rule for the mock.") He also asked to remove the white lines.

`docs/ui/screenshots/effects/bold-v2-{dark,light}.png` shows the result of this ADR:

- **Glow:** a 12 px halo at 0.6 on the primary. Rows of buttons are 12 px apart while Glow is on.
- **Glass:** a gloss above the text band, and no line.
- **Tactile:** a 3 px edge.

The forces are those of ADR-0120:

- opaque colour tokens and contrast rules;
- zero cost when unused: a `BoxShadow` item for each bound shadow;
- focus rings stay readable;
- the live colour stays fixed;
- high contrast stays plain;
- no animation;
- Slint's software renderer draws no shadows.

## Decision

### Settings and formats

1. **Settings.** A theme's `effects` is `"calm"`, `"glow"`, `"glass"` or `"tactile"`. The editor's segmented control shows Calm, Glow, Glass and Tactile.
   - `Effects::from_name("vivid")` gives Glow, and `name()` never returns `"vivid"`.
   - A stored `"vivid"` is kept as stored (ADR-0120's round-trip rule):
     - a save that does not touch the Effects row writes it back unchanged, and the theme is not dirty;
     - the editor shows it as Glow, and writes `"glow"` only when the user picks a segment.
   - **Downgrade:** a theme saved as `"glow"` or `"glass"` reads as calm in an ADR-0120 build, with its effects notice (unknown setting). This is acceptable because effects never carry meaning.
   - Theme documents stay at format 2.
2. **Token file format 4.**
   - It has `[effects.glow]`, `[effects.glass]` and `[effects.tactile]`. Each holds every token of decision 3.
   - It still needs `[font]`.
   - `EffectTables::parse` refuses an unknown table, such as `[effects.vivid]`. `EffectValues::parse` refuses an unknown token, such as `sheen.*` or `glow.*`.
   - **Reading a format 3 file:**
     - `[effects.vivid]` becomes Glow and `[effects.tactile]` becomes Tactile.
     - Every new token is 0.
     - `sheen.*` is dropped, and `glow.*` becomes `halo.*`.
     - Glass resolves to calm, with the notice "This build's token file has no Glass effects".
   - A format 2 file is read as ADR-0120 says.
3. **Tokens.** Each table holds all of them. Bounds are inclusive, and fractions run from 0 to 1.

   | Token | Kind | Bounds | Meaning |
   | --- | --- | --- | --- |
   | `effects.shadow.blur` | length | 0-4 | As ADR-0120 |
   | `effects.shadow.offset` | length | 0-2 | As ADR-0120 |
   | `effects.shadow.alpha.dark` / `.light` | fraction | 0-0.6 | As ADR-0120 |
   | `effects.halo.blur` | length | 0-16 | The primary's outer glow (decision 8) |
   | `effects.halo.alpha` | fraction | 0-0.8 | Its opacity at the box edge |
   | `effects.key.edge` | length | 0-3 | Raised from 2 |
   | `effects.key.shade` | fraction | 0-0.6 | Raised from 0.4. The key edge is the rest fill mixed toward black by this |
   | `effects.press` | length | 0-3 | The press stays ≤ key edge |
   | `effects.gloss` | fraction | 0-0.4 | The face's top is its fill mixed toward white by this, fading out before the text band (decision 5) |
   | `effects.face.dark` | fraction | 0-0.3 | A ceiling: at most, the face's bottom is its fill mixed toward black by this |
   | `effects.rim.mix` | fraction | 0-0.8 | The border colour: the kind's rim colour (decision 7) mixed into `color.border.subtle` by this |
   | `effects.inner.blur` | length | 0-8 | Blur of an inner glow (an inset shadow) in the rim colour |
   | `effects.inner.alpha` | fraction | 0-0.4 | Its opacity |

   - The parse rule is `key.edge + shadow.offset + shadow.blur ≤ 4` (`EFFECT_EXTENT_MAX`). The halo is outside this rule; decision 8 bounds it.
   - ADR-0120's scaling into the slot's extent stays for everything except the halo, and keeps press ≤ key edge.
   - `effects.sheen.*` and `effects.glow.*` are removed. ADR-0122 has no highlight line.

### Colours and checks

4. **One definition of every derived colour, computed in Rust.**
   - `slot()` computes every derived colour and hands Slint finished colours. Slint mixes nothing. The colours are:
     - the gradient stops for each kind and state (rest, hover, pressed);
     - the rim;
     - the inner glow;
     - the halo;
     - the key edge.
   - The checks use the same function.
   - Mixing is in encoded sRGB:
     - `toward_white(c, f) = c + (255 − c)·f` per channel;
     - `toward_black(c, f) = c·(1 − f)`;
     - rounding to nearest.
   - The stop colours are exact. Skia interpolates between stops with its own rounding, so the checks keep a margin of 0.05 on every ratio.
5. **The face gradient and its check.** When a button draws effects, its face background is a vertical gradient with three stops:
   1. **top, at 0 %:** the fill mixed toward white by `gloss`;
   2. **the band top:** the fill. The band top is where the ink band starts. The ink band is the `type.label` line box centred in the control height, so its top is at `(height − type.label.line) / 2`, as a fraction of the height. That is 6 of 32 px for a regular button, so the gloss fades out above the text.
   3. **bottom, at 100 %:** the fill mixed toward black by the scaled `face.dark`.

   Between the band top and the bottom, the gradient is linear from the fill to the bottom colour.
   - Hover and press swap in that state's stops. Nothing animates.
   - A SplitButton's hovered or pressed part takes that state's gradient as well, not a flat patch.
   - **Pairs.** For each kind, `slot()` checks this table against the existing rules (`CONTRAST_RULES` and `THEME_CONTRAST_RULES`), with the 0.05 margin.

     | Kind | Fills (rest, hover, pressed) | Inks | Ring |
     | --- | --- | --- | --- |
     | primary | `accent`, `.hover`, `.pressed` | `text.inverse` | no: `border.focus` on `accent` is about 1:1 in calm, and ADR-0118 draws the ring over the lip |
     | secondary, framed ghost | `button.fill`, `.hover`, `.pressed` | `text.primary`, `text.secondary`, chip pairs | `border.focus` |
     | checked | `accent.subtle`, `accent.subtle.hover` | `text.primary`, `text.secondary`, `accent` | `border.focus` |
     | danger with text | `action.destructive.subtle`, `.hover` | `state.error`, `text.secondary` | `border.focus` |

   - **Where the pairs are checked.**
     - Each ink is checked at both ends of the ink band, for every control height (`size.control.sm`, `.md` and `.lg`, after density). The gradient is monotonic per channel below the band top, so the two band ends bound every row of ink.
     - The gloss never reaches the band, so it never changes a text pair.
     - A ring pair is checked at the bottom edge.
     - At the top edge, the gloss turns off while the button's `focus.ring` shows. The top stop then equals the fill.
   - **Search.** `face.dark` is scaled down per kind, in steps of 0.01, to the largest value that passes for every state and control height of the kind. A kind's face therefore has one shape in every state and size, and becomes flat when nothing passes. `gloss` is not scaled, because it touches no pair.
   - **Skipped pairs.** A pair that already fails in calm is skipped, whether ADR-0072 warns about it or there is no rule for it. A user theme with a known warning therefore does not also lose its effects.
   - **Notices.** Scaling shows no notice. Glass shows the effects notice "This theme's colours leave no room for Glass shading" when `gloss` is 0 and every kind's `face.dark` is 0.
   - **The built-in colours give these `face.dark` values** (from the ceiling 0.3, regular density):

     | Kind | Dark | Light |
     | --- | --- | --- |
     | primary | 0.10 | 0.30 |
     | secondary | 0.30 | 0.04 |
     | checked | 0.30 | 0.05 |
     | danger | 0.30 | 0.03 |

     The gloss is the same 0.3 on every kind in both bases.
6. **Where the checks run, and what sits under a ring.**
   - **The checks run in `resolve`, for every density.**
     - `ResolvedTheme::effect_draw(density)` picks the values for the density in use, and `slot()` reads them.
     - `resolve` runs the search of decision 5 for regular, roomy and compact steps 1 to 3. That is five sets and a few hundred ratios, well under 1 ms.
     - The effects notice is stored once in `ResolvedTheme::warnings`. It is shown when it applies to any density, and says which density when it does not apply to all of them.
     - The strict built-in check in `Tokens::parse` runs the same loop.
   - **Under the button's own ring:**
     - the gradient's lower part stays (the ring keeps its rule at the bottom edge);
     - the gloss, the rim and the inner glow turn off while `focus.ring` shows, as the sheen and glow do in ADR-0120;
     - the halo is off as well (decision 8).

     A SplitButton keeps ADR-0120's rule: every effect outside its shape is off while either ring shows.
   - **The inner glow under the ink.**
     - The inner glow's residual (ADR-0120's linear model, plus 0.5 px for the Gaussian tail) is blended into the face at the ink band's nearest distance from the edge before the text pairs are checked.
     - That distance is `(control height − type.label.line) / 2`: 2 px on a dense button and 4 px on a regular one, in regular density.
     - When the check fails, `inner.alpha` scales down in the same way as `face.dark`.
   - **The live check.** ADR-0120 decision 9's check (ΔE2000 ≥ 15 from `color.state.live`) covers every derived colour: key edge, halo, rim, inner glow and every gradient stop of every kind.

### Drawing

7. **By kind.** Only these combinations are drawn. Every other combination is none. Each setting uses only the columns its token values make non-zero.

   | Kind | Shadow | Halo | Gloss and shading | Rim | Inner glow | Key edge | Press |
   | --- | --- | --- | --- | --- | --- | --- | --- |
   | `primary` | yes | yes (`accent`) | yes | no | no | yes | yes |
   | `secondary`, framed ghost | yes | no | yes | `accent` | `accent` | yes | yes |
   | checked | as its kind | no | yes | no: keeps its `accent` outline | no | as its kind | yes |
   | `danger` with text | yes | no | yes | `action.destructive` | `action.destructive` | yes | yes |
   | `live` | neutral shadow only | no | no: fixed colour | no | no | no | yes |
   | unframed ghost, icon-only danger, disabled | no | no | no | no | no | no | no |
   | `SplitButton` | as its kind, whole shape | as its kind | one gradient across both parts | as its kind | as its kind | as its kind | no |

   - **ADR-0118's primary lip** stays. It sits at the top edge, over the gloss.
   - **The rim:**
     - it draws where calm draws no outline;
     - it must stay distinguishable from a checked button's `accent` outline: ΔE2000 ≥ 10 between the rim and `accent`, or `rim.mix` scales down;
     - the screenshots show a checked and an unchecked button side by side in every setting.
   - **The SplitButton divider:** its pair is checked at both band ends.
8. **The halo, and the button gap.** The halo is the primary's outer glow. It is the one effect that reaches past the extent, and it never reaches a neighbour.
   - **The button gap.** A new theme slot length, `Theme.button-gap`, is the space between buttons in a row or column of buttons:
     - `space.1` in calm, Glass and Tactile;
     - `max(space.1, effects.halo.blur + 1 px)` in Glow. The extra 1 px covers the Gaussian tail, which Skia draws with sigma = blur/2.
     - It is computed in `slot()` after density.
     - The built-in Glow value is 12 px, so the gap is 13 px.
   - **Rows use it.**
     - Every row or column of library buttons in the app files uses `Theme.button-gap` for its spacing, instead of `space.1`. That covers toolbar rows, dialog actions (`DialogActions`), card actions and the SplitButton row, where today's `space.2` becomes `max(space.2, button-gap)`.
     - The ui-review checklist item "a row of buttons uses `space.1` or more" becomes "a row of buttons uses `Theme.button-gap`".
     - The halo therefore fades out before the next box, in both directions and whatever the tree order. No neighbour's ring, ink or live colour is covered. No blend check is needed, only the gap rule.
   - **Layouts change with Glow.** Rows get wider by about 8 px per gap. The rendered test and the size test cover it:
     - preferred sizes stay identical across settings within a component;
     - rows may grow.

     A panel that clips its content cuts the halo where its padding is narrower than the blur. That is allowed: a clipped halo hides nothing.
   - **The halo is off while the primary's own ring shows.** It does not follow hover or press (ADR-0120 decision 12).
   - **A primary that stands next to a non-button control** (a field, a slider) in the same row is the case the gap does not cover. The ui-review checklist gains the item "a primary is separated from other controls by `Theme.button-gap`". The rendered test includes such a row.
9. **Structure and cost.**
   - ADR-0120 decision 10's structure stays: the bases, then the face, then the lip and the content.
     - The halo is the primary's second base, as ADR-0120's glow base was.
     - The face background becomes the gradient. A face whose stops all equal its fill draws a flat colour.
   - Every new child is conditional on its resolved value, not only on its kind:
     - `if fx-halo` (alpha > 0);
     - `if fx-inner: Rectangle { inner-shadow-* }` (alpha > 0).

     Skia rasterises and caches an inset shadow even at blur 0, so binding `inner-shadow-*` on the face would give every lively setting a `BoxShadow`.
   - Calm adds no drawn item. `Theme.button-gap` is a plain length binding.
   - **The shadow cache.** Slint's cache holds 16 shadows (`MAX_CACHED_SHADOWS`). In Glow, a primary uses two entries (shadow and halo), and so does a secondary (shadow and inner glow).
     - The implementing PR counts the distinct button sizes in the main window per setting.
     - The performance gate (ADR-0120 decision 13, per setting) adds a gradient sweep (hover across a row of gradient buttons) and a meter-redraw run (where the GPU path redraws the whole window).
     - A setting that thrashes the cache fails the gate.
10. **Tests.**
    - **Unit tests:**
      - the alias round trip;
      - format 3 → 4, and the format 4 refusals;
      - the search of decision 5 (a built-in kind at its limit, a pair failing in calm that is skipped, every density);
      - the gloss stops at the band top for every control height;
      - the inner glow scaling;
      - the rim's ΔE from `accent`;
      - the live ΔE of every derived colour;
      - `button-gap ≥ halo.blur + 1` in every density;
      - the built-in file is refused when a setting would need a notice.
    - **The rendered test** (`effects_render.rs`):
      - It renders Dark and Light, regular and compact, in each setting.
      - Its rule becomes: every ring pixel keeps at least min(3, its calm ratio) against the ground pixel beside it in the same render. Every pixel of a neighbour's box equals its calm render, apart from that neighbour's own effects. The halo must not reach it.
      - Text is not measured from anti-aliased pixels. The test samples the face colour at the ink band's ends next to the label and computes the ratio against the known ink colour.
      - Its rows are the ones it has today, plus a checked button and a primary beside a field.
    - **`drawn.rs`:**
      - It keeps reading `Theme.effects-*` atoms as calm.
      - Its face test becomes: the gradient stops are chosen by the same kind and state branches as `face-fill`.
      - A row's `spacing: Theme.button-gap` is a length, so the reader is not affected.
11. **Defaults.** Built-in themes stay calm. The token tables are:
    - **Glow:**
      - halo 12 px at 0.6;
      - rim 0.55;
      - inner glow 6 px at 0.25;
      - shadow blur 3, offset 1, alpha 0.5 dark and 0.3 light.
    - **Glass:**
      - gloss 0.3;
      - face.dark 0.3 (a ceiling);
      - shadow blur 3, offset 1, alpha 0.5 dark and 0.3 light.
    - **Tactile:**
      - edge 3, press 2, shade 0.55;
      - shadow blur 1, offset 0, alpha 0.5 dark and 0.3 light;
      - gloss 0.12.

    The resolved values go in the screenshots' captions. The owner approves those values, not the ceilings.
12. **Policing** is unchanged: only the library draws effects. `check-ui` already counts `@linear-gradient`, `inner-shadow-*` and `Theme.effects-*`. `Theme.button-gap` is not an effect and is not counted.
13. **U12 wording.** The last bullet of U12 becomes:

    > The calm setting has no skeuomorphism, gradients, heavy shadows or decorative animation. A theme may choose livelier effects (ADR-0120, ADR-0122); they are then drawn only by library components, never carry meaning, never use or spread the live colour, never lower a pair below its contrast rule, never reach another control, and never animate.

    `principles.md` changes in the implementing PR.

## Consequences

- Users get three looks that read at a glance, and the look still travels with the theme:
  - **Glow:** a strong halo on the primary, plus a coloured rim and an inner glow on the others;
  - **Glass:** gloss and shading with a soft shadow;
  - **Tactile:** a deeper key edge and press.
- **Glow changes layout.** Button rows are about 8 px wider per gap, and a theme switch reflows them. This is a first: until now, a theme never changed a layout beyond density.
- The check grows from "derived colours far from live" to every button pair at the ink band, for every kind, state, size and density. This adds work in `resolve`, still well under 1 ms.
- **On acceptance:**
  - ADR-0120's Review gets the pointer "decisions 1, 5, 6, 7, 8, 9, 10, 11 and 16 superseded in part by ADR-0122".
  - ADR-0118's Review gets "decision 3 superseded in part for Glow".
  - `docs/adr/README.md` updates the rows.
- **Follow-up (the implementing PRs):**
  - **Docs:**
    - the themes spec: settings, format 4, the tokens, decisions 4 to 8, the "Checks" paragraph and the test list;
    - the design system: per-setting rows, the Effects paragraph and `button-gap`;
    - the glossary row "Effects (theme)";
    - U12;
    - the two ui-review items.
  - **Code:**
    - `tokens.toml` format 4;
    - `effects.rs`: `Effects::ALL`, `LIVELY`, `index`, `draw(density)` and `check`;
    - `build.rs`: the slot fields and `button-gap`;
    - Button, IconButton and SplitButton drawing;
    - every button row in the app files;
    - the editor's labels and hint;
    - the `EffectsProbe` rows;
    - the `RBS_EFFECTS` names in the measure mode;
    - `effects_render.rs` and `drawn.rs`, as decision 10 describes.
  - **Measurements** on the Mac and on Windows (RTX 4090, AMD iGPU, skia-software).
  - **Screenshots** in dark, light and high contrast.

## Alternatives considered

- **Push ADR-0120's values harder only:** done. At its bounds the settings still look alike (evidence above).
- **A halo over the 4 px gap with a blend check:**
  - It needs a contrast rule that depends on spacing and on Slint's tree order. It would draw over the ink of earlier siblings and under the ink of unfilled neighbours.
  - It would tint the fixed live colour and preview pictures.
  - It scales down to about Vivid.
  - Rejected: the owner wants the halo strong ("glow dışarı doğru olsun", "let the glow go outward"), and a gap that covers the halo makes every check local again.
- **No halo, with Glow drawn by its rim and inner glow only:** safe, but the owner chose the outward glow.
- **A highlight line inside the top edge:** rejected. It reads as a stray white line (owner, 2026-11-14). The gloss fades instead.
- **One face value for every kind:** with the built-in colours the tightest pair allows about 0.01, so Glass would be flat everywhere. Per-kind scaling of `face.dark`, plus a gloss kept out of the text band, keeps each kind as bold as its own pairs allow.
- **Translucent (real glass) faces:** would break the opaque-colour rule and every contrast check. Rejected; Glass is drawn with opaque gradients.
- **Per-theme free effect values:** still too many interacting values (ADR-0120's reason stands).

## Review

### Round 1 (independent AI session, architecture-review, 2026-11-14)

Verdict: accept with changes. The reviewer found these problems:

- the Glass values failed the text pairs on the gradient ends;
- the halo lay over earlier neighbours;
- the supersession header was incomplete;
- effects sat under the focus ring;
- the rendered test's invariant was wrong;
- the inner glow needed a conditional child;
- the defaults broke the reach rule;
- the format and alias details were missing;
- the by-kind rules were prose rather than a table;
- the `mix` argument order;
- the live check;
- the cache cost was not stated;
- the follow-up list had gaps.

All were answered in revision 2.

### Round 2 (independent AI session, architecture-review, 2026-11-14)

Verdict: accept with changes. The reviewer raised these points:

- **R1:** the Tactile defaults broke the bounds ADR-0120 kept, and the extent rule kept its glow term.
- **R2:** the pair list and ring rule were wrong for the primary, and the search order was undefined.
- **R3:** the halo's neighbour model was incomplete.
- **R4:** the halo tinted the live colour.
- **R5:** the density-dependent checks had no home.
- **R6:** the rendered test compared against the wrong ground.
- **R7:** the rim changed ADR-0118 decision 3 and resembled the checked state.
- **R8:** the resolved values were missing, and the halo resolved close to Vivid.
- Nits O1 to O12.

The owner then chose an outward halo with a rule update, and the removal of the highlight line. This revision (3) answers:

| Point | Answered in |
| --- | --- |
| R1 | Header and decision 3 |
| R2 | Decision 5 |
| R3, R4, R8 (halo) | Decision 8: the button gap keeps the halo off every neighbour, so the blend check and tree order no longer matter |
| R5 | Decision 6 |
| R6 | Decision 10 |
| R7 | Header and decision 7 |
| R8 (faces) | Decision 5 |
| O1 | Decision 7, which has no highlight line now |
| O2 and O12 | Decision 5 |
| O3 | Decision 9 |
| O4 | Decision 4 |
| O5 | Decisions 6 and 8 |
| O6 and O7 | Header and decision 2 |
| O8 | Decision 6 |
| O9 | Decision 10 |
| O10 | Decision 11 |
| O11 | Moot |

Round 3 is pending.
