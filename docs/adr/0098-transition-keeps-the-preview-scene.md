# ADR-0098: Transition keeps the preview scene; swapping is a setting, off by default

- Status: Accepted
- Date: 2026-11-10
- Deciders: Salih (owner)
- Builds on: ADR-0061 (studio mode), ADR-0063 (the program is held while its scene is also the preview scene), ADR-0056 (app settings in `app.toml`), ADR-0076 (Settings window parts)
- Supersedes in part: ADR-0061 decision 3 ("Transition is a cut, and it swaps"): only the swap (the rest of that decision stands, decision 1 below); ADR-0063 decision 1: when a hold starts and when it ends (decision 2 below)

## Context

ADR-0061 decision 3 made Transition swap: the preview scene goes on air and the old program scene goes to Preview. It listed a "swap off" option as a follow-up. The owner tested studio mode on Windows (2026-11-10) and found the swap wrong as the default: after a transition the scene he was preparing left Preview, and the next click on Transition put the old scene back on air. He wants the picture in Preview to go to Program and Preview to keep showing the same scene; the user changes the preview scene when they want to. A setting may offer the swap, but it must be off by default.

Without the swap, Preview and Program show the same scene after a transition. ADR-0063 already covers that state: while `preview == program` the host holds a copy of the program scene, so edits made in Preview reach the program only through the next transition.

## Decision

1. **Transition keeps the preview scene.** `canvas.transition` (the Transition button, its shortcut and the registry entry `canvas.transition`) sets the program to the preview scene and leaves the preview scene as it is. Preview and Program then show the same scene, and ADR-0063 decision 1 holds the program from that moment: edits made in Preview after the transition go on air only with the next transition (ADR-0063 decision 3). Everything else in ADR-0061 decision 3 stays: a cut, one schedule swap, a live action that is not undoable, and no change when both are already the same scene with nothing held.
2. **A hold lasts while its scene is on air.** ADR-0063 decision 1 let the copy go as soon as the preview scene was no longer the program scene. With decision 1 every transition ends with preview == program, so the usual next step (preview another scene to prepare it) would put the edits made in Preview since the transition on air without a transition. Now a hold starts when the program scene is also the preview scene and nothing of it is held: entering studio mode, previewing the scene on air, a transition that keeps the preview scene (decision 1), the studio's start, or an undo or redo that put another scene on air. It ends only when another scene goes on air (a transition, an undo or redo, removing the scene), when studio mode ends, or with a scene collection switch. A transition while the program scene is in preview and held drops the copy and holds the scene as it is now (ADR-0063 decision 3, unchanged). Undo and redo never change program or preview (ADR-0061 decision 5), so they keep the hold; an undo of an edit to the held scene changes the scene, not the copy on air.
3. **Swapping is an app setting, off by default.** `AppSettings` (`app.toml`, ADR-0056) gains `swap_after_transition: bool` (`#[serde(default)]`, false; the file format stays 1, since an older build ignores the key and a newer one reads its absence as false). When it is true, a transition between two different scenes also sets the preview to the old program scene, as ADR-0061 decision 3 did. The setting belongs to this computer and to no output profile or scene collection, like `on_close`. The studio host runs on its own thread: it gets the value at start (`HostConfig::swap_after_transition`) and each change through a host command (`Command::SetSwapAfterTransition`), so a control API or MCP (ADR-0052) client's `canvas.transition` follows the app's current setting. An older build that saves `app.toml` drops the key (it then reads as false).
4. **Settings window.** Settings › General gets a card "Studio mode" with one Toggle row (ADR-0076 parts, tokens only): "Swap scenes after a transition", explained as "After Transition, Preview shows the scene that was on air. Off: Preview keeps the scene that went on air." The change applies to the next transition and is saved at once.
5. **Control API.** The meaning of `canvas.transition` changes, so it is protocol 1.31: from 1.31 the preview keeps its scene unless the app's setting is on; 1.9 to 1.30 always swapped. The spec counts a change of meaning as major; this is a stated exception, accepted by the owner: the preview outcome of `canvas.transition` is defined by the app's setting, and 1.31 documents the default change. A major version for one outcome of one live action would cost every client a migration for no change in what it sends. The reply stays `{}`; clients read the new program and preview from `scene.changed` (or `scene.list`), as they did.
6. **Zero cost.** One boolean read per transition; no thread, timer or per-frame work.
7. **Tests.** Host tests: a transition with the setting off puts the preview scene on air and keeps it in preview (`scene.changed` with `preview == program`); with the setting on (sent to a running host), the old program goes to preview as before. Hold tests: after a transition the program is held; an edit to it, then previewing another scene, does not put the edit on air; undo and redo keep the hold; a transition while held with the swap on does not swap (one scene) and holds again. Settings tests: the key round-trips, its absence reads as false. The UI rules apply to the card (proposal, screenshots in dark, light and high contrast, owner approval before merge).

