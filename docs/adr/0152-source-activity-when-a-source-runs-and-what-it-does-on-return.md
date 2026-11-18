# ADR-0152: Source activity: one rule for when a source runs and what it does when it returns

- Status: Proposed
- Date: 2026-11-18
- Deciders: Salih (owner)
- Builds on: [ADR-0013](0013-zero-cost-when-unused.md), [ADR-0024](0024-node-restart-with-back-off.md), [ADR-0086](0086-recording-sources-separately-iso.md), [ADR-0105](0105-timed-show.md), [ADR-0107](0107-temporal-sources-sleep-when-hidden.md), [ADR-0108](0108-media-file-source.md), [ADR-0109](0109-browser-source.md), [ADR-0124](0124-pause-recording-and-hold-stream.md), [ADR-0128](0128-scene-transitions.md), [ADR-0129](0129-text-source.md), [ADR-0135](0135-transitions-t-bar-scene-override-rehearse.md), [ADR-0139](0139-remote-guests-rbs-to-rbs-over-webrtc.md), [ADR-0140](0140-stinger-transition.md), [ADR-0143](0143-performance-rules-for-every-change.md), [ADR-0149](0149-browser-page-automation-zoom-and-background-pages.md), [ADR-0150](0150-scrolling-layers-in-the-compositor.md)
- Changes (on acceptance): ADR-0107 decision 7 (classification by type id) and the keep-warm clause of decision 8 are superseded by decisions 3 and 5 here; decision 8's rule that only ADR-0105 fills the cue set stays. ADR-0132 decision 5's "no reload when shown setting" is changed by browser `on_return: restart` (decision 6). ADR-0108's `restart_when_shown`, ADR-0149's `keep_running` and ADR-0140's `arm()` override of `restart_when_shown` are restated on the common policy (decisions 6 and 10); their behaviour does not change.

## Context

Which sources keep running when nobody sees them is decided today in many places, each for one type or one feature:

| Where | Rule today |
| --- | --- |
| Scene model `wanted_sources` / `warm_sources` (capture, still, text, audio, plugins) | Active and linked to the program compositor while placed in any scene, hidden or off air. Released only by **Deactivate** (`source.set_active false`, the `standby` hint). |
| ADR-0107 (media, browser, by type id) | Active only while a shown scene (program, studio-mode preview, bound hold scene, any canvas) draws it with `visible` true. Otherwise Standby: no thread, decoder or process. |
| ADR-0108 decision 10 (media) | Preview only cues. `restart_when_shown` (default true) moves the cursor to the range start when it leaves air. Paused sources release the decoder after `pause_release_s`. |
| ADR-0149 decision 5 (browser `keep_running`, not built yet) | Keeps the page process while placed, 1 picture per second, picture node unscheduled. Needs the network permission. |
| ADR-0139 decision 12 (guest) | Capture class: connected while placed; video decode stops while no shown scene draws it. |
| ADR-0150 decision 5 (scroll) | A run starts when the first list on a canvas (program, preview, incoming) contains its key, ends when none does. |
| ADR-0128 / ADR-0140 / ADR-0135 (transitions) | The outgoing scene's sources and the stinger stay on air until the transition ends; a manual T-bar's incoming scene stays cued until it lands. `arm()` overrides `restart_when_shown` for the stinger. |
| ADR-0086 decision 8 (ISO tracks) | A source with a separate track stays Active for the whole recording. |
| ADR-0129 decision 7 (text) | Its own "on-air set" decides which text sources are polled and re-rendered. |

Each rule is right for its case, but the user meets a different word per type, a plugin cannot choose, and each new type (NDI, slide show, audition player) would add another rule. The owner asked for one structure every source uses, with a recommended default per type and an override per source (2026-11-18).

**What OBS does.** libobs counts *active* (drawn by the program) and *showing* (drawn anywhere: program, preview, projectors, multiview). Types add their own checkboxes: media "Restart playback when source becomes active" and "Close file when inactive"; browser "Shutdown source when not visible" and "Refresh browser when scene becomes active"; VLC and slide show "Visibility behavior"; image "Unload image when not showing"; video capture "Deactivate when not showing". Weak spots: a different word per type; the multiview marks every source as showing and wakes every page and file; a sleeping camera is black for its warm-up; "always play" decodes a film nobody sees.

