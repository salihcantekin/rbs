# ADR-0077: Source health, re-picking a source's target, and the Source settings window

- Status: Proposed
- Date: 2026-11-08
- Deciders: Salih (owner)
- Builds on: ADR-0039 (scene source types), ADR-0038 (I/O nodes, `prepare` finds a lost target), ADR-0062 (filters survive a settings change), ADR-0049 (workspace, floating windows), ADR-0057 (themes), ADR-0013 (zero cost when unused)

## Context

The owner added a Window capture of a terminal and closed the terminal. Today the source row then shows red text ("Not running: ..."), nothing says in plain words that the window is gone, and there is no way to point the source at another window: the only option is to delete it and add a new one, which loses its name, layout, filters and place in other scenes. The same holds for every source: a camera that was unplugged, a microphone that changed, a display that was removed.

OBS Studio has a Properties dialog per source (window, camera, microphone, ...), where the target is changed and device-specific options live (a camera's resolution, frame rate and format, its device controls). RBS has the pieces (settings keys `adapter`/`target`/`width`/`height`/`fps`, the binder keeps filters when settings change, `Command::ListTargets`, OS pickers) but no UI or control API that edits an existing source's settings.

Forces: rule 15 and U6 (tokens only), rule 16 (visual change approved before merge), P13 (a source that is not shown is not opened, and nothing may poll for a lost source without need), U20 (every UI action has a control API command), the preview must never disappear when a dialog opens.

## Decision

1. **Source health is a state, not a message.** `SourceStatus` gains `Unavailable { reason, can_retry }` next to `Running`, `Idle` and `Failed`. Reasons are typed: `TargetGone` (window closed, display removed, device unplugged), `PermissionDenied`, `Busy` (device used by another app), `Other(text)`. The binder and `prepare` already report these (permanent `TargetNotFound` and friends, ADR-0038); the app maps them, nothing new runs per frame.
2. **How an unavailable source looks.** In the Sources panel the row is dimmed (`Theme.text-disabled` name) and shows a small warning icon (`Theme.state-warning`, a library icon, 16 px token size) in front of the type icon, with the reason as a tooltip and accessible label, and a one-line second row "Window closed" with a text button "Choose another..." The coloured sentence is removed. Colour is never the only signal (U-rules): the icon shape and the text carry it. In the picture panel the item keeps its frame and handles (dashed outline, token) and shows a centred icon and the reason, drawn by the UI layer, not the compositor, so the compositor gets no new per-frame branch (P6, rule 10). Audio rows show the same icon in place of the level meter. High-contrast uses `state-warning` as defined by the theme.
3. **Re-pick for every source type.** "Source" is the first tab of Properties (before Transform), shown for every source with a device or file: a target list for the type (displays, windows, cameras, input devices, output devices, applications; an image's file; a colour is edited there too) with a Refresh button and the adapter's "Choose..." entries (OS pickers). Choosing a target sets the `adapter` and `target` settings of the same `Source`. Name, id, items in every scene, layout, filters, volume and global flag stay. The binder already treats a settings change as a replaced source with break-before-make (scene-sources spec, step 6), so an exclusive camera works. One re-pick is one undo step (`EditKind::Retarget`).
4. **Suggestions for a lost window or device.** For an unavailable window source, the list opens with "Same application" first: windows of the application that the old target belonged to, found by the application name stored with the target (`target_app`, a new optional settings key written when a window is added or picked; a title is stored too, `target_title`, for the label "was: zsh - Terminal"). Other types list the current targets as usual. The suggestions use the adapter's `enumerate`, on the control thread, only while the Source tab is open or the user presses Refresh: no polling (P13).
5. **No automatic reconnect in this ADR.** OBS and others re-find windows by title in the background. That needs polling or OS notifications and a matching rule that can pick the wrong window. It is left as a later opt-in setting ("Reconnect when the window comes back", off by default), event-driven where an adapter can say so (capture port addition) and otherwise a slow poll that exists only while a source is unavailable and shown. It needs its own decision.
6. **The Source settings window.** A floating window (ADR-0049, one per source, reused if open) opened by double-click on a source row, by "Source settings..." in the row's context menu and by a button on the Source tab. It is never modal and never covers the preview by design: it opens beside the main window's centre of attention, placed by the same window placement rule as the Settings window, so the picture panel stays visible. It has the sections that apply to the source type, in a fixed order, built from a declarative description (`SettingsSchema`: sections of typed fields: choice, number with unit, toggle, text, file, colour) provided by `rbs-sources` per type, so a plugin source type can describe its own fields later (plugin SDK; this ADR defines the schema for built-in types only):
   - **Source**: target, as in decision 3.
   - **Video** (display, window, camera): size and frame rate (`width`, `height`, `fps`), for cameras the list of modes the device offers (`CameraMode`: size, pixel format, rate range), "Draw cursor" for display and window.
   - **Audio** (microphone, system, application): device and the sync offset (already a setting).
   - **Device controls** (cameras: exposure, white balance, focus, zoom ...): not in this ADR; it needs a capture port extension (capabilities per control, P3) and its own ADR. The section is absent, not disabled, until then.
   Edits are held in the window and applied with **Apply** or **OK** as one undo step; **Cancel** drops them. Applying restarts the device when a field needs it, with break-before-make. Fields that never need a restart (cursor, volume, sync offset) stay in Properties and are not duplicated as live-only fields here.
7. **Control API (U20).** Minor version 1.13: `source.set_settings` (a patch of the stored settings keys, validated strictly like an add, same single undo step), `source.get_settings`, and `status` on `source.list` extended with the typed reason. The UI uses the same commands.
8. **Zero cost when unused (P13).** An unavailable source holds no device and no node work. The settings window instantiates nothing until it is opened; closing it frees its model. Listing targets runs on demand.
9. **Phases.** (a) This ADR and the spec updates. (b) `SourceStatus::Unavailable`, the row and picture indicators, API status. (c) The Source tab, re-pick, `source.set_settings`, undo. (d) The Source settings window with Video and Audio sections. (e) Later, each with its own decision: device controls, opt-in auto reconnect.

## Consequences

- A source whose window or device disappeared is recoverable in two clicks without losing layout, filters or scene placement.
- All source types use one path to change their target, tested once in the conformance suite of the host (settings patch, break-before-make, undo).
- `rbs-sources` gains a settings schema per built-in type and `rbs-app` a new floating window; the scene document format is unchanged except for the optional `target_app` and `target_title` keys, which older builds ignore (unknown keys are ignored).
- Specs to update with the code: scene-sources (new keys, schema), control-api (1.13), ui-design-system (state-warning usage, Source settings window component), glossary ("Source settings", "Unavailable").

## Reference tools (CLAUDE.md rule 17)

- **OBS Studio**: a per-source Properties dialog with device-specific options and a "Deactivate" toggle; a closed window shows a black picture with no indicator in the Sources list. It matches windows by `title:class:exe` and can re-find them by title, which fails when the title changes.
- **Where this falls short today (RBS)**: no way to edit a source after adding it; a lost source is only red text.
- **What RBS does better by design**: an explicit unavailable state with a reason and a fix action in the row; "same application" suggestions rather than title matching; one undo step for a re-pick; the same operation through the control API; nothing polls while the source is healthy or hidden.

## Alternatives considered

- **Keep delete and re-add as the way to change a target.** Loses name, layout, filters and placement in other scenes. Rejected.
- **A modal OK/Cancel dialog like OBS.** Hides the picture the user wants to look at while choosing a camera mode. A floating, non-modal window keeps the preview visible.
- **Only a Properties tab, no window.** Enough for the target, too cramped for camera modes and later device controls.
- **Automatic reconnect now.** Needs background polling and a wrong-window risk; deferred to an opt-in (decision 5).
- **Draw the unavailable state in the compositor.** A new per-frame branch for a rare state (rule 10). The UI layer draws it.

## Review

Pending: independent review per ADR-0014 (rule 11). The owner approves the visual proposal first (rule 16).
