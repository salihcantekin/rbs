---
name: ui-review
description: Checklist for any RBS change to user interface code, tokens, components, UI text or plugin UI. Use before finishing a UI change and when reviewing one.
---

# UI review

Rules: `docs/ui/principles.md` (U1-U21). Design system: `docs/specs/ui-design-system.md`. Decision: ADR-0015.

1. **Scope.** If the change alters a core flow (U1), adds a component or changes a token name, it is architecture scope: an independent review is required (`architecture-review`, ADR-0014).
2. **Tokens and components (U6).** No hard-coded colours, sizes, fonts, radii or durations. Only library components; a new component is added to the library with all states documented.
3. **Disclosure (U2).** New options are placed at the right level (Simple, Advanced, Expert) with a default and a one-line explanation.
4. **Core tasks (U1, U3).** No core task gets more interactions than before. Direct manipulation still works.
5. **Live safety (U4).** Undo works for the new actions. Nothing restarts outputs silently.
6. **Responsiveness (U5).** No work over 8 ms on the UI thread; hidden panels do not render or poll.
7. **Accessibility (U7).** Keyboard-only walkthrough done; focus visible; accessible names set; colour is never the only signal; contrast checked; reduce-motion respected.
8. **Text (U8, U13).** All strings in Fluent files; glossary terms; sentence case; layout survives 40 % longer strings.
9. **Status and errors (U9).** New failure modes have a plain-language message with a fixing action.
10. **Panels and workspaces (U21).** A new panel has a stable id (with parameters if it can exist per canvas), works at any size, can be closed, floated, reopened and moved with the keyboard alone, and does nothing while hidden. Layout state stays in the UI crate and uses `ui.*` commands, never the engine command log. The live indicator keeps state, duration and outputs in every workspace.
11. **Streaming specifics (U15-U20).** Canvas being edited is clear; mixer, settings, notifications, projectors and control-surface rules hold; no automatic modal while live.
12. **Evidence in the PR.** Screenshots in dark, light and high-contrast themes; short screen recording for interaction changes; checklist answers (OK / Issue / N/A, one line each).

The author fills this in; a different reviewer verifies the screenshots and the keyboard walkthrough.
