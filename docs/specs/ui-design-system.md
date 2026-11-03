# Spec: UI design system

- Status: Draft
- Owner crate(s): `rbs-ui` (toolkit-neutral model: panels, layout, workspaces, layout undo, tokens, live indicator), `rbs-app` (Slint desktop shell, planned)
- Related ADRs: [0008](../adr/0008-ui-as-api-client.md), [0013](../adr/0013-zero-cost-when-unused.md), [0015](../adr/0015-ui-design-rules.md), [0016](../adr/0016-native-desktop-ui.md), [0017](../adr/0017-flexible-panels-and-workspaces.md), [0022](../adr/0022-native-ui-toolkit.md)
- Rules: [UI principles](../ui/principles.md)

## Purpose

The single source of visual and interaction decisions for every RBS interface.

Binding now (once ADR-0015 is accepted): token names, the component list and contracts, the main window structure, the core flow interaction limits, and the list of non-undoable actions. Tunable: token values, which are initial and will be adjusted in Phase 2 usability testing.

## Tokens

Tokens are defined once in a data file ([`crates/rbs-ui/tokens.toml`](../../crates/rbs-ui/tokens.toml)) and generated into the native toolkit's format (ADR-0016), and exported as CSS variables only for remote clients outside the desktop app. UI code references tokens by name only (rule U6).

File layout (format 1): `[fixed]` holds the colours no theme may change (`color.state.live`, `color.text.on-live`); `[themes.dark]`, `[themes.light]` and `[themes.high-contrast]` each define exactly the theme colour names below, as opaque `#rrggbb`; `[length]` (logical pixels: the spacing, control height and radius tokens below), `[duration]` (milliseconds: `motion.fast`, `motion.normal`) and `[type]` (`{ size, line, weight }` for the five text styles) hold the rest. Token names are lowercase words joined by `.` or `-`. A custom theme is a complete token file; its `[fixed]` values must equal the built-in ones (`#c4262e` and `#ffffff`), otherwise it is refused. Elevation and easing tokens are added to the file together with the components that use them. `rbs_ui::tokens` parses the file and fails on a missing, unknown or invalid token, a changed fixed colour, and any failed contrast rule:

- text: `color.text.primary` and `.secondary` on every surface, `color.text.inverse` on `color.accent`, and `color.text.primary` on `color.accent.subtle`: at least 4.5:1 (7:1 in `high-contrast`); `color.text.on-live` on `color.state.live`: at least 4.5:1 in every theme, because both are fixed;
- non-text: `color.border.focus` on `surface.base` and `.raised`, `color.border.strong`, `color.state.*` and `color.action.destructive` on `surface.raised`, `color.meter.*` on `surface.sunken`, and `color.state.live` on `surface.base`: at least 3:1;
- `color.state.error`, `color.action.destructive` and `color.meter.clip` differ in hue from `color.state.live` by at least 45 degrees (rule U12).

### Colour (semantic names, per theme)

| Token | Use |
| --- | --- |
| `color.surface.base` / `.raised` / `.overlay` / `.sunken` | Window background, panels, popovers, wells |
| `color.text.primary` / `.secondary` / `.disabled` / `.inverse` | Text |
| `color.border.subtle` / `.strong` / `.focus` | Dividers, inputs, focus ring |
| `color.accent` / `.accent.hover` / `.accent.subtle` | Primary actions, selection |
| `color.state.live` / `color.text.on-live` | Live and recording only (rule U12), and the text on it; fixed, themes cannot change them |
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