Forces: P13 and ADR-0143 (nothing placed but unused may cost a thread, device, process or per-frame work); P3 (no type names in engine code); P7 (a failed wake is isolated); the owner's feature-parameters rule (a choice the user cares about is a saved setting reachable from the control API); device warm-up (about 0.3 to 2 s for USB cameras and capture cards, 0.25 to 2.4 s for a cold browser); exclusive devices another app may take while released.

## Decision

1. **Four levels, computed by the binder on each sync, plus Off.** For each source, highest first:
   - **On air**: exactly today's `BindReport::on_air_sources`, extended to every type: drawn with `visible` true by a canvas's program scene (with its global scene and timed-show cues), by a bound hold scene (ADR-0124), or by the outgoing `drawn`/`heard` set or the stinger of a running transition (ADR-0128 decision 6, ADR-0140 decision 4). The incoming scene of a manual T-bar (ADR-0135) is not on air until it lands, as today.
   - **Shown**: not on air, but drawn with `visible` true by a canvas's studio-mode `preview` scene (a cue there counts here, not as on air) or by the incoming scene of a manual T-bar before it lands. (A timed transition's incoming scene is already the program, ADR-0128 decision 7.) Scene and source projectors join this level when they exist (ADR-0125 later work). A multiview never counts: its tiles show the type's placeholder.
   - **Placed**: an item in any scene of the open collection, hidden or off air.
   - **Unplaced**: in no scene.

   Opacity does not lower a level (ADR-0107 decision 3). Every canvas in the collection counts, as `running_sources` does today. Program projectors, the virtual camera and outputs show a program, so they add nothing. **Off** (Deactivate, the `standby` hint) wins over every level and every forced cause.

