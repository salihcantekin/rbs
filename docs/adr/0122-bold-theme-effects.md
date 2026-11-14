# ADR-0122: Bold theme effects: Glow, Glass and a deeper Tactile

- Status: Proposed
- Date: 2026-11-14
- Deciders: Salih (owner)
- Builds on: [ADR-0120](0120-theme-effects.md) (theme effects), [ADR-0118](0118-tint-buttons.md) (Tint buttons), [ADR-0057](0057-user-themes-and-appearance-settings.md), [ADR-0072](0072-theme-colour-rules-warn.md), [ADR-0013](0013-zero-cost-when-unused.md), [ADR-0015](0015-ui-design-rules.md)
- Supersedes in part these ADR-0120 decisions:
  - decision 1: the setting names (Vivid);
  - decision 5: the editor labels;
  - decision 6: the token table;
  - decision 7: derived colours;
  - decision 8: effects by kind;
  - decision 9: these parts:
    - "text and icons never sit on an effect … no text pair changes";
    - "checks cover only the pairs an effect changes";
    - "nothing of an effect sits under a focus ring";
  - decision 10: the face structure (a gradient face, more children);
  - decision 11: these parts:
    - "may reach the gap but never another box";
    - the sheen clamp;
  - decision 16: the U12 wording.

  Everything else in ADR-0120 stays, in particular the extent formula of decision 11 for everything every button draws.

## Context

ADR-0120 shipped Calm, Vivid and Tactile. At its tightest bounds the owner found the settings too alike. Those bounds are:

- 4 px reach past the box;
- a 2 px sheen band;
- no face gradient.

The owner's words (2026-11-14 21:07): "the differences are not visible without looking closely; options like Glowy or Glassy that are clearly different would be better". The evidence:

- `docs/ui/screenshots/effects/max-{dark,light}.png`: ADR-0120 at its bounds;
- the mock-up `docs/ui/screenshots/effects/bold-{dark,light}.png` (Calm, Glow, Glass, Tactile), which the owner approved as the direction ("uygun", 21:26).

The mock-up was drawn freehand, outside the library and outside the contrast rules. Its gaps are wider than `space.1`, and its gradients are stronger than the built-in colours allow under the text rules (see decision 5). It shows the direction, not the values that ship. The values that ship are the resolved ones, and the owner approves them on screenshots of the real components (decision 9).

Most of what makes a look read at a glance is **the face**:

- a vertical gradient;
- a highlight line;
- a coloured rim;
- an inner glow.

These stay inside the box. Only the primary's **halo** (the outer glow) reaches out further than ADR-0120 allows.

The forces are those of ADR-0120:

- opaque colour tokens and contrast rules;
- zero cost when unused: a `BoxShadow` item for each bound shadow;
- focus rings stay readable;
- the live colour stays fixed;
- high contrast stays plain;
- no animation;
- Slint's software renderer draws no shadows.

Two Slint facts shape the design:

- `a.mix(b, f)` gives weight `f` to `a`.
- A child's `drop-shadow` is painted at that child's place in tree order. So a primary's halo lies **over** every sibling drawn before the primary (left of it, or above it) and **under** every sibling drawn after it.

## Decision

### Settings and formats

