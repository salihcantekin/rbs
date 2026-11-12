# ADR-0106: A source that has both picture and sound names the queue reader as the mixer producer

- Status: Proposed
- Date: 2026-11-12
- Deciders: Salih (owner)
- Builds on: [ADR-0004](0004-single-master-clock.md), [ADR-0026](0026-partitions-and-queues.md), [ADR-0037](0037-audio-graph.md), [ADR-0039](0039-scene-source-types.md), [ADR-0096](0096-capture-audio-companions-and-camera-microphone.md)

## Context

ADR-0039 leaves audio of a source that also has video undecided. The binder reports `AudioOnVideoSource` and mixes nothing for that source ([crates/rbs-scene/src/binder.rs](../../crates/rbs-scene/src/binder.rs)). The video node stays in the render partition. Its samples would have to cross into the audio partition, and the mixer would see the partition-queue reader (ADR-0026) as the producer. The binder cannot name that reader today, so it cannot build a strip that matches the sound.

A media file and a browser page both need this path. Shipping either of them silent, and later attaching a different producer, would change what a strip refers to after people have saved shows. Capture types do not: a camera's microphone is a companion source (ADR-0096), not a second output of the camera node.

Forces: one master clock (ADR-0004); the render thread must not wait on audio (P6); the mixer of one canvas hears only that canvas (ADR-0039); a strip without a matching producer is silent (ADR-0037).

## Decision

1. **One node may have a video output and an audio output.** Video stays in the render partition. Audio crosses into the audio partition on the existing partition queue (ADR-0026). The scene binder must record the reader endpoint's node id on `BindReport`. The mixer strip's producer for that source must be that reader, in the mixer's partition. The video node must not be the strip producer.
2. **Capture types stay video-only on their capture node.** A camera microphone must remain the ADR-0096 companion. This decision must not fold a device's microphone into the camera node.
3. **Timestamps use the master clock** already used by audio inputs. This decision must not add a second clock. Drift correction for this path is the same correction an audio input already applies.
4. **Until this ADR is implemented, `AudioOnVideoSource` remains the behaviour.** A media or browser source must not ship as a silent picture whose sound is added later by changing the strip's producer. Sound and picture of such a source land together, with the reader named from the first build that mixes them.
5. **Reader ids belong to `Lowering` only.** `sync` writes the logical audio link and lists `(canvas, source, link)` on `BindReport`. The composition root keeps one `Lowering`, calls `lower` after `sync`, then fills each mixer input's producer with that crossing's `reader`. The binder must not allocate reserved ids. Strips stay keyed by `SourceId`. A settings change (`replaced_sources`) rebuilds every canvas strip that heard the source, after the next `lower`.
6. **One reader per canvas.** Each canvas whose program scene hears the source has its own crossing. That canvas's mixer producer is that crossing's reader.
7. **No audio filters on this path in this ADR.** The producer is the queue reader of the source node's own audio pin. `NoVideoPins` stays. Filters on that sound are a later decision.
8. **A quiet crossing is a silent strip.** Dropped blocks stay `QueueStats`. No new `BindProblem`. Source health stays the source node's fault. Endpoint liveness follows ADR-0026 decision 3: the queue is Active only while its logical link is in the plan. Clients never see reader ids. No scene-format change. `BindReport` stays `#[non_exhaustive]`. The decode-to-writer path, when it exists, needs `hot-path-review`. The binder does not.

## Consequences

- A file or a page can be one source in the mixer, heard on the canvas whose program scene shows it, and not on another canvas.
- `BindReport` grows a name for the reader. The report and `BindProblem` stay `#[non_exhaustive]` (ADR-0039 decision 7).
- The implementation must update the scene-sources spec (the open question this ADR answers), the scene-model binder section, the audio-graph spec, and the control API only if a client can see the new producer. No scene-format number is claimed here.
- Negative: lowering must be visible to the binder, which today stops at the logical graph. The implementation has to expose reserved reader ids without letting `rbs-scene` depend on an adapter.
- `AudioOnVideoSource` stays in the tree until that implementation replaces it. Callers must still handle the problem.

## Alternatives considered

