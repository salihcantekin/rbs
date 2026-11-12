# ADR-0107: Temporal sources sleep unless a shown scene draws them

- Status: Proposed
- Date: 2026-11-12
- Deciders: Salih (owner)
- Builds on: [ADR-0013](0013-zero-cost-when-unused.md), [ADR-0039](0039-scene-source-types.md), [ADR-0074](0074-still-image-and-colour-sources.md)

## Context

A capture source that is placed in any scene stays wanted while the user has not deactivated it. Hiding the item, or switching to a scene that does not draw it, leaves the device open. A still stays uploaded while it is scheduled and keeps its CPU pixels for the life of the node (ADR-0074). That is right for a camera and for a logo that must appear on the next frame.

A media file and a browser page are different. A hidden browser that keeps a process, and a hidden film that keeps a decoder, cost CPU, GPU, and memory while nobody sees them. The product vision names this case: a browser source that costs nothing while hidden. The scene model has no class for "produces frames over time, so sleep when not drawn."

A scene-change fade draws a source at opacity 0 while `visible` stays true. Tearing the decoder or the browser process down for that fade would drop the first frames when the picture returns.

## Decision

1. **Capture sources and stills keep today's rule.** A hidden item that is still placed stays wanted. The device stays open, or the still's texture stays while scheduled. This ADR must not change that.
2. **Media and browser sources are temporal.** A temporal source must be Active only while a program or preview scene that is actually shown draws it with `visible` true. A scene that merely contains the item, and is not the shown program or the shown preview, must not keep it Active. `visible` false means Standby, with one exception in decision 4.
3. **Opacity 0 with `visible` true stays Active.** A scene-change fade must not stop the decoder or the browser process.
4. **A timed-show cue that draws the item keeps it Active** for the cue, even when `visible` is false. That cue is the timed-show decision (reserved as ADR-0105) once that ADR is accepted. Until then, no cue exists and decision 2 is the whole rule.
5. **Sleep means no thread and no decoder.** While a temporal source is not Active it must hold no decoder thread, no file reader, no browser process, and no timer (P13). Waking it is the Active transition, not a poll. A failed wake is the node's existing restart and back-off (ADR-0024) and the source's Unavailable state. The binder does not keep a last frame.
6. **Which scenes count.** For each canvas, the shown scenes are its `program` scene and its `preview` scene when that field is set. A temporal source is Active when the union of those scenes, across every canvas in the collection, draws it with `visible` true (opacity does not matter). A hidden placement does not keep it Active. A follower's shown scene is the scene it is showing now, including a followed program. Temporal sources are left out of `warm_sources`. Capture and still text in the scene-model spec stays as it is; the temporal sentence is only: a hidden temporal source is Standby and is not linked.
7. **Classification is the type id**, `org.rbs.source.media` and `org.rbs.source.browser` only. No new node capability. Other types, including plugins, stay on today's wanted rule until a later decision.
8. **No cue code in this implementation.** Decision 4 names a later exception. The first implementation must not take a forced-draw set and must not branch on a cue. There is no per-source "keep warm when hidden" override.

## Consequences

- A film or a page in a scene that is off air costs nothing until that scene is shown.
- Showing it again pays the start cost (open the file, start the process). `restart_when_shown` on a media source (ADR-0108) decides whether playback starts over.
- The implementation must update the scene-model spec (`wanted_sources` / scheduling for this class only) and the scene-sources spec. Capture and still tests must stay green without a new branch in their hot path.
- Negative: a hidden temporal source is not warm. The first frame after show waits on open. That delay is accepted; keeping the process warm while hidden is the cost this ADR refuses.
- ADR-0105, if accepted, is the only extra reason a hidden temporal item stays Active. This ADR does not define the cue.

## Alternatives considered

- **Sleep every hidden source, including cameras.** A camera that reopens on every show stalls and can miss an exclusive device. Capture stays warm.
- **Sleep on opacity 0 as well.** A fade would stop and restart the decoder mid-transition.
- **Keep temporal sources warm whenever they are placed, like captures.** Rejected: a hidden browser would keep a process for every scene that mentions it (P13, and the vision line this ADR records).

## Review

Owner approval: Salih, 2026-11-12 (implement in parallel with ADR-0106, ADR-0108, and ADR-0109).

