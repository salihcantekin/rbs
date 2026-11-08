# ADR-0072: Theme colour rules warn instead of refusing; every editor change shows at once

- Status: Proposed
- Date: 2026-11-08
- Deciders: Salih (owner)
- Supersedes in part: ADR-0057 decisions 3 (theme rules), 5 (fallback of a theme that fails a check) and 6 (theme editor); rules U7 and U12 (custom themes)
- Spec: [themes](../specs/themes.md)

## Context

ADR-0057 checks every resolved user theme with the contrast rules of the design system, the hue rule against live red and a ΔE2000 15 distance from `color.state.live`, besides bounds and rules across tokens. A theme that fails any rule is not used: at start its slot falls back to the built-in theme of its base, and in the theme editor the whole draft is held (not applied) and cannot be saved while one problem remains.

The owner tested the editor on macOS on 2026-11-08 and found it unusable for designing colours: one change (window background to a mid blue) broke the 3:1 rule of the live red on it, after which every later change, valid or not, stayed invisible and Save stayed off. The rule texts ("color.state.live on color.surface.base has contrast 1.73, needs 3") did not say what to change. The owner's decision (2026-11-08): a user must be able to pick any colours for their own theme; a poor choice gets a small warning but is never blocked. A colour choice cannot crash or break the app; it can only make the app hard to read, which the user sees at once and can undo.

Bounds and rules across tokens are different: a 2 px control, a line height taller than the control that holds it, or a 0 ms motion break the layout or rule U7/U11 targets in ways the user may not be able to see or reach back from, and the editor's fields already keep values within bounds.

## Decision

1. **Colour rules are warnings for user themes.** In a resolved user theme, these rules produce *warnings*, not errors: every contrast rule (`CONTRAST_RULES` and `THEME_CONTRAST_RULES`, normal and high-contrast thresholds), the hue rule of `color.state.error`, `color.action.destructive` and `color.meter.clip` against live, `color.state.live` on the live surfaces, and the ΔE2000 distance of every theme colour from live. A theme with warnings resolves and is used, at start and in the editor, and is saved like any other.
2. **Everything else still refuses the theme**, as in ADR-0057: a fixed or non-themeable token, a value of the wrong kind, a value out of bounds, a rule across tokens, too many unknown names, a bad font family name, a file over 64 KiB. These are *errors*.
3. **Built-in themes keep every rule as an error.** `Tokens::parse` refuses a built-in token file whose themes fail any rule, colour rules included, so RBS never ships a theme with a warning. `color.state.live` and `color.text.on-live` stay fixed and refused in a theme (U12).
4. **`ThemeProblem` gets a severity and a rule.** `rbs_ui::tokens::ThemeProblem` gains `severity: Severity` (`Error` or `Warning`) and `rule: Option<ColorRule>` (the contrast pair with its ratio and need, or the live hue or look rule, so a UI words it with labels). `Tokens::resolve` returns `Err(ThemeProblems)` only when an error exists, and then the list holds errors and warnings together (the editor shows both of a held draft); on success the `ResolvedTheme` carries its `warnings` (empty for a built-in theme; it takes part in `PartialEq`). `rbs_ui::tokens::suggest_colour` returns the colour with the same hue and saturation and the nearest lightness that clears a token's warnings, for the editor's Fix button. The crates are unpublished, so the contract version is the document `format`, unchanged. A file named by `RBS_TOKENS` is checked like the built-in file (every rule an error).
5. **The editor applies every change at once.** Each edit is applied to every window that uses the theme as soon as it resolves, warnings or not; Save is on whenever the draft differs from the saved theme and has no error; a value still being typed counts as a change, as before. A draft with an error is held exactly as ADR-0057 decision 6 describes (the field's bounds make that rare). Cancel returns to the saved theme. **Ways back from an unreadable theme:** an edit that takes primary text below contrast 1.5 on a surface the editor draws (`surface.base`, `.raised`, `.overlay`) in a theme in use asks "Keep this look?" (15 s, Escape is Undo, as for a font change); the Cancel button restores the saved theme; `rbs-app --reset-appearance` covers an unreadable theme at start. An unreadable *saved* theme at start is covered by `--reset-appearance` and by the Appearance page's theme picker, whose built-in themes are always readable.
6. **Warnings are words with a way to fix them.** A warning names the tokens by their labels, not their ids, and is shown on both rows of the pair it concerns. The live colour warnings ("the Live badge is hard to see on this colour") are shown above the others. Each warning says what to change (U9) and, when `suggest_colour` finds one, the row has a **Fix** button. The owner chose this presentation on 2026-11-08 (plain sentence and Fix); screenshots go to the owner before merge.
7. **U12 is amended.** "A theme is a token file that must pass the contrast check and cannot change `color.state.live`" becomes "A theme is a token file that cannot change `color.state.live`; built-in themes pass the contrast check, and a user theme that does not is used with a warning."

## What keeps live unambiguous

`color.state.live` and `color.text.on-live` stay fixed in every theme, and live is never shown by colour alone (U7, U12): the badge always carries its words. A theme (including an imported one) can still make other colours look like live, so the live warnings are the loudest ones, are listed first in a row, and are counted in the editor.

## Consequences

- Users can design freely and see each change at once; Save works for every change the fields allow.
- A user theme, including an imported one, can be hard to read. The Appearance page lists its warnings in the theme list and the editor, so the cause is visible; built-in themes and the fallback stay readable for recovery.
- `every_drawn_pair_has_a_contrast_rule` stays: the rules still exist and still guard the built-in themes; it only changes what a user theme's failure means.
- A theme on a `high-contrast` base that breaks the 7:1 thresholds keeps its base and `Theme.high-contrast`; its warnings show like any other.
- Applying every change at once is user-paced (a resolve and a slot update per change, well under 1 ms) and costs nothing while the user does nothing (P13).
- Tests change (besides the rules above): the ADR-0057 property test now says every override set either resolves with only colour warnings or is refused for an error; `Tokens::parse` still refuses each kind of colour rule in a built-in file; an unreadable colour asks "Keep this look?"; Fix clears a token's warnings; `every_drawn_pair_has_a_contrast_rule` is unchanged. Earlier list: user themes that fail a colour rule resolve with warnings (one test per rule kind), errors still refuse, the built-in file still refuses a failing theme, start uses a theme with warnings instead of falling back, and the editor applies a failing colour draft and enables Save.

## Alternatives considered

- **Keep refusing, explain better** (a banner saying the changes are not shown): no ADR change, but designing stays blind until every rule passes. The owner rejected blocking.
- **Hold only the failing token** while the rest of the draft applies: safe, but a pair rule often fails on the token the user did not change, which is confusing.
- **All rules as warnings, bounds and cross-token rules too**: maximum freedom, but those values can break the layout or make controls too small to reach and the fields already keep them in range; left as errors.
- **A setting that turns the checks off**: one more option to explain for the same outcome; warnings that never block make it unnecessary.

## Review

Pending: independent review per ADR-0014.
