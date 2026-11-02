# ADR-0017: Flexible panels and saved workspaces

- Status: Accepted
- Date: 2026-11-02
- Amends: ADR-0015 (replaces the layout part of rule U11 with the new rule U21)

## Context

Streamers use the app differently while preparing and while live. During a stream many want only the program view, live controls and audio, sometimes on a second monitor next to chat. A fixed layout either shows too much or hides what someone needs. The project owner asked for panels that can be opened and closed freely and for saved layouts that can be switched, especially for live use.

## Decision

1. Rule U21 in [UI principles](../ui/principles.md#u21-flexible-panels-and-workspaces) is binding: every panel can be closed, moved, docked, tabbed, floated and placed on any monitor; only the live indicator always stays visible.
2. Arrangements are saved as workspaces in a documented, versioned TOML format ([design system spec](../specs/ui-design-system.md#panels-and-workspaces-contract-for-u21)). Workspaces can be exported, imported and switched with one interaction or a shortcut, and optionally switched automatically when going live and when the stream ends.
3. RBS ships built-in workspaces (Edit, Live, Minimal, Audio) as defaults that users can change and restore.
4. Hidden panels cost nothing (ADR-0013); switching workspaces never touches sources or outputs.
5. Workspaces are client-side UI state owned by the UI crate. They are controlled through client-scoped `ui.*` control API commands that are routed to one UI client and never written to the engine command log; layout changes have their own undo history ([control API](../specs/control-api.md#client-scoped-ui-commands)).
6. The live indicator (live and recording state, duration, active outputs, in text) and the workspace and Panels menus stay in the window frame in every workspace, so U4 holds even with every panel closed.
7. The docking system is part of the component library and must be supported by the chosen native toolkit (ADR-0016). It is added to the toolkit requirements.

## Consequences

- A docking and floating-window system is a significant piece of the component library and a requirement for the toolkit prototype; if the chosen toolkit lacks one, we build it.
- Every panel must work at any size and in any position, so panels are designed responsively from the start.
- Workspace files become a public format with versioning, a JSON Schema and migration. Display placement uses EDID fingerprints locally and is stripped on export.
- No workspace shortcuts are bound by default, to avoid AltGr and desktop environment clashes.

## Alternatives considered

- Fixed layout with a few presets: simpler, but does not meet the owner's requirement or the needs of power users.
- Free layout without saved workspaces: flexible, but users would rebuild their live layout by hand every time.

## Review

- 2026-11-02, independent AI review (fresh session given only this ADR, the UI principles, the design system spec, the control API spec, ADR-0013, ADR-0015 and ADR-0016): changes requested. Findings and resolutions:
  1. Collapsed live indicator could lose U4 information; closing key panels while live was unguarded. Fixed: the indicator keeps state, duration and outputs in text; undo toast when closing program, live controls or mixer while live; menus live in the window frame.
  2. `workspace.switch` was not in the control API and layout is client state. Fixed: client-scoped `ui.*` namespace, routed by client id, not in the engine command log.
  3. Vendor display string, no units, no schema, unclear ownership. Fixed: EDID fingerprint with connector, size and scale; logical rect; matching order; JSON Schema; export strips hardware ids; UI crate owns the format; `locked` defined.
  4. Missing cases. Fixed: studio mode reopens preview, per-canvas panel ids, projectors in workspaces, separate layout undo, keyboard-only docking, F6 focus cycling and accessible names, delete confirmation.
  5. Terms could be confused. Fixed: glossary separates workspace, scene collection and output profile; floating windows follow U11 sizes.
  6. Ctrl+Alt+digit clashes with AltGr. Fixed: no default workspace shortcuts.
  7. Missing tests. Fixed: zero redraws and polls for hidden panels; automatic switching sends nothing to sources or outputs.
- 2026-11-02, re-review by the same independent session: approve, findings 1-7 resolved. Minor follow-ups applied (flow table wording, toast on automatic switch).
- Owner approval: Salih, 2026-11-02 (project thread; the owner requested this feature).
