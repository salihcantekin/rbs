# ADR-0152: Source activity: one rule for when a source runs and what it does when it returns

- Status: Proposed
- Date: 2026-11-18
- Deciders: Salih (owner)
- Builds on: [ADR-0013](0013-zero-cost-when-unused.md), [ADR-0024](0024-node-restart-with-back-off.md), [ADR-0105](0105-timed-show.md), [ADR-0107](0107-temporal-sources-sleep-when-hidden.md), [ADR-0108](0108-media-file-source.md), [ADR-0109](0109-browser-source.md), [ADR-0124](0124-pause-recording-and-hold-stream.md), [ADR-0128](0128-scene-transitions.md), [ADR-0143](0143-performance-rules-for-every-change.md), [ADR-0149](0149-browser-page-automation-zoom-and-background-pages.md), [ADR-0150](0150-scrolling-layers-in-the-compositor.md)
- Changes (on acceptance): ADR-0107 decisions 7 and 8 (classification by type id, no keep-warm override) are superseded by decisions 3 and 4 here. ADR-0108's `restart_when_shown` and ADR-0149's `keep_running` become values of the common policy (decision 6); their behaviour does not change.

## Context

Which sources keep running when nobody sees them is decided today in five places, each for one type:

| Where | Rule today |
| --- | --- |
| Scene model `wanted_sources` (capture, still, text, audio, plugins) | Runs while placed in any scene, hidden or off air. Released only by **Deactivate** (`source.set_active false`, the `standby` hint). |
| ADR-0107 (media, browser by type id) | Runs only while a shown scene (program, studio-mode preview, bound hold scene, any canvas) draws it with `visible` true. Otherwise Standby: no thread, no decoder, no process. |
| ADR-0108 `restart_when_shown` (media, default true) | Back on air starts at the range start, or resumes at the stored cursor. Preview only cues. |
| ADR-0149 `keep_running` (browser, default false) | Keeps the page process while placed, 1 picture per second while hidden. Needs the network permission. |
| ADR-0150 scroll runs (any source with a scroll) | A run ends when no list on the canvas contains it; seen again it starts over. Shared by program, preview and transitions. |

Each rule is right for its type, but the user sees five different words for one question ("what happens when this is not on screen?"), a plugin cannot choose, and the next types (NDI, remote guests of ADR-0139, a slide show, an audition player) would each add a sixth. The owner asked for one structure that every source uses, with a recommended default per type and an override per source (2026-11-18).

**What OBS does.** libobs counts two things per source: *active* (drawn by the program output) and *showing* (drawn anywhere: program, studio-mode preview, projectors, multiview). Types react with their own checkboxes: media "Restart playback when source becomes active" and "Close file when inactive"; browser "Shutdown source when not visible" and "Refresh browser when scene becomes active"; VLC and image slide show "Visibility behavior" (stop and restart, pause and unpause, always play); image "Unload image when not showing"; video capture "Deactivate when not showing". Known weak spots: every type spells the rule differently; the multiview renders every scene, so it marks every source as showing and wakes every page and file the user placed; a camera that sleeps shows black for its warm-up on the next cut; "always play" keeps decoding a film nobody sees.

Forces: P13 and ADR-0143 (nothing placed but unused may cost a thread, a device, a process or per-frame work); rule 4 (capabilities, not type names, in engine code); the owner's feature-parameters rule (a choice the user cares about is a saved setting reachable from the control API); warm-up of devices (USB cameras and capture cards take about 0.3 to 2 s to their first picture, browser cold start about 0.25 to 2.4 s measured in the browser thread); exclusive devices that another app may take while released.

## Decision

