# ADR-0113: An experience level and the first-run choice

- Status: Accepted
- Date: 2026-11-13
- Deciders: Salih (owner)
- Amends: rule U2 (two sentences rewritten, one added, and its row in "How rules are checked"); rule U10 (adds the first-run question)
- Depends on: [ADR-0112](0112-calm-look-and-roomy-density.md) (`roomy`)
- Spec: [ui-design-system](../specs/ui-design-system.md); the app settings file `app.toml` (ADR-0056)

## Context

About nine in ten users have used OBS; the rest have not, and the owner wants both served by one app (2026-11-13). Rule U2 defines three levels (Simple, Advanced, Expert) and says every setting declares its level in a settings schema; U10 defines a first-run guide. Neither is built. There is no settings schema: app settings are hand-written rows (76 `SettingRow` uses in six files). What exists is the section: `SectionHeader` (a header row only) and the shells that hold a body, `FeatureSection`, `SettingsCard` and the private `Section` of `studio.slint`. All three fold with `visible: false`, because Slint cannot put `@children` under an `if`; a folded body is hidden, not removed. ADR-0094 A3 prescribes a nested "Advanced" section.

The app's own settings are in `app.toml` (`AppSettings` in `rbs-app` `src/background.rs`, format 1), where a field with a default is added without a format change.

## Decision

1. **The level belongs to a section, not to a row, and is written where the section is used.** A section of level advanced or expert is placed under an `if` inside its shell: `card := SettingsCard { more-hidden: !Experience.advanced; if Experience.advanced || card.show-all : FeatureSection { ... } }` (and `Experience.expert` for expert). `Experience` is a Slint global with two booleans set from the setting. The shells that hold sections (`SettingsCard`, `FeatureSection`, and a Properties or Mixer panel body) get two properties: `more-hidden`, which the author binds to the level of what the shell leaves out, and `show-all`, which the shell's own line sets (decision 4). A whole card of a higher level is written the same way one step up, with the page as its shell. No per-row level and no schema are introduced. Rows of a higher level are grouped into a section of that level, as ADR-0094 A3 already asks for "Advanced". An `if` at the call site holds concrete elements, not `@children`, so the section is not instantiated when its condition is false. A section that holds a setting with a validation error or a problem line (ADR-0094 D2, U9) is drawn whatever the level: that state is part of its condition.
2. **A user setting `experience`** has the values `simple`, `standard` and `pro`. It is a field of `AppSettings` in `app.toml`, so the format stays 1. When the field is absent, an existing install reads as `pro` (the user saw everything before) and a new install as `standard`. `app.toml` is written only when a General setting changes, so an existing install is one whose user data folder already holds any user file (a workspace, a scene collection, `appearance.toml` or `app.toml`) at the first start of the new build. It is changed in Settings > General and by the registry command `ui.experience.select`, a picker like `ui.theme.select` (ADR-0050), so it is a control API command (U20). Density gets the same: `ui.density.select`.
3. **What is drawn.** With `simple`, advanced and expert sections are not instantiated. With `standard`, advanced sections are (folded or open as saved, ADR-0078) and expert ones are not. With `pro`, all are. This applies wherever sections are used: Settings, Properties, Source settings and the Mixer's menus. U16 stands: mute, volume, monitoring and filters are simple.
4. **Nothing is stranded.** A shell whose `more-hidden` is true and whose `show-all` is false ends with one quiet line that it draws itself, "More options are hidden", and a `Show` action that sets its `show-all`; the unit is that one shell, not the page. It is not saved: in a window it lasts until the window closes, in a docked panel until the panel is closed or the app quits. The line is a `StatusLine` of kind info. A hidden setting keeps applying its stored value; the line is always there to say that more exists. No page filter exists today; when one is built (ADR-0094 C3) it must match hidden rows by name from data and set `show-all` on their shell.
5. **First run** asks one question with three cards (Simple, Standard, Pro), and offers the OBS import, automatic stream setup (U10) and the theme. It can be skipped; skipping means `standard`. `AppSettings` gets a second field, `first_run_done`. When it is absent, an existing install (as defined in decision 2) reads as true and a new install as false; the page is shown once while it is false and sets it. A user who upgrades therefore sees no page and no hidden option; they can choose a level in Settings > General. If `app.toml` is unreadable the app starts from defaults as today and the page shows again. The choice is applied once:

   | Choice | `experience` | Density (ADR-0112) | Starting layout |
   | --- | --- | --- | --- |
   | Simple | `simple` | roomy | Built-in Edit workspace with the Stats and Outputs tabs closed |
   | Standard | `standard` | regular | Built-in Edit workspace as shipped |
   | Pro | `pro` | compact | Built-in Edit workspace as shipped, studio mode on |

   Every choice keeps what the built-in Edit workspace docks: Scenes, Sources, the picture, Properties and the Mixer. Nothing the frame shows is removed, so each U1 core task starts as it does today and "Add and adjust a filter" keeps its count. These are adjustments applied once, not new built-in workspaces (U21 stands); "Reset workspace" returns the shipped Edit workspace, which only adds the two closed tabs back. Picking a card never replaces a workspace: it closes the two tabs in the current Edit workspace (Simple) or turns studio mode on (Pro), and sets density.
