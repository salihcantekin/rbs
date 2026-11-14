# ADR-0122: Bold theme effects: Glow, Glass and a deeper Tactile

- Status: Proposed
- Date: 2026-11-14
- Deciders: Salih (owner)
- Builds on: [ADR-0120](0120-theme-effects.md) (theme effects), [ADR-0118](0118-tint-buttons.md) (Tint buttons), [ADR-0057](0057-user-themes-and-appearance-settings.md), [ADR-0072](0072-theme-colour-rules-warn.md), [ADR-0013](0013-zero-cost-when-unused.md), [ADR-0015](0015-ui-design-rules.md)
- Supersedes in part:
  - **ADR-0120, decision 1:** the setting names. Every mention of "Vivid" in ADR-0120 (decisions 1, 2, 3, 4, 5, 12 and 13) reads as Glow.
  - **ADR-0120, decision 5:** the editor labels.
  - **ADR-0120, decision 6:**
    - the token table and its bounds for `key.edge` and `key.shade`;
    - the parse rule `key.edge + max(offset + blur, glow.blur) ≤ extent`, which becomes `key.edge + offset + blur ≤ extent`.
  - **ADR-0120, decision 7:** derived colours.
  - **ADR-0120, decision 8:** effects by kind.
  - **ADR-0120, decision 9:**
    - "text and icons never sit on an effect … no text pair changes";
    - "checks cover only the pairs an effect changes".
  - **ADR-0120, decision 10:** the face structure: a gradient face and more children.
  - **ADR-0120, decision 11:**
    - the sheen clamp;
    - the scaling sentence, which loses its glow term.
  - **ADR-0120, decision 16:** the U12 wording.
  - **ADR-0118, decision 3:** "an outline only where the fill does not separate", for Glow only (the rim, decision 7).

  Everything else in ADR-0120 stays. That includes the extent of decision 11: nothing reaches past it.

## Context

ADR-0120 shipped Calm, Vivid and Tactile. At its tightest bounds the owner found the settings too alike. Those bounds are:

- 4 px reach past the box;
- a 2 px sheen band;
- no face gradient.

The owner's words (2026-11-14 21:07): "the differences are not visible without looking closely; options like Glowy or Glassy that are clearly different would be better". The evidence:

- `docs/ui/screenshots/effects/max-{dark,light}.png`: ADR-0120 at its bounds.
- The mock-up `docs/ui/screenshots/effects/bold-{dark,light}.png`, with Calm, Glow, Glass and Tactile. The owner approved it as the direction ("uygun", 21:26).

The mock-up was drawn freehand. It used free values, wider gaps than `space.1`, and no contrast rules. Under the rules, with the built-in colours, the result is different. `docs/ui/screenshots/effects/bold-vs-rules-{dark,light}.png` shows the mock-up and the resolved values side by side:

- **Glow's outer halo cannot stay strong.** At the 4 px gap it would cover the neighbour's focus ring. Slint paints a child's shadow in tree order, so the halo also lies over the ink of earlier siblings, including the fixed live colour. The rules scale it to about 0.37 alpha and a 5-9 px blur, which is close to ADR-0120's Vivid. The owner is asked how to go on (decision card, 2026-11-14). This revision follows the recommended option, no halo, until the owner answers.
- **Glass's gradients are one-sided.** Text contrast leaves room on one end only.

Most of what makes a look read at a glance is **the face**:

- a vertical gradient;
- a highlight line;
- a coloured rim;
- an inner glow.

All of these stay inside the box, so they need no extent and touch no neighbour.

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

1. **Settings.** A theme's `effects` is `"calm"`, `"glow"`, `"glass"` or `"tactile"`.
   - The editor's segmented control shows Calm, Glow, Glass and Tactile.
   - `Effects::from_name("vivid")` gives Glow. `name()` never returns `"vivid"`.
   - A stored `"vivid"` is kept as stored, by ADR-0120's round-trip rule:
     - A save that does not touch the Effects row writes it back unchanged, and the theme is not dirty.
     - The editor shows such a theme as Glow, and writes `"glow"` only when the user picks a segment.
   - **Downgrade.** In an ADR-0120 build, a theme saved as `"glow"` or `"glass"` reads as calm, with its effects notice (unknown setting). This is acceptable because effects never carry meaning.
   - Theme documents stay at format 2.