1. **Settings.** A theme's `effects` is `"calm"`, `"glow"`, `"glass"` or `"tactile"`.
   - The editor's segmented control shows Calm, Glow, Glass and Tactile.
   - `Effects::from_name("vivid")` gives Glow, and `name()` never returns `"vivid"`.
   - A stored `"vivid"` is kept as stored (ADR-0120's round-trip rule). A save that does not touch the Effects row writes it back unchanged, and the theme is not dirty. The editor shows such a theme as Glow, and writes `"glow"` only when the user picks a segment.
   - Downgrade: a theme saved as `"glow"` or `"glass"` reads as calm in an ADR-0120 build, with its effects notice (unknown setting). This is acceptable because effects never carry meaning.
   - Theme documents stay at format 2.
2. **Token file format 4.**
   - It has `[effects.glow]`, `[effects.glass]` and `[effects.tactile]`. Each holds every token of decision 3.
   - It still needs `[font]`.
   - A format 4 file is refused when it has `[effects.vivid]` or any `effects.sheen.*` token (`deny_unknown_fields`).
   - A format 3 file is read like this:
     - `[effects.vivid]` becomes Glow, and `[effects.tactile]` becomes Tactile. Every new token is 0, and `sheen.*` is dropped (Tactile's sheen of 0.12 disappears; this is the only loss).
     - Glass resolves to calm with the notice "This build's token file has no Glass effects".
   - A format 2 file is read as ADR-0120 says.
3. **Tokens.** Each table holds all of them. Bounds are inclusive, and fractions run from 0 to 1.
   - ADR-0120's shadow, key edge and press tokens stay with their bounds and the rule press ≤ key edge.
   - `effects.glow.*` is renamed `effects.halo.*`, because "glow" inside `[effects.glass]` would read as a contradiction.
   - `effects.sheen.*` is removed; the highlight line replaces it.
   - **Face fractions are ceilings.** Decision 5 scales each one down per kind until that kind's pairs pass.

   | Token | Kind | Bounds | Meaning |
   | --- | --- | --- | --- |
   | `effects.halo.blur` | length | 0-14 | Blur of the primary's halo (outer glow) |
   | `effects.halo.alpha` | fraction | 0-0.8 | Its opacity at the box edge |
   | `effects.face.light` | fraction | 0-0.4 | At most, the face's top is its fill mixed toward white by this |
   | `effects.face.dark` | fraction | 0-0.3 | At most, the face's bottom is its fill mixed toward black by this |
   | `effects.highlight.alpha.dark` / `.light` | fraction | 0-0.9 | A `size.border` white line inside the top edge, inset by the button's own radius |
   | `effects.rim.mix` | fraction | 0-0.8 | The border of a secondary or framed ghost takes its kind's ink colour mixed into `color.border.subtle` by this |
   | `effects.inner.blur` | length | 0-8 | Blur of an inner glow (inset shadow) in the same colour |
   | `effects.inner.alpha` | fraction | 0-0.4 | Its opacity |

### Colours and checks

4. **One definition of every derived colour, computed in Rust.**
   - `slot()` computes every derived colour and hands Slint finished colours. Slint does no mixing.
   - This covers the gradient ends (top and bottom) for each kind and state (rest, hover, pressed), the rim, the inner glow, the halo and the key edge.
   - The checks use the same function, so what is checked is bit-identical to what is drawn.
   - The mixing is in encoded sRGB:
     - `toward_white(c, f) = c + (255 − c)·f` per channel;
     - `toward_black(c, f) = c·(1 − f)`;
     - rounding to nearest.
   - `slot()` runs after density (`pack`), so every check that depends on a length sees the density's lengths.
5. **The face gradient and its check.**
   - When a button draws effects, its face background is a vertical gradient from the top colour to the bottom colour.
   - Hover and press swap in that state's pair of colours. Nothing animates.
   - For each kind and state, `slot()` scales `face.light` and `face.dark` down independently, to the largest values (in steps of 0.01) at which all of the following pass:
     1. every text and icon pair that kind draws (the button pairs of `THEME_CONTRAST_RULES`) passes its rule on the gradient colour at the top and the bottom of the ink band. The ink band is the `type.label` line box centred in the control height of the density, for both its regular and its small sizes. The gradient is linear and each channel is monotonic, so the two band ends bound every row in between.
     2. `color.border.focus` keeps 3:1 on both gradient ends. The ring sits at the edge.
     3. the existing rules for the flat fill are unchanged.
   - One end can scale to 0 while the other stays. With the built-in colours a dark theme's secondary keeps a deep bottom and an almost flat top, and Light's mostly lightens the top. That is the look that ships.
   - A pair that already fails in calm (ADR-0072 warns about it) is skipped by this check, so a user theme with a known warning does not also lose its effects.
   - Scaling shows no notice. A setting whose main feature scales to nothing does show the effects notice, naming the kind:
     - Glass: both face fractions are 0 on every kind;
     - Glow: the halo scales to 0 (decision 7).
6. **What sits under a focus ring.**
   - The gradient stays under a button's own ring, and the ring keeps 3:1 on both ends (decision 5).
   - The highlight line, the rim and the inner glow turn off while the button's `focus.ring` shows, as the sheen and glow do in ADR-0120.
   - A SplitButton keeps ADR-0120's rule: every effect outside its shape is off while either ring shows.
   - The inner glow's residual at the ink band's nearest distance from the edge is blended into the face for the text pairs of decision 5. With the built-in sizes that distance is 4 px regular and 2 px compact.
   - The live check (ADR-0120 decision 9: ΔE2000 ≥ 15 from `color.state.live`) covers every derived colour: the key edge, halo, rim, inner glow and both gradient ends of every kind.
7. **The halo (the primary's outer glow).** This is the one effect allowed past ADR-0120's extent.
   - **Reach.** It may reach `effects.halo.blur` past the box, over the gaps and over neighbours.
   - **Layering.** Slint paints it over the siblings drawn before the primary and under the ones drawn after. The check therefore uses the **over-ink model**: the halo residual (ADR-0120's linear model, colour `accent` at the residual alpha) is composited over **both** the neighbour's ink and its ground before their contrast is taken.
   - **Neighbours.** The list is derived from what a library control next to a button can draw, not written by hand:
     - every button fill and state of decision 8 with its pairs;
     - `button.chip` with `accent`;
     - `surface.base`, `.raised`, `.overlay` and `.sunken` with their text, accent and state inks;
     - fields and dropdowns: `surface.sunken` with `text.primary` and `text.secondary`;
     - `color.state.live` with `text.on-live`;
     - `accent` with `text.inverse` (two primaries side by side);
     - `color.border.focus` on each of those grounds, 3:1.
   - **Distances,** after density:
     - **ring distance:** the neighbour's ring is at the gap. `min(space.1 − key.edge, space.2 − size.focus-ring)` covers a key edge below the box and a SplitButton's outside ring.
     - **ink distance:** neighbour ink is at the gap plus that control's inner padding (`space.2` for a Button).
   - **Scaling.** When a check fails, `slot()` lowers `halo.alpha` first, down to half its value. Then it lowers `halo.blur`, then alpha again, until every check passes, and to 0 if nothing passes.
   - **Focus.** The halo is off while the primary's own ring shows.
   - **Look.** The asymmetry is accepted: a neighbour drawn before the primary is tinted on top, and one drawn after is clean. The residual at the ring distance is what the check bounds. Panels that clip their content cut the halo where their padding is narrower than the blur; that is allowed, because a clipped halo hides nothing.
   - **Tests.** See decision 10.

### Drawing

8. **By kind.** Only these combinations are drawn. Every other combination is none.

   | Kind | Shadow | Halo | Gradient | Highlight | Rim | Inner glow | Key edge | Press |
   | --- | --- | --- | --- | --- | --- | --- | --- | --- |
   | `primary` | yes | yes | yes | yes | no (keeps ADR-0118's lip instead of the highlight where both would draw: the lip wins, the highlight is off) | no | yes | yes |
   | `secondary`, framed ghost | yes | no | yes | yes | `accent` mix | `accent` | yes | yes |
   | checked | as its kind | no | yes | yes | no (keeps its `accent` outline) | no | as its kind | yes |
   | `danger` with text | yes | no | yes | no | `action.destructive` mix | `action.destructive` | yes | yes |
   | `live` | neutral shadow only | no | no (fixed colour) | no | no | no | no | yes |
   | unframed ghost, icon-only danger, disabled | no | no | no | no | no | no | no | no |
   | `SplitButton` | as its kind, whole shape | as its kind | one gradient across both parts | as its kind | as its kind | as its kind | as its kind | no |

   - The rim draws even where calm draws no outline (Dark's `button-outline` is false). It is the visible difference of Glow.
   - The SplitButton divider's pair is checked on both gradient ends.
   - A round (icon-only) button insets its highlight by its own radius.
   - Each setting uses only some of these columns, through its token values. With the default values, Glow uses the halo, rim and inner glow, Glass the gradient, highlight and shadow, and Tactile the key edge, press, shadow and a faint gradient.
9. **Structure and cost.**
   - ADR-0120 decision 10's structure stays: bases, then the face, then the lip and the content. The face background becomes the gradient.
   - The highlight is its own conditional child of the face, `if fx-highlight`.
   - The inner glow is its own conditional child, `if fx-inner: Rectangle { inner-shadow-* }`. Skia rasterises and caches an inset shadow even at blur 0, so binding `inner-shadow-*` on the face would give every lively setting a `BoxShadow`.
   - Calm adds no drawn item.
   - The reach of everything except the halo stays within ADR-0120's extent (4 px built in, 2 px compact). The Tactile default is therefore edge 3 plus a 1 px shadow, not the mock-up's edge 4 plus shadow.
   - **Shadow cache.** Slint's cache holds 16 shadows (`MAX_CACHED_SHADOWS`), and an inset image is sized exactly to its element. In Glow, a secondary uses two entries (shadow base and inner glow), and a primary uses two (shadow and halo).
     - The implementing PR counts the distinct button sizes in the main window for each setting.
     - The performance gate (ADR-0120 decision 13, per setting) adds a gradient sweep (hover across a row of gradient buttons) and a meter-redraw run, where the GPU path redraws the whole window.
     - A setting that thrashes the cache fails the gate.
10. **Tests.**
    - **Unit tests:**
      - the alias round trip;
      - format 3 → 4 reading and the format 4 refusals;
      - the scaling of decision 5: a built-in kind at its limit, and a pair already failing in calm that is skipped;
      - the halo check, with each distance and the scaling order;
      - the live ΔE check of every derived colour;
      - the built-in file is refused when a setting's checks would need a notice.
    - **The rendered test** (`effects_render.rs`) changes its pass rule from "every ring pixel is unchanged from calm" to: every ring pixel keeps 3:1 against the calm ground beside it, and every neighbour's ink keeps its rule. It covers:
      - regular and compact;
      - a horizontal row with a focused neighbour on each side of a primary with a halo;
      - a vertical pair;
      - the SplitButton row;
      - each setting.
    - `drawn.rs`:
      - It keeps reading `Theme.effects-*` atoms as calm.
      - Its test that the face's background equals the root's calm expression becomes: the face's background in calm equals the root's calm expression.
      - The gradient colours are `Theme.effects-*` properties, so the reader never sees them as a drawn pair. Decision 5 checks them.
11. **Defaults.** Built-in themes stay calm. The token tables start from the mock-up's values as ceilings:
    - **Glow:** halo 14 px at 0.75, rim 0.55, inner glow 6 px at 0.25, shadow 3/1;
    - **Glass:** face 0.3 light, 0.3 dark, highlight 0.45 dark / 0.9 light, shadow blur 3, offset 1;
    - **Tactile:** edge 3, press 2, shade 0.55, shadow blur 1, offset 0, face 0.12 light.

    The resolved values after decisions 5 and 7 go in the screenshots' captions, and the owner approves those, not the ceilings.
12. **Policing** is unchanged: only the library draws effects. `check-ui` already counts `@linear-gradient` and `inner-shadow-*`.
13. **U12 wording.** The last bullet of U12 becomes:

    > The calm setting has no skeuomorphism, gradients, heavy shadows or decorative animation. A theme may choose livelier effects (ADR-0120, ADR-0122); they are then drawn only by library components, never carry meaning, never use or spread the live colour, never lower a pair below its contrast rule, never cover a focus ring below 3:1, and never animate.

    `principles.md` changes in the implementing PR.

## Consequences

- Users get three looks that read at a glance, and they still travel with the theme.
- With the built-in colours the gradients are one-sided:
  - a dark theme's secondary darkens toward the bottom;
  - Light's lightens toward the top;
  - the primary keeps both ends.

  Glass reads through its highlight line and gradient more than through a symmetric sheen. The owner judges this on real screenshots before acceptance.
- The halo is a new kind of rule: a contrast check that depends on spacing and layering. It is lopsided by Slint's tree order. Its rendered test covers both sides.
- The checks grow from "derived colours far from live" to every button pair at the ink band and the ring at both ends, for every kind, state and density. This is more work in `slot()`, still well under 1 ms.
- **On acceptance:** ADR-0120's Review gets the pointer "decisions 1, 5, 6, 7, 8, 9, 10, 11 and 16 superseded in part by ADR-0122", and `docs/adr/README.md` updates both rows.
- **Follow-up (the implementing PRs):**
  - **Specs and docs:**
    - the themes spec: settings, format 4, tokens, decisions 5 to 7, and the test list;
    - the design system: per-setting rows and the Effects paragraph, which today says "rows at `space.1` or more never overlap";
    - the glossary row "Effects (theme)" names;
    - the U12 wording;
    - the ui-review row-spacing item: the halo may now cover the gap.
  - **Code:**
    - `tokens.toml` format 4;
    - `effects.rs`: `Effects::ALL`, `LIVELY`, `index`, `draw` and `check`;
    - `build.rs`: the slot fields;
    - Button, IconButton and SplitButton drawing;
    - the editor labels and hint;
    - the `EffectsProbe` rows;
    - `RBS_EFFECTS` names in the measure mode;
    - `effects_render.rs` and `drawn.rs`, as in decision 10.
  - **Measurement and screenshots:**
    - measurements on the Mac and on Windows (RTX 4090, AMD iGPU, skia-software);
    - screenshots in dark, light and high contrast.

## Alternatives considered

- **Push ADR-0120's values harder only:** done. At its bounds the settings still look alike (evidence above).
- **One face value for every kind:** rejected. With the built-in colours the tightest pair (danger ink on its hover fill in Dark, white on `accent` in Light) allows about 0.01, so Glass would be flat everywhere. Scaling per kind and per end keeps each kind as bold as its own pairs allow.
- **Checking text at the face's top and bottom edges instead of the ink band:** stricter with no benefit, because no ink sits at the edges.
- **Per-base face fractions:** these would still be bound by the tightest kind. Per-kind scaling covers what they would.
- **Wider gaps for every button row** (a `space.button-row` token raised by the effects): this would let every effect reach further, but it changes layouts per theme and touches every row in the app. Kept for later if the halo check proves too tight, and for a deeper Tactile.
- **A row-level layer under all buttons for the halo:** this would make the halo symmetric, but it needs every row in the app to host it. Rejected for now; the over-ink check makes the layering safe.
- **A halo kept inside the extent with more spread:** safer, but it reads like ADR-0120's Vivid. It is the fallback if the owner rejects the halo screenshots.
- **Translucent (real glass) faces:** this would break the opaque-colour rule and every contrast check. Rejected; Glass is drawn with opaque gradients.
- **Per-theme free effect values:** still too many interacting values (ADR-0120's reason stands).

## Review

### Round 1 (independent AI session, architecture-review, 2026-11-14)

Verdict: accept with changes. The reviewer found these problems:

- the mock-up's Glass values fail the text pairs on the gradient ends (Dark `text.primary` 4.40:1, Light white on `accent` 2.93:1);
- the halo lies over earlier neighbours' ink and rings in Slint's tree order, which the under-ink blend model missed;
- the check's distances, inputs and neighbour list were incomplete;
- the supersession header was incomplete (ADR-0120 decisions 9, 11 and 16);
- the highlight, rim and inner glow sat under the focus ring;
- the rendered test's invariant would break;
- the inner glow needed its own conditional child;
- the reach rule contradicted the defaults;
- the format and alias details were missing;
- the by-kind rules were prose rather than a table;
- Slint's `mix` argument order;
- the live check did not cover the new colours;
- the shadow-cache cost was not stated;
- the follow-up list had gaps.

This revision answers each point:

| Review point | Answered in |
| --- | --- |
| Glass values fail the text pairs | decision 5 and the Alternatives |
| Halo layering | decision 7 |
| Check distances, inputs, neighbours and scaling | decisions 4 and 7 |
| Supersession header | the header and decision 13 |
| Effects under the focus ring | decision 6 |
| Rendered test | decision 10 |
| Inner glow child | decision 9 |
| Reach rule against the defaults | decisions 9 and 11 |
| Formats and the alias | decisions 1 and 2 |
| Effects by kind | decision 8 |
| `mix` order | decision 4 |
| Live check | decision 6 |
| Cost | decision 9 |
| Follow-up list | the Consequences |

The optional suggestions were taken too: rename to halo; skip pairs already failing in calm; put the resolved values in the captions; note the mock-up's wider gaps in the Context. Round 2 is pending.
