# Spec: Themes and appearance

- Status: Draft
- Owner crate(s): `rbs-ui` (`tokens`: theme model, resolving, checks, bounds, token metadata), `rbs-user-data` (theme and appearance documents, theme library), `rbs-app` (Settings › Appearance, theme editor, applying themes to windows and native layers)
- Related ADRs: [0015](../adr/0015-ui-design-rules.md), [0049](../adr/0049-main-window-areas-panel-states-and-sizes.md), [0051](../adr/0051-user-data-profiles-and-scene-collections.md), [0057](../adr/0057-user-themes-and-appearance-settings.md)
- Rules: [UI principles](../ui/principles.md) U7, U11, U12, U17

## Purpose

Users change the look of RBS with themes: files that override design tokens on top of a built-in theme, kept in a library, shared as single files, and chosen in Settings › Appearance. This spec defines the theme file, the appearance file, the checks every theme passes and how the app applies a theme.

## Scope

In scope: the theme document (format 1), resolving a theme on its base, checks and bounds, the theme library, the appearance document, the Appearance page's behaviour, applying a theme at run time.

Out of scope, later changes: themes in `.rbspack` packs; font files; a curated accent shortcut; detecting the OS high-contrast setting; theme methods in the control API other than `ui.theme.reload`; a text size preference. The token names, groups and built-in values are in the [design system spec](ui-design-system.md#tokens).

## Contract

### Themeable tokens (`rbs_ui::tokens`)

| Group | Tokens | Value | Bounds |
| --- | --- | --- | --- |
| Colours | every name in `THEME_COLORS` | `"#rrggbb"` (opaque) | contrast and hue rules of the design system |
| Font | `font.family` | array of family names | 0 to 8 names, each 1 to 64 characters, no control characters or text direction marks |
| Text | `type.caption`, `.body`, `.label`, `.title`, `.heading` | `{ size, line, weight }`, whole style | size 11 to 48, line from size to 2 x size, weight 300 to 900 |
| Spacing | `space.1` ... `space.8` | logical px | 0 to 64 |
| Spacing | `size.panel.pad`, `size.section.gap`, `size.field.gap` | logical px | 8 / 12 / 6 to 64 (floors, ADR-0076) |
| Controls | `size.control.sm` / `.md` / `.lg` | logical px | 24 to 64 (U7) |
| Shapes | `radius.sm` / `.md` / `.lg` | logical px | 0 to 32 |
| Lines | `size.border` | logical px | 1 to 4 |
| | `size.focus-ring` | logical px | 2 to 6 (U7) |
| Icons | `size.icon.box` / `size.icon.stroke` | logical px | 12 to 32 / 1 to 3 (U7) |
| Menus | `size.menu.sm` / `.md` | logical px | 160 to 480 |
| RBS menu | `size.app-menu.w` / `.categories` / `.list-h` | logical px | 480 to 960 / 120 to 320 / 240 to 720 |
| Motion | `motion.fast` / `motion.normal` | ms | 120 to 200 (U12; reduce motion turns motion off) |

`THEMEABLE` lists them with metadata for the editor: name, label in words, group, value kind, bounds. Not themeable, refused in a theme: `color.state.live`, `color.text.on-live` (U12), `color.caption.close-hover`, `color.caption.close-text` (the Windows close button as the OS draws it, ADR-0071), `size.caption.w`, `size.window.*`, `size.floating.*`, `size.settings.*`, `step.*`, `size.tracking.caps`, `size.meter.peak`, `size.meter.bar`, `size.meter.scale`, `size.meter.scale-line`, `size.meter.readout`, `size.mixer.strip`, `size.mixer.splitter`, `size.mixer.controls`, `size.mixer.min-h`, `size.transition.*`, `size.library.*`, `delay.*`.

The built-in token file gains `[font] "font.family" = []` (empty: the OS UI font); `tokens.toml` becomes format 2 and `Tokens::parse` still reads format 1 files (given the empty family), so `RBS_TOKENS` files keep working.

### Theme rules

Run by `Tokens::resolve` on every resolved theme and by `Tokens::parse` on the built-in file (each theme in it), so built-in tuning is held to the same rules:

- **Contrast**: the rules of the design system plus `color.border.focus` on `color.surface.overlay` and `.sunken` (3:1), `color.text.inverse` on `color.accent.hover` (4.5:1, 7:1 high contrast), `color.accent` on every surface (3:1), `color.text.secondary` on `color.accent.subtle` (4.5:1, 7:1 high contrast), `color.state.live` on `color.surface.sunken` (3:1), and the pairs the desktop UI draws besides: `color.state.error` (error text) on every surface and on `color.accent.subtle` (4.5:1, 7:1 high contrast), `color.border.focus` on `color.accent.subtle`, `color.border.strong` on `color.surface.base`, `.sunken`, `.overlay` and `color.accent.subtle`, `color.brand` on `color.surface.raised` and `.overlay` (3:1; the RBS mark in the menu button), and `color.accent` on `color.accent.subtle` (3:1; the last three for a Checkbox on a hovered or selected row, ADR-0062), and `color.state.warning` on `color.surface.overlay` and `color.accent.subtle` (3:1; the mark of an unavailable source on a hovered or selected row, ADR-0077). Test `every_drawn_pair_has_a_contrast_rule` (in `rbs-app`, which owns the `.slint` files) reads them and fails when a text, border or icon colour is drawn on a theme colour no rule covers, or covers only with 3:1 when it is text (4.5:1). It expands components per instance (the instance's properties over the defaults, children at `@children`), takes the nearest ancestor `background` (through `transparent`; a popup or window starts afresh; a colour that is not a theme token ends the search), follows `if` conditions and ternaries with each condition as one atom shared by every expression of that instance (so a button's background and text take the same branch), and checks a `FocusRing` against what is around its control. Exempt: `color.text.disabled` (inactive controls, WCAG 1.4.3) and `color.border.subtle` (dividers and resting outlines that carry no meaning alone). Accepted without a rule: `color.state.live` on `color.surface.raised` and `.overlay` (the fixed live colour is not a theme's to fix and live always shows its words, ADR-0057 review item 1); the test lists them by name, so any other live pair fails. Not seen: a sibling drawn under a text and colours set from Rust (native layers).
- **Live red reserved**: every theme colour differs from `color.state.live` by at least ΔE2000 15 (`MIN_DELTA_E_FROM_LIVE`); the 45 degree hue rule for `color.state.error`, `color.action.destructive` and `color.meter.clip` stays.
- **Across tokens**: the `line` of `type.caption`, `type.body` and `type.label` and `size.icon.box` are at most `size.control.sm`; `size.control.sm <= .md <= .lg`; `space.1 <= space.2 <= space.3 <= space.4 <= space.6 <= space.8`; `radius.sm <= .md <= .lg`; text sizes `caption <= body <= title <= heading` and `body <= label <= title`; `size.focus-ring <= space.1`; `size.field.gap <= size.section.gap`. A token below its floor (`MINIMUMS`: `size.focus-ring`, `size.icon.*`, `size.panel.pad` 8, `size.section.gap` 12, `size.field.gap` 6, `size.tab.indicator` 2, `size.window.min-*`) makes the file refused with "<token> must be at least <floor> (<rule>)"; panel padding has a ceiling of 64 like the other spacing tokens. `size.tab.indicator` is not themeable.
- **Size**: a theme file over 64 KiB (`MAX_THEME_FILE`) is not read; more than 256 unknown token names (`MAX_UNKNOWN_TOKENS`) fails the theme.

### Theme document (`kind = "theme"`, format 1)

```toml
format = 1
kind = "theme"
name = "Midnight"
base = "dark"
author = "Salih"
description = "Deep blue surfaces with a warm accent"

[colors]
"color.surface.base" = "#0b1020"
"color.accent" = "#ff9f43"

[font]
"font.family" = ["Inter", "Segoe UI", "Helvetica Neue"]

[type]
"type.body" = { size = 14, line = 20, weight = 400 }

[length]
"radius.md" = 8

[duration]
"motion.fast" = 100
```

| Key | Meaning |
| --- | --- |
| `base` | Required: `dark`, `light` or `high-contrast` |
| `author`, `description` | Optional display text: one line, no control characters or text direction marks, at most 64 and 200 characters |
| `colors`, `font`, `type`, `length`, `duration` | Optional tables of overrides, keyed by token name; a token belongs to its table's group (a colour under `[length]` is refused) |

Other top-level keys are refused (ADR-0051 decision 2). Unknown token names inside the tables are the theme exception to ADR-0051's unknown-key rule (ADR-0057 decision 3): Inside the tables, a key that is a valid token name (lowercase words joined by `.` or `-`) but not known to this build is kept, rendered back on save in its table, not applied, and reported (`ResolvedTheme::unknown`); an invalid name is refused. Rendering is deterministic and writes overrides sorted by name.

### Resolving and checking

`Tokens::resolve(&self, theme: &ThemeOverrides) -> Result<ResolvedTheme, ThemeProblems>` on the built-in tokens of the running build:

- starts from `base`'s colour set and the shared lengths, durations, text styles and font family, then applies each override;
- runs every check of the design system and every theme rule above and returns all problems at once, each `ThemeProblem { token: Option<String>, reason, severity, rule }` naming the token it belongs to (the editor shows it beside the token). **Errors** (the theme is refused): a non-themeable token, a value of the wrong kind, a value out of bounds, each failed rule across tokens. **Warnings** (ADR-0072; the theme resolves and is used): each failed contrast rule (naming the foreground token, with the ratio found and needed), each failed hue rule, each colour too close to live; contrast thresholds are the high-contrast ones when `base` is `high-contrast`. `Err` carries errors and warnings together; `Ok(ResolvedTheme)` carries `warnings`. Built-in token files (and `RBS_TOKENS`) treat every rule as an error. `suggest_colour(tokens, theme, token)` returns the nearest-lightness colour that clears the token's warnings;
- `ResolvedTheme` holds `base`, one colour set (plus the fixed colours), lengths, durations, text styles, the font family list and the unknown names.

Property test: for any set of overrides drawn from the themeable tokens with values in and out of bounds, `resolve` either returns a theme whose only problems are colour warnings or returns at least one error, and never panics.

### Theme library (`rbs-user-data`)

`UserData::themes() -> Library<Theme>` on `themes/` with every rule of the [user data spec](user-data.md#libraries-library). Export scrubs nothing. Built-in themes are not in the library; the app lists them first, as Dark, Light and High contrast, with the ids `builtin:dark`, `builtin:light`, `builtin:high-contrast`. Duplicating a built-in theme creates a user theme with that base and no overrides; duplicating a user theme copies its base and overrides.

### Appearance document (`appearance.toml`, `kind = "appearance"`, format 2)

| Key | Type | Default |
| --- | --- | --- |
| `mode` | `"system"` or `"fixed"` | `"system"` |
| `dark` | theme id | `"builtin:dark"` |
| `light` | theme id | `"builtin:light"` |
| `theme` | theme id | `"builtin:dark"` |
| `density` | `"regular"` or `"compact"` | `"regular"` |

Format 1 is format 2 without `density` (read as `regular`). Compact spacing uses the next smaller `space.*` and `size.control.*` token. The smallest step is unchanged. No new pixel value is introduced.

`UserData::appearance()` returns the defaults and the reason when the file cannot be read; `set_appearance(&WriterLock, ..)` writes it through the user data writer. A theme id is `builtin:<name>` or a library id. An id that names no usable theme falls back to `builtin:<base>` when the theme's `base` can be read (envelope and `base` key only), else to `builtin:dark` (for `dark` and `theme`) or `builtin:light` (for `light`); the app shows a note and does not rewrite the file. **Deleting:** an id whose theme file is gone (the theme was deleted) is not such a case: deleting a theme in use sets the slots that name it to `builtin:<base>` of the deleted theme and saves the choice, and at start (before the user data writer lock is taken) a slot naming a missing file is set to `builtin:dark` (`dark`, `theme`) or `builtin:light` (`light`) and saved, with no note. `rbs-app --reset-appearance` moves `appearance.toml` to `appearance.toml.reset.bak` (numbered when taken) and starts with the defaults.

### Settings › Appearance (`rbs-app`)

- Theme choice: "Use the system appearance" (on by default) uses the built-in Dark theme while the system is dark and the built-in Light theme while it is light. A saved custom dark or light slot is read as that built-in pair. Choosing a theme in the list (Use) turns the choice to one theme (`fixed`) and does not change the system pair, so turning the switch back on returns to the built-in pair. Turning the switch off without choosing a theme keeps the built-in theme of the scheme the windows are showing. "Compact spacing" uses the next smaller spacing and control size; off, the theme's own lengths are used. The themes list shows each theme with a swatch of `color.surface.base`, `color.text.primary` and `color.accent`. A change is applied at once and written to `appearance.toml`.
- Themes list: each row ends with its actions as icon buttons (`IconButton`, accessible names "Use <name>", "Edit <name>" or "Duplicate <name> to edit" for a built-in theme, "Duplicate <name>", and for user themes "Export <name>…" and "Delete <name>…"; a built-in row leaves the places of Export and Delete empty so the icons line up), acting on that row's theme (it is selected first). Delete asks in a callout under the list. Import (a ghost button with the `import` icon) is at the right of the card's description line. A selected user theme has its Name field under the list (Rename). File dialogs as for output profiles. An unusable theme shows its first problem and "Edit" to see all; it cannot be used. A usable theme with colour warnings (ADR-0072) shows "N colour warnings" with an amber mark under its name; importing a theme that has warnings selects it and says so in the page note ("Imported \"X\". It has N colour warnings, M about the Live colour, which can hide whether you are live. Open it to see them."), and the theme is added and usable.
- Theme editor: groups Colours, Text, Font, Spacing and size, Shapes, Motion; per token a label, the control (ColorPicker; NumberField; text styles as size, line and weight fields; font family as an editable list with a note naming the family in use), a reset button that removes the override, and its problems. The font family field suggests the installed families whose names start with, then contain, the name being typed after the last comma (at most 8; shown under the field; Down and Up choose, Tab completes and keeps typing, Return completes and confirms, Escape hides the list, a click completes and confirms the whole name). They are read the first time something is typed (P13), in the same list as Add font. Every edit without an error is applied to every window at once when a slot uses the theme, colour warnings included (ADR-0072); editing a theme no slot uses shows a notice under the title ("This theme is not in use, so the windows do not show its changes. Pick it under Theme in use to see them.") in place of the hint. The page opens with the theme the windows use now selected in the list, and a picker whose theme cannot be used shows the built-in theme used instead. The editor's lists update row by row, so a menu open in a row (Add font) stays open while "Keep this look?" counts down; a colour that breaks a rule shows a warning in plain words on both rows of its pair (live warnings first) with a **Fix** button that moves its lightness to the nearest passing value, and a count under the title; an edit with an error (a value out of bounds) is shown with its problem and not applied; Save is on for any change without an error; a value still being typed (before Return or leaving the field) turns Save on, and Save applies it first, so it never needs leaving the field; Save and Cancel (Close while nothing changed) are at the top of the editor, once. Each token is a row: its label, hint and problem on the left, its fields and Reset on the right (the font family field takes the row's width under the label; empty, it shows "System font" in `text.secondary`; beside it **Add font** opens the searchable picker (`Overlays.picker-*`, ADR-0094 B1, the same overlay as Add source) over the families installed on this computer, sorted by name, without the OS's hidden dot-names, with a filter field (names containing the text, without regard to case); a pick goes first in the list and is applied like a typed list. The families are read the first time the picker opens, never before, P13). Beside the editor the theme list is a narrow column (`size.menu.sm`) at the top, without the "Built in" chips; it is never stretched to the editor's height; Cancel restores the saved theme. Unknown tokens are listed read-only as "Not used by this version". Built-in themes open read-only with "Duplicate to edit".
- Keep this look: changing `font.family` in the editor of a theme a slot uses, or a colour edit that takes primary text below contrast 1.5 on `surface.base`, `.raised` or `.overlay`, shows "Keep this look?" with Keep and Undo; without an answer within 15 seconds (`KEEP_LOOK_TIMEOUT`, measured on the app's injected clock) the previous look comes back. Escape is Undo. Choosing a theme, or turning "Use the system appearance" on or off, is kept at once and is not undone on that timer. The themes list selects the theme that was just chosen.
- While `RBS_TOKENS` or `RBS_THEME` is set, the page shows a note that a development override is in effect.

### Applying (`rbs-app` `look`)

- **Slots.** The generated `Theme` global has `slots: [ThemeSlot; 2]`: slot 0 is used while the OS scheme is dark, slot 1 while it is light. `ThemeSlot` holds every theme colour, every themeable length, duration and text style, and `high-contrast: bool` (base is `high-contrast`). `Theme.active` is `Palette.color-scheme == light ? 1 : 0`, unless `Theme.os-scheme` is 0 or 1. On Windows the menus stay the system's and a dark theme asks for the dark menu, a light theme for the light one; that request changes what Palette reports, so the app sets `os-scheme` from the system setting (dark 0, light 1, high contrast 1) and updates it when that setting changes. Every `Theme.*` colour, `Theme.high-contrast` and `Tokens.*` themeable value reads `slots[active]`. The fixed colours and non-themeable tokens stay single values. `Theme.choice` is removed. Built-in defaults: slot 0 Dark, slot 1 Light.
- **Filling the slots.** `system` mode: slot 0 = resolved `dark`, slot 1 = resolved `light`. `fixed` mode: both = resolved `theme`. `RBS_THEME=<dark|light|high-contrast>`: both = that built-in theme. `RBS_TOKENS=<file>`: the themes resolve on that file instead of the built-in one.
- **On a change** of choice or theme, the shell sets the slots of every open window and gives native layers their new colours (preview selection outline, meters); each new window gets the same slots when it opens.
- **Font.** `look` is given a `FontCatalog` (`fn has_family(&self, name: &str, sample: &str) -> bool`): in the app the system font database, in tests a fake. The window's `default-font-family` is the first name in `font.family` for which `has_family` is true with the UI language's sample text (its alphabet and digits); none: the OS UI font (empty string). The choice is made again when the UI language changes.
- **Reading.** Files are read at start (the themes `appearance.toml` names), when the Appearance page opens (envelopes) and on the `ui.theme.reload` command (View › Reload themes): it reads the token file (`RBS_TOKENS` when set), `appearance.toml` and the theme files again, resolves the two slots as at start (with `RBS_THEME`, that built-in theme of the new file), applies them to every open window and refreshes the Appearance page; an open draft in the editor stays open and is checked again against the new file, and what cannot be used is reported as at start. Opening the page also reads the files again. Nothing watches files or polls (P13).
- **View › Theme** (`ui.theme.select`): a picker of at most the first ten themes (built-in Dark, Light and High contrast, then user themes by library name). Picking one runs the same Fixed-mode choice as Settings › Appearance › Use, without opening Settings. While Appearance follows the system, no choice is checked; picking still switches to Fixed.

## Errors and failure behaviour

Nothing panics. A theme that cannot be read or has an error is never applied (colour warnings do not count); the slot uses its built-in fallback and the Appearance page says why. Library errors are `DataError` (user data spec); check failures are `ThemeProblems`. A new error rule (a bound or a rule across tokens) can refuse a saved theme that passed before; a new colour rule only adds a warning: it falls back the same way and the page names the failed rule, so the user can fix the colour in the editor.

## Performance requirements

User-paced; never on a hot path. Resolving and checking a theme takes well under 1 ms; applying sets each window's globals once per change. No cost while the user does nothing.

## Conformance tests

- `crates/rbs-ui` (`tokens`): every built-in theme resolves with no overrides to its built-in values; each override group applies; fixed and non-themeable tokens refused; each bound at and beyond its edge; each contrast and hue rule reported with its token, also several at once; high-contrast thresholds on a high-contrast base; unknown valid names kept and reported, invalid names refused; format 1 token files read with an empty font family; the property test above.
- `crates/rbs-user-data/tests/themes.rs`: theme documents round-trip with stable text (overrides sorted, unknown names kept in their tables); refused documents (missing or unknown base, unknown top-level keys, a token in the wrong table, over-long author or description); library create, duplicate, import and export of themes; appearance defaults, round trip and unreadable file.
- `crates/rbs-ui` (`tokens`), additionally: the built-in file passes every theme rule; each new contrast pair, the ΔE rule (a theme setting `color.accent` or `color.meter.high` to the live colour is refused) and each cross-token rule refused at its edge; weight 299 and motion 119 and 201 refused; files over 64 KiB and 257 unknown names refused; the property test extended to the cross-token rules.
- `crates/rbs-app` (`look`): `every_drawn_pair_has_a_contrast_rule`; every themeable token in `THEMEABLE` has a generated slot field and setter (golden list); a resolved theme applied at run time changes a colour, a length, a duration, a text style and the font family of an open window and of a window opened later; native layer colours follow a change; a failing theme changes nothing; in `system` mode with two user themes, switching `Palette.color-scheme` switches every value and `Theme.high-contrast`; `fixed` mode fills both slots; fallbacks for a missing, newer, failing and high-contrast based theme; the font chosen is the first family the fake catalogue has with the sample text; Keep this look reverts after the timeout on a `TestClock` and keeps on Keep; `--reset-appearance` moves the file aside.