2. **Token file format 4.**
   - It has `[effects.glow]`, `[effects.glass]` and `[effects.tactile]`. Each holds every token of decision 3.
   - It still needs `[font]`.
   - `EffectTables::parse` refuses an unknown table, such as `[effects.vivid]`. `EffectValues::parse` refuses an unknown token, such as `sheen.*` or `glow.*`.
   - A format 3 file is read like this:
     - `[effects.vivid]` becomes Glow, and `[effects.tactile]` becomes Tactile.
     - Every new token is 0. `sheen.*` and `glow.*` are dropped, so their sheens and Vivid's 4 px glow are lost.
     - Glass resolves to calm, with the notice "This build's token file has no Glass effects".
   - A format 2 file is read as ADR-0120 says.
3. **Tokens.** Each table holds all of these tokens. Bounds are inclusive, and fractions run from 0 to 1.

   | Token | Kind | Bounds | Meaning |
   | --- | --- | --- | --- |
   | `effects.shadow.blur` | length | 0-4 | As ADR-0120 |
   | `effects.shadow.offset` | length | 0-2 | As ADR-0120 |
   | `effects.shadow.alpha.dark` / `.light` | fraction | 0-0.6 | As ADR-0120 |
   | `effects.key.edge` | length | 0-3 | Raised from 2 |
   | `effects.key.shade` | fraction | 0-0.6 | Raised from 0.4. The key edge is the rest fill mixed toward black by this |
   | `effects.press` | length | 0-3 | Press ≤ key edge |
   | `effects.face.light` | fraction | 0-0.4 | A ceiling: at most, the face's top is its fill mixed toward white by this |
   | `effects.face.dark` | fraction | 0-0.3 | A ceiling: at most, the face's bottom is its fill mixed toward black by this |
   | `effects.highlight.alpha.dark` / `.light` | fraction | 0-0.9 | A white line of `size.border` inside the top edge, inset by the button's own radius |
   | `effects.rim.mix` | fraction | 0-0.8 | The border colour: the kind's rim colour (decision 7) mixed into `color.border.subtle` by this |
   | `effects.inner.blur` | length | 0-8 | Blur of an inner glow (an inset shadow) in the rim colour |
   | `effects.inner.alpha` | fraction | 0-0.4 | Its opacity |

   - `effects.glow.*` and `effects.sheen.*` are removed.
   - The parse rule is `key.edge + shadow.offset + shadow.blur ≤ 4` (`EFFECT_EXTENT_MAX`).
   - ADR-0120's scaling into the slot's extent stays, without a glow term, and keeps press ≤ key edge.

### Colours and checks

4. **One definition of every derived colour, computed in Rust.**
   - `slot()` computes every derived colour and hands Slint finished colours. Slint mixes nothing.
   - Derived colours are:
     - the gradient's two ends, for each kind and state (rest, hover, pressed);
     - the rim;
     - the inner glow;
     - the key edge.
   - The checks use the same function as the drawing.
   - The mixing is in encoded sRGB:
     - `toward_white(c, f) = c + (255 − c)·f` per channel;
     - `toward_black(c, f) = c·(1 − f)`;
     - rounding to nearest.
   - The gradient's end colours are exact. Skia interpolates the rows in between with its own rounding. The check therefore needs a margin of 0.05 on every ratio.
