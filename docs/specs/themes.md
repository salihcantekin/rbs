# Spec: Themes and appearance

- Status: Draft
- Owner crate(s): `rbs-ui` (`tokens`: theme model, resolving, checks, bounds, token metadata), `rbs-user-data` (theme and appearance documents, theme library), `rbs-app` (Settings › Appearance, theme editor, applying themes to windows and native layers)
- Related ADRs: [0015](../adr/0015-ui-design-rules.md), [0049](../adr/0049-main-window-areas-panel-states-and-sizes.md), [0051](../adr/0051-user-data-profiles-and-scene-collections.md), [0057](../adr/0057-user-themes-and-appearance-settings.md)
- Rules: [UI principles](../ui/principles.md) U7, U11, U12, U17

## Purpose

Users change the look of RBS with themes: files that override design tokens on top of a built-in theme, kept in a library, shared as single files, and chosen in Settings › Appearance. This spec defines the theme file, the appearance file, the checks every theme passes and how the app applies a theme.

## Scope

In scope: the theme document (format 1), resolving a theme on its base, checks and bounds, the theme library, the appearance document, the Appearance page's behaviour, applying a theme at run time.

Out of scope, later changes: themes in `.rbspack` packs; font files; a curated accent shortcut; detecting the OS high-contrast setting; theme methods in the control API; a text size preference. The token names, groups and built-in values are in the [design system spec](ui-design-system.md#tokens).

## Contract

### Themeable tokens (`rbs_ui::tokens`)

| Group | Tokens | Value | Bounds |
| --- | --- | --- | --- |
| Colours | every name in `THEME_COLORS` | `"#rrggbb"` (opaque) | contrast and hue rules of the design system |
| Font | `font.family` | array of family names | 0 to 8 names, each 1 to 64 characters, no control characters |
| Text | `type.caption`, `.body`, `.label`, `.title`, `.heading` | `{ size, line, weight }`, whole style | size 11 to 48, line from size to 2 x size, weight 100 to 900 |
| Spacing | `space.1` ... `space.8` | logical px | 0 to 64 |
| Controls | `size.control.sm` / `.md` / `.lg` | logical px | 24 to 64 (U7) |
| Shapes | `radius.sm` / `.md` / `.lg` | logical px | 0 to 32 |
| Lines | `size.border` | logical px | 1 to 4 |
| | `size.focus-ring` | logical px | 2 to 6 (U7) |
| Icons | `size.icon.box` / `size.icon.stroke` | logical px | 12 to 32 / 1 to 3 (U7) |
| Menus | `size.menu.sm` / `.md` | logical px | 160 to 480 |
| Motion | `motion.fast` / `motion.normal` | ms | 0 to 500 |

`THEMEABLE` lists them with metadata for the editor: name, label in words, group, value kind, bounds. Not themeable, refused in a theme: `color.state.live`, `color.text.on-live` (U12), `size.window.*`, `size.floating.*`, `step.*`.

The built-in token file gains `[font] "font.family" = []` (empty: the OS UI font); `tokens.toml` becomes format 2 and `Tokens::parse` still reads format 1 files (given the empty family), so `RBS_TOKENS` files keep working.

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

Other top-level keys are refused (ADR-0051 decision 2). Inside the tables, a key that is a valid token name (lowercase words joined by `.` or `-`) but not known to this build is kept, rendered back on save in its table, not applied, and reported (`ResolvedTheme::unknown`); an invalid name is refused. Rendering is deterministic and writes overrides sorted by name.

### Resolving and checking

`Tokens::resolve(&self, theme: &ThemeOverrides) -> Result<ResolvedTheme, ThemeProblems>` on the built-in tokens of the running build:

- starts from `base`'s colour set and the shared lengths, durations, text styles and font family, then applies each override;
- runs every check and returns all problems at once, each `ThemeProblem { token: Option<String>, reason }` naming the token it belongs to (the editor shows it beside the token): a non-themeable token, a value of the wrong kind, a value out of bounds, each failed contrast rule (naming the foreground token, with the ratio found and needed), each failed hue rule; contrast thresholds are the high-contrast ones when `base` is `high-contrast`;
- `ResolvedTheme` holds `base`, one colour set (plus the fixed colours), lengths, durations, text styles, the font family list and the unknown names.

Property test: for any set of overrides drawn from the themeable tokens with values in and out of bounds, `resolve` either returns a theme that passes every check of the built-in file or returns at least one problem, and never panics.

### Theme library (`rbs-user-data`)

`UserData::themes() -> Library<Theme>` on `themes/` with every rule of the [user data spec](user-data.md#libraries-library). Export scrubs nothing. Built-in themes are not in the library; the app lists them first, as Dark, Light and High contrast, with the ids `builtin:dark`, `builtin:light`, `builtin:high-contrast`. Duplicating a built-in theme creates a user theme with that base and no overrides; duplicating a user theme copies its base and overrides.

### Appearance document (`appearance.toml`, `kind = "appearance"`, format 1)

| Key | Type | Default |
| --- | --- | --- |
| `mode` | `"system"` or `"fixed"` | `"system"` |
| `dark` | theme id | `"builtin:dark"` |
| `light` | theme id | `"builtin:light"` |
| `theme` | theme id | `"builtin:dark"` |

`UserData::appearance()` returns the defaults and the reason when the file cannot be read; `set_appearance(&WriterLock, ..)` writes it through the user data writer. A theme id is `builtin:<name>` or a library id. An id that names no usable theme falls back to `builtin:dark` (for `dark` and `theme`) or `builtin:light` (for `light`); the app shows a note and does not rewrite the file.

### Settings › Appearance (`rbs-app`)

- Theme choice: "Follow system" with "When the system is dark" and "When the system is light" pickers, or one theme. Pickers list built-in themes first, then user themes by name, each with a swatch of `color.surface.base`, `color.text.primary` and `color.accent`. A change is applied at once and written to `appearance.toml`.
- Themes list: Use, Edit (user themes), Duplicate, Rename, Delete, Import, Export (file dialogs as for output profiles). An unusable theme shows its first problem and "Edit" to see all; it cannot be used.
- Theme editor: groups Colours, Text, Font, Spacing and size, Shapes, Motion; per token a label, the control (ColorPicker; NumberField; text styles as size, line and weight fields; font family as an editable list with a note naming the family in use), a reset button that removes the override, and its problems. A passing edit is applied to every window at once; a failing edit is shown with its problem and not applied; Save is disabled while any problem remains; Cancel restores the saved theme. Unknown tokens are listed read-only as "Not used by this version". Built-in themes open read-only with "Duplicate to edit".
- While `RBS_TOKENS` or `RBS_THEME` is set, the page shows a note that a development override is in effect.

### Applying (`rbs-app` `look`)

- The shell keeps the resolved themes in use: in `system` mode the dark and light ones, in `fixed` mode one. The generated `Theme` and `Tokens` globals hold a value per slot for every themeable token, and `Theme.active` picks the slot, so an OS scheme change needs no Rust code and no file reads.
- On a change of choice or of a theme, the shell sets the globals of every open window, sets the window's default font family to the first installed family of `font.family` (none installed or empty: the OS UI font), and gives native layers their new colours (preview selection outline, meters). Each new window gets the same values when it opens.
- Files are read at start (the themes `appearance.toml` names), when the Appearance page opens (envelopes) and on the `ui.theme.reload` command. Nothing watches files or polls (P13).

## Errors and failure behaviour

Nothing panics. A theme that cannot be read or fails a check is never applied; the slot uses its built-in fallback and the Appearance page says why. Library errors are `DataError` (user data spec); check failures are `ThemeProblems`.

## Performance requirements

User-paced; never on a hot path. Resolving and checking a theme takes well under 1 ms; applying sets each window's globals once per change. No cost while the user does nothing.

## Conformance tests

- `crates/rbs-ui` (`tokens`): every built-in theme resolves with no overrides to its built-in values; each override group applies; fixed and non-themeable tokens refused; each bound at and beyond its edge; each contrast and hue rule reported with its token, also several at once; high-contrast thresholds on a high-contrast base; unknown valid names kept and reported, invalid names refused; format 1 token files read with an empty font family; the property test above.
- `crates/rbs-user-data/tests/themes.rs`: theme documents round-trip with stable text (overrides sorted, unknown names kept in their tables); refused documents (missing or unknown base, unknown top-level keys, a token in the wrong table, over-long author or description); library create, duplicate, import and export of themes; appearance defaults, round trip and unreadable file.
- `crates/rbs-app` (`look`): a resolved theme applied at run time changes a colour, a length, a duration, a text style and the font family of an open window; a failing theme changes nothing; both system slots are applied and `Theme.active` switches between them.
