# ADR-0120: Theme effects: depth and effects chosen with the theme

- Status: Accepted
- Date: 2026-11-14
- Deciders: Salih (owner)
- Builds on:
  - [ADR-0057](0057-user-themes-and-appearance-settings.md) (themes)
  - [ADR-0072](0072-theme-colour-rules-warn.md) (colour rules warn)
  - [ADR-0112](0112-calm-look-and-roomy-density.md) (calm look)
  - [ADR-0118](0118-tint-buttons.md) (Tint buttons)
  - [ADR-0013](0013-zero-cost-when-unused.md)
- Supersedes in part:
  - [ADR-0112](0112-calm-look-and-roomy-density.md) decision 6 ("No shadows"), for themes whose effects are not calm;
  - under [ADR-0015](0015-ui-design-rules.md) decision 1 (a U rule changes only by a superseding ADR), the last bullet of U12 in [UI principles](../ui/principles.md) ("No skeuomorphism, gradients, heavy shadows or decorative animation"), for themes whose effects are not calm.
- Spec: [themes](../specs/themes.md), [ui-design-system](../specs/ui-design-system.md), [glossary](../ui/glossary.md), [performance budgets](../specs/performance-budgets.md#desktop-ui)

## Context

ADR-0112 set the calm look. Its decision 6 left shadows out, because the token model has no shadow kind and colour tokens are opaque. U12 forbids gradients, heavy shadows and decorative animation.

On 2026-11-14 the owner compared three button looks drawn with Slint's Skia renderer: Glow (sheen, a glow under the main action, soft shadows), Tint (flat tinted fills, icon chips) and Key (a darker bottom edge and a press). The evidence is `docs/ui/screenshots/buttons/round2-{dark,light}.png`.

The owner chose Tint for every theme (ADR-0118). They also asked that users decide how lively the app looks, calm or livelier, as part of the theme they pick. This covers buttons now and other controls later.

Glow and Key in that evidence show the direction only. Glow there draws a whole-face gradient and a shadow that grows on hover, and this ADR forbids both. The per-setting screenshots of the follow-up (rule 16) are what the owner approves before a livelier setting is offered.

The user-facing name is **Effects**: Calm, Vivid, Tactile. The owner chose this name on 2026-11-14 (decision card). The glossary lists "style" as a word not to use, because it means a Look. It also lists "effect" under Filter and Transition: users must not call those effects. Decision 5 adds an "Effects (theme)" row for the editor's label only. Filter and Transition keep "effect" banned for what they mean.

Forces:

- **Opaque colours.** Colour tokens are opaque, and every contrast rule assumes opaque pairs (ADR-0057). Shadows, glows and sheens need alpha, blur and offsets.
- **No per-control knobs.** Per-control effect knobs in a theme would be dozens of interacting values with no meaningful bounds.
- **User themes only warn.** A user theme's colour rules warn and never refuse (ADR-0072). The editor's Fix button moves a colour to the nearest passing value.
- **Zero cost when unused (P13).**
  - In Slint 1.18, any bound `drop-shadow-*` makes the compiler add a `BoxShadow` item, whatever its value. The item is a sibling drawn just before its element (`lower_shadows.rs`).
  - Skia skips drawing it only when offset, blur and spread are all zero, or alpha is 0.
  - The shared shadow cache keeps 16 entries. Its key includes size, radii, blur and colour, so every distinct button size and colour is its own entry. A per-item cache in front of it is invalidated by any geometry or colour change.
  - Slint's software renderer draws no box shadow at all.
- **Fixed colours.** High contrast must stay plain, and the fixed live colour (U12) must not change or spread.
- **Reduce motion.** Reduce motion (U7) is specified but not implemented yet.
- **Tight spacing.** Buttons sit close together.
  - IconButton rows use `space.1`: 4 px, and at least `size.focus-ring` by the rule `size.focus-ring ≤ space.1`. The toolbar uses `space.2`.
  - Button's and IconButton's focus ring is drawn inside the box. SplitButton's two rings sit `size.focus-ring` outside its shape (`components.slint`, SplitButton).

## Decision

### The choice

1. **A theme has effects.** The theme document gets an optional top-level key `effects` with the value `"calm"`, `"vivid"` or `"tactile"`. A missing key means calm.
   - The key is a `ThemeBody` field kept as raw text, so it round-trips. It is not a `THEMEABLE` token, and it is not in a token table.
   - A value that is not a string is refused like a bad `base`.
   - A string that is not a known name is kept and reads as calm (decision 2). It must pass `valid_name` (as a theme name does), or it is refused.
   - `themes.md` lists `effects` among the top-level keys.
2. **Versioning.**
   - A theme whose *stored* `effects` is calm (or missing) is written as format 1, with no `effects` key (`Document::write_format`, as scene collections do). Older builds keep reading every calm theme, including the built-in ones.
   - A theme whose stored `effects` is anything else is written as format 2. This covers vivid, tactile, a value this build does not know, and a high-contrast theme that asks for vivid. The format follows the stored key, never the resolved effects, so the key is never dropped.
   - Older builds refuse format 2 and fall back to `builtin:<base>` (`theme.rs:read_base`), as ADR-0057 prescribes for an unknown format. The fallback does not show the theme's overrides, and `write` refuses to overwrite the newer file. This is an accepted consequence.
   - Switching a calm theme to vivid writes format 2 over format 1. `document::write` copies the old file once to `<id>.toml.format1.bak`, which the library ignores.
   - An unknown value reads as calm and shows an **effects notice** (decision 9): "This theme uses effects this version does not have; it shows as Calm". The value is kept unchanged on save, so a later setting needs no format 3.
   - Reading format 1 as format 2 is a no-op upgrade. A format 1 file that contains an `effects` key (a hand edit) is read and honoured. The next save writes it as the format its value needs.
3. **High contrast is always calm.** A theme on the `high-contrast` base resolves to calm, whatever its `effects` says, and keeps the key. The editor shows the choice disabled, with that reason.
4. **Built-in themes stay calm**, so the default look is Tint (ADR-0118). No new built-in theme ids are added. A user makes a vivid or tactile theme by choosing it in the theme editor.
5. **The theme editor** gets an "Effects" row (Calm, Vivid, Tactile) as a segmented control.
   - The row is its own entry in the editor's list (`appearance.rs`), in the Shapes group, not one built from `THEMEABLE`. Reset sets calm.
   - An unknown stored value shows as Calm (as it resolves), and choosing a segment replaces it. When effects cannot apply at all (high contrast, decision 3; a format 2 token file, decision 6), the whole control is disabled with the reason. SegmentedControl gets no per-option disable.
   - A change is applied at once, like a colour edit. It does not trigger "Keep this look?": that stays for `font.family` and for a colour that takes primary text below 1.5 (themes spec). Decision 9 already keeps the effects readable.
   - The control API does not change. `ui.theme.*` commands do not read or return the theme document, and switching a theme switches its effects with it.
   - The glossary gets an "Effects (theme)" row: "How lively a theme's controls look: Calm, Vivid or Tactile. Only the theme editor's label; a filter or transition is never an effect." Its "Not" column lists Style. The Filter and Transition rows keep "Effect" in theirs.

### Effects as tokens

6. **Effect values are tokens, not code constants.**
   - The built-in token file goes to **format 3**, which adds `[effects.vivid]` and `[effects.tactile]`. Calm has no table: it is all zero by definition.
   - Format 3 still requires `[font]`, as format 2 does.
   - A format 2 file (an older `RBS_TOKENS`) is read without effect tables. Every theme then resolves to calm, with the effects notice "This build's token file has no effects". The editor shows the Effects control disabled with that reason.
   - `RawTokens` already refuses unknown tables (`deny_unknown_fields`). It gains an optional `effects` field, so format 3 can carry the tables and format 2 does not need them.
   - Each table holds the same names. Each value has a kind and inclusive bounds:

   | Token | Kind | Bounds | Meaning |
   | --- | --- | --- | --- |
   | `effects.shadow.blur` | logical px | 0 to 4 | Blur radius of the drop shadow |
   | `effects.shadow.offset` | logical px | 0 to 2 | Downward offset of the drop shadow |
   | `effects.shadow.alpha.dark` / `.light` | fraction | 0 to 0.5 | Opacity of the black shadow, per base |
   | `effects.glow.blur` | logical px | 0 to 4 | Blur radius of the glow |
   | `effects.glow.alpha` | fraction | 0 to 0.5 | Opacity of the glow |
   | `effects.sheen.alpha` | fraction | 0 to 0.3 | Opacity of the white sheen |
   | `effects.sheen.band` | logical px | 0 to 4 | Height of the sheen band at the top of the face, before the clamp of decision 11 |
   | `effects.key.edge` | logical px | 0 to 2 | Height of the darker edge under the face |
   | `effects.key.shade` | fraction | 0 to 0.4 | A mix of the fill's encoded sRGB channels toward black: 0 is none, 0.4 is 40 % black |
   | `effects.press` | logical px | 0 to 2 | How far the face moves down while pressed |

   - A **fraction** is a new value kind: a TOML float, stored as `f32`.
   - `Tokens::parse` checks the bounds, and these rules:
     - `effects.press` ≤ `effects.key.edge`;
     - **extent**: `effects.key.edge + max(effects.shadow.blur + effects.shadow.offset, effects.glow.blur)` ≤ `EFFECT_EXTENT_MAX`. The bases reach the key edge below the box and carry the shadow and glow from there, so their reach below the box adds up. `EFFECT_EXTENT_MAX` is 4 logical px, a constant, not a token. The press does not add to the extent, because the shadow does not move (decision 10).
   - The effect tokens are not themeable one by one. A theme chooses only its effects.
   - `ResolvedTheme` gains the resolved effects (`built_in_theme` sets calm). `rbs-app-ui/build.rs` emits the resolved values as `ThemeSlot` fields, in the built-in defaults and in `slot()`.
   - The golden list test (`look.rs`, against `SLOT_TOKENS`) gains these fields explicitly. They are not in `THEMEABLE`, so the property and bounds tests over `THEMEABLE` are unchanged.
7. **Derived colours.**
   - Shadow: black at the base's alpha.
   - Glow: the kind's **rest** fill (`accent` for primary) at the glow alpha. It does not follow hover or press, so hover never changes a blur image.
   - Sheen: white at the sheen alpha.
   - Key edge: the rest fill mixed toward black by `effects.key.shade`.
8. **Effects by kind.** Only these combinations are drawn. Every other combination is none.

   | Kind | Shadow | Glow | Sheen | Key edge | Press |
   | --- | --- | --- | --- | --- | --- |
   | `primary` | yes | yes (`accent`) | yes | yes | yes |
   | `secondary`, framed ghost | yes | no | yes | yes | yes |
   | `danger` with text | yes | no | no | yes | yes |
   | `live` | neutral shadow only | no | no | no | yes |
   | checked (any kind; an unframed ghost when checked, on its `accent.subtle` fill, counts as framed) | as its kind | no | no | as its kind | yes |
   | `ghost` unframed, icon-only danger, any disabled button | no | no | no | no | no |
   | `SplitButton` | as its kind, for the whole shape | as its kind | as its kind | as its kind | no |

   - A transparent face takes no shadow: Skia would draw the blurred rectangle through it as a dark blob.
   - Live keeps the fixed live colour alone. No halo or edge spreads it (U12).
   - SplitButton takes no press. Its parts share one clipped background, and moving one part would leave a strip of fill above it.
9. **What effects may not do, and what happens when they would.**
   - Checks cover only the pairs an effect changes:
     - the derived key edge and glow colours against `color.state.live`: ΔE2000 ≥ 15;
     - `color.border.focus` on the toolbar ground (`color.surface.base`) at the outer edge of the clamped extent, blended with a neighbour's strongest shadow or glow: 3:1. This is a check of the clamp; with the clamp as defined it equals the plain pair.
   - Text and icons never sit on an effect: the sheen band is clamped clear of all ink (decision 11), and shadow, glow and key edge are outside the face. So no text pair changes, and the existing colour rules and their warnings (ADR-0072) apply unchanged.
   - When an effect check fails, that theme's effects resolve to calm. The theme stays usable with all its colours, and the colour warnings are untouched. The editor shows an **effects notice** naming the failed check.
   - The effects notice is a `ThemeProblem` with a new `Severity::Notice`, `rule: None` and `token: "effects"`. It is not a colour rule:
     - the editor's and import note's warning counts filter `severity == Warning`, not `!is_error()` (`appearance.rs`), so a notice never counts as a colour warning;
     - Fix does not touch it, and it never refuses the theme;
     - the editor routes `token: "effects"` to the Effects row, not to the general problems at the top.

     Decisions 2 and 6 use the same notice.
   - **Built-in token files are strict.** `Tokens::parse` already refuses a token file whose built-in themes have any problem. It also runs the effect checks for every built-in base and every setting, and refuses the file on a failure, so a shipped file cannot make vivid silently calm.
   - **Nothing of an effect sits under a focus ring.** A Button's glow is off while its keyboard focus ring shows (`focus.ring`, not `has-focus`; pointer focus shows no ring). A SplitButton's shadow, glow **and key edge** are all off while either of its rings shows, because the rings sit outside its shape and the bottom segment would lie on the key edge (`border.focus` on a derived key edge is 1.4 to 2.7:1 with the built-in colours).

### Drawing and cost

10. **Structure.** In calm, Button keeps its face as its own root `background` and border, as today. In vivid or tactile, inside `if Theme.effects-… > 0`, the library draws, in this order:
    1. the **bases**: opaque rectangles in the key edge colour, from `y = effects.press` to the box's bottom plus the key edge. Each carries one `drop-shadow-*`: the **shadow base** (black) and, for primary, a **glow base** (`accent`). One Slint element draws one shadow, so a primary has two bases, two `BoxShadow` items and two shadow-cache entries. The bases never move, so their blur images never change on press. Their top inset means the pressed face covers them completely: no strip of another fill shows above it. The part of the bases below the box is the key edge.
    2. the **face**: an opaque copy carrying the fill (rest, hover and pressed), border, border colour and radius, with `clip: true` so the sheen child is rounded with it. It moves down by `effects.press` while pressed. No clip surrounds it, so its sibling shadow is never clipped. Because press ≤ key edge, it never leaves the box plus the edge.
    3. the primary lip of ADR-0118 (declared after the face, so it stays above it; it moves with the face), then the content, which moves with the face. In calm the content's offset binding is 0.

    - **"Draws effects"** is one predicate per button: the setting is not calm, and the button's kind and state row in decision 8 has any effect. Only then are the bases and face drawn, and only then does the root draw neither background nor border. Every other button (an unframed ghost, an icon-only danger button, a disabled one) keeps today's root background and border, including its hover fill.
    - Calm therefore adds no drawn item and no `BoxShadow`. Each conditional is still a slot per instance with a dependency on `Theme`, which decision 13 measures.
    - The root, the hit area and the focus ring never move.
    - SplitButton draws the base as an unclipped sibling copy of its shape, and leaves the shape as it is.
    - `every_drawn_pair_has_a_contrast_rule` reads only ancestors, so it would not see the face, a sibling of the content. The reader treats every `Theme.effects-*` condition atom as false (calm), and a test asserts that the face's background expression is the root's calm expression. This keeps the pairs checked once and the copies in step.
11. **Geometry.**
    - The control's box is its layout size, hit area and focus-ring target in every setting.
    - Shadow, glow and key edge reach at most the **extent** past the box. The extent is one value per slot, `max(0, min(EFFECT_EXTENT_MAX, space.1, space.2 − size.focus-ring))`. It is computed in `slot()`, on the lengths after density (`pack` applies density after `resolve`), so compact is covered. With the built-in lengths it is 4 px in regular and roomy, and 2 px in compact (where `space.2` takes 4). It can be 0. The effect children scale blur, offset, key edge and press down so that `key.edge + max(offset + blur, glow.blur)` fits it (the sum of decision 6), keeping press ≤ key edge.
    - Button rows in the library use at least `space.1`, so a neighbour's effect may reach the gap but never another box, and never in hit testing. A Button's ring is inside its box, so it is never covered.
    - The `space.2 − size.focus-ring` term keeps a neighbour's effect short of a SplitButton's outside ring. The toolbar, the only place with SplitButtons, uses `space.2`. A Button needs no knowledge of its neighbour.
    - Button rows are laid out in the app files. The ui-review checklist gains the item "a row of buttons uses `space.1` or more". The rendered test covers the three rows it names.
    - The sheen band is clamped in `slot()` to `max(0, (size.control.sm − max(size.button.chip, size.icon.box, type.label.line)) / 2)`, one value for every button. With the built-in sizes that is 2 px, so ink, which starts at 2 px or lower on a dense button, never sits on it.
    - Tests:
      - every library component's preferred and minimum sizes are identical in every setting and every density;
      - a **rendered** test in its own integration test binary (the platform is set once per process, and Skia's real font metrics differ from the mock font), with the testing backend's `renderer-skia` dev-dependency feature (`renderer_name = "skia"`) reading pixels, because the software renderer draws no shadows. It runs in regular and compact. It checks a keyboard-focused Button between two neighbours at `space.1`, the Go live / Record SplitButton row at `space.2`, an IconButton row, and a vertical pair (a Button above an IconButton at `space.1`). In every setting each focus ring must be fully drawn.
12. **No animated effects.**
    - Shadow and glow geometry and colour must not animate or change with hover or press. The base never moves and the glow uses the rest fill.
    - Hover changes the face fill, as today. The press offset is instant.
    - When reduce motion is implemented, it covers effects too. Until then, effects add no animation at all (U7).
13. **Performance gate.** Measured against the [Desktop UI budget](../specs/performance-budgets.md#desktop-ui), release build. Today's gate script measures the toolkit prototypes, not `rbs-app`, and has no sweeps. The implementing PR therefore adds an `rbs-app` measure mode, like `remote_preview`'s `RBS_DEMO_SECONDS`. It opens the main window with the toolbar, Mixer and Properties, runs scripted hover and resize sweeps, and logs presented-frame times, p50, p95, p99 and max. It runs on Windows and the Mac.
    - Comparisons: calm against current `main`, and vivid and tactile against calm.
    - **Noise:** the spread (max − min) of five runs of the baseline in the same sitting.
    - Scenarios:
      - idle: no visible panel animates, not live;
      - the main window with toolbar, Mixer (meters running) and Properties;
      - a hover sweep across the toolbar;
      - a window resize sweep.
    - Hardware:
      - Windows RTX 4090 and the AMD iGPU. Meters are still drawn by Slint there, and Slint's GPU path redraws the whole window, so every effect image is redrawn at the meter rate. This is the worst case.
      - Windows with Slint's `skia-software` renderer forced (`SLINT_BACKEND=winit-skia-software`).
      - Mac M4 Pro.
    - GPU memory: Task Manager's dedicated GPU memory of the process on Windows, and `footprint` / Activity Monitor's GPU memory on the Mac.
    - **Calm** must be within noise of `main`.
    - **A setting is offered** only if all of these hold on the iGPU and on the CPU raster run:
      - idle stays at 0.0 % CPU with no redraw timer;
      - p95 frame time rises by at most 0.5 ms, and the maximum by at most 2 ms;
      - GPU memory (iGPU) rises by at most 16 MB.

### Scope and policing

14. **Scope now: `Button`, `IconButton`, `SplitButton`.** Each later component needs its own spec rows, screenshots in every setting (dark and light), a measurement, and the owner's approval before it takes effects.
    - Later components: SegmentedControl's chosen plate, Toggle's knob, Slider's thumb, Settings cards.
    - `ListRow` and the `Tabs` indicator never take shadows (repeated rows and a 2 px line).
15. **Only the library draws effects.**
    - `cargo xtask check-ui` gains a fourth count: `drop-shadow-`, `inner-shadow-`, `@linear-gradient`, `@radial-gradient`, `@conic-gradient` and `Theme.effects-` in any file outside the library files (`library` in `docs/ui/baseline.toml`, today `components.slint`). Comment lines do not count. Baseline 0: the colour picker's gradients are in `components.slint`.
    - The xtask reads a missing key as 0 and writes it with `--lower`, so nobody edits `baseline.toml` by hand. Its comment about which checks are counted is updated by the same code change.
    - The implementing PR updates every place that says check-ui counts three things:
      - CLAUDE.md rule 18 ("counts three things");
      - the check list in `principles.md`;
      - the module doc of `xtask/src/check_ui.rs`;
      - its `--except` error;
      - the ui-review skill.
    - Plugin panels (U14) get effects only through library components.
16. **U12 wording.** The last bullet of U12 becomes:

    > The calm setting has no skeuomorphism, gradients, heavy shadows or decorative animation. A theme may choose livelier effects (ADR-0120); they are then drawn only by library components, never carry meaning, never use or spread the live colour, never lower contrast, and never animate.

    `principles.md` changes in the same PR as this ADR.
17. **On acceptance:**
    - ADR-0112's Review gets a pointer: "decision 6 superseded in part by ADR-0120".
    - The `docs/adr/README.md` row for 0112 says the same, and a row for 0120 is added.

### Left to the implementing change

These details do not change the decision. The implementing PR settles them and the review checks them:

- A field or dropdown laid out right after an effect-bearing button needs the same spacing as a button row. The checklist item of decision 11 covers any control beside a button, not only buttons.
- Where the Effects notice is stored. If it goes in `warnings`, update the themes test that counts them. With a format 2 token file, only themes that ask for more than calm get the notice. The strict built-in check skips high contrast, which is always calm.
- The sheen sits inside the face's border and is drawn under the ADR-0118 lip, so the two do not stack.
- A disabled SplitButton whose ▾ part stays usable draws no effects, like any disabled button.
- `drawn.rs` treats `Theme.effects-*` atoms as false, including inside a local property that reads them.
- A pressed live button's face moves at most `effects.press` down, past the box over its neutral shadow base (live has no key edge). The live colour reaches at most `effects.press` below the box, within the extent, and nothing around it takes the colour, so it does not spread (U12).
- A button inside a clipping container loses its effect at the clip edge. That is accepted, and panels that clip keep a padding of at least the extent around buttons where it matters.

## Consequences

- Users get a livelier look with no new settings surface. It travels with the theme, and can be shared and imported like any theme.
- A theme cannot make controls unreadable, cover a focus ring or overlap hit areas through effects. An effect that would breaks is dropped to calm with a notice, and never refuses the theme.
- Older builds read every calm theme. They refuse vivid and tactile themes and fall back to the base.
- **Follow-up work:**
  - themes spec: the `effects` key, versioning, token file format 3, the effect tables and bounds, the Effects notice kind;
  - design system spec: Button, IconButton and SplitButton rows per setting (decision 8's table);
  - glossary row;
  - `rbs-ui`:
    - fraction kind;
    - parse checks and extent;
    - `ResolvedTheme` effects;
    - the effect checks and high contrast forced to calm;
    - the sheen and extent clamps;
  - `rbs-user-data`: `write_format` on the stored value, and the round trips;
  - `rbs-app-ui/build.rs`: ThemeSlot fields;
  - the editor row;
  - the check-ui count and its documentation;
  - measurements;
  - screenshots of each setting in dark and light for the owner's approval.
- **Tests:**
  - resolve property test over valid themes, with an effects dimension;
  - high contrast forced to calm for every base and setting, with the key kept and written as format 2;
  - calm written as format 1; vivid, tactile and unknown values written as format 2;
  - an unknown value reads as calm, shows the notice and round-trips unchanged;
  - format 1 reads as calm; format 1 with an `effects` key is read;
  - token file format 2 resolves every theme to calm with the notice;
  - the effect fields in the ThemeSlot golden list;
  - an effect check failing on a crafted theme gives calm plus the notice, and the colour warnings are unchanged;
  - ΔE from live on the derived colours, and no coloured effect on live;
  - extent bound and clamp, sheen clamp;
  - the rendered focus-ring test (decision 11);
  - sizes identical across settings;
  - the check-ui count.

## Alternatives considered

- **Per-token effect values in the theme**: the most flexible, but many interacting values with no meaningful bounds. Rejected for now. A later ADR can open single values once the closed set has been used.
- **A separate app setting, not in the theme**: simpler to switch, but the owner asked for it as part of the theme, and a theme author could then not ship a coherent look. Rejected.
- **Only one look for everyone**: ignores the owner's request.
- **Translucent colour tokens**: break the opaque-colour rule and every contrast check. Rejected.
- **Binding `drop-shadow-*` with zero values in calm**: simpler code, but adds a `BoxShadow` item to every control in every theme. Rejected for P13.
- **The planned `elevation.*` tokens** (`ui-design-system.md`, for raised surfaces such as popups): effects are not elevation. Effects are a theme's look for controls. Elevation, when it comes, describes layers and must reuse the effect machinery (bases, extent, conditional children), so the two do not grow apart.
- **`inner-shadow-*` for the key edge and sheen**: draws inside the box, with no outside extent. It is worth trying for the key edge in the implementing change, but it is still a `BoxShadow` item with the same cache cost and the same conditional-child rule. Not decided here.
- **Refusing a whole theme when an effect fails**: contradicts ADR-0072, where user themes only warn. Rejected for dropping the effects to calm with a notice.

## Review

- **Round 1** (2026-11-14, independent AI session, architecture-review): changes requested. 7 major, 7 minor and 3 nits, all resolved:
  - U12 amended (16);
  - effect values as tokens (6);
  - contrast (9);
  - conditional children (10);
  - a measurable gate (13);
  - versioning (2);
  - geometry (11);
  - supersession header;
  - check-ui policing (15);
  - reduce motion (12);
  - built-in style themes dropped (4);
  - API (5);
  - scope (14);
  - tests;
  - per-base alpha;
  - evidence committed;
  - editor (5).
- **Round 2** (2026-11-14, independent AI session): changes requested. 2 major, 7 minor and 3 nits, all resolved in the next text:
  - N1, extent;
  - N2, kind table;
  - N3, face structure;
  - N4, stored-value format;
  - N5, no "Keep this look?";
  - N6, token table;
  - N7, check-ui wording;
  - N8, supersession bookkeeping;
  - N9, gate;
  - N10 to N12, wording.
- **Round 3** (2026-11-14, independent AI session): changes requested. 2 major, 10 minor and 8 nits. Resolutions:
  - Major:
    - M1: no clip around the face; the shadow sits on a non-moving base, so the press does not extend it; the key edge is in the extent; a slot-resolved extent replaces the row clamp; SplitButton's outside rings are protected by a smaller neighbour extent and by its own shadow turning off while a ring shows (9, 10, 11).
    - M2: checks only on pairs an effect changes; a failure drops the effects to calm with a new `Effects` notice kind and never touches colour warnings (ADR-0072) (9).
  - Minor:
    - m1: the glow uses the rest fill (7, 12).
    - m2: the face copy carries the border; the lip stays above the face; the reason for an empty root is corrected (10).
    - m3: `effects` is a top-level field, not `THEMEABLE` (1, 5, 6).
    - m4: the sheen band is clamped clear of ink (11).
    - m5: the rendered test uses headless Skia (11).
    - m6: the gate is based on the Desktop UI budget, with noise and GPU memory defined and the idle row added (13).
    - m7: the term is "Effects", with a glossary row (Context, 5; owner chose it 2026-11-14).
    - m8: format 3 details (6).
    - m9: the follow-up list is completed (6, 10, Consequences).
    - m10: SplitButton takes no press, with an unclipped base (8, 10).
  - Nits:
    - n1: the Context is corrected.
    - n2: the cache key is explained.
    - n3: the redundant check is removed.
    - n4: the check-ui places and comment lines are covered (15).
    - n5: the colour space of the shade is named (6).
    - n6: the backup file is mentioned (2).
    - n7: `inner-shadow` is listed under Alternatives.
    - n8: the `RawTokens` reason is corrected (6).
- **Round 4** (2026-11-14, independent AI session): changes requested, with 2 major findings, 10 minor and 9 nits. Resolutions:
  - Major:
    - M1: the bases are inset by the press, so no strip shows. A primary has two bases, one for the shadow and one for the glow, and the cache cost is stated (10).
    - M2: a SplitButton's key edge is also off while a ring shows (9).
  - Minor:
    - m1: the SplitButton limit is folded into the per-slot extent (11).
    - m2: the clamps are computed after density (11).
    - m3: the drawn-pair reader treats effects as calm, and the face expression is asserted equal (10).
    - m4: `Severity::Notice` and the editor filters are defined, and built-in token files are strict (9).
    - m5: the sheen arithmetic is corrected and gets a floor (11).
    - m6: the glossary rows are settled (Context, 5).
    - m7: check-ui counts `inner-shadow-` and `@conic-gradient`, and the baseline key is written by `--lower` (15).
    - m8: an `rbs-app` measure mode replaces the prototype script (13).
    - m9: the whole control is disabled (5).
    - m10: a per-button "draws effects" predicate is defined (10).
  - Nits:
    - n1: the clamp check is named as such (9).
    - n2: the colour space of the shade is named (6).
    - n3: everything scales into the extent, which can be 0 (11).
    - n4: unknown values must pass `valid_name` (1).
    - n5: a checked ghost counts as framed (8).
    - n6: the relation to `elevation.*` is stated (Alternatives).
    - n7: the row spacing goes into the checklist (11).
    - n8: the rendered test runs in its own binary (11).
    - n9: the file becomes `0120-theme-effects.md` when committed.
- **Round 5** (2026-11-14, independent AI session): every round-4 finding was resolved, with one new blocking finding:
  - R5-M1: below the box, the reach of shadow and glow adds to the key edge. Fixed: the extent rule is now `key.edge + max(offset + blur, glow.blur)` (6), and the rendered test gains a vertical pair (11).
  - Nits r5-n1 to r5-n7 are listed under "Left to the implementing change".
  - Follow-up check by the same reviewer: **approve with nits**. The two wording nits (the scaling sentence in 11 and the live bullet) are fixed in this text.
- Renumbered from 0118 to 0120 on 2026-11-14, because 0118 became Tint buttons and 0119 was taken. Renamed from "style" to "effects" in round 3 (glossary).
- 2026-11-14 18:12: owner approval ("onaylıyorum") after the round 5 follow-up check. Status Accepted.
