# UI principles

These are binding rules for every user interface of RBS: the desktop app, web panels and plugin-provided UI. They are decided in [ADR-0015](../adr/0015-ui-design-rules.md). A UI change that breaks a rule is not merged unless a new ADR changes the rule.

Rules use **must** and **must not**. Each rule has an ID (U1, U2, ...) so reviews and issues can cite it.

**Interaction** means one click, tap, key press or drag. Typing a name or value counts as one interaction.

## Who we design for

| Persona | Needs |
| --- | --- |
| First-time streamer | Go live with a good-looking setup in minutes, without reading docs |
| Regular creator | Fast scene and source work, reliable live control, no surprises |
| Producer / power user | Dense information, keyboard control, multiple canvases and outputs, audio routing |

The default experience is designed for the first two. Power features are one step away, never in the way (U2).

## U1. Core tasks are fast and obvious

Each core task below must be **started** from the main window in at most two interactions, without opening a menu bar menu or a settings dialog. The full flow, with its maximum interaction count, is defined in the [design system spec](../specs/ui-design-system.md#core-flows-contract-for-u1).

| Core task | Target for a first-time user (usability test) |
| --- | --- |
| Create a scene | under 10 s |
| Add a source (screen, window, camera, image, text, media, browser, audio) | under 20 s |
| Move, resize and crop a source | under 15 s, directly on the preview |
| Add a filter to a source and adjust it | under 30 s |
| Pick and apply a transition between scenes | under 15 s |
| Mute or adjust an audio source | under 5 s |
| Go live and stop | under 5 s each, after first setup |
| Start and stop recording | under 5 s each |

Before a minor release that changes one of these flows, at least five people from the first persona (remote, unmoderated tests are fine) perform it. The flow passes when the median time is within the target and at least 80 % complete it unaided. A failing flow blocks the release unless the owner waives it with a written reason in the release notes.

## U2. Progressive disclosure, three levels

1. **Simple** (default): the controls needed for the core tasks, with sensible defaults chosen from probed capabilities.
2. **Advanced**: shown with an explicit "Advanced" toggle or section in the same place. It does not move the user to another screen.
3. **Expert**: raw settings, the media graph view, per-node metrics, adapter selection. Reached from a single "Expert" entry point.

Rules:
- Every setting declares its level (`simple`, `advanced`, `expert`) in its settings schema. The level is chosen in review using the personas above, and the review checks the declared level, not the rendered screen.
- A setting that the second persona changes regularly must not be `expert`.
- Every option shows its default and a one-line explanation on hover or focus.

## U3. Direct manipulation first

- Sources are positioned, resized, rotated and cropped directly on the preview with handles, snapping guides and keyboard nudging. Numeric fields exist as well, but are never the only way.
- Drag and drop works everywhere it is meaningful: files and windows onto the preview to add sources, sources between scenes, filters to reorder.
- Every list item has a context menu with the same actions as its toolbar.

## U4. Safe when live

- The live state (streaming, recording, which outputs) must be visible at all times, in the same place, and must not rely on colour alone.
- Studio mode (separate preview and program) is available in one interaction. While live, edits to the program scene are marked clearly.
- Every edit to scenes, sources, filters, transitions, audio and settings is undoable through the command log.
- Actions that cannot be undone are listed in the design system spec (for example: end stream, stop recording, delete a scene collection). They need a confirmation while live.
- The UI must never stop or restart outputs as a side effect of a settings change without saying so first.

## U5. The UI never blocks and never stutters

- No operation runs on the UI thread for more than 8 ms. Long work runs in the engine or in the background with progress shown.
- The UI renders at the display refresh rate during interaction and animation. Input-to-visual-feedback latency must be under 50 ms.
- When nothing changes on screen, the UI does not redraw. Meters animate only while visible and while their audio is active; the preview redraws at the canvas frame rate only while visible.
- Panels that are hidden or collapsed do no rendering and no polling. Hidden plugin web views go to Standby (P13, ADR-0013).
- The desktop preview and program use the engine's shared GPU surface, not frames copied over IPC (ADR-0008). Remote web panels receive a reduced-rate encoded preview instead.

## U6. One design system, no exceptions

- All colours, spacing, radii, typography, elevation and motion come from design tokens ([ui design system spec](../specs/ui-design-system.md)). Hard-coded values are not allowed in UI code.
- All UI is built from the shared component library. A new component is added to the library, documented and reviewed before it is used.
- One icon set and one font family (with tabular figures for numbers). One accent colour per theme.

## U7. Accessible by default

- Contrast meets WCAG 2.2 AA (4.5:1 for text, 3:1 for UI elements and graphics).
- Every action is reachable by keyboard; focus is always visible; a complete, rebindable shortcut map exists.
- Every control has an accessible name for screen readers.
- Text scales to 200 % without loss of function.
- Status, meters and alerts never rely on colour alone (shape, icon or text as well).
- "Reduce motion" setting is respected.

## U8. Global by default

- All user-facing strings go through the i18n system (Project Fluent). No concatenated sentences, no text in images.
- Layouts work with 40 % longer strings and with right-to-left languages.
- Dates, numbers and units follow the user's locale.

## U9. Clear status and helpful errors

- A status bar always shows: live and recording state with duration, bitrate, dropped frames with their cause, CPU and GPU load.
- Problems are described in plain language with what happened, why, and a button that fixes it or goes to the right setting. Error codes are secondary detail.
- Warnings appear before trouble when possible (for example, "your upload speed is lower than the bitrate").

## U10. Guide, do not lecture

- First run offers an auto-configuration wizard that uses capability probing to pick encoder, resolution and bitrate, and can be skipped.
- First run also offers importing an OBS scene collection and settings.
- Every empty state (no scenes, no sources, no filters) shows the next action as a button, not just text.
- Scene templates (just chatting, gameplay with camera, podcast, vertical) are offered when creating a scene.

## U11. Layout

- Default layout: scenes and sources on the left, preview (and program in studio mode) in the centre, audio mixer and controls at the bottom, properties of the selected item on the right.
- Panels are dockable; users can save and switch workspaces. "Reset layout" is always available.
- The main window must work from 1280 x 720 upwards. Below that, panels collapse into tabs; nothing is cut off.
- Interactive targets are at least 24 x 24 px at 100 % scale with spacing that meets WCAG 2.5.8; 28 px is the default and 24 px is only used in dense Advanced and Expert views.

## U12. Visual style

- Modern, calm and dense where needed: neutral surfaces, one accent colour, clear hierarchy through type size and weight rather than borders and boxes.
- Dark and light themes are both first-class and follow the OS setting by default. High-contrast theme is provided.
- Colour meanings are reserved. `color.state.live` (red) is used only for live and recording. Errors, clipping and destructive actions use their own tokens with a hue clearly distinct from live, and always with an icon or text. Amber is for warnings, green for healthy, and the accent for selection and primary actions.
- Users may pick the accent from a curated set and may install custom themes. A theme is a token file that must pass the contrast check and cannot change `color.state.live`.
- Motion is short (120-200 ms), purposeful (shows where something came from or went), and never delays an action.
- No skeuomorphism, gradients, heavy shadows or decorative animation.

## U13. Words are part of the design

- User-facing terms come from the [glossary](glossary.md) and are used consistently.
- Labels are short, sentence case, and describe the result ("Add camera"), not the mechanism.

## U14. Plugins look native

- Plugin settings are generated from their schema with library components (ADR-0008); plugins do not draw their own settings UI.
- Plugin panels that use a web view receive the design tokens as CSS variables and the component styles, and must use them. Panels that break U6, U7 or U12 are flagged in the plugin registry.
- Plugin dock panels run isolated from the main UI in the sandbox or process tier. Native-tier plugins may not provide panels directly; they declare settings schemas, or ship a separate sandbox or process panel. A crashing or hung panel is replaced by an error card with a reload button; the rest of the UI keeps working (P7).
- Plugins cannot place UI in the status bar, title bar or live controls.
- Plugin registry submissions run an automated lint on panel styles; panels with hard-coded colours or failing contrast are marked non-compliant.

## U15. Multiple canvases are easy

- The canvas being edited is always shown and can be switched in one interaction.
- Scenes belong to one canvas or are shared; the scene list shows which.
- Horizontal and vertical canvases can be previewed side by side. Sources can be placed in several canvases at once with independent layout.

## U16. Mixer

- Meters show dBFS on a labelled scale, with peak hold and a clip indicator (not colour alone).
- Each strip offers mute, volume, monitoring and filters at the Simple level; tracks, routing to buses and per-output mixes are Advanced.
- Volume is adjustable by drag, scroll, keyboard and typed value.

## U17. Settings

- One settings window with a search field and a flat list of sections. No nested dialogs; a setting is never more than one section away.
- Changes apply immediately where safe, and are undoable. Settings that need an output restart say so and offer to apply when not live.

## U18. Notifications

- Three levels only: status bar indicator (ongoing state), toast (one-off information, auto-dismissed, keyboard reachable), modal dialog (decision needed now).
- While live, modal dialogs are not opened automatically; problems go to the status bar and toasts.

## U19. Projectors and multiple monitors

- Any canvas, scene, source or the multiview can be opened as a projector on any display, in one interaction from its context menu.
- Projectors have no chrome and do not steal focus.

## U20. Control surfaces

- Every UI command is a control API command (ADR-0008), so Stream Deck, MIDI, touch panels and hotkeys can trigger anything the UI can.
- A touch-friendly layout of live controls (scenes, transition, go live, record, mute) is available for tablets and secondary screens.

## How rules are checked

| Rule | Check |
| --- | --- |
| U6 | Lint for hard-coded colours, sizes and fonts in UI code (planned); review |
| U5 | UI frame-time and UI-thread-blocking metrics in debug builds; CI scenario |
| U7 | Automated contrast check on tokens; keyboard-only and screen reader walkthrough per release |
| U1, U2, U10 | Usability tests with target times and success rate |
| U2 | Declared setting levels reviewed against the personas |
| U14 | Plugin registry style lint |
| U15-U19 | `ui-review` checklist; usability tests for canvas switching and the mixer |
| U18 | Automated test: no modal dialog opens automatically while live |
| U20 | Test that every UI command maps to a control API command |
| All | `ui-review` skill checklist on every UI pull request, with light and dark screenshots |
