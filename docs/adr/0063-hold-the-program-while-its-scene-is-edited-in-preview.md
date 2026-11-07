# ADR-0063: Hold the program while its scene is edited in preview

- Status: Proposed
- Date: 2026-11-07
- Deciders: Salih (owner)
- Builds on: ADR-0061 (studio mode), ADR-0021 (program and preview per canvas), ADR-0020 (graph swaps), ADR-0013 (zero cost when unused)

## Context

The owner tested ADR-0061 on the Mac (2026-11-07). Everything passed except one case: studio mode starts with the program scene in preview (ADR-0061 decision 1), and a click on the scene on air previews it again. Preview and Program then show the same scene, so every edit made in Preview (a move, a hidden item, a new source) is on air at once, and Transition does nothing. Studio mode exists to prepare a picture without touching the one on air, so this case defeats it.

The owner asked that the user never see a copy: no extra scene in the Scenes panel, the saved collection or the control API.

## Decision

1. **The program is held while its scene is also the preview scene.** When a canvas is in studio mode with `preview == program`, the desktop app's host takes a copy of the program scene (its items, with their layouts, visibility, lock and per-canvas overrides) at the moment that state begins: entering studio mode, previewing the scene on air, removing the preview scene (ADR-0061 decision 5), a transition between one scene and itself (decision 3 below), the start of the studio, and an undo or redo that put another scene on air (the copy belongs to the scene it was taken from and is dropped when another scene is on air). Edits made after that change the scene, which Preview shows; the program shows the copy. A restart of the studio (a new canvas size, a recovery) keeps the copy, with its layouts rescaled like the scene's when the canvas size changes; switching scene collections drops it.
2. **The copy lives only in the run.** The host passes the studio a run collection: the user's collection plus the copy, with the canvas's program set to the copy. The user's collection, the saved document, undo, the Scenes panel, the Sources panel, `scene.list`, `scene.changed`, `source.list` and `source.changed` never contain it; they keep naming the real scene as the program. The copy's scene id is `0xF000_0000`, far above any id a collection issues, and each of its items has the id of the item it copies plus `0xF000_0000`, so the binder keeps the program compositor and layer nodes from one hold to the next (ADR-0020). An item whose source or nested scene is deleted while held leaves the copy. If the copy does not fit the run collection all the same (an id or name a loaded file already uses), the host takes a fresh copy of the scene as it is; if that does not fit either, nothing is held and the studio runs the user's collection.
3. **Transition puts the edits on air.** With `preview == program`, `canvas.transition` (and the Transition button and shortcut) drops the copy, so the program shows the scene as it is now, and the host at once holds a new copy of it. It is one studio update and one schedule swap, as a cut between two scenes is. With nothing held, it changes nothing, as before.
4. **What is not held.** Sources are shared between scenes, so a source's own settings (its capture target, filters, volume, mute, sync offset) reach the program at once, as they do for any source placed in both preview and program scenes. Only the scene's items are held. The Mixer and `source.list` list the real program scene's sources: an audio source added while held plays on the program only after the transition, and one removed from the scene while held (not deleted) is no longer listed but still plays on the program until the transition. Leaving studio mode ends the hold, so the program then shows the scene as it is, as it would after a transition. The studio runs one canvas (ADR-0041), so only that canvas's program is held.
5. **Zero cost.** Outside studio mode, or with another scene in preview, nothing is held and the studio runs the user's collection itself. While held, each edit builds the run collection once (a clone on the control thread, not on a real-time path).

## Consequences

- Studio mode prepares the next picture in every case, including the default one.
- The run collection has one more scene than the user's; code that reads the studio's collection back must not show it to the user (no code does today).
- Tests: a unit test that the copy keeps the scene as it was, is never in the user's collection and keeps its ids; an API test that edits with the program scene in preview never list a copy and that transitions run without an error.
- Specs: UI design system (Studio mode), control API (Studio mode section: transition between one scene and itself).

## Alternatives considered

- **Visible duplicate scene** (as some studio apps offer): a real scene the user sees in the list. Rejected by the owner: the user must not see a copy.
- **Refuse edits while preview == program**, or show a warning: the default state of studio mode would then be read-only, which is surprising and gives the user nothing to prepare.
- **Freeze the program compositor's layers in the engine**: the compositor would stop following the binder for that canvas, so source changes and new nodes would need a second path; the run collection reuses the normal binding.

## Review

Independent review per ADR-0014, 2026-11-07, by a fresh AI session given only this ADR, the ADRs it builds on, the specs and the code. Verdict: accept with changes. Findings and how they were settled:

1. Blocking: the copy outlived its scene (an undo that removed the program scene, a collection switch). The copy now records the scene it came from and is taken again when another scene is on air; a collection switch drops it (decision 1).
2. Blocking: a copy that no longer fit the run collection (a deleted source, a used id or name) let edits go on air while the host still reported a hold. Items whose source or scene is gone leave the copy, and a copy that still does not fit is taken again or dropped (decision 2).
3. A canvas size change left the copy's layouts at the old size. They are rescaled with the scene's.
4. The ADR said a restart takes a new copy while the code kept it. Decision 1 now says a restart keeps it.
5. Leaving studio mode, other canvases and sources removed while held were undocumented. Decision 4 and the specs now state them.
6. The tests did not cover a stale copy or a deleted source. Unit tests and the API test now do; the program's picture itself is checked only through the run collection, as no test reads a picture back.
7. Item ids depended on position. A copy's item id is now its item's id plus `0xF000_0000`.
8. The copy's name could appear in a failure message. It is now the scene's name with "(on air)".