5. **The face gradient and its check.**
   - When a button draws effects, its face background is a vertical gradient from the top colour to the bottom colour.
   - Hover and press swap in that state's pair of colours, and nothing animates.
   - A SplitButton's hovered or pressed part takes that state's gradient too, not a flat patch.
   - **Pairs.** For each kind, `slot()` checks this table. The rules are the existing ones (`CONTRAST_RULES` and `THEME_CONTRAST_RULES`), plus the 0.05 margin.

     | Kind | Fills (rest, hover, pressed) | Inks | Ring |
     | --- | --- | --- | --- |
     | primary | `accent`, `.hover`, `.pressed` | `text.inverse` | no: `border.focus` on `accent` is about 1:1 in calm, and ADR-0118 draws the ring over the lip |
     | secondary, framed ghost | `button.fill`, `.hover`, `.pressed` | `text.primary`, `text.secondary`, chip pairs | `border.focus` |
     | checked | `accent.subtle`, `accent.subtle.hover` | `text.primary`, `text.secondary`, `accent` | `border.focus` |
     | danger with text | `action.destructive.subtle`, `.hover` | `state.error`, `text.secondary` | `border.focus` |

   - **Where.** Each ink is checked at both ends of the ink band: the `type.label` line box, centred in each control height (`size.control.sm`, `.md` and `.lg` after density). The gradient is linear and monotonic per channel, so the two band ends bound every row of ink. A ring pair is checked at both box edges, where the ring lies.
   - **Search.** The ceilings are scaled down per kind, in steps of 0.01:
     1. `face.light`, with `face.dark` at 0, down to the largest value that passes;
     2. then `face.dark`, with that `face.light`, the same way.

     The result is the minimum over every state and control height of the kind, so a kind's face has one shape in every state and size. When nothing passes, both are 0 (a flat face).
   - **Skipped pairs.** A pair that already fails in calm is skipped. That covers a pair ADR-0072 warns about and a pair with no rule. So a user theme with a known warning does not also lose its effects.
   - **Notices.** Scaling shows no notice. Glass shows the effects notice "This theme's colours leave no room for Glass shading" when every kind's face is flat.
   - **The look this gives with the built-in colours** (light / dark fraction, from the Glass ceilings 0.3 and 0.3, regular density):

     | Kind | Dark | Light |
     | --- | --- | --- |
     | primary | 0.30 / 0.10 | 0.01 / 0.30 |
     | secondary | 0 / 0.30 | 0.30 / 0.04 |
     | checked | 0.05 / 0.30 | 0.30 / 0.05 |
     | danger | 0 / 0.30 | 0.30 / 0.03 |

     The gradients are one-sided: Dark darkens toward the bottom, and Light mostly lightens the top. Glass reads through its highlight line, its shadow and this shading.
6. **Where the checks run, and the focus ring.**
   - **The checks run in `resolve`, for every density.** `ResolvedTheme::effect_draw(density)` picks the scaled values for the density in use, and `slot()` reads them.
     - `resolve` runs the search of decision 5 for regular, roomy and compact steps 1 to 3. That is five sets, a few hundred contrast ratios, well under 1 ms.
     - The effects notice is stored once in `ResolvedTheme::warnings`. It is shown when it applies to any density, and says which one when not all of them.
     - The strict built-in check in `Tokens::parse` runs the same loop.
   - **Under the button's own ring.**
     - The gradient stays under the button's ring. The ring keeps its rule on both ends (decision 5).
     - The highlight line, the rim and the inner glow turn off while `focus.ring` shows, as the sheen and glow do in ADR-0120.
     - A SplitButton keeps ADR-0120's rule: every effect outside its shape is off while either ring shows.
   - **The inner glow under the ink.**
     - The inner glow's residual (ADR-0120's linear model, plus 0.5 px for the Gaussian tail) is blended into the face at the ink band's nearest distance from the edge before the text pairs are checked.
     - That distance is `(control height − type.label.line) / 2`: 2 px on a dense button and 4 px on a regular one, in regular density.
     - When the pairs fail, `inner.alpha` scales down the same way as the face.
   - **The live check.** ADR-0120 decision 9's check (ΔE2000 ≥ 15 from `color.state.live`) covers every derived colour: the key edge, the rim, the inner glow and both gradient ends of every kind.
   - Nothing reaches past ADR-0120's extent, so no neighbour, ring or live colour is touched outside the box.

### Drawing

7. **By kind.** Only these combinations are drawn. Every other combination is none. Each setting uses only the columns its token values make non-zero.

   | Kind | Shadow | Gradient | Highlight | Rim | Inner glow | Key edge | Press |
   | --- | --- | --- | --- | --- | --- | --- | --- |
   | `primary` | yes | yes | no: ADR-0118's lip sits there | no | no | yes | yes |
   | `secondary`, framed ghost | yes | yes | yes | `accent` | `accent` | yes | yes |
   | checked | as its kind | yes | yes | no: keeps its `accent` outline | no | as its kind | yes |
   | `danger` with text | yes | yes | no | `action.destructive` | `action.destructive` | yes | yes |
   | `live` | neutral shadow only | no: fixed colour | no | no | no | no | yes |
   | unframed ghost, icon-only danger, disabled | no | no | no | no | no | no | no |
   | `SplitButton` | as its kind, whole shape | one gradient across both parts | as its kind | as its kind | as its kind | as its kind | no |

   - **The rim.**
     - It draws where calm draws no outline, which supersedes ADR-0118 decision 3 for Glow.
     - It must stay distinguishable from a checked button's `accent` outline. The check is ΔE2000 ≥ 10 between the rim and `accent`. When the check fails, `rim.mix` scales down.
     - The screenshots show a checked and an unchecked button side by side in every setting.
   - **The SplitButton divider.** Its pair is checked on both gradient ends.
   - **A round (icon-only) button** insets its highlight by its own radius.
