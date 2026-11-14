# ADR-0122: Bold theme effects: Glow, Glass and a deeper Tactile

- Status: Proposed
- Date: 2026-11-14
- Deciders: Salih (owner)
- Builds on: [ADR-0120](0120-theme-effects.md) (theme effects), [ADR-0118](0118-tint-buttons.md) (Tint buttons), [ADR-0057](0057-user-themes-and-appearance-settings.md), [ADR-0072](0072-theme-colour-rules-warn.md), [ADR-0013](0013-zero-cost-when-unused.md)
- Supersedes in part: ADR-0120 decisions 6 (the token table and the extent rule as the only limit), 7 (derived colours), 8 (effects by kind), 10 (no whole-face gradient), 11 (sheen clamp) and the setting names of decision 1 (Vivid). Everything else in ADR-0120 stays.

## Context

ADR-0120 shipped Calm, Vivid and Tactile. At its tightest bounds (4 px reach past the box, a 2 px sheen band, no face gradient) the owner found the settings too alike: "the differences are not visible without looking closely; options like Glowy or Glassy that are clearly different would be better" (2026-11-14 21:07). The evidence: `docs/ui/screenshots/effects/max-{dark,light}.png` (ADR-0120 at its bounds) and the mock-up `docs/ui/screenshots/effects/bold-{dark,light}.png` (Calm, Glow, Glass, Tactile), which the owner approved as the direction ("uygun", 21:26).

What makes a look read at a glance is mostly **the face**: a vertical gradient, a highlight line, a coloured rim, an inner glow. These stay inside the box, so they need no extent and touch no neighbour. Only two things reach out further than ADR-0120 allows: the primary's glow (about 12 px in the mock-up) and Tactile's deeper edge (4 px plus its shadow).

Forces as in ADR-0120: opaque colour tokens and contrast rules, zero cost when unused (a `BoxShadow` item per bound shadow), focus rings never covered, live colour fixed, high contrast plain, no animation, Slint's software renderer drawing no shadows.

## Decision

