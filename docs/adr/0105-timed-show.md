# ADR-0105: Timed show draws a hidden item for a deadline without undo

- Status: Proposed
- Date: 2026-11-12
- Deciders: Salih (owner)
- Builds on: [ADR-0059](0059-scenes-lock-and-source-audio-in-the-studio.md), [ADR-0060](0060-shortcuts-in-app-and-global.md), [ADR-0013](0013-zero-cost-when-unused.md), [ADR-0107](0107-temporal-sources-sleep-when-hidden.md)

## Context

Sponsors, arrows, and lower-third plates are often left hidden in the scene and shown for a few seconds on a hotkey. Writing that show into the scene document would put it on the undo stack: Ctrl+Z after the cue ends would leave the item permanently visible. Live controls such as volume stay off the user undo stack (ADR-0059). Hide (`visible`) is still a document edit and must stay undoable.

ADR-0107 keeps media and browser Active while a timed-show cue draws them even if `visible` is false. That cue is this ADR.

## Decision

1. **An item may store `show_for_ms`.** Absent means none. When set, it must be in `1..=600_000`. That field is document state and one undo step (`item.set_show_for`). The eye (`visible`) stays the persistent flag and stays undoable. Persistence uses the next free scene-format optional key on the item (same pattern as other optional item fields); older builds drop unknown keys on save only if the format rules already say so, otherwise the format bump refuses older readers.
2. **`item.show` is live**, like `scene.switch`. It must not change `visible`, must not push an undo record, and must not be restored by Ctrl+Z. It is host-session state only; after reload there is no cue until Show is pressed again. Control API clients with `control` scope may call it (U20 / ADR-0052 live class).
3. **Show rules.** Show requires `show_for_ms` set. If `visible` is true, Show does nothing (no cue, no deadline). If `visible` is false, Show starts or restarts the deadline from now + `show_for_ms`. Mid-cue Show restarts the deadline. If the eye is opened during a cue, that edit is persistent and undoable; the cue is dropped and the deadline must not hide the item later.
4. **Cue map.** The host holds `ItemId → deadline` on the studio clock. Expiry is one wake per running cue (or the next studio control tick that compares the clock to the soonest deadline). While the map is empty, nothing schedules and nothing scans items (P13). Tests use `TestClock`, not sleep. Undo of a move keeps the cue; undo that removes the item drops it. Save must not persist the map.
5. **Draw predicate.** `flatten` (and the program/preview layer lists built from it) takes an optional set of cued item ids from the host. An item is drawn if `visible` is true, or if its id is in that set, and its scene is a shown scene of that canvas: the canvas `program`, or `preview` when set (same “shown” as ADR-0107 decision 6). Nested content under a hidden nested scene stays omitted unless that nested scene item itself is cued or visible. Global-scene items follow the same rule on every scene of that canvas. A cue on an item whose scene is not shown on the program compositor must not appear on program output.
6. **Audio.** A cued hidden item counts as drawn for `visible_sources` and the program mixer as well as for video. Silence only if the source has no audio or is muted.
7. **Binder hook.** The host passes the set of source ids of currently cued, drawn items into `SceneBinder::sync` (or `running_sources`) each sync. Temporal Active (ADR-0107) is the union of sources drawn with `visible` true on shown scenes and sources in that cued set. Capture and stills stay on today’s warm rule. Parallel implementation with ADR-0107 is allowed: the argument defaults to empty until this ADR’s host map exists.
8. **Hot path.** Merging the cued id set into flatten is allowed only as a lookup of the active cue map (bounded by running cues). The implementation PR must run `hot-path-review` on that merge. No per-frame scan of all items.
9. **UI.** The Sources row has a Show control, separate from the eye, and a remaining-time label while a cue runs. The command is in the registry (ADR-0060). Protocol: `item.set_show_for` undoable, `item.show` live. No protocol minor is claimed here.
10. **No fade, slide, or opacity animation in this ADR.** Those are a later hot-path change on top of ADR-0100.

## Consequences

- A logo can sit hidden and appear for N seconds without polluting undo.
- Spec follow-ups: scene-model (`show_for_ms`, flatten/draw while cue), scene-sources or host cue map, control-api, ui-design-system, glossary, ADR-0107 implementation of the cue Active set.
- Negative: first version is hard cut on and off. Temporal sources pay wake cost when a cue starts if they were Standby.
- Visual UI needs rule 16 approval before merge.

## Reference tools (CLAUDE.md rule 17)

- **OBS Studio** uses Show/Hide, hotkeys, and often a visibility transition filter or a third-party timed source. Undo of visibility is a scene edit; accidental undo after a sponsor bumper can put the bumper back.
- **Where that falls short:** no first-class “show for N ms” off the undo stack; creators wire macros.
- **What RBS does by this decision:** duration is document state; the cue is live session state; the eye stays the lasting visibility.

