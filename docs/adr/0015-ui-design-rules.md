# ADR-0015: Binding UI design rules and a single design system

- Status: Accepted, amended by [ADR-0017](0017-flexible-panels-and-workspaces.md) and [ADR-0049](0049-main-window-areas-panel-states-and-sizes.md) (U5)
- Date: 2026-11-02
- Amends: ADR-0014 (adds core UI flows, new UI components and token names to the architecture review scope)

## Context

A modern, clear and easy UI is one of RBS's main promises, and a common weak point of OBS: dated look, settings spread across dialogs, many tasks requiring docs or plugins. Without binding rules, every contributor and every AI agent makes local UI choices, and the result drifts into inconsistency. UI quality also has to survive plugins that add their own panels.

## Decision

1. The rules U1-U20 in [UI principles](../ui/principles.md) are binding for all RBS interfaces, including plugin UI where stated. They can only be changed by a superseding ADR.
2. One design system ([ui design system spec](../specs/ui-design-system.md)) defines tokens and components. UI code uses tokens and library components only.
3. User-facing terms come from the [glossary](../ui/glossary.md).
4. The core tasks in U1 have time and success-rate targets checked by usability tests before minor releases; a failing flow blocks the release unless the owner waives it with a written reason (as defined in U1).
5. Every UI pull request follows the `ui-review` skill: light and dark screenshots, keyboard check, token use, glossary terms. UI changes that alter a core flow, add a component or change a token name are architecture-scope changes under ADR-0014 and need an independent review.
6. These rules are toolkit-independent. The desktop toolkit decision is a separate ADR and must meet the toolkit requirements in the spec.

## Consequences

- Consistent, accessible UI regardless of who writes it; AI agents have concrete, checkable rules.
- Building the component library and token pipeline comes before feature screens in Phase 2.
- Some plugin authors will find the rules restrictive; native-looking plugins are the intended result.
- Usability testing needs at least five testers per affected flow per minor release; remote unmoderated tests keep this practical, and the owner can waive with a written reason.

## Alternatives considered

- Guidelines instead of rules: easier to write, ignored under time pressure.
- Copying OBS's layout and interaction model: familiar to existing users, keeps its weaknesses. We keep familiar names and positions where they work (Scenes, Sources, Mixer panels) and change the rest.

## Review

- Reviewer: independent AI session (fresh context; given only this ADR, the UI rules, specs, principles and related ADRs), reviewing as architect and UX designer
- Date: 2026-11-02
- Verdict: approve after changes
- Concerns: (1) core-flow limits contradicted the spec -> "interaction" defined, per-flow maximums in the spec; (2) red reserved but used for errors -> only `color.state.live` reserved, distinct error/destructive tokens; (3) target size contradiction -> 24 px floor, 28 px default; (4) "everything undoable" contradiction -> edits undoable, non-undoable list in spec; (5) silent widening of ADR-0014 -> declared as amendment; (6) missing areas -> U15-U20 (canvases, mixer, settings, notifications, projectors, control surfaces), OBS import; (7) idle redraw and remote preview -> U5 updated; (8) impractical release gate -> median time, success rate, owner waiver; (9) Space as cut key -> Ctrl/Cmd+Enter; (10)-(17) glossary, setting levels, font, binding scope, naming, plugin lint, theming, caption size fixed. Second round: spec contracts for setting levels, dock panels, canvases and remote preview added or recorded as open items.
- Owner approval: Salih, 2026-11-02 (project thread)