Reviewer: independent AI session
Date: 2026-11-12
Verdict: changes requested
Concerns:
1. **Binder contract clash** — Today `wanted_sources()` drives which sources are Active, and `warm_sources(canvas)` keeps program-compositor links for every wanted source placed on any scene the canvas may show (including hidden items and scenes that are not the current program). Decision 2 requires temporal sources to be Active only when the *shown* program or preview scene draws them with `visible` true. The ADR does not name a replacement set (for example a per-canvas `active_temporal_sources`) or say whether temporal sources leave `warm_sources`, so implementers must guess how `plan()` and link retention interact with sleeping nodes. -> **open** (specify the function(s) and binder state rules before acceptance).
2. **Scene-model paragraph is still universal** — Consequences require a spec update, but the decision does not restate the corrected rule for `visible_sources` / audio-on-air vs temporal sleep. Capture/still text must stay verbatim; temporal text must not read like “hiding leaves the device open.” Without the exact replacement wording, independent implementers and conformance tests can diverge. -> **open** (paste the intended spec sentences into the ADR or link a spec PR).
3. **“Actually shown” preview** — Studio mode can keep a preview scene selected while the operator watches program output only, or show a preview panel that is not the compositor the ADR means. Decision 2 does not tie Active to a concrete signal (preview compositor scheduled, preview display attached, `Canvas::preview` equals the flattened scene). A browser could stay Active in a stale preview scene. -> **open** (define the predicate in scene-model terms).
4. **Multi-canvas and followed program** — A shared-scene temporal source can be placed once and appear on several canvases; canvas followers (scene-model follow rules) can show another canvas’s program. Nothing states whether temporal Active is union across canvases, per output canvas, or driven only by the canvas that owns the item. One hidden placement on canvas B could keep a decoder alive while canvas A shows the item, violating P13. -> **open** (per-canvas activation rule and follower interaction).
5. **Temporal classification and plugins** — The decision hinges on “media and browser” vs capture/still but does not state the capability or `NodeDescriptor` flag adapters expose (ADR-0039 territory). Third-party sources could be misclassified as capture-warm or temporal-cold with no conformance test named here. -> **open** (declare the capability in the ADR consequence list and idle-cost check scope).
6. **ADR-0105 hook without shape** — Decision 4 reserves Active while a timed-show cue runs with `visible` false, but no stub rule says how flatten/binder will detect that cue before ADR-0105 lands. Implementations may hard-code absence today and break when the cue arrives, or add speculative branches. -> **open** (state “no behaviour until ADR-0105” and forbid cue-specific code paths in the first implementation, or accept ADR-0105 first).
7. **Idle proof (P10 / ADR-0013 rule 7)** — The motivation is idle cost, but the ADR does not cite an idle-cost check (hidden browser in an off-air scene, decoder thread count, process handle). Consequences mention keeping capture tests green, not a new deterministic idle assertion for temporal sleep. -> **open** (name the performance-budgets check or binder test that proves P13 for this class).
8. **Wake failure containment (P7)** — Sleep tears down decoder/browser; wake on scene switch is on the activation path. The ADR accepts latency but not how a failed wake is isolated (last frame, silence, user notification) relative to sources that stayed warm. -> **open** (one sentence aligning with P7 / existing source restart policy).
9. **Simpler per-source policy** — Alternatives reject “warm whenever placed” but not a user-visible “keep running when hidden” on temporal types (opt-in warmth without changing capture defaults). Mandating cold sleep for all media may surprise users who hidden-placement-warm today via `wanted_sources`. -> **open** (owner accepts no override, or ADR adds an explicit non-goal).

Resolutions (author, 2026-11-12), written into decisions 6–8: (1)(2) temporal sources are omitted from `warm_sources`; the spec sentence is decision 6. (3)(4) shown means each canvas's `program` and its `preview` when set; Active is the union, so one visible placement is enough and a hidden one is not. (5) classification is the two type ids only. (6) no cue parameter and no cue branch until ADR-0105. (7) the binder test `a_hidden_temporal_source_is_standby` is the idle check (Standby, not linked). (8) a failed wake uses ADR-0024 and Unavailable. (9) no keep-warm override; the owner already approved mandatory sleep.
