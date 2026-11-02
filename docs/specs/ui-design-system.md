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

## Core flows (contract for U1)

Interactions are counted as defined in the UI principles. "Max" is the limit for the whole flow with default choices.

| Flow | Steps | Max |
| --- | --- | --- |
| Add source | "+" on the Sources panel (or drop a file or window onto the preview: 1 drag, done) -> pick a type from large icons with search, recent first -> confirm in the picker, which shows a live preview -> the source is selected on the preview with handles | 3 |
| Add and adjust a filter | Select the source on the preview or list -> "+" in the Filters section of Properties -> pick from a searchable list with previews -> adjust in place; before/after toggle | 3 + adjustments |
| Choose a transition | Transition control next to Go live shows the current one -> pick another (duration inline) | 2 |
| Create a scene | "+" on Scenes -> type a name (optional template) and confirm | 3 |
| Go live | Go live button (first time only: pick service, sign in or paste key, automatic checks for bandwidth and encoder) | 1 after setup |

## Actions that cannot be undone

End stream, stop recording, delete a scene collection, delete a recording file, sign out of a service. "Remove from scene" (undoable) and "Delete source" (removes it from every scene; undoable) are distinct commands. Everything else that edits scenes, sources, filters, transitions, audio or settings is undoable.

## Keyboard

Every command is in the command palette (Ctrl/Cmd+K) with its shortcut. Defaults include: switch to scene 1-9 (Ctrl/Cmd+1..9), transition preview to program (Ctrl/Cmd+Enter; Space also works while the preview or program has focus), undo/redo, nudge source (arrows, Shift for 10 px), duplicate, delete, toggle studio mode. All shortcuts are rebindable; global hotkeys are opt-in.

## Requirements on the UI toolkit

The native toolkit chosen under ADR-0016 (Slint, iced or Qt through `cxx-qt`) must satisfy:

1. Renders the engine's shared GPU surface for preview and program without CPU copies.
2. Supports the tokens and components above, including custom drawing for meters and the preview canvas at display refresh rate.
3. Accessibility APIs on Windows, macOS and Linux (screen readers, keyboard focus).
4. Fluent-based i18n and right-to-left layout.
5. Idle CPU near zero when nothing changes on screen.
6. A licence compatible with ADR-0010.

## Open questions

- Final font choice and licence.
- Expressiveness of the declarative plugin panel format (forms, lists, charts, live previews).