8. **Structure and cost.**
   - ADR-0120 decision 10's structure stays: the bases, then the face, then the lip and the content. The face background becomes the gradient.
   - A face whose resolved ends both equal its fill draws a flat colour.
   - Every new child is conditional on its resolved value, not on the kind alone:
     - `if fx-highlight` (alpha > 0);
     - `if fx-inner: Rectangle { inner-shadow-* }` (alpha > 0).

     Skia rasterises and caches an inset shadow even at blur 0, so binding `inner-shadow-*` on the face would give every lively setting a `BoxShadow`.
   - Calm adds no drawn item.
   - **The shadow cache.** Slint's cache holds 16 shadows (`MAX_CACHED_SHADOWS`), and an inset image is sized exactly to its element. In Glow a secondary uses two entries (shadow base and inner glow).
     - The implementing PR counts the distinct button sizes in the main window for each setting.
     - The performance gate (ADR-0120 decision 13, per setting) adds:
       - a gradient sweep: hover across a row of gradient buttons;
       - a meter-redraw run, where the GPU path redraws the whole window.

       A setting that thrashes the cache fails the gate.
9. **Tests.**
   - **Unit tests:**
     - the alias round trip;
     - format 3 → 4 reading, and the format 4 refusals;
     - the search of decision 5, including a built-in kind at its limit, a pair failing in calm that is skipped, and every density;
     - the inner glow's scaling;
     - the rim's ΔE from `accent`;
     - the live ΔE of every derived colour;
     - the built-in file is refused when any setting would need a notice.
   - **The rendered test** (`effects_render.rs`) renders Dark and Light, regular and compact, each setting.
     - Its rule becomes: every ring pixel keeps at least min(3, its calm ratio) against the ground pixel beside it **in the same render**.
     - Text is not measured from anti-aliased pixels. The test samples the face colour at the ink band's ends next to the label and computes the ratio against the known ink colour.
     - The rows are the ones it has today, plus a checked button.
   - **`drawn.rs`:**
     - It keeps reading `Theme.effects-*` atoms as calm.
     - Its face test becomes: the gradient colours are chosen by the same kind and state branches as `face-fill`.

     The gradient colours are `Theme.effects-*` properties, so decision 5 checks them, not the reader.
10. **Defaults.** Built-in themes stay calm. The token tables are:
    - **Glow:**
      - shadow blur 3, offset 1, alpha 0.5 dark and 0.3 light;
      - rim 0.55;
      - inner glow 6 px at 0.25;
      - face 0.12 light (a faint gradient).
    - **Glass:**
      - shadow blur 3, offset 1, alpha 0.5 dark and 0.3 light;
      - face 0.3 light and 0.3 dark (ceilings);
      - highlight 0.45 dark and 0.9 light.
    - **Tactile:**
      - edge 3, press 2, shade 0.55;
      - shadow blur 1, offset 0, alpha 0.5 dark and 0.3 light;
      - face 0.12 light.

    The resolved values after decisions 5 and 6 go in the screenshots' captions. The owner approves those, not the ceilings.
11. **Policing** is unchanged: only the library draws effects. `check-ui` already counts `@linear-gradient` and `inner-shadow-*`.
12. **U12 wording.** The last bullet of U12 becomes:

    > The calm setting has no skeuomorphism, gradients, heavy shadows or decorative animation. A theme may choose livelier effects (ADR-0120, ADR-0122); they are then drawn only by library components, never carry meaning, never use or spread the live colour, never lower a pair below its contrast rule, and never animate.

    `principles.md` changes in the implementing PR.

## Consequences

- Users get three looks that read at a glance, and they still travel with the theme:
  - **Glow:** a coloured rim and an inner glow;
  - **Glass:** shading, a highlight line and a soft shadow;
  - **Tactile:** a deeper key edge and press.
- No effect reaches a neighbour, so spacing, focus rings and the live colour need no new rule.
- The checks grow from "derived colours far from live" to every button pair at the ink band and the ring at both edges, for every kind, state, size and density. This is more work in `resolve`, still well under 1 ms.
- The primary looks the same in Glow as in calm, apart from its shadow. A bolder primary would need either a halo past the extent (left out for now, see below) or a primary rim.
- **On acceptance:**
  - ADR-0120's Review gets the pointer "decisions 1, 5, 6, 7, 8, 9, 10, 11 and 16 superseded in part by ADR-0122".
  - ADR-0118's Review gets "decision 3 superseded in part for Glow".
  - `docs/adr/README.md` updates the rows.