## Consequences

- The default behaviour changes for every user: Transition no longer flips between two scenes. Going back to the previous scene takes a click on it in Scenes (or its shortcut) and a Transition, or the swap setting.
- After a transition the old program scene leaves both Preview and Program, so its sources stop, and going back to it waits for them to start again (with the swap they kept running in Preview); the owner prefers predictability.
- ADR-0063's hold now outlives a change of the preview scene: edits to the scene on air (through the API, since the app edits the preview scene) wait for a transition to it. ADR-0063 decision 4's gap lasts as long: the Mixer and `source.list` list the real program scene while the held copy is what is heard and seen. A held copy that must be taken again (it no longer fits) is taken of the same scene, whatever is previewed.
- The control API spec counts a change of meaning as a major change; this one goes out as minor 1.31 by an owner-accepted exception (decision 5).
- Specs change with it: UI design system (Transition, Settings › General), control API (`canvas.transition`), user data (`app.toml`).

## Alternatives considered

- **Keep the swap as the default and add the setting.** The owner rejected it (2026-11-10).
- **A setting per scene collection or output profile.** The swap is how the person likes to work, not a property of a show; it stays with the computer, like `on_close`.
- **A `swap` parameter on `canvas.transition`.** Can come later if a control surface needs both behaviours; the setting covers the owner's need.
- **The setting in the Transition rail.** The rail is narrow and already holds the transition controls; a rarely changed preference belongs in Settings.

## Review

Independent review per ADR-0014, 2026-11-10, independent AI session (given the ADR, the ADRs it builds on, the specs and the code): changes requested, 7 findings, all resolved in this revision.

1. Blocking: the hold ended when another scene was previewed, so edits went on air without a transition, now the usual case: decision 2 (hold lasts while its scene is on air; supersedes ADR-0063 decision 1 in part), specs updated.
2. A hold starting at a transition was not in ADR-0063's list; undo and redo not covered: decision 2.
3. The control API meaning changed without a version: decision 5 (protocol 1.31), control API spec.
4. How the setting reaches the host thread: decision 3.
5. `app.toml` missing from the user data spec; older builds drop the key: user data spec, decision 3.
6. Test gaps: decision 7.
7. Wording: header, Consequences, decision 4.

Round 2, 2026-11-10, same reviewer on the revision and the implementation: approve with changes, 3 findings, resolved here: (1) a minor version for a change of meaning goes against the control API spec's rule: stated as an exception in decision 5, open for the owner's acceptance; (2) a held copy taken again after it stopped fitting needed preview == program, so the hold could end while another scene was previewed: `copy_of` takes the same scene again, with a unit test; (3) ADR-0063 decision 4's gap now lasts longer: Consequences and the control API spec.

Owner acceptance, 2026-11-10: Salih tested the change on Windows and approved the merge, including the minor-version exception in decision 5.
