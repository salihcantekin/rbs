# ADR-0113: An experience level and the first-run choice

- Status: Proposed
- Date: 2026-11-13
- Deciders: Salih (owner)
- Amends: rule U2 (adds a default for which levels are shown); rule U10 (adds the first-run question)
- Spec: [ui-design-system](../specs/ui-design-system.md), [user-data](../specs/user-data.md)

## Context

About nine in ten users have used OBS; the rest have not, and the owner wants both served by one app (2026-11-13). Rule U2 defines three levels (Simple, Advanced, Expert) shown "in the same place", and U10 a first-run guide. Neither is built. There is no settings schema with a level per setting: app settings are hand-written rows (76 `SettingRow` uses in six files). What does exist is the section: `SectionHeader` and `FeatureSection`, whose folded body is not instantiated (ADR-0094 A3), and the nested "Advanced" section that ADR-0094 A3 prescribes.

## Decision

1. **The level belongs to a section, not to a row.** `SectionHeader` and `FeatureSection` get a `level` property: `simple` (default), `advanced` or `expert`. No per-row level and no new schema are introduced. Rows that belong to a higher level are grouped into a section of that level, as ADR-0094 A3 already asks for "Advanced".
2. **A user setting `experience`** has the values `simple`, `standard` and `pro`. It is stored with the app's general settings in the user data folder and changed in Settings > General and by the registry command `ui.experience.set` (so it is a control API command, U20).
3. **What is drawn.** With `simple`, sections of level advanced and expert are not instantiated. With `standard`, advanced sections are drawn (folded or open as saved) and expert ones are not. With `pro`, all are drawn. This applies wherever sections are used: Settings, Properties, Source settings and the Mixer's menus. U16 stands: mute, volume, monitoring and filters are simple.
4. **Nothing is stranded.** Where a card or panel has sections that the level leaves out, it ends with one quiet line, "More options are hidden", and a `Show` action that draws them for that card until the window closes. The line is a `StatusLine` of kind info. A search result inside a hidden section does the same and scrolls to it. A hidden setting keeps applying its stored value; the line is always there to say that more exists.
5. **First run** asks one question with three cards (Simple, Standard, Pro), and offers the OBS import, automatic stream setup (U10) and the theme. It can be skipped; skipping means `standard`. The choice is applied once:

   | Choice | `experience` | Density (ADR-0112) | Starting layout |
   | --- | --- | --- | --- |
   | Simple | `simple` | roomy | Built-in Edit workspace with Properties auto-hidden to its edge tab and Stats, Outputs and Library closed |
   | Standard | `standard` | regular | Built-in Edit workspace as shipped |
   | Pro | `pro` | compact | Built-in Edit workspace with studio mode on and Outputs and Stats open |

   Every starting layout keeps Scenes, Sources, the picture, the Mixer, Transition and the live controls, so each U1 core task still starts from the main window. These are adjustments to the built-in Edit workspace applied once, not new built-in workspaces (U21 stands).
6. **Later changes of `experience`** change only which sections are drawn. They never move a panel and never change density.
7. **Wording added to U2:** "The `experience` setting decides which levels are shown by default; a place that leaves options out says so and offers to show them." **Added to U10:** "First run asks how the user wants to start (Simple, Standard, Pro) and sets the starting layout, density and shown levels from the answer."

## Consequences

- Each existing advanced or expert setting must sit in a section with that level. Assigning levels is a visible change per screen and goes through the owner's approval with that screen's migration (ADR-0114).
- Zero cost (rule 10): a section above the level is not instantiated; the level adds one property read per section when it is built, nothing per frame.
- An OBS import brings advanced values that a Simple user does not see; the "More options are hidden" line is the trace. A stronger mark (which hidden values differ from their defaults) needs defaults per setting and is left for a later decision.
- Tests: a unit test on the function that maps `experience` and a section level to shown or not; one UI test per window type that a hidden section is not instantiated and that `Show` draws it.
- Reversal: removing the setting draws everything again; the stored value is ignored. The first-run page is independent of the rest.

## Alternatives considered

- **A level on every row with a settings schema.** The right long-term shape and what U2 literally says, but the schema does not exist and would have to be built for every page before any benefit. The section level can move to rows later without changing what the user sees.
- **Advanced in place only, no global setting** (U2 as written): every user meets every Advanced section; the beginner is not helped on the first screen.
- **A separate simple app or mode with its own screens:** two products to maintain.
- **A level switch in every Settings page header:** repeats one global setting on every page; the in-place line of decision 4 does the job where it is needed.

## Review

Independent review per ADR-0014. Round 1 (2026-11-13, independent AI session, changes requested) reviewed a larger text; its concerns 8 and 9 and alternative (b) of 15 are answered here. The full round 1 record is in ADR-0115.
