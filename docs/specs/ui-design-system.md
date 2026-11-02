# Spec: UI design system

- Status: Draft
- Owner crate(s): `rbs-ui-tokens`, `rbs-app` (planned)
- Related ADRs: [0008](../adr/0008-ui-as-api-client.md), [0013](../adr/0013-zero-cost-when-unused.md), [0015](../adr/0015-ui-design-rules.md), [0016](../adr/0016-native-desktop-ui.md)
- Rules: [UI principles](../ui/principles.md)

## Purpose

The single source of visual and interaction decisions for every RBS interface.

Binding now (once ADR-0015 is accepted): token names, the component list and contracts, the main window structure, the core flow interaction limits, and the list of non-undoable actions. Tunable: token values, which are initial and will be adjusted in Phase 2 usability testing.

## Tokens

Tokens are defined once in a data file (`ui/tokens.toml`, planned) and generated into the native toolkit's format (ADR-0016), and exported as CSS variables only for remote clients outside the desktop app. UI code references tokens by name only (rule U6).

### Colour (semantic names, per theme)

| Token | Use |
| --- | --- |
| `color.surface.base` / `.raised` / `.overlay` / `.sunken` | Window background, panels, popovers, wells |
| `color.text.primary` / `.secondary` / `.disabled` / `.inverse` | Text |
| `color.border.subtle` / `.strong` / `.focus` | Dividers, inputs, focus ring |
| `color.accent` / `.accent.hover` / `.accent.subtle` | Primary actions, selection |
| `color.state.live` | Live and recording only (rule U12); themes cannot change it |
| `color.state.warning` / `.healthy` | Status |
| `color.state.error` / `color.action.destructive` | Errors and destructive actions; hue clearly distinct from `color.state.live`, always with icon or text |
| `color.meter.low` / `.mid` / `.high` / `.clip` | Audio meters (paired with markers, never colour alone) |

Themes: `dark` (default when the OS is dark), `light`, `high-contrast`. Every pair of text and surface tokens must pass the U7 contrast check; CI verifies it from the token file.

### Typography

One family (initial choice: Inter, with the OS UI font as fallback). Numbers in meters and stats use its tabular figures.

| Token | Size / line height (px at 100 %) | Weight |
| --- | --- | --- |
| `type.caption` | 12 / 16 | 400 |
| `type.body` | 13 / 20 | 400 |
| `type.label` | 13 / 20 | 500 |
| `type.title` | 15 / 22 | 600 |
| `type.heading` | 20 / 28 | 600 |

### Space, size, shape, motion

| Group | Tokens |
| --- | --- |
| Spacing (4 px grid) | `space.1`=4, `space.2`=8, `space.3`=12, `space.4`=16, `space.6`=24, `space.8`=32 |
| Control height | `size.control.sm`=24 (dense Advanced/Expert views only), `.md`=28 (default), `.lg`=32 |
| Radius | `radius.sm`=4, `radius.md`=6, `radius.lg`=10 |
| Elevation | `elevation.0` (flat), `elevation.1` (popovers), `elevation.2` (dialogs); subtle, no heavy shadows |
| Motion | `motion.fast`=120 ms, `motion.normal`=180 ms, `motion.easing.standard`; all motion disabled under "reduce motion" |

## Components

The library is the only way to build UI (rule U6). Each component has: states (default, hover, pressed, focused, disabled, error), keyboard behaviour, accessible name and role, light/dark/high-contrast screenshots, and usage notes.

Initial set: Button (primary, secondary, ghost, danger), IconButton, Toggle, Checkbox, Radio, Select, Combobox, Slider (with numeric entry), NumberField (drag to change), TextField, ColorPicker, Tabs, SegmentedControl, List and TreeList (drag reorder, inline rename), ContextMenu, Popover, Dialog, Toast, Tooltip, PropertyGrid (generated from a settings schema), EmptyState, StatusBadge (live, recording, warning), AudioMeter, AudioStrip (meter, fader, mute, monitor, filters), PreviewCanvas (handles, snapping, guides), SceneCard (thumbnail, name, live badge), TransitionPicker (preview thumbnail), CommandPalette, KeyboardShortcutHint.

## Main window structure

```
+--------------------------------------------------------------------------+
| Title bar: scene collection, workspace, command palette, settings        |
+-----------+------------------------------------------+-------------------+
| Scenes    |  Preview  (| Program in studio mode)     | Properties of the |
| (cards)   |                                          | selected scene,   |
|-----------|                                          | source, filter or |
| Sources   |                                          | output            |
| in scene  |                                          |                   |
+-----------+------------------------------------------+-------------------+
| Mixer (audio strips)                     | Transition | Go live | Record  |
+--------------------------------------------------------------------------+
| Status bar: live/rec state + time, bitrate, dropped frames, CPU, GPU     |
+--------------------------------------------------------------------------+
```

## Panels and workspaces (contract for U21)

Workspaces are client-side UI state. The desktop UI crate owns the panel model and the workspace format; the engine and `rbs-core` know nothing about them (P1).

### Panel model

- Every panel has a stable id (`rbs.scenes`, `rbs.sources`, `rbs.preview`, `rbs.program`, `rbs.properties`, `rbs.mixer`, `rbs.live-controls`, `rbs.transition`, `rbs.outputs`, `rbs.stats`, `rbs.status-bar`, plugin panels as `<plugin-id>.<panel>`), a title, an icon, a default dock position and an optional shortcut.
- Panels that can exist more than once take parameters in the id: `rbs.preview?canvas=vertical`, `rbs.program?canvas=horizontal`. Without a parameter they show the canvas being edited.
- A layout is a tree per window: splits (horizontal or vertical, with ratios) whose leaves are tab stacks of panels. Floating windows and projectors have their own tree or content plus their position.
- The live indicator is not a panel and cannot be removed (U4, U21). The workspace menu and Panels menu are part of the window frame, not panels.