1. **Four presence levels, computed by the binder on each sync.** For every source the binder derives one level, highest first:
   - **On air**: a program scene or a bound hold scene (ADR-0124) of any canvas draws it with `visible` true, counting timed-show cues (ADR-0105) and the incoming scene of a running transition (ADR-0128). This is ADR-0107's on-air set.
   - **Shown**: not on air, but a shown scene draws it: a canvas's studio-mode `preview`, an open Interact window (ADR-0132), and a scene or source projector once one exists (ADR-0125 later work). The multiview, when it exists, never counts: its tiles show the last picture or the type's placeholder.
   - **Placed**: an item in any scene of the open collection, hidden or off air.
   - **Unplaced**: in no scene.

   Opacity does not lower a level (ADR-0107 decision 3). Levels are the union over every canvas (main and vertical). Program projectors, the virtual camera and outputs show a program, so they add nothing; whether anything streams or records does not change a level. **Off** (the user's Deactivate, the `standby` hint) wins over every level.

2. **Each source has one saved activity policy.** `Source.activity: Option<SourceActivity>`, a field beside `audio`, not a type setting, so changing it never replaces the node (ADR-0039) and every type, plugins included, has the same three keys. Absent means the type's default (decision 3).

   | Key | Values | Meaning |
   | --- | --- | --- |
   | `run` | `placed`, `shown`, `on_air` | The lowest level at which the source is Active. Below it, Standby: no thread, device, process, timer or per-frame work (P13). |
   | `on_return` | `continue`, `resume`, `restart` | What the source does when it goes on air again after it was not on air. `continue`: as if it had kept running (a film or a ticker is where it would be now). `resume`: where it stopped. `restart`: from the beginning (rewind, reload the page, restart the ticker). |
   | `linger_s` | 0..=600, default 0 | Seconds a source stays Active after its level drops below `run`, so a quick switch back does not pay the start again. |

   `on_return` acts on the on-air edge, as ADR-0108 already does: entering the preview does not restart anything, the Take does. A source that slept and cannot keep its state (a browser page) restarts whatever `on_return` says; `continue` for a film is a seek to the computed position, not decoding while asleep, so it costs nothing while asleep.

3. **The type declares what it supports, not the engine.** `rbs-scene`'s `NodeTypes` gains `activity(type_id) -> ActivityTraits` (default method), listing the `run` and `on_return` values the type supports and its default policy. The default method answers `run: placed` only and no `on_return`: today's wanted rule, so every existing plugin keeps its behaviour. `rbs-sources` answers for the built-in types, and the plugin SDK manifest gains the same optional field at its next version. This replaces ADR-0107 decision 7: the binder never tests a type id; "temporal" means "default `run` is not `placed`". A policy value the type does not support is `invalid_params`.

4. **Recommended defaults per type.**

   | Type | `run` default (allowed) | `on_return` default (allowed) | Why |
   | --- | --- | --- | --- |
   | Camera, capture card | `placed` (`placed`, `shown`, `on_air`) | none | Warm-up and exclusive devices: a camera that sleeps is black on the next cut, and another app can take it. Users who want the light off use `shown` with a linger, or Deactivate. |
   | Display, window and screen capture | `placed` (`placed`, `shown`, `on_air`) | none | Same as today. `shown` is offered for heavy captures (two native-rate display captures are a large part of the measured idle cost, perf audit 2026-11); it becomes the recommendation once decision 9 exists. |
   | Audio input, audio output, application audio | `placed` (`placed`, `shown`) | none | Meters and the mixer need them; sleeping a microphone loses the first syllable. |
   | Image | `placed` (`placed`, `shown`) | none | A logo must be on the next frame. `shown` frees the texture of large images (OBS "Unload image when not showing"). |
   | Colour, text (static and live) | `placed` only | text timers: `continue` only | Cost is one texture. Countdown and stopwatch run state is live state and keeps counting (ADR-0129 decision 11). |
   | Media file | `shown` (`placed`, `shown`, `on_air`) | `restart` (`continue`, `resume`, `restart`) | ADR-0107 and ADR-0108 as today. `placed` keeps the decoder ready but paused, for an instant start. `continue` gives OBS's "always play" at no cost while hidden. |
   | Browser | `shown` (`placed`, `shown`, `on_air`) | `continue` (`continue`, `restart`) | ADR-0107 and ADR-0109 as today. `placed` is ADR-0149's `keep_running`, with its network-permission rule and 1 picture per second while hidden. `restart` with `placed` is OBS's "Refresh browser when scene becomes active". |
   | Scroll (an item's layout, any source) | follows its source | `restart` (`continue`, `restart`), saved as `Scroll.on_return` | ADR-0150 decision 5 as today. `continue` keeps the run's anchor while the item is placed on that canvas: a number in a map, no work. |
   | Remote guest (ADR-0139), NDI (plugin) | `placed` (`placed`, `shown`) | none | Reconnecting takes seconds and may need the other side. |

5. **Cost (P13, ADR-0143).** Levels and policies are evaluated on the control side in `sync`, which already walks the shown scenes; no per-frame check is added and inactive nodes stay out of the scheduled graph (rule 10). A source whose policy is its type's default costs exactly what it costs today. `linger_s` 0 adds nothing; above 0 it arms one control-side wake per lingering source on the injected clock, cancelled when the level comes back, never a poll. `continue` stores an anchor (engine timestamp and position), no thread.

6. **Migration.** The next scene-format version maps media `restart_when_shown` true to `on_return: restart` and false to `resume`, and browser `keep_running` true to `run: placed`, then drops both keys. `source.set_settings` keeps accepting the two old keys for one protocol minor and maps them, with a deprecation note in `api.describe`. Behaviour of every existing document is unchanged.

7. **Studio mode and canvases.** The studio-mode preview is Shown, so `run: shown` warms a camera, opens a file (cued, ADR-0108) or starts a page (ADR-0109's slot priority keeps program first) before the Take; the Take then pays nothing. `run: on_air` keeps the preview empty for that source (the layer draws its last picture if it has one) and is meant for costly sources whose preview the operator does not need. A source placed on both canvases has one node and one policy; its level is the highest across canvases.

8. **Control API (next minor).** `source.set_activity {source, run?, on_return?, linger_s?}` (one undo step, `source.changed`, `null` resets to the type default), `item.set_scroll` gains `on_return`. `source.status` reports `activity: off | asleep | lingering | warm | shown | on_air` and `warm_up_ms` (decision 9). `api.describe` lists each type's `ActivityTraits` so the UI, the MCP server and Stream Deck read the same allowed values. The Properties UI is a separate, visual step (rule 16).

9. **Warm-up is measured and can hold a cut (separately acceptable).** The host records each source's last time from Active to first picture (`warm_up_ms`, live state, not saved). A collection setting `transitions.ready_wait_ms` (0..=3000, default 0 = off) lets a Take or scene switch whose incoming scene draws an Active source without a first picture wait up to that long before the transition starts; with every source warm (the usual case) it waits nothing. This is beyond OBS, which cuts to black. The owner may accept decisions 1 to 8 without this one.

## Consequences

- One question, one place: Properties shows "When not on screen" (`run`) and "When it comes back" (`on_return`) for every source, with only the values its type allows. Plugins get the same choice by declaring traits.
- Defaults keep today's behaviour exactly, so this is a refactor plus new choices, not a behaviour change.
- The binder's `running_sources` becomes "every source whose level reaches its `run`, or lingers"; ADR-0107's tests stay and gain policy cases. The media control handle reads `on_return` instead of `restart_when_shown`; the browser supervisor reads `run` instead of `keep_running`.
- Negative: one more top-level `Source` field and a scene-format bump; `continue` for media needs a seek on wake, so a long GOP may show the cued picture for a moment longer than `resume`.
- Specs to update in the implementing PRs: scene model (levels, `running_sources`, `SourceActivity`), scene sources (traits per type, migration), control API, plugin SDK manifest, glossary ("Deactivate", "On air", "Shown").
- Phases: (1) model, traits, binder, migration, API, no UI; (2) Properties UI with dark and light screenshots; (3) linger and `warm_up_ms`; (4) the ready wait (decision 9); (5) media and scroll `continue`. Phases 1 and 3 touch no hot path; phase 4 touches transitions and needs measurements (P15).
- Tests: binder property test that a source is Active exactly when its level reaches `run` or it lingers, for random collections, canvases and policies; Standby holds nothing (existing conformance); linger on `TestClock` without sleeping; migration round trip.

## Alternatives considered

- **Keep adding a checkbox per type (OBS's way).** Cheapest per feature, but the user meets a new word for each type and plugins cannot join.
- **One "visibility behaviour" list (stop, pause, always play).** Mixes when a source runs with what it does on return; it cannot say "keep the page running but reload it on air".
- **Sleep every hidden source, cameras included.** Black cuts and lost exclusive devices; rejected again as in ADR-0107.
- **Let the multiview count as shown.** Repeats OBS's problem of waking every placed page and film.
- **Scripted events per source (on show: reload, seek, run a script).** Too general for a setting; automation can do it later through the control API.

## Review

Independent review per ADR-0014 is pending. This ADR stays Proposed until it is recorded and the owner approves.