- A panel id is `<namespace>.<panel>` in lowercase ASCII letters, digits, `.`, `-` and `_`, at most 128 bytes, with optional parameters `?key=value&key=value` in the same characters. Each key appears once and parameters are stored sorted by key, so one panel has exactly one id.
- Every panel has a stable id (`rbs.scenes`, `rbs.sources`, `rbs.preview`, `rbs.program`, `rbs.properties`, `rbs.mixer`, `rbs.live-controls`, `rbs.transition`, `rbs.outputs`, `rbs.stats`, `rbs.status-bar`, plugin panels as `<plugin-id>.<panel>`), a title, an icon, a default dock position and an optional shortcut.
- Panels that can exist more than once take parameters in the id: `rbs.preview?canvas=vertical`, `rbs.program?canvas=horizontal`. Without a parameter they show the canvas being edited.
- A layout is a tree per window: splits (horizontal or vertical, with ratios) whose leaves are tab stacks of panels. Floating windows and projectors have their own tree or content plus their position.
- Each built-in panel declares how often it must redraw while visible: on change only (lists, properties, controls), at most N times per second (`rbs.mixer` 30 Hz, `rbs.stats` and `rbs.status-bar` 1 Hz), or at display rate (`rbs.preview`, `rbs.program`). The shell runs at most one redraw timer, at the fastest rate any visible panel needs, plus 1 Hz for the live indicator while live or recording, and no timer otherwise. Hidden tabs and closed panels never count.
- `rbs.program`, `rbs.live-controls` and `rbs.mixer` are guarded: closing them while live (directly or by closing their floating window) shows the undo toast of U21.
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
- The format is versioned; RBS reads older versions and migrates them. A file with a newer `format` is refused with a message, never rewritten.
- Validation: unknown keys are errors; there is exactly one `main` window and it is first; a panel id appears at most once in the whole workspace; a split has at least two children, one positive ratio per child, ratios summing to 1 (within 0.01, then normalised) and at most 16 levels of nesting; a tab stack may carry `active` (index of the visible tab, default 0); only the main window's root may be an empty stack (`{ tabs = [] }`, every panel closed); a floating window holds at least one panel; a projector has `content` and no `layout`; names have 1 to 64 characters, no control characters, and are unique ignoring case (Unicode lowercase, not locale-specific).
- Files are named after a slug of the workspace name and written atomically (temporary file, then rename). Files that fail to parse are listed as problems and left untouched. Built-in workspaces need no file; saving a changed built-in writes an override file, and restoring its default removes it.

### Built-in workspaces

| Name | Main window |
| --- | --- |
| Edit | Scenes over Sources on the left, Preview in the centre, Properties on the right; below: Mixer (tabs with Stats and Outputs), Transition, Live controls |
| Live | Preview and Program side by side over the Mixer; on the right Live controls (tab with Transition) over Stats (tab with Scenes) |
| Minimal | Program over Live controls |
| Audio | Program and Live controls over a large Mixer |

### Switching

- Client-scoped control API commands `ui.workspace.list` and `ui.workspace.switch(name)` ([control API](control-api.md#client-scoped-ui-commands)), available in the command palette and the workspace menu. Shortcuts are user-assigned; none is bound by default (U21).
- Optional `on_go_live` and `on_stream_end` workspace settings, stored in the UI settings, not in the scene collection or output profile.
- Switching only changes UI state; it never reconfigures sources or outputs.
- Layout changes and switches go into the UI's own layout undo history, not the engine command log. A history step is a whole-workspace snapshot; the moves of one splitter drag form one step; changes that leave the layout as it was record nothing; at least 100 steps are kept.
- Every layout change is a command (open, close, show tab, move to stack, move to edge, float, dock, close window, resize, lock, switch). Pointer and keyboard both issue these commands. When `locked` is set, close, move to stack, move to edge, float, dock, close window and resize are refused if they come from the pointer; open, show tab, lock or unlock and switch are always allowed, and every command from the palette, a shortcut or the control API applies.
- The result of every command is validated against the rules above; a command whose result would break one (for example splits nested deeper than 16 levels) is refused, so a workspace can always be saved and loaded again.
- A switch that leaves a guarded panel closed while live shows the same undo toast as closing it. An automatic switch (on go live or stream end) shows a toast naming the previous workspace with a one-click way back.
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

The native toolkit chosen under ADR-0016 (Slint, iced or Qt through `cxx-qt`; ADR-0022 proposes Slint) must satisfy:

1. Renders the engine's shared GPU surface for preview and program without CPU copies.
2. Supports the tokens and components above, including custom drawing for meters and the preview canvas at display refresh rate.
3. Accessibility APIs on Windows, macOS and Linux (screen readers, keyboard focus).
4. Fluent-based i18n and right-to-left layout.
5. Idle CPU near zero when nothing changes on screen.
6. A docking system with tabs, splits and floating windows across monitors (U21), or the ability to build one.
7. A licence compatible with ADR-0010.
8. Rich visualisations (meters, equalizer curves, spectrum, histogram, scopes): light graphics with the toolkit's vector primitives; dense or per-pixel graphics drawn on the GPU and shown as textures. Provisional until ADR-0022 is accepted.

The comparison prototype and its measurements are in [`prototypes/ui-toolkit`](../../prototypes/ui-toolkit/README.md); the choice is recorded in [ADR-0022](../adr/0022-native-ui-toolkit.md).

## Open questions

- Final font choice and licence.
- JSON Schema for the workspace file (promised by ADR-0017): not delivered yet; the Rust parser in `rbs-ui` is the reference until it is.
- Expressiveness of the declarative plugin panel format (forms, lists, charts, live previews).
