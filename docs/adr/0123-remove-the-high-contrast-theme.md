# ADR-0123: Remove the built-in high-contrast theme; Dark and Light are the built-in themes

- Status: Proposed
- Date: 2026-11-15
- Deciders: Salih (owner)
- Supersedes in part:
  - **ADR-0071:** in decision 2 (Accessibility), the sentence that RBS uses its high-contrast theme while a Windows contrast theme is on.
  - **ADR-0112:** the high-contrast lines of decisions 2 and 3 ("High contrast keeps its colours", the three-theme figures, High contrast keeping the panel outline).
  - **ADR-0057:** the third built-in theme in the context and in decisions 1 and 3 (`base` is `dark` or `light`; there are no high-contrast thresholds), and review item 6 (fallback keeps high contrast).
  - **ADR-0072:** the high-contrast thresholds in decision 1 and the consequence that a `high-contrast` theme keeps `Theme.high-contrast`.
  - **ADR-0115:** decision 8, the high-contrast screenshot. Screenshots for approval are dark and light.
  - **ADR-0118:** the High contrast rows and figures of decisions 3 and 6 (the rule that high contrast outlines every unfilled kind in `border.strong`, the 7:1 thresholds, "all three built-in themes") and of its consequences.
  - **ADR-0120:** decision 3 (high contrast is always calm), the high-contrast case of decisions 2 and 5, and the strict built-in check that skips high contrast.
  - **ADR-0122:** "high contrast stays plain" and the high-contrast screenshot.
- Builds on: [ADR-0057](0057-user-themes-and-appearance-settings.md), [ADR-0013](0013-zero-cost-when-unused.md), [ADR-0015](0015-ui-design-rules.md)

## Context

RBS ships three built-in themes: `dark`, `light` and `high-contrast` (rule U12, ADR-0057). Every theme is a full colour set in `crates/rbs-ui/tokens.toml`, every contrast rule carries a second, stricter high-contrast minimum, the generated `Theme` global has a `high-contrast` flag that library components branch on, the effects (ADR-0120, ADR-0122) have a "high contrast is always calm" rule, and every visible change needs a third screenshot when it touches a surface, border or outline (ADR-0115 decision 8).

The owner decided on 2026-11-15 that Dark and Light are enough for now and that high contrast is removed from the whole system, including the screenshot sets. Keeping a theme nobody tests well costs every UI change a third check and keeps branches in the shared components.

The OS's own high-contrast setting is a separate thing: on Windows the app still reads it (`win::high_contrast`, ADR-0071) and counts it as a light OS scheme, so the title bar and the theme slot follow the light side. That stays; RBS no longer switches to a theme of its own for it.

## Decision

1. **Two built-in themes.** The token file defines `[themes.dark]` and `[themes.light]` only; a token file with any other theme table is refused, as before for an unknown table. `rbs_ui::tokens::THEMES` is `["dark", "light"]`, and `Base` has `Dark` and `Light`.
2. **One contrast minimum per rule.** `CONTRAST_RULES` and `THEME_CONTRAST_RULES` are `(foreground, background, minimum)`. The high-contrast column (7:1 for text) is removed with the theme.
3. **No high-contrast branches.** The generated `Theme` global has no `high-contrast` property and `ResolvedTheme` has no `high_contrast()`. Components draw outlines from `Theme.button-outline` and `Theme.panel-outline` alone; nothing in the UI, the effects resolution or the editor tests for high contrast. ADR-0120 decision 3 is gone: a theme's effects resolve the same way on both bases.
4. **Old files keep opening.** `high-contrast` is a retired base name (`rbs_ui::tokens::RETIRED_BASES`) that reads as `dark`, everywhere a base is read:
   - a user theme with `base = "high-contrast"` opens on Dark with its overrides, and is written back with `base = "dark"` the next time it is saved;
   - `appearance.toml` slots that name `builtin:high-contrast` read as `builtin:dark`, and are written back that way the next time the appearance is saved;
   - `RBS_THEME=high-contrast` gives Dark;
   - `ui.theme.switch {id: "builtin:high-contrast"}` (control API) uses Dark instead of being refused.
   No notice is shown: Dark is the closest look, and a theme's own overrides still apply. Nothing is migrated on disk until the user saves.
5. **Screenshots are dark and light.** Rule 16's evidence and every review checklist ask for dark and light only. Older ADRs that list a high-contrast screenshot in their procedure follow this decision. High-contrast screenshots that an accepted ADR links as its evidence (ADR-0118's `buttons/tint-high-contrast.png`) stay as history; the others are deleted.

## Consequences

- Smaller token file, generated globals and components; every UI change needs two screenshots instead of three for surface, border and outline changes.
- A user who chose High contrast sees Dark after the update, and can pick Light or make a user theme with stronger contrast (the theme editor and the contrast rules stay). The colour rules still warn on a user theme that breaks the normal minimums (ADR-0072).
- Rule U12's "High-contrast theme is provided" is removed from `docs/ui/principles.md`; the design system and themes specs list two themes.
- Bringing high contrast back later is a new ADR: a third theme table, its thresholds and its screenshots. Detecting the OS high-contrast setting stays a listed follow-up of ADR-0057 (themes spec, Scope).
- Contract: `rbs_ui::tokens::Base` loses a variant, `ResolvedTheme::high_contrast()` goes, the contrast rule tuples lose a field, and the Slint `Theme.high-contrast` property and `ThemeSlot` field go. Only workspace crates (`rbs-ui`, `rbs-user-data`, `rbs-app-ui`, `rbs-app`) use them; no plugin SDK, scene format or control API schema changes. The theme and appearance file formats keep their numbers, since every old file still reads.
- Tests: the high-contrast tests are removed; a test pins that `high-contrast` reads as Dark in a theme body, in a `builtin:` id and in `read_base`.

## Alternatives considered

- **Keep the theme but hide it from the picker.** Leaves the tokens, branches and checks in place for a theme nobody can choose; rejected (no dead tokens).
- **Refuse files that name `high-contrast`.** A user theme would fall back with an error and lose its overrides; reading it as Dark keeps the user's work.
- **Map it to Light.** High contrast was a dark look (black surfaces, light text); Dark is the closer match.

## Review

Round 1: independent AI session (fresh context, given only the ADR, the specs, the superseded ADRs and the diff), 2026-11-15. Verdict: approve with changes. One blocking point, eight non-blocking:

1. Blocking: ADR-0071 decision 2 says RBS uses its high-contrast theme under a Windows contrast theme. Resolved: ADR-0071 added to the supersede list and the index; Context says what happens now (the OS setting counts as light).
2. ADR-0118 has high-contrast lines in decision 6 and the consequences too. Resolved: the entry names decisions 3 and 6 and the consequences.
3. ADR-0112 decisions 2 and 3 name High contrast. Resolved: added to the supersede list and the index.
4. The kept `buttons/tint-high-contrast.png`. Resolved: decision 5 keeps screenshots an accepted ADR links as evidence.
5. No contract section; `ui.theme.switch` with the old id. Resolved: a Contract consequence, decision 4 and the control API spec name the mapping.
6. Write-back of `appearance.toml` untested. Resolved: `appearance_defaults_round_trip_and_unreadable_file` writes a `builtin:high-contrast` file, saves it and checks `builtin:dark`.
7. The base error text listed the names by hand. Resolved: built from `THEMES`.
8. Follow-up wording differed from the themes spec. Resolved: the Consequences point to the spec's Scope line.
9. Older ADRs list high-contrast screenshots in their procedures. Resolved: decision 5 says they follow it.

Owner approval: pending.