- **Keep the source silent and add sound in a later release.** Rejected: the strip's producer would change under saved shows (decision 4).
- **Put the whole source in the audio partition and queue video back to render.** The render path is the one that already composites; moving every picture across a queue adds a copy the compositor does not need.
- **Decode the file twice, once as a video source and once as an audio source.** Two clocks, two file readers, and a drift the master clock is there to avoid.
- **Treat a camera microphone as this path.** ADR-0096 already gives it a companion. Folding it in would reopen that decision and wake a second device inside the camera node.

## Review

Owner approval: Salih, 2026-11-12 (implement in parallel with ADR-0107, ADR-0108, and ADR-0109).

Reviewer: independent AI session
Date: 2026-11-12
Verdict: changes requested
Concerns:

1. **BindReport vs lowering lifecycle.** Decision 1 requires the scene binder to put the partition queue reader's `NodeId` on `BindReport`, but readers exist only after `rbs_graph::partition::Lowering::lower` splits cross-partition links (`partition.rs` allocates reserved ids keyed by `(Link, from, to)`). Today `sync` emits only a logical graph and `BoundAudio::producer` is documented as the chain tail in the mixer's partition (`binder.rs`). The ADR names the gap in Consequences but does not decide who reserves the id, when, or how strip rebuild stays stable across `replaced_sources`. Without a spelled contract (e.g. composition root runs a shared `Lowering` after bind and merges `Crossing::reader` into mixer inputs, or `rbs-graph` exposes deterministic reservation the binder calls without duplicating offset state), two implementations can disagree and saved strips can point at the wrong node. -> Add an explicit decision on bind → lower → strip ordering and the single source of truth for reader ids; spec it in scene-model audio binding.
2. **Audio filter chains (ADR-0082) on video+audio sources.** The decision fixes the mixer producer as the queue reader, but scene-model still forbids audio filters on sources with video (`NoVideoPins`) and places all audio-filter nodes in the source's partition. For a file or browser source, filters on the sound are a product expectation; the reader is only the raw crossing tail unless filters also cross partitions or live in the audio partition ahead of the mixer. -> State whether v1 ships reader-only (no audio filters on these types), or extend the decision with partition placement and producer = tail after any in-partition audio chain.
3. **One source, several mixers.** The same program-visible source on multiple canvases implies multiple logical audio links and thus multiple reader endpoints (`Lowering` issues one queue per crossing). The ADR does not say each canvas's `BoundAudio.producer` is that crossing's reader, nor how `replaced_sources` updates every strip when settings change. -> Document multi-canvas behaviour and require a binder/lowering test mirroring the random suite for audio-only sources.
4. **Observability and failure (P7).** Queue readers are ordinary nodes; a stalled decode in render can starve the audio partition without a contained `BindProblem`. Decision 3 defers to existing audio-input drift correction but does not say what the user sees when the crossing drops blocks or the reader never gets samples. -> Tie failure to existing source health / strip silence rules or name new report fields; otherwise ops cannot distinguish "silent strip" from "broken crossing".
5. **Hot-path and idle cost (P10, P13, P15).** Crossing audio adds writer work on render cadence and reader work at block cadence (ADR-0037). The ADR does not require idle behaviour when the source is Standby but still warm-linked for video (`warm_sources` vs mixer `visible_sources` asymmetry is intentional for video but must not leave queue endpoints Active without audio consumers). -> Reference ADR-0037 queue costs and state that endpoint liveness follows ADR-0026 decision 3 tied to the logical link's plan bit; flag implementation for `hot-path-review` when touch decode → writer paths.
6. **Spec and semver surface.** Consequences list spec updates but not whether `BoundAudio` / control API clients see logical source node vs reader (strip identity stays `SourceId` per ADR-0039). -> Clarify control API and scene-format impact in the implementation PR; keep `#[non_exhaustive]` but document semver.
7. **Open-questions wording.** `scene-sources.md` open question still groups "cameras with microphones" with video+audio sources; decision 2 excludes capture nodes. -> Tighten that bullet so implementers do not route camera mic through this ADR.

Resolutions (author, 2026-11-12), written into decisions 5–8: (1) `Lowering` owns reader ids; the host fills producers after `lower`. (2) No audio filters on this path. (3) One reader per canvas. (4) A quiet crossing is a silent strip; health stays on the source node. (5) Endpoint liveness follows ADR-0026 decision 3; hot-path review applies to the decode-to-writer path only. (6) Strips stay `SourceId`; no scene-format change. (7) Camera microphones stay ADR-0096.