2. **Forced causes.** Each makes a source Active at a stated level whatever its `run` is, never over Off:
   - an ISO track (ADR-0086 decision 8) in a running recording raises its source to **Shown** for the recording, so a browser track runs at full rate with sound, and a media track behaves as in the preview (cued, or paused, per ADR-0108);
   - an open Interact window (ADR-0132) raises its page to **Shown**;
   - the stinger of a running transition (armed for that transition by ADR-0140's `arm()`, not merely chosen as the stinger) is **On air** (decision 1).

   A source held only by a forced cause is linked to the compositors that draw it, if any, and its node stays in the scheduled graph through the cause itself (the ISO track's export, the Interact window's view), as ADR-0086 and ADR-0132 already require. Each is an input set to `sync` that is empty when unused, so it adds no scan (as `running_sources_cued` today). Monitoring (`source.set_monitor`) is not a cause: it is a saved mode and must not wake a hidden film or page; a test shows a monitored hidden media source stays Standby. ADR-0129's text set keeps its meaning but is named the **text live set** in the specs, so "on air" has one meaning.

3. **Each source has one saved activity policy.** `Source.activity: Option<SourceActivity>`, a field beside `audio`, not a type setting, so changing it never replaces the node (ADR-0039). Absent means the type's default (decision 5).

   | Key | Values | Meaning |
   | --- | --- | --- |
   | `run` | `placed`, `shown`, `on_air` | The lowest level at which the source is Active. Below it, Standby: no thread, device, process, timer or per-frame work (P13). |
   | `on_return` | `continue`, `resume`, `restart` | What the source does at its type's return edge (decision 6). `continue`: as if it had kept running. `resume`: where it stopped. `restart`: from the beginning. |
   | `linger_s` | 0..=600, default 0 | Seconds a source stays Active after its level drops below `run`. |

4. **What Active means below Shown.** A source Active at Placed (its `run` is `placed`, or it lingers) is **warm**; a forced cause raises a source to Shown or On air (decision 2), never to warm. Only Shown and On air link a source to a compositor through `visible_sources`; at warm, `warm_sources` keeps the program link only for types whose traits say `linked_while_warm` (capture and stills, as today). Warm is ADR-0013's "Standby kept warm by user policy" row (or Active and unscheduled), and it is counted in the ADR-0087 estimate. Per built-in type:
   - Capture, still, audio, text: as today (open, linked, processed).
   - Media: ADR-0108 decision 10's table: cued at the stored cursor, or paused or ended; `pause_release_s` still releases the decoder, so a `placed` film starts at once only within that time.
   - Browser: ADR-0149 decision 5's background page: process running, picture node unscheduled, 1 picture per second, sound drained, background slot class after program and preview (ADR-0109 decision 7).
   - Guest: connected, video decode stopped (ADR-0139 decision 12).

   A lingering source that no shown scene draws does what a warm one of its type does; one drawn in the preview behaves as Shown. Linger applies when the level drops, including when the source is removed from its last scene; deleting the source ends it (decision 8).

5. **The type declares what it supports.** `rbs-scene`'s `NodeTypes` gains `activity(type_id) -> ActivityTraits` (default method), with: the allowed `run` and `on_return` values, the default policy, the return edge (decision 6), `linked_while_warm`, and the permission each value needs. The default method answers `run: placed` only, no `on_return`, linked while warm: today's wanted rule, so every existing plugin behaves as now. `rbs-sources` answers for the built-in types. The binder never tests a type id; a source is temporal when its effective policy's `run` is not `placed`. The binder checks a saved policy against the traits at each sync, and the API checks it at write (decision 10). A type may offer `run` above `placed` only if its Active transition starts the open and returns without waiting (ADR-0107 decision 5, now for every type), shown by a conformance test where a fake `open` that never returns does not block `sync` (a hang guard only, ADR-0032). For plugin sources the host enforces this: it calls a plugin instance's open on a worker of that plugin (one per plugin, so one plugin's hung open never delays another plugin's wake), never on the control thread; a timed-out call is abandoned for an in-process plugin and its process is ended for an out-of-process one, written into `plugin-sdk.md`; and an open that has not returned within the start limit the type declares (at most 15 s) is `Unavailable` with reason `start_timeout`. A failed wake is ADR-0024 restart with back-off and `Unavailable` with a reason (`device_busy` for an exclusive device another app holds); the layer stays empty meanwhile.

   Recommended defaults and allowed values now:

   | Type | `run` default (allowed) | `on_return` default (allowed) | Why |
   | --- | --- | --- | --- |
   | Camera, capture card, display, window, screen capture | `placed` (`placed`) | none | Warm-up and exclusive devices. `CaptureSourceNode::prepare` opens synchronously, so `shown` and `on_air` wait for an async open (phase 4). Deactivate stays the way to release a device. |
   | Audio input, audio output, application audio | `placed` (`placed`) | none | Meters and the mixer need them; a sleeping microphone loses the first syllable. |
   | Image, colour, text | `placed` (`placed`) | none | Cost is one texture. Countdown and stopwatch run state is live state (ADR-0129 decision 11). Image `shown` waits for an async load. |
   | Media file | `shown` (`placed`, `shown`, `on_air`) | `restart` (`continue`, `resume`, `restart`) | ADR-0107 and ADR-0108 as today. `continue` gives OBS's "always play" with no work while hidden. |
   | Browser | `shown` (`placed`, `shown`, `on_air`) | `continue` (`continue`, `restart`) | ADR-0107 and ADR-0109 as today. `placed` is ADR-0149's `keep_running`. `restart` reloads the page when it goes on air (OBS's "Refresh browser when scene becomes active"). A page that slept always loads fresh. |
   | Guest (ADR-0139), NDI (plugin) | `placed` (`placed`) | none | Reconnecting takes seconds and may need the other side. |
   | Scroll (an item's layout) | follows its source | `restart` (`continue`, `restart`), saved as `Scroll.on_return` | ADR-0150 as today; see decision 6. |

   The permission column of the traits: browser `run: placed` and browser `linger_s` > 0 need the network permission (ADR-0149 decision 5), so `source.set_activity` without it is `not_permitted` with reason `network`.

6. **Return edges, per type, as today.**
   - **Media.** ADR-0108 decision 10's table, with `restart_when_shown` true read as `on_return: restart` and false as `resume`; the cursor moves on the off-air edge, so the preview shows the first picture before the next Take. `continue` adds rows: leaving air stores an anchor (engine time and position); only a film that leaves air playing is anchored; a paused or ended film follows the `resume` rows. Going on air again seeks to the anchored position plus the time since, clamped to the play range, wrapped when `loop` is true, `ended` past the end when false. A source never on air anchors at the range start. Nothing decodes while away. After an app restart `continue` behaves as `resume`. ADR-0140's `arm()` overrides `on_return` (as `resume` from the range start) for that run only.
   - **Browser.** The on-air edge: `restart` reloads when the page goes on air and was not on air before; a page that wakes on that same edge (`run: on_air`) loads once, not twice.
   - **Scroll.** ADR-0150's run-key edge per canvas (a list on that canvas, preview included, contains the key). `restart` is today's rule. `continue` keeps a run's anchor while the item stays placed on that canvas after no list contains it, so it resumes at the computed position; a change of cross-axis size still makes a new key and a new run.

7. **Delivery to the source.** Each sync reports `BindReport::level_changes: Vec<(SourceId, Level, Option<OnReturn>)>` (`None` for types without `on_return`) for sources whose level or effective `on_return` changed, and no work when none did. The host writes each to the source's control handle on the control path (`set_level(level, on_return)`; media `set_on_air`, `restart_when_shown` as a live key, and the guest's shown set become this), never through `process`. When the host adopts a created or replaced node it writes that source's current level and `on_return` before the node first runs, as `studio.rs` writes the on-air set today, so a new node never runs on a handle default. `arm()` saves and restores `on_return` on the handle as it does `restart_when_shown` today. Plugins get a non-real-time `on_level(level, on_return)` call in the plugin SDK, made only on loaded, Active instances (plugin SDK rule 6: nothing runs before first use); an instance gets its current values when it becomes Active. The manifest gains a per-type table (`[[source_type]] id = ..., activity = { run = [...], on_return = [...], default = {...}, return_edge = "...", linked_while_warm = ..., needs = { <value> = "<permission>" }, start_limit_s = ... }`). Omitted fields take the default method's values (`run = ["placed"]`, no `on_return`, `linked_while_warm = true`, no permissions, `start_limit_s = 15`). This is a minor SDK version: an older host ignores the table, so the plugin runs at `placed` as today.