1. **Settings.** A theme's `effects` is `"calm"`, `"glow"`, `"glass"` or `"tactile"`. `"vivid"` is read as `"glow"` and is kept as stored (ADR-0120's round-trip rule); the editor shows it as Glow and writes `"glow"` once the user picks a setting. The editor's segmented control shows Calm, Glow, Glass, Tactile. Theme documents stay format 2.
2. **Token file format 4** has `[effects.glow]`, `[effects.glass]` and `[effects.tactile]`, each holding every token below. A format 3 file is read with its `[effects.vivid]` as Glow and no Glass (Glass then resolves to calm with the notice "This build's token file has no Glass effects"); a format 2 file stays as ADR-0120 says.
3. **Tokens** (each table holds all; bounds inclusive; fractions 0 to 1). ADR-0120's tokens stay with these bounds: `effects.shadow.blur` 0-8, `effects.shadow.offset` 0-3, `effects.shadow.alpha.dark`/`.light` 0-0.6, `effects.glow.blur` 0-14 (primary only), `effects.glow.alpha` 0-0.8, `effects.key.edge` 0-4, `effects.key.shade` 0-0.6, `effects.press` 0-3, with press ≤ key edge. `effects.sheen.*` is removed (the face gradient replaces it). New:

   | Token | Kind | Bounds | Meaning |
   | --- | --- | --- | --- |
   | `effects.face.light` | fraction | 0-0.4 | The face's top is its fill mixed toward white by this |
   | `effects.face.dark` | fraction | 0-0.3 | The face's bottom is its fill mixed toward black by this |
   | `effects.highlight.alpha.dark` / `.light` | fraction | 0-0.9 | A `size.border` white line inside the top edge, inset by `radius.button` |
   | `effects.rim.mix` | fraction | 0-0.8 | The border of an unfilled kind takes `color.accent` mixed into `color.border.subtle` by this (Glow's rim); 0 keeps the Tint outline rule |
   | `effects.inner.blur` | length | 0-8 | Blur of an inner glow (an `inner-shadow`) in the kind's accent, unfilled kinds only |
   | `effects.inner.alpha` | fraction | 0-0.4 | Its opacity |

4. **Reach.** The **base extent** of ADR-0120 still holds for everything every button draws: `key.edge + shadow.offset + shadow.blur ≤ extent` per slot, scaled down as before; the extent constant becomes 8 px, still bounded by `space.1` and `space.2 − size.focus-ring` per slot, so it is 4 px with the built-in spacing. The **primary glow** is the one exception: it may reach `effects.glow.blur` past the box, over the gaps and over neighbours. It is allowed only when a **blend check** passes: for each neighbour fill (`color.button.fill`, the danger fill, `color.accent.subtle`, `color.surface.base`, `.raised`), the fill blended with the glow's residual (ADR-0120's linear model) at distance `space.1` still gives its text and `color.border.focus` their contrast rules. Otherwise the glow is scaled down until it passes, and to 0 if nothing passes; the editor shows no notice for scaling. The glow stays off while the primary's own ring shows, and the rendered test gains a focused neighbour on each side of a glowing primary.
5. **Face.** When the face draws effects, its background is a vertical gradient from `fill.mix(white, face.light)` at the top to `fill.mix(black, face.dark)` at the bottom (encoded sRGB, the same mix as the key shade). Hover and press change the fill, so both ends follow; nothing animates. The highlight line and the inner glow sit inside the face and move with it. Text and icons sit on the face, so **every text and ink pair of a button is checked on both gradient ends** with its existing rule (4.5:1 or 3:1, high contrast is calm anyway); a failure drops the theme's effects to calm with the effects notice naming the pair, as ADR-0120 decision 9 does.
6. **By kind** (replacing ADR-0120 decision 8): primary takes all; secondary, framed ghost and checked take all but the primary glow (checked keeps its `accent` outline and takes no rim mix); danger with text takes the shadow, the key edge, the face gradient and the press, no rim or inner glow in accent (its rim and inner glow use `color.action.destructive`); live keeps the neutral shadow and the press only; unframed ghost, icon-only danger and disabled draw nothing. SplitButton as its kind, without press.
7. **Defaults.** Built-in themes stay calm. The initial values are the mock-up's: Glow (glow 14 px at 0.75 on primary, rim 0.55, inner glow 8 px at 0.25, a faint gradient), Glass (face 0.28 light, 0.15 dark, highlight 0.45 dark / 0.9 light, shadow 8 px offset 3), Tactile (edge 4, press 3, shade 0.55, face 0.12 light). Final values come with the screenshots the owner approves.
8. **Cost.** The same gate as ADR-0120 decision 13, per setting, adding a gradient sweep (hover across a row of gradient buttons). A gradient is a shader fill with no cache; the inner glow is one more `BoxShadow` per drawing button, only in Glow. Calm adds nothing.
9. **Policing** unchanged: only the library draws them (`check-ui` counts `@linear-gradient` and `inner-shadow-*` already).

## Consequences

- Users get three looks that read at a glance, and they still travel with the theme.
- The primary glow may overlap neighbours. The blend check keeps every neighbour readable and every ring at 3:1, but it is a new kind of rule (a contrast check that depends on spacing), and its rendered test must cover it.
- The checks grow from "derived colours far from live" to "every button pair on both gradient ends": more work in `resolve`, still well under 1 ms.
- Follow-up: themes spec (settings, format 4, tokens, blend check), design system rows per setting, glossary row "Effects (theme)" names, editor labels, `tokens.toml` format 4, `effects.rs` (`draw`, `check`), `build.rs` fields, Button/IconButton/SplitButton drawing, the rendered test (focused neighbours beside a glowing primary, gradient ends), measurements on the Mac and Windows (RTX 4090, AMD iGPU, skia-software), screenshots in dark, light and high contrast.

## Alternatives considered

- **Push ADR-0120's values harder only**: done; at its bounds the settings still look alike (evidence above).
- **Wider gaps for every button row** (a `space.button-row` token raised by the effects): lets every effect reach further, but changes layouts per theme and touches every row in the app. Kept for later if the blend check proves too tight.
- **Translucent (real glass) faces**: would break the opaque-colour rule and every contrast check. Rejected; Glass is drawn with opaque gradients.
- **Per-theme free effect values**: still too many interacting values (ADR-0120's reason stands).

## Review

Independent review per ADR-0014: pending.