### Workspace file

Workspaces are stored as versioned TOML in the user's config directory, one file per workspace, so they can be shared and kept in version control. A JSON Schema for the format is published with the UI crate and checked in CI.

```toml
format = 1
name = "Minimal live"
locked = true

[[windows]]
kind = "main"
layout = { split = "vertical", ratios = [0.78, 0.22], children = [
  { tabs = ["rbs.program"] },
  { tabs = ["rbs.live-controls", "rbs.mixer"] },
] }

[[windows]]
kind = "floating"
layout = { tabs = ["rbs.stats"] }
placement = { display = "edid:DEL-41A3-7H2K9", connector = "DP-2", size = [2560, 1440], scale = 1.5, rect = [0, 0, 1280, 720] }

[[windows]]
kind = "projector"
content = "program?canvas=vertical"
placement = { display = "edid:SAM-7012-HX55", connector = "HDMI-1", size = [1920, 1080], scale = 1.0, fullscreen = true }
```

- `placement.display` is a stable fingerprint derived from the display's EDID (manufacturer, product code, serial). `connector`, `size` (physical pixels) and `scale` help when the fingerprint is ambiguous. `rect` is in logical pixels relative to that display.
- Matching order when a workspace is applied: (1) same fingerprint, (2) same connector with the same size, (3) any display with the same size and scale, (4) no match: the window docks into the main window and a projector is not opened. A rect that would fall outside the matched display is clamped to it.
- Export removes `placement` display details (fingerprint, connector) and keeps only size, scale and relative rect, so exported workspaces are portable and contain no hardware identifiers. Import then matches by step (3) or docks.
- `locked` blocks pointer moves, resizes and close buttons. Commands from the palette, shortcuts and the control API still apply.
- Unknown panel ids (for example from an uninstalled plugin) are kept in the file and shown as a placeholder that offers to install or remove them.
- The format is versioned; RBS reads older versions and migrates them.

### Switching

- Client-scoped control API commands `ui.workspace.list` and `ui.workspace.switch(name)` ([control API](control-api.md#client-scoped-ui-commands)), available in the command palette and the workspace menu. Shortcuts are user-assigned; none is bound by default (U21).
- Optional `on_go_live` and `on_stream_end` workspace settings, stored in the UI settings, not in the scene collection or output profile.
- Switching only changes UI state; it never reconfigures sources or outputs.
- Layout changes and switches go into the UI's own layout undo history, not the engine command log.
- Deleting a workspace asks for confirmation; deleting a built-in workspace is not possible, only restoring its default.

## Core flows (contract for U1)

Interactions are counted as defined in the UI principles. "Max" is the limit for the whole flow with default choices.

| Flow | Steps | Max |
| --- | --- | --- |
| Add source | "+" on the Sources panel (or drop a file or window onto the preview: 1 drag, done) -> pick a type from large icons with search, recent first -> confirm in the picker, which shows a live preview -> the source is selected on the preview with handles | 3 |
| Add and adjust a filter | Select the source on the preview or list -> "+" in the Filters section of Properties -> pick from a searchable list with previews -> adjust in place; before/after toggle | 3 + adjustments |
| Choose a transition | Transition control next to Go live shows the current one -> pick another (duration inline) | 2 |
| Create a scene | "+" on Scenes -> type a name (optional template) and confirm | 3 |
| Switch workspace | Workspace menu in the window frame, or a user-assigned shortcut | 2 (1 with a shortcut) |
| Go live | Go live button (first time only: pick service, sign in or paste key, automatic checks for bandwidth and encoder) | 1 after setup |

## Actions that cannot be undone

End stream, stop recording, delete a scene collection, delete a recording file, sign out of a service. "Remove from scene" (undoable) and "Delete source" (removes it from every scene; undoable) are distinct commands. Everything else that edits scenes, sources, filters, transitions, audio or settings is undoable.

## Keyboard

Every command is in the command palette (Ctrl/Cmd+K) with its shortcut. Defaults include: switch to scene 1-9 (Ctrl/Cmd+1..9), transition preview to program (Ctrl/Cmd+Enter; Space also works while the preview or program has focus), undo/redo, nudge source (arrows, Shift for 10 px), duplicate, delete, toggle studio mode. All shortcuts are rebindable; global hotkeys are opt-in.

## Requirements on the UI toolkit

The native toolkit chosen under ADR-0016 (Slint, iced or Qt through `cxx-qt`; ADR-0019 proposes Slint) must satisfy:

1. Renders the engine's shared GPU surface for preview and program without CPU copies.
2. Supports the tokens and components above, including custom drawing for meters and the preview canvas at display refresh rate.
3. Accessibility APIs on Windows, macOS and Linux (screen readers, keyboard focus).
4. Fluent-based i18n and right-to-left layout.
5. Idle CPU near zero when nothing changes on screen.
6. A docking system with tabs, splits and floating windows across monitors (U21), or the ability to build one.
7. A licence compatible with ADR-0010.
8. Rich visualisations (meters, equalizer curves, spectrum, histogram, scopes): light graphics with the toolkit's vector primitives; dense or per-pixel graphics drawn on the GPU and shown as textures (ADR-0019).

The comparison prototype and its measurements are in [`prototypes/ui-toolkit`](../../prototypes/ui-toolkit/README.md); the choice is recorded in [ADR-0019](../adr/0019-native-ui-toolkit.md).

## Open questions

- Final font choice and licence.
- Expressiveness of the declarative plugin panel format (forms, lists, charts, live previews).