8. **Cost (P13, ADR-0143).** Levels, forced causes and policies are evaluated on the control side in `sync`, which already walks the shown scenes; no per-frame check is added and inactive nodes stay out of the scheduled graph (rule 10). A source with its type's default policy costs what it costs today. `linger_s` 0 adds nothing; above 0 it arms one control-side wake per lingering source on the injected clock, cancelled when the level returns. Linger ends at once on Off, source delete, collection switch and studio stop; across a studio restart its wake and every `continue` anchor are rebased by the signed clock difference, as ADR-0150 does for scroll runs.

9. **Studio mode and canvases.** Accepted visible effects: a media source with `continue` is cued in the preview at its position computed when it was cued, and jumps to the position computed at the Take; a browser with `restart` keeps its last picture on air until the reloaded page's first paint, for at most 2 s (ADR-0149 decision 2's rule). The studio-mode preview is Shown, so a `shown` source opens before the Take (ADR-0128 decision 7's ready gate then has nothing to wait for), media is cued, and a page takes a slot after program pages. With `run: on_air` the preview shows nothing for that source (Standby holds no texture, ADR-0107 decision 5); it is meant for costly sources whose preview the operator does not need. A source placed on both canvases has one node and one policy; its level is the highest across canvases.

10. **Saved files, API and migration.**
    - Scene format 23 (the next after 22; confirmed at merge) maps media `restart_when_shown` true to `on_return: restart` and false to `resume`, and browser `keep_running` true to `run: placed`, and drops both keys. Every existing document keeps its behaviour.
    - A saved policy that the type's current traits do not allow (plugin missing or updated, imported file) is not a load error: the type default is used, the saved value stays in the file, and the binder reports a `BindProblem` entry `activity_unsupported`, which the host shows as a source warning. Through `source.set_activity` the same value is `invalid_params`.
    - Control API, next protocol minor (confirmed at merge): `source.set_activity {source, run?, on_return?, linger_s?}` (one undo step, `source.changed`, `null` resets a key to the type default); `item.set_scroll` gains `on_return`; `source.list` rows and `source.changed` carry `activity` (`off`, `asleep`, `lingering`, `warm`, `shown`, `on_air`) and `warm_up_ms` (the last time from Active to first picture, live state, for types that report a first picture); `api.describe` lists each type's traits. `source.set_settings` keeps accepting `restart_when_shown` and `keep_running` and maps them, and `source.get_settings` keeps returning them computed from the policy, marked deprecated (an old-key write is still one undo step), until the next major protocol version (removing them is a major change under the control API versioning rules).
    - The OBS importer (ADR-0131) maps "Restart playback when source becomes active", "Close file when inactive", "Shutdown source when not visible", "Refresh browser when scene becomes active" and "Unload image when not showing" to the policy where the type allows the value, and reports the rest.

11. **rbs-scene signatures.** `is_temporal`, `running_sources` and `warm_sources` need the traits, so they take `&dyn NodeTypes` (or move onto `SceneBinder`). The closure `NodeTypes` used by tests gains a way to declare traits, and the existing ADR-0107 tests declare media and browser explicitly so they do not silently fall back to `placed`.

## Consequences

- One question, one place: Properties shows "When not on screen" (`run`), "When it comes back" (`on_return`) and "Keep running for" (`linger_s`) for every source, with only the values its type allows. Plugins join by declaring traits.
- Defaults keep today's behaviour; the new choices are opt-in.
- Negative: one more top-level `Source` field and a format bump; the SDK gains a call and a manifest table; `continue` for media needs a seek on return, so a long GOP shows the cued picture a moment longer than `resume`.
- Specs to update in the implementing PRs: scene model (levels, forced causes, `SourceActivity`, signatures), scene sources (traits per type, warm behaviour, migration), control API, plugin SDK, text spec ("text live set"), glossary ("On air", "Shown", "Deactivate").
- Phases: (1) model, traits, levels, forced causes, delivery, migration, API, and ADR-0149's `keep_running` behaviour, which is not built yet, as `run: placed` for browser; no UI. (2) Properties UI, with dark and light screenshots for approval. (3) linger and `warm_up_ms`. (4) async open for capture and images, then `shown`/`on_air` for them (warm-up shown next to the setting). (5) media and scroll `continue`. No phase edits a hot path except phase 4's capture open, which is measured (P15).
- Tests: a binder property test that a source is Active exactly when Off is unset and (its level reaches `run`, or it lingers, or a forced cause holds it at the cause's level), over random collections, canvases, policies and cause sets; each forced cause; the T-bar and stinger edges; ISO with `run: on_air`; the non-blocking open conformance; linger on `TestClock` without sleeping, including its teardown cases and a studio-restart rebase; media `continue` positions (never on air, loop, end, range clamp); load of an unsupported value; migration round trip.
- Split off: making ADR-0128 decision 7's 250 ms `READY_WAIT` a collection setting is a separate decision if measurements of `warm_up_ms` ask for it.

## Alternatives considered

- **Keep adding a checkbox per type (OBS's way).** Cheapest per feature, but a new word per type and no way for plugins to join.
- **One "visibility behaviour" list (stop, pause, always play).** Mixes when a source runs with what it does on return; it cannot say "keep the page running but reload it on air".
- **Two keys without `linger_s`.** Simpler, but a quick back-and-forth would reopen a page or a file each time; linger is cheap when 0, so it stays.
- **Sleep every hidden source, cameras included.** Black cuts and lost exclusive devices; rejected again as in ADR-0107.
- **Let the multiview count as shown.** Repeats OBS's problem of waking every placed page and film.
- **Scripted events per source (on show: reload, seek, run a script).** Too general for a setting; automation can do it through the control API later.

## Review

Round 1, independent AI session, 2026-11-18, changes requested (`/mnt/project-files/source-activity/review-round1.md`). Resolutions (author, 2026-11-18):

1. Active below Shown undefined. -> Decision 4: warm and lingering per type, linking rule (`visible_sources` at Shown and On air, `linked_while_warm` trait for `warm_sources`), ADR-0013 row, estimate; media "instant" corrected to within `pause_release_s`.
2. No runtime signal for types and plugins. -> Decision 7: `BindReport::level_changes`, control handles, SDK `on_level`, manifest `[[source_type]]` table, minor SDK version.
3. Blocking capture open. -> Decision 5: non-blocking Active transition is a trait condition with a conformance test; capture, image and audio offer `placed` only until phase 4; wake failure stated.
4. On air differs from today. -> Decision 1: On air is today's `on_air_sources` (outgoing, stinger, hold); T-bar incoming stays Shown; `arm()` in decision 6.
5. Edges not as today. -> Decision 6: media on ADR-0108's table (off-air cursor move), scroll on ADR-0150's run-key edge.
6. Missing causes. -> Decision 2: forced causes (ISO, Interact, monitoring, stinger); text set renamed "text live set"; guest in decision 4; context table extended.
7. Decision 9 vs ADR-0128's gate. -> Removed; split off in Consequences.
8. `source.status`. -> Decision 10: `source.list` rows and `source.changed`.
9. Permissions. -> Decision 5: permission per value in the traits; browser `placed` and linger need `network`.
10. Slots, Interact, last picture. -> Decision 4 (background slot class), decision 2 (Interact forced), decision 9 (preview shows nothing).
11. Load of unsupported values. -> Decision 10.
12. Signatures and test `NodeTypes`. -> Decision 11.
13. Migration details. -> Decision 10 (format 23, minor, old keys both ways for one minor, OBS import); `keep_running` built in phase 1.
14. Edges. -> Decision 1 (cues by scene, globals with program, every canvas as today), decision 8 (linger teardown and rebase), decision 6 (media `continue`), tests in Consequences.
15. Context table. -> Extended.

Round 2, independent AI session, 2026-11-18, changes requested (`/mnt/project-files/source-activity/review-round2.md`): 13 of 15 round-1 findings resolved. Resolutions (author, 2026-11-18):

1. Forced causes lacked levels; monitoring changed defaults. -> Decision 2: ISO and Interact raise to Shown, stinger On air; monitoring is not a cause, with a test; Interact listed once.
2. Delivery lacked the policy; new nodes got no write; plugins loaded early. -> Decision 7: level and `on_return` together, written at adoption, `arm()` saves `on_return`, `on_level` only on Active instances.
3. Timed transition incoming. -> Decision 1.
4. Armed stinger. -> Decision 2.
5. Old keys. -> Decision 10: kept until the next major protocol version.
6. Who checks the policy. -> Decision 5: binder at sync, API at write.
7. Plugin blocking open. -> Decision 5: host calls it off the control thread, `start_timeout`.
8. Manifest fields. -> Decision 7.
9. Temporal by effective policy. -> Decision 5.
10. Text row. -> Decision 5 table.
11. Visible effects. -> Decision 9 states them as accepted.

Round 3, independent AI session, 2026-11-18, **approve with changes** (`/mnt/project-files/source-activity/review-round3.md`): all round-2 findings resolved, no blocking finding. Minor findings resolved (author, 2026-11-18): 1 forced causes Active whatever `run`, test fixed (decision 2, Consequences); 2 one worker per plugin, timeout per tier (decision 5); 3 ADR-0132 and the narrowed ADR-0107 clause in the Changes line; 4 `continue` anchors only a playing film (decision 6); 5 lingering in preview, linger on removal (decision 4); 6 one load on the wake edge (decision 6); 7 `BindProblem` and `invalid_params` (decision 10); 8 manifest defaults (decision 7); 9 `Option<OnReturn>`, undo step, scheduling of forced sources (decisions 7, 10, 2).

Owner acceptance: pending.