## Alternatives considered

- **Write visibility true then false into the undo stack.** Ctrl+Z after the cue restores a permanent show. Rejected.
- **Auto-remove the undo records when the cue ends.** Same complexity as never writing them, with race risk. Rejected.
- **A separate “temporary source” type.** Logos and colours already exist; the missing piece is timing. Rejected.

## Review

Owner approval: Salih, 2026-11-12 (implement in parallel with ADR-0103 and ADR-0104).

Reviewer: independent AI session
Date: 2026-11-12
Verdict: changes requested
Concerns:
1. **Flatten still says hidden items are omitted** — [scene-model.md](../specs/scene-model.md) rule 81 leaves hidden items out of `flatten`. Timed show must draw them without setting `visible` true. The ADR lists a spec follow-up but does not state the replacement rule (for example a host-supplied set of item ids, or `flatten` taking an overlay), nor how nested hidden scenes interact. Without that sentence in the decision, flatten and compositor tests can disagree with binder behaviour. -> **open** (paste the flatten/draw predicate into decision 3 or 4, including global scenes and nested scenes).
2. **`running_sources` and ADR-0107** — ADR-0107 decision 4 defers the temporal exception to this ADR. Decision 4 here names the exception but not how the binder learns the cue (host-only map read on `sync`, extra argument to `running_sources`, or a dedicated helper). ADR-0107 review concern 6 stays open until this ADR names the hook shape and forbids or allows early stub code. -> **open** (one decision bullet: cue set ownership, when it is consulted, and that temporal Active is union of visible draw plus cued hidden draw per ADR-0107 decision 6).
3. **Audio while a hidden item is cued** — `visible_sources` drives mixer links for program audio. A cued draw is video-only in the decision text. A hidden browser or clip drawn for sponsor time could stay silent on air, or wake without audio, unless the ADR says otherwise. -> **open** (explicit non-goal or rule: audio follows `visible` only, or cue counts as drawn for `visible_sources` too).
4. **`Show` on an already-visible item** — Decision 2 says "Show on a visible item does nothing" and "Show again restarts the deadline" in the same list. Operators may expect Show to (re)start the timer on a visible bumper. Clarify whether restart applies only when `visible` is false, or whenever `show_for_ms` is set and Show is pressed (including mid-cue). -> **open** (single consistent rule; UI label behaviour follows it).
5. **Cue expiry without a idle timer (P13)** — Decision 3 forbids a timer while no cue runs, which is right. It does not say how the deadline is evaluated (studio clock tick on the control/render path, one scheduled wake per active cue, or both). A per-frame scan of all items would violate the spirit of P13 when many hidden sponsors exist. -> **open** (state the expiry mechanism and that inactive collections with no running cue add no periodic work).
6. **Save, reload and API clients** — Reload must not replay a cue (stated). Remote `item.show` after reload with unchanged `show_for_ms` is not covered; live commands that do not persist are usually acceptable but should match ADR-0052 effect class. -> **open** (one line on control API parity and that cues are host-session state only).
7. **Document format** — `show_for_ms` is document state but no format bump or user-data key rule is named (ADR-0059 pattern for new optional item keys). -> **open** (same change as spec follow-up: format minor or optional key policy).
8. **Hot path (P15)** — Forcing hidden layers into the compositor list touches flatten or its caller each frame while a cue runs. Acceptable if bounded to active cues, but the ADR defers motion to ADR-0100 and does not call for `hot-path-review` on the merge point. -> **open** (owner accepts bounded cue map lookup on draw; checklist in implementation PR).
9. **Preview-only placement** — Decision 4 says a preview-only item must not go on air. The scene model does not define "preview-only"; tie this to studio mode, which scene is the program compositor's input, and U15 scope so a cue on an item only in a non-program scene cannot appear on program output. -> **open** (predicate in scene-model terms, mirroring ADR-0107 "shown" scenes).
10. **Acceptance order with ADR-0107** — Both are Proposed; 0107 implementation explicitly omits cue branches until 0105 is accepted. Landing 0107 first without the hook named here risks a second binder churn. -> **open** (owner accepts parallel implementation with a named extension point, or sequences acceptance).

Resolutions (author, 2026-11-12), written into decisions 1–10: (1) flatten takes a host cued-id set; drawn if visible or cued, on shown scenes only. (2) host passes cued source ids into sync; empty default until the map exists. (3) cue counts for audio `visible_sources` too. (4) Show only when `visible` is false; restarts mid-cue; visible item ignores Show. (5) one wake / soonest deadline; empty map means no work. (6) cues are session-only; API live parity. (7) optional item key on the next free format. (8) bounded cue-map lookup + hot-path-review. (9) shown = program and preview when set. (10) parallel OK with empty cued-set argument.
