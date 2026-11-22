# ADR-0173: A companion sound is shown under its picture in the Sources list

- Status: Accepted
- Date: 2026-11-22
- Deciders: Salih (owner)
- Spec: [ui-design-system](../specs/ui-design-system.md) (Sources panel)
- Builds on: [ADR-0096](0096-capture-audio-companions-and-camera-microphone.md) (companions; its decisions are unchanged, it never said how the Sources list shows one), [ADR-0065](0065-global-sources-and-safe-source-removal.md) (Global group), [ADR-0114](0114-screen-grammar.md) (ListRow), [ADR-0142](0142-companion-sound-leaves-the-scene-with-its-picture.md) (a companion leaves a scene with its picture)

## Context

"Capture audio too" (ADR-0096) adds the picture's sound as an ordinary audio source with the hint `companion_of`. The Sources list shows every item of the scene as one row, so a window captured with its sound shows as two unrelated rows ("YouTube" and "YouTube sound"). The owner tested it on the Mac (2026-11-22): "they must show as one in the lists". The owner chose on 2026-11-22: one row for the picture, the sound row opening under it; the Mixer keeps its own strip for the sound. The owner then asked for an arrow at the start of the row that opens a part under it, closed by default.

## Decision

1. **Nested, not merged.** The companion stays an ordinary audio source with its own item, fader, filters, sync offset and Mixer strip (ADR-0096 unchanged). Only the Sources list changes: a companion whose picture has a row in the same group of the list (Global, or This scene) is drawn directly under the picture's row, indented by the new token `size.list.indent` (24, the icon and its gap, so its icon sits under the picture's name), and is not a row of its own elsewhere. A companion whose picture is not in that group (placed in another scene on its own, or its picture global while it is not) keeps a top-level row, as today. The list has one row per source, so a picture or companion placed twice in a group is still one row. A picture with several companions nests all of them, in item order. A window sound shared by several windows of one application (ADR-0096 decision 3) names one picture in `companion_of`: it nests under that window only, and stays top-level when only a sibling window is in the group (a known limit).
2. **A disclosure arrow at the row's start.** `ListRow` gains an optional disclosure: a `chevron-right` (closed) or `chevron-down` (open) before the icon, `accessible-expandable` and `accessible-expanded` on the row (as the existing disclosures in `main_window.slint`), with Right and Left arrow opening and closing it and a click on the arrow toggling it (a click elsewhere still selects). Only a row that has something under it shows the arrow; today that is a picture with a companion in its group. While any row of a group has an arrow, every row of that group keeps the arrow's width (`size.icon.box`), so the icons stay in one column. Show/Hide and Lock stay on the row (the most used actions while live, one click as in OBS), and the other actions stay in the row's menu and Properties: the open part holds only what is under the row, never a copy of its actions. While closed, a muted companion shows a `speaker-off` mark on the picture row (U7: the accessible name says ", sound muted"; an open-able row's name says "has sound"). The owner suggested the arrow on 2026-11-22.
3. **Closed by default, open in memory.** The sound row is closed until the user opens it, and stays open per picture while the app runs (not saved). A selected companion is always shown (selected from the Mixer, after an undo, or just added), as a selected global source shows in a closed Global group: selecting a companion opens its picture, and in a closed Global group shows the picture row too. Keyboard (tree pattern): Left on a sound row moves the focus to its picture; closing while the sound row has focus moves the focus to the picture row; a closed sound row is not in the list, so moving through the list skips it. Open states of sources that no longer exist are dropped when the rows are made again.
4. **The sound row.** The companion's row with its name, warning and note, and on the right its level as text (the Mixer's readout text, `rbs_ui::fader::volume_text`), a mute `IconButton` (`speaker` / `speaker-off`, the same call as the Mixer strip's mute, so in Own levels it edits the program scene's level as the strip and Properties › Sound do, ADR-0091) and Show/Hide. In studio mode the list shows the preview scene while the Mixer plays the program scene, so the level and mute are hidden there while the listed scene is not the program scene, and so is the closed row's muted mark. Its menu, rename, and Delete or Backspace asking to remove it from the scene work as on any audio row. It cannot be dragged (sound rows never could); dragging the picture moves both, and a drop on an open sound row counts as a drop on its picture. The remove question stays as ADR-0142 left it (the sound is not named); naming it is a separate visual change offered to the owner. Its accessible name adds "sound of <picture>".
5. **Where it lives.** The app orders and marks the rows (`rbs-app`, `studio_ui/sound_nest.rs`) from `SourceRow::companion_of`, which the host already sends. No host, scene, control API or format change; `source.list` and `scene.sources` keep one row per item. A plugin source type with a companion gets the same nesting, because it follows the hint, not the kind.
6. **Spec and library.** ui-design-system.md gets the `ListRow` disclosure and a Sources panel paragraph in the same change, `size.list.indent` is added to `tokens.toml`, and the `ui-review` checklist applies. The arrow column and the Global header's chevron are shown in the dark and light screenshots the owner approves (rule 16).
7. **Tests.** Unit tests of the pure ordering in `sound_nest.rs`: a companion moves under its picture; several companions keep item order; a companion of a global picture in This scene stays top-level; a companion whose picture is only in a nested scene stays top-level; open states of deleted sources are dropped; a selected companion opens its picture.
8. **Zero cost.** Ordering runs only when the rows are made again, over the rows already in hand; nothing runs per frame.

## Consequences

- The Sources list shows one entry per captured picture, which reads as OBS users expect, while the sound keeps every audio control.
- The Mixer still shows the companion as its own strip; a later change may group them there too.
- The order of rows no longer equals the item order for companions; layer moves were never offered on sound rows, so nothing the user can do depends on it.

## Alternatives considered

- **One source with picture and sound (ADR-0106 two-node source).** Merges the rows for real, but changes the scene format and loses the companion's own filters, sync offset and the "several windows of one application share one sound" rule of ADR-0096.
- **Hide the companion from the Sources list.** Reached only from the Mixer and Properties; the user loses rename, show/hide and remove in the list.
- **Always open.** Two rows again for every captured window; the owner asked for one.
- **An arrow on every row, opening its actions (delete, show/hide, rename) and the sound.** Offered by the owner. Show/Hide would take two clicks while live, and the actions already live in the row's menu and Properties; the list would grow without new abilities. The arrow appears where there is something under the row, and can carry more later (filters, for example).
- **A sound button among the row's icons instead of the arrow.** First proposal; the arrow at the start is the usual tree pattern and leaves the trailing icons as they are.

## Review

Independent review per ADR-0014, 2026-11-22, fresh AI session: changes requested, 10 findings, all taken into the text: mute in studio mode (hidden while the listed scene is not the program), duplicates and several companions, the shared window sound limit, a selected companion opens its picture, keyboard focus, `accessible-expandable`, drop on a sound row, the remove question (kept out, offered separately), spec and an own indent token, the header (Builds on, not Supersedes) and the tests.

Owner: chose the arrow only on rows with something under them (decision card, 2026-11-22). Owner approval of the ADR and the dark and light screenshots: Salih, 2026-11-22 ("onaylıyorum").

As built: `ListRow` disclosure (`components.slint`), `studio_ui/sound_nest.rs` (ordering, marks, open state, six unit tests), `source_list_calls.rs` (`open-sound`), the Sources panel rows (`studio.slint`), `size.list.indent`; screenshots from `tests/sound_nest_shots.rs`.
