# ADR-0122: Bold theme effects: Glow, Glass and a deeper Tactile

- Status: Accepted
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
      - the test "every library component's preferred and minimum sizes are identical in every setting and every density": in Glow the library containers that opt in to the halo (decision 8) grow by the zone;
      - "a neighbour's effect may reach the gap but never another box" (around a primary, decision 8's clear zone applies);
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

`docs/ui/screenshots/effects/bold-v2-{dark,light}.png` showed this direction and was approved ("yeni görseller çok daha iyi olmuş", "the new images are much better", 21:58). `bold-v3-{dark,light}.png` draws it with the exact resolved values, a checked button and real button heights:

- **Glow:** a 12 px halo at 0.6 on the primary, which keeps a 13 px clear zone.
- **Glass:** a gloss and bottom shading, one strong side per kind, and no line.
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
   1. **Top, at 0 %:** the fill mixed toward white by `gloss`.
   2. **The gloss stop:** the fill. Its position is a fraction of the button's own height.
      - Its lowest allowed position is the **band top**, `(height − type.label.line) / 2`. That is 4 of 28 px on a regular button (`size.control.md`), 2 of 24 on a dense one, and 6 of 32 on a roomy one.
      - Where the kind's text pairs still pass with the gloss reaching into the band, the stop moves down, at most to 45 %. Decision 5's search sets it.
      - The stop is computed per instance from `height`, because callers may set a height. A button is never shorter than `size.control.sm`, so the three heights of the check bound every instance.
   3. **Bottom, at 100 %:** the fill mixed toward black by the scaled `face.dark`.

   - Between the gloss stop and the bottom, the gradient is linear from the fill to the bottom colour.
   - Hover and press swap in that state's stops. Nothing animates.
   - A SplitButton's hovered or pressed part takes that state's gradient as well, not a flat patch.

   **Pairs.** For each kind, `slot()` checks the pairs in this table, using the existing rules (`CONTRAST_RULES` and `THEME_CONTRAST_RULES`):

   | Kind | Fills (rest, hover, pressed) | Inks | Ring |
   | --- | --- | --- | --- |
   | primary | `accent`, `.hover`, `.pressed` | `text.inverse` | no: `border.focus` on `accent` is about 1:1 in calm, and ADR-0118 draws the ring over the lip |
   | secondary, framed ghost | `button.fill`, `.hover`, `.pressed` | `text.primary`, `text.secondary`, chip pairs | `border.focus` |
   | checked | `accent.subtle`, `accent.subtle.hover` | `text.primary`, `text.secondary`, `accent` | `border.focus` |
   | danger with text | `action.destructive.subtle`, `.hover` | `state.error`, `text.secondary` | `border.focus` |

   **The bar.** A pair must reach `min(rule + 0.05, its calm ratio)`.
   - The 0.05 margin covers Skia's rounding between stops.
   - A pair that sits within the margin in calm may not get worse than calm, and it does not flatten the kind for that reason alone.
   - Only a pair whose calm ratio is under its rule is skipped. ADR-0072 already warns about such a pair, so a user theme with a known warning does not also lose its effects.
   - Every pair in the table has a rule today, so each one is always checked.

   **Where the pairs are checked.**
   - Each ink is checked at both ends of the ink band, for `size.control.sm`, `.md` and `.lg` after density. The gradient is monotonic per channel within each segment, so the colours at the band ends and at the gloss stop bound every row of ink. When the stop lies inside the band, the colour at the stop is checked too.
   - A ring pair is checked at the bottom edge.
   - At the top edge the gloss turns off while the button's `focus.ring` shows: the top stop then equals the fill.

   **Search.** The gloss stop is one fraction `s` per kind. The effective stop of an instance is `max(band top / height, s)`. Per kind, in steps of 0.01:
   1. `face.dark` is scaled down to the largest value that passes for every state and height of the kind.
   2. Then `s` grows from 0 while every pair still passes, to at most 0.45.

   A kind's face therefore has one shape in every state and size. `gloss` itself is not scaled, because at the band top it touches no pair.

   **Notices.** Scaling shows no notice. Glass shows the effects notice "This theme's colours leave no room for Glass shading" when `gloss` is 0 and every kind's `face.dark` is 0.

   **The built-in colours give these values.** They come from the ceilings: `face.dark` 0.3, `gloss` 0.3, and the gloss stop at 45 % at most. The values are checked over every height.

   | Kind | Dark: `face.dark` | Dark: gloss stop | Light: `face.dark` | Light: gloss stop |
   | --- | --- | --- | --- | --- |
   | primary | 0.09 | 45 % | 0.30 | band top |
   | secondary | 0.30 | band top | 0.03 | 45 % |
   | checked | 0.30 | 10 % | 0.03 | 45 % |
   | danger | 0.30 | band top | 0.02 | 45 % |

   Glow and Tactile resolve as follows:
   - **Glow:**
     - Inner glow: 0 in Dark for every kind. `text.secondary` on `button.pressed` (4.54 in calm) and the danger pairs leave no room. In Light it is 0.19 for secondary and 0.05 for danger.
     - Rim: 0.55 everywhere, which passes the ΔE ≥ 10 check from `accent`.
     - In Dark, Glow's secondaries are therefore drawn by their rim alone.
   - **Tactile:**
     - Tactile runs the same search on its own tokens: gloss 0.12 and `face.dark` 0, so there is no bottom shading.
     - Its resolved `s`:
       - Dark: primary 45 %, checked 14 %, secondary and danger band top;
       - Light: primary band top, the other kinds 45 %.

   Each kind gets one strong side:
   - **Where the ink is dark** (Light's secondaries and Dark's primary), the gloss is tall.
   - **Where the ink is light** (Dark's secondaries and Light's primary), the gloss is a 2 to 6 px band at the top, and the shading is at the bottom.

   `docs/ui/screenshots/effects/bold-v3-{dark,light}.png` draws exactly these values.
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
     - The inner glow's residual (`effects::residual`: `alpha · max(0, 1 − d / blur)` at distance `d`, evaluated at `d − 0.5 px` for the Gaussian tail) is blended into the face at the ink band's nearest distance from the edge before the text pairs are checked.
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
8. **The halo and its clear zone.** The halo is the primary's outer glow. It is the one effect that reaches past the extent, and it never reaches anything else.
   - **The halo's reach.**
     - Slint cuts a drop shadow's texture at exactly `blur` past the shape. So the halo ends `halo.blur` past the box on every side, with a hard step of about 1.4 % `accent` (0.5·erfc(√2)·0.6).
     - The halo base runs to the box's bottom plus the key edge. So a setting with `halo.alpha` above 0 must have `key.edge` 0 (a parse rule), and the halo reaches the same distance on all four sides.
   - **The clear zone.**
     - A new theme slot length, `Theme.halo-clear`, is `halo.blur + 1 px` when the setting draws a halo, and 0 otherwise. The 1 px is a rounding margin.
     - It is computed in `slot()`. It is the same in every density, because the halo is.
     - Within `halo-clear` of a primary Button's box, on every side, nothing may be drawn: no other control, text, divider or picture, and no clip edge of its container. Only the ground that the primary itself stands on may be there.
     - Beside a SplitButton, the zone is `halo-clear + size.focus-ring`, because its rings sit outside its shape.
   - **A halo only where the layout opts in.**
     - Button and SplitButton get the property `halo-room: bool`, false by default. A primary draws its halo only when the setting has one **and** its instance sets `halo-room: true`.
     - A library container that holds a primary passes the property through, also false by default: the app's `ToolbarCell` now, and `DialogActions` in its own PR. EmptyState gets the pass-through only once a fixed-size EmptyState exists. It opts in per instance, never for every instance.
     - Every other primary looks exactly as in calm, apart from its shadow. It needs no zone, and no layout around it changes.
     - The choice is made by layout, in the app files. Nothing is decided per frame.
   - **A position that opts in reserves the zone.**
     - The zone is reserved whatever the button's current kind. Go live becomes secondary or `live` while live, and the layout must not jump at that moment. So the zone is reserved for the position, not for the current kind.
     - Every spacing or padding at the position is `max(<today's value>, Theme.halo-clear)`, or `max(<today's value>, Theme.halo-clear + size.focus-ring)` beside a SplitButton. Today's value is `space.1`, `space.2`, `size.field.gap`, a panel padding, or a fixed height such as the frame bar's.
     - Each such spacing is written `Theme.halo-clear > 0 ? max(<today's value>, Theme.halo-clear [+ size.focus-ring]) : <today's value>`. So in Calm, Glass and Tactile every layout stays exactly as it is, including a 0 spacing beside a SplitButton.
     - A container whose size the user can drag (a resizable panel) never opts in, because a dragged size cannot keep the zone. EmptyState instances fill splitter-sized panels today, so none of them opts in.
     - Two adjacent opted-in primaries share one zone between them, and their halos may overlap on the ground.
   - **The first positions to opt in:**
     - **The frame bar's Go live,** a Button or a SplitButton through `ToolbarCell`.
       - **Height.** In Glow the frame bar grows from `size.control.md + 2·space.1 + size.border` to `size.control.md + 2·(halo-clear + size.focus-ring) + size.border`. That is about 22 px more (59 px instead of 37).
       - **Caption regions.** Today FrameBar republishes its caption regions (ADR-0071) only when `lead-end`, `end-start`, `max-x` or `caption-on` change, so a height change alone would leave Windows with a stale caption row. FrameBar therefore also calls `publish-regions()` on `changed height`. A test switches Glow → Calm on a narrow window and checks the published row height. It checks the published height, not that the handler fired, because Go live's padding changes in the same switch.
       - **Other items keep their size.** Every other frame bar item keeps today's size, centred in the taller row. The toolbar separator gets a fixed height, `size.control.md − 2·space.1`, instead of stretching.
       - **The window buttons are the one exception** (ADR-0071). They span the full row height, as the caption row they report does, so `row_h` and the button height stay equal for the snap-layout hit test.
       - **Horizontal.** The end group keeps `space.2` between its items. Go live's own cell gets side padding of `max(space.2, halo-clear + size.focus-ring) − space.2` (0 outside Glow). That is about 7 px more on each side, toward the item before it and toward Record (a SplitButton whose dot is the fixed live colour).
       - **Which cell opts in.** The binding `halo-room: item.id == "stream.toggle"` sits at the `Shell.toolbar-fixed` instance in FrameBar, so Record's cell is not padded. `FIXED_COMMANDS` keeps Go live before Record, and the chips and the LiveIndicator come after Record, outside the zone.
       - **Fit.** `the_frame_row_fits_1280_by_720_in_every_density` gains Glow (U11).
     - **DialogActions in held-edit windows,** once DialogActions is implemented (ADR-0114). A window opts in only if its minimum size includes the zone. Inline confirmations in panels never opt in: Delete source, Remove global source, Make new key.

     Every other primary keeps `halo-room: false`. That includes:
     - TransitionGo and TransitionPanel (user-sized);
     - every EmptyState (splitter-sized panels);
     - the stream panel;
     - the setting-row primaries;
     - Properties "Show now";
     - the toast action;
     - the Appearance keep bar;
     - the shortcuts and vcam rows.

     A later change may opt more positions in, with screenshots, under this decision.
   - **The ui-review checklist** gains the item "a Button with `halo-room: true` has nothing but its ground within `Theme.halo-clear` on every side". A neighbour's own ADR-0120 effects are the one exception: its shadow, key edge, rim and inner glow are not ink, and the extent keeps them within 4 px of that neighbour's box.
   - **Layouts change in Glow, and only at the opted-in positions.** The space grows by up to the zone on each side, in every density. A switch to or from Glow re-lays those rows out once.
   - **The halo is off while the primary's own ring shows.** It does not follow hover or press (ADR-0120 decision 12).
9. **Structure and cost.**
   - ADR-0120 decision 10's structure stays: the bases, then the face, then the lip and the content.
     - The halo is the primary's second base, as ADR-0120's glow base was.
     - The face background becomes the gradient. A face whose stops all equal its fill draws a flat colour.
   - Every new child is conditional on its resolved value, not only on its kind:
     - `if fx-halo` (alpha > 0);
     - `if fx-inner: Rectangle { inner-shadow-* }` (alpha > 0).

     Skia rasterises and caches an inset shadow even at blur 0, so binding `inner-shadow-*` on the face would give every lively setting a `BoxShadow`.
   - Calm adds no drawn item. `Theme.halo-clear` is 0 there, so the layouts that use it keep today's spacing.
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
      - `halo-clear = halo.blur + 1` when the halo draws, and 0 otherwise;
      - the parse rule `key.edge = 0` when `halo.alpha > 0`;
      - the gloss stop search, and the per-instance stop position for each height;
      - the built-in file is refused when a setting would need a notice.
    - **The rendered test** (`effects_render.rs`):
      - It renders Dark and Light, regular and compact, in each setting.
      - Its rule becomes: every ring pixel keeps at least min(3, its calm ratio) against the ground pixel beside it in the same render.
      - For the halo, each opted-in position is also rendered in Glow with **every effect of the primary off**, keeping `halo-clear`. In that render, every pixel within the primary's box plus `halo-clear` and outside the box must equal the ground. The only exceptions are a neighbour's own effects, within the extent of that neighbour's box. This tests that nothing else lies in the zone.
      - The halo tests run on the real FrameBar through `MainWindow`, built from hand-made toolbar data in `effects_render.rs` (Skia, one platform per process), not on a copy.
      - **The halo against a focus ring:**
        - The test renders Glow with the halo on, both as plain Buttons and as `vertical-menu` SplitButtons.
        - It focuses each neighbour of Go live in turn: the item before it, Record and Record ▾.
        - It applies the ring rule to each ring.
        - The zone check uses `halo-clear + size.focus-ring` beside a SplitButton.
      - `effects_never_change_the_layout` becomes: sizes are identical in every setting, except at opted-in positions in Glow, which grow by exactly the zone.
      - Text is not measured from anti-aliased pixels. The test samples the face colour at the ink band's ends next to the label and computes the ratio against the known ink colour.
      - Its rows are the ones it has today, plus a checked button.
      - The DialogActions row waits for DialogActions to be implemented.
      - `EffectsProbe` grows to fit the new rows and gains one opted-in row, so that the layout exception is exercised. Its fixed window size is test data.
    - **`drawn.rs`:**
      - It keeps reading `Theme.effects-*` atoms as calm.
      - Its face test becomes: the gradient stops are chosen by the same kind and state branches as `face-fill`.
      - A `max(..., Theme.halo-clear)` spacing is a length, so the reader is not affected.
11. **Defaults.** Built-in themes stay calm. The token tables are:
    - **Glow:**
      - halo 12 px at 0.6, key edge 0;
      - rim 0.55;
      - inner glow 6 px at 0.25;
      - shadow blur 3, offset 1, alpha 0.5 dark and 0.3 light.
    - **Glass:**
      - gloss 0.3, with the gloss stop at most 45 %;
      - face.dark 0.3 (a ceiling);
      - shadow blur 3, offset 1, alpha 0.5 dark and 0.3 light.
    - **Tactile:**
      - edge 3, press 2, shade 0.55;
      - shadow blur 1, offset 0, alpha 0.5 dark and 0.3 light;
      - gloss 0.12.

    The resolved values are in decision 5's table, and `bold-v3-*` draws them. The owner approves those values, not the ceilings.
12. **Policing** is unchanged: only the library draws effects. `check-ui` already counts `@linear-gradient`, `inner-shadow-*` and `Theme.effects-*`. `Theme.halo-clear` is not an effect and is not counted.
13. **U12 wording.** The last bullet of U12 becomes:

    > The calm setting has no skeuomorphism, gradients, heavy shadows or decorative animation. A theme may choose livelier effects (ADR-0120, ADR-0122); they are then drawn only by library components, never carry meaning, never use or spread the live colour, never lower a pair below its contrast rule, never reach another control (the primary's halo keeps a clear zone), and never animate.

    `principles.md` changes in the implementing PR.

## Consequences

- Users get three looks that read at a glance, and the look still travels with the theme:
  - **Glow:** a strong halo on the primaries that have room for it, and a coloured rim on the others, with an inner glow in Light;
  - **Glass:** gloss and shading with a soft shadow;
  - **Tactile:** a deeper key edge and press.
- **Glow changes layout at the opted-in positions.** The frame bar grows by about 22 px, and Go live gets about 7 px more on each side. Later, held-edit dialog actions get more room around the primary. A switch to or from Glow re-lays them out once. This is a first: until now a theme never changed a layout beyond density.
- The check grows from "derived colours far from live" to every button pair at the ink band, for every kind, state, size and density. This adds work in `resolve`, still well under 1 ms.
- **On acceptance:**
  - ADR-0120's Review gets the pointer "decisions 1, 5, 6, 7, 8, 9, 10, 11 and 16 superseded in part by ADR-0122; Vivid is renamed Glow throughout".
  - ADR-0118's Review gets "decision 3 superseded in part for Glow".
  - `docs/adr/README.md` updates the rows.
- **Follow-up (the implementing PRs):**
  - **Docs:**
    - the themes spec: settings, format 4, the tokens, decisions 4 to 8, the "Checks" paragraph and the test list;
    - the design system: per-setting rows, the Effects paragraph and `halo-clear`;
    - the glossary row "Effects (theme)";
    - U12;
    - the two ui-review items.
  - **Code:**
    - `tokens.toml` format 4;
    - `effects.rs`: `Effects::ALL`, `LIVELY`, `index`, `draw(density)` and `check`;
    - `build.rs`: the slot fields and `halo-clear`;
    - Button, IconButton and SplitButton drawing;
    - the `halo-room` property on Button and SplitButton, its pass-through on `ToolbarCell`, and the frame bar's opted-in Go live, separator height and `changed height => publish-regions()`;
    - `effects_render.rs` grows from Dark and compact step 1 to Dark and Light, regular and compact;
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
  - Rejected: the owner wants the halo strong ("glow dışarı doğru olsun", "let the glow go outward"). A clear zone that covers the halo makes every check local again.
- **A wider gap in every button row while Glow is on** (revision 3): it shrinks calm rows that use `space.2` today, and it grows rows that hold no primary, overrunning `size.mixer.min-h`. It is replaced by the clear zone around primaries only.
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

### Round 3 (independent AI session, architecture-review, 2026-11-14)

Verdict: accept with changes. The reviewer raised these points:

- **1:** the halo reached non-button content in every direction.
- **2:** the halo covered a SplitButton neighbour's outside ring.
- **3:** the gap ignored the key edge.
- **4:** a gap in every button row shrank calm rows and grew rows without a primary.
- **5:** the `face.dark` table failed the margin, and the margin was underspecified.
- **6:** the band arithmetic used the wrong height, and the gloss was only 4 px.
- **7:** the rendered-test invariant was untestable.
- **8:** the screenshots lacked a checked button and the resolved values.
- **Optional:** the halo's hard step and clip edge; naming the residual model; relayout cost; the wording of the skipped pairs.

Revision 4 answers:

| Point | Answered in |
| --- | --- |
| 1, 2, 3, 4 | Decision 8: a clear zone around primaries only, plus `size.focus-ring` beside a SplitButton, and `key.edge` 0 with a halo |
| 5 and 12 | Decision 5: the bar `min(rule + 0.05, calm)`, and the table recomputed over every height |
| 6 | Decision 5: the heights are corrected, and the gloss stop moves down to 45 % where the pairs allow |
| 7 | Decision 10: a render with the halo forced off as the reference |
| 8 | `bold-v3-*` and decision 5's table |
| 9 | Decision 8: the hard step, and the clip edge inside the zone |
| 10 | Decision 6 |
| 11 | Decision 8 and the Consequences |

### Round 4 (independent AI session, architecture-review, 2026-11-14)

Verdict: accept with changes. The reviewer found that the decision 5 table, the halo's reach, the key-edge rule, the SplitButton term, the inner-glow model and format 3 reading hold. The concerns were:

- user-sized containers cannot keep the zone;
- the frame bar is a fixed height;
- buttons change kind at runtime;
- ADR-0120 decision 11's layout test conflicts;
- Glow's resolved values were missing (the inner glow is 0 in Dark), and the Dark mock-up drew one;
- the halo test proved nothing.

Minor points:

- the gloss stop variable was undefined;
- the SplitButton zone is 11 px;
- the list of places was incomplete;
- the test extension was not listed as follow-up work.

Revision 5 answers:

| Point | Where it is answered |
| --- | --- |
| User-sized containers, frame bar, kind changes | Decision 8: a halo only at positions that opt in (`halo-room`); the zone is reserved for the position; resizable panels never opt in; the frame bar's growth is stated |
| ADR-0120 decision 11's layout test | Header and decision 10 |
| Glow's resolved values | Decision 5; `bold-v3-dark.png` redrawn with no inner glow in Dark |
| The halo test | Decision 10 |
| Minor points | Decisions 5 and 8, and the follow-up list |

### Round 5 (independent AI session, architecture-review, 2026-11-14)

Verdict: accept with changes. The reviewer confirmed that the frame bar arithmetic, the decision 5 table, the Dark Glow mock-up and the position-reserved zone hold. The concerns were:

1. EmptyState sits in resizable panels.
2. DialogActions is not implemented yet and is also used inline.
3. `halo-room` was missing on SplitButton.
4. The halo test failed by construction because of the primary's own shadow.
5. The frame bar's horizontal zone and the test on a copy.
6. Tactile's stops.

Revision 6 answers:

| Concern | Where it is answered |
| --- | --- |
| 1 to 3 | Decision 8: pass-through properties, EmptyState not opted in, DialogActions per instance and later |
| 4 and 5 | Decision 10, and decision 8's horizontal spacing |
| 6 | Decision 5: Tactile's own search and resolved `s` |
| Optional a to c and e | Taken |
| Optional d | Cosmetic |

### Round 6 (independent AI session, architecture-review, 2026-11-14)

Verdict: accept with changes. Every answer from round 5 holds in the code: the pass-through, the frame bar arithmetic, the order of `FIXED_COMMANDS`, the forced-calm high contrast, Tactile's values, and the real FrameBar being testable. Three small required changes:

1. **Caption regions:** they are not republished when only the height changes.
2. **Toolbar separator:** it stretched with the taller bar.
3. **Neighbour rings:** no test checked a neighbour's ring against a lit halo.

Revision 7 answers them in decisions 8 and 10. It also takes the optional suggestions:

- Go live's cell padding, and opting in by item id;
- where the halo test lives;
- no EmptyState pass-through for now;
- DialogActions in its own PR;
- the Vivid rename in the ADR-0120 pointer;
- an opted-in row in `EffectsProbe`.

### Round 7 (independent AI session, architecture-review, 2026-11-14)

Verdict: accept with one small change. All three round-6 items are fixed and buildable, and the optional suggestions created no contradiction.

- **The change:** the window buttons are an exception to "other items keep their size", because the snap-layout hit test uses the full row height. Added to decision 8.
- **Optional points, taken:**
  - the caption test checks the published height;
  - the opt-in binding sits at the `Shell.toolbar-fixed` instance;
  - the growth is stated as "up to the zone".

The review is complete.

### Owner approval

2026-11-14 23:20: owner approval (Salih: "merge et. onaylıyorum"). Status Accepted.

### Measurement note: gradient hover cost (2026-11-15)

The Mac measurement of decision 9, repeated past the first-run page (main `ad0a0402`, M4 Pro, 12 runs): idle 0 frames, resize, redraw and hover within the gates, the max rise at most +0.90 ms. The gradient sweep exceeds the p95 gate of +0.5 ms for Glass (+0.63 ms) and Tactile (+0.57 ms); the worst frame is 4.15 ms against a 16.7 ms budget, drawn only when a hover state changes. Owner (Salih, 2026-11-15): "çok sorun değil bu taşmalar", so the overrun is accepted and the gate stays as written. Numbers: `docs/ui/screenshots/effects/measurements-adr-0122.md`. The AMD iGPU, skia-software and Windows runs remain open.

2026-11-17: decision 8's frame-bar position, decision 10's frame-bar test lines and the frame-bar line in Consequences are superseded by [ADR-0136](0136-no-halo-in-the-frame-bar.md): the frame bar no longer opts in to the halo and keeps its height in Glow.