6. **Later changes of `experience`** change only which sections are drawn. They never move a panel and never change density.
7. **U2 as amended.** The rule "Every setting declares its level (`simple`, `advanced`, `expert`) in its settings schema. The level is chosen in review using the personas above, and the review checks the declared level, not the rendered screen." becomes: "Every setting has a level (`simple`, `advanced`, `expert`). Until a settings schema exists, the level is declared by the section the setting sits in (ADR-0113). The level is chosen in review using the personas above, and the review checks the declared level, not the rendered screen." Level 3, "Reached from a single \"Expert\" entry point.", becomes: "Expert sections are drawn in place when `experience` is `pro`, and otherwise reached through the \"More options are hidden\" line. The media graph view and per-node metrics keep a single \"Expert\" entry point." Added: "The `experience` setting decides which levels are drawn; a place that leaves options out says so and offers to show them." In "How rules are checked" the U2 row becomes "Declared section levels reviewed against the personas". **Added to U10:** "First run asks how the user wants to start (Simple, Standard, Pro) and sets the starting layout, density and drawn levels from the answer."

## Consequences

- Each existing advanced or expert setting must sit in a section with that level. Assigning levels is a visible change per screen and goes through the owner's approval with that screen's migration.
- Zero cost (rule 10): a section above the level is under a false `if` and is not instantiated; nothing is read per frame. The first screen migrated (Settings > Stream) is the proof: its UI test asserts that an expert section's element does not exist in `simple` and appears after `Show`.
- An OBS import brings advanced values that a Simple user does not see; the "More options are hidden" line is the trace. A stronger mark (which hidden values differ from their defaults) needs defaults per setting and is left for a later decision.
- Tests: a unit test on the function that maps `experience` to the two booleans; `app.toml` round trips with and without the new fields; one UI test per window type as above.
- Reversal: removing the setting draws everything again; the stored value is ignored. The first-run page is independent of the rest.

## Alternatives considered

- **A `level` property on `SectionHeader` and `FeatureSection`** (round 2 text): the shells hide with `visible: false`, so the property could not keep a section from being instantiated, and `SectionHeader` has no body to hide.
- **A level on every row with a settings schema.** The right long-term shape and what U2 said, but the schema does not exist and would have to be built for every page before any benefit. The section level can move to rows later without changing what the user sees.
- **Advanced in place only, no global setting** (U2 as written): every user meets every Advanced section; the beginner is not helped on the first screen.
- **A separate simple app or mode with its own screens:** two products to maintain.
- **Auto-hiding Properties in Simple** (round 2 text): adds an interaction to a core flow that the spec limits.
- **A level switch in every Settings page header:** repeats one global setting on every page; the in-place line of decision 4 does the job where it is needed.

## Review

Independent review per ADR-0014. Round 1 (2026-11-13, independent AI session, changes requested) reviewed a larger text; its concerns 8 and 9 and alternative (b) of 15 are answered here. The full round 1 record is in ADR-0115.

Round 2 (2026-11-13, independent AI session, changes requested on all four texts). The full record is in ADR-0115. Concerns 3, 12, 13 and 18 are answered here: the level is an `if` at the call site and the Context no longer claims folded bodies are removed (3); the starting layouts keep Properties docked and make no claim about Transition (12); the U2 sentences are rewritten, not only added to (13); storage is `app.toml`, with `first_run_done`, a picker command, a density command and a defined life for `Show` (18).

Round 3 (2026-11-13, independent AI session; this text: changes requested with one blocking concern). The full record is in ADR-0115. Answered here: the unit of `Show` is the shell, which gets `more-hidden` and `show-all` and draws the line itself (4, blocking); an upgrading user gets no page and `pro`, and a card never replaces a workspace (5); a section with a problem is always drawn, and the page filter is a requirement on later work (6).

Follow-up check by the same reviewer (2026-11-13, texts at `26c3d03`): approve, with one non-blocking concern, that many existing users have no `app.toml` and would have read as new installs. Fixed here: an existing install is recognised by any user file in the user data folder.

- 2026-11-13: Owner (Salih) accepts this ADR after the independent review recorded above (rounds 1 to 3 and the follow-up check; reviewer verdict: approve). Status: Accepted.