- **Follow-up (the implementing PRs):**
  - **Specs and docs:**
    - the themes spec: settings, format 4, tokens, decisions 4 to 6, the "Checks" paragraph and the test list;
    - the design system: per-setting rows and the Effects paragraph;
    - the glossary row "Effects (theme)";
    - the U12 wording.
  - **Code:**
    - `tokens.toml` format 4;
    - `effects.rs`: `Effects::ALL`, `LIVELY`, `index`, `draw(density)` and `check`;
    - `build.rs`: the slot fields;
    - Button, IconButton and SplitButton drawing;
    - the editor labels and hint;
    - the `EffectsProbe` rows;
    - the `RBS_EFFECTS` names in the measure mode;
    - `effects_render.rs` and `drawn.rs`, as in decision 9.
  - **Measurement and screenshots:**
    - measurements on the Mac and on Windows (RTX 4090, AMD iGPU, skia-software);
    - screenshots in dark, light and high contrast.

## Alternatives considered

- **Push ADR-0120's values harder only.** Done; at its bounds the settings still look alike (evidence above).
- **A halo past the extent, for the primary.**
  - This needs a blend check that depends on spacing and on Slint's tree order:
    - it lies over the ink of earlier siblings and under that of later ones;
    - under the ink of unfilled neighbours.
  - It would also tint the fixed live colour and preview pictures.
  - With the 4 px gap the rules scale it to about 0.37 alpha and a 5-9 px blur (2-4 px in compact), close to Vivid.
  - Left out pending the owner's choice (decision card, 2026-11-14).
- **Wider gaps for button rows while Glow is on** (a `space.button-row` token raised by the effects). This would give the halo room, but it changes layouts per theme and touches every row in the app. It is the way back to a halo if the owner wants one later.
- **One face value for every kind.** With the built-in colours the tightest pair allows about 0.01, so Glass would be flat everywhere. Per-kind scaling keeps each kind as bold as its own pairs allow.
- **Checking text at the face's top and bottom edges** instead of at the ink band. This is stricter with no benefit, because no ink sits at the edges.
- **Translucent (real glass) faces.** These would break the opaque-colour rule and every contrast check. Rejected; Glass is drawn with opaque gradients.
- **Per-theme free effect values.** Still too many interacting values (ADR-0120's reason stands).

## Review

### Round 1 (independent AI session, architecture-review, 2026-11-14)

Verdict: accept with changes. The reviewer found these problems:

- The Glass values failed text pairs on the gradient ends.
- The halo lay over earlier neighbours.
- The supersession header was incomplete.
- Effects sat under the focus ring.
- The rendered test's invariant was wrong.
- The inner glow needed a conditional child.
- The defaults broke the reach rule.
- Format and alias details were missing.
- The effects by kind were not in a table.
- The `mix` argument order was unclear.
- The live check had gaps.
- The cache cost was not stated.
- The follow-up list had gaps.

All of these were answered in the next revision.

### Round 2 (independent AI session, architecture-review, 2026-11-14)

Verdict: accept with changes. The reviewer raised these points:

- **R1:** the Tactile defaults broke ADR-0120's kept bounds, and the extent parse rule still held the glow term.
- **R2:** the pair list and the ring rule were wrong for the primary, and the search order was undefined.
- **R3:** the halo neighbour model was incomplete.
- **R4:** the halo tinted the live colour.
- **R5:** density-dependent checks had no home.
- **R6:** the rendered test compared against the wrong ground.
- **R7:** the rim changed ADR-0118 decision 3 and resembled the checked state.
- **R8:** the predicted resolved values were missing; the halo resolved close to Vivid.

There were also nits O1 to O12.

This revision answers them as follows:

| Point | Answer |
| --- | --- |
| R1 | The header and decision 3 |
| R2 | Decision 5 |
| R3, R4 | The halo was removed, pending the owner's choice; see the Alternatives |
| R5 | Decision 6 |
| R6 | Decision 9 |
| R7 | The header and decision 7 |
| R8 | The Context and decision 5 |
| O1 | The table in decision 7 |
| O2 | Decision 5 |
| O3 | Decision 8 |
| O4 | Decision 4 |
| O5 | Decision 6 |
| O6, O7 | The header and decision 2 |
| O8 | Decision 6 |
| O9 | Decision 9 |
| O10 | Decision 10 |
| O11 | Moot without the halo |
| O12 | Decision 5 |

Round 3 is pending.
