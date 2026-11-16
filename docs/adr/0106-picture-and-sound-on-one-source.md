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

1. **A source with picture and sound is two nodes fed by one producer.** The picture node runs in the render partition and has video outputs only. The sound node runs in the audio partition and has one audio output. Both belong to one `SourceId` and one source instance. They share one producer (a decode worker, or a browser process) through two lock-free rings that the instance owns: a picture ring read by the picture node and a sound ring read by the sound node. Sound must not cross from the render partition into the audio partition on a partition queue. The render partition must not write audio. The producer must never wait on the sound ring: it paces itself by master time (ADR-0108 decision 5 for a file), and when the sound ring is full it drops the new block and counts it. So a sound node that is not scheduled (its mixer has no sink, or the audio partition stalls) never stops the picture.
2. **The sound node is the strip producer.** The binder allocates a second node id for the sound node of a picture-and-sound type, puts it in the audio partition, and makes it the producer of that source's mixer input in every mixer that hears the source: each canvas's program mixer and each hold mixer (ADR-0124). One sound node feeds them all. When the binder has no audio partition (`with_audio` not set), no sound node is created and the producer pushes no sound. `BindReport::picture_sound` lists `(source, picture node, sound node)`, and a replaced source appears in `replaced_sources` (`(source, old, new)`) once for its picture node and once for its sound node. The scene binder learns the pair from `NodeTypes`: the trait gains a method with a default of `None` that names the sound node type of a picture node type. `rbs-sources` implements it from its type registry; `NodeDescriptor` and the plugin SDK do not change. Reader ids and `assign_queue_readers` are not used for this path. Strips stay keyed by `SourceId`.
3. **Sound runs on the audio block cadence.** The sound node writes exactly one block of `AudioGraphConfig.block_frames` per audio tick, at the graph's format. The producer resamples and remaps channels before it pushes to the sound ring, so the sound node only copies. The producer cuts blocks on the audio graph's block grid (`block_time(tick)`, audio-graph invariant 1). The sound node writes the block whose stamp is within half a block of the tick's `block_time`, drops and counts every older block in the ring in that tick (a loop bounded by the ring size), and writes silence for that tick when the next block is later. On underrun it does not wait. A producer whose sound arrives on its own clock (a browser process) applies the audio input's time map and steering (audio-graph spec) before it pushes; a file worker defines its own time and stamps by construction. A source with no sound writes silence and does not count it as an underrun. A render stall does not stop sound.
4. **One master clock, and lip sync is part of the contract.** The producer stamps every picture and every sound block with master time (ADR-0004). The picture node publishes the picture whose stamp is due at the render tick. The sound node writes the block whose stamp matches the audio tick's block time, so both reach the program at the same master time. The audio path's own fixed latency (mixer and output) is the latency every audio input already has and is not added again. This path does not add a second clock. A TestClock check must show that a stamped picture and its sound reach the program compositor and the program mixer at the same master time, within one audio block.
5. **Node types.** The picture node uses the type's own descriptor (for example `org.rbs.media`, video output only). The sound node uses a second fixed descriptor of the same crate (for example `org.rbs.media.sound`, one audio output). Both are in `node_types()` and every instance uses them (scene-sources invariant 2). Whenever the binder has an audio partition, the pair is fixed for the life of the source, including a file or page with no sound: its sound node writes silence. Without an audio partition the instance has a picture node only (decision 2).
6. **Capture types stay video-only on their capture node.** A camera microphone must remain the ADR-0096 companion. This decision must not fold a device's microphone into the camera node.
7. **Until this ADR is implemented, `AudioOnVideoSource` remains the behaviour.** A media or browser source must not ship as a silent picture whose sound is added later by changing the strip's producer. Sound and picture of such a source land together, with the sound node as the producer from the first build that mixes them.
8. **No audio filters on this path in this ADR.** Filters on that sound are a later decision. `NoVideoPins` stays.
9. **Health and cost.** The sound node is Active exactly when the picture node is, by the same rule (ADR-0107 for temporal types). Standby holds no ring. A quiet producer is a silent strip, with no new `BindProblem`. Source health stays on the source. No scene-format change. `BindReport` stays `#[non_exhaustive]`. The producer-to-ring path and both nodes' `process` take `hot-path-review` when they land.

## Consequences

- A file or a page is one source in the mixer, heard on each canvas whose program scene (or bound hold scene) shows it, and not on another canvas.
- The binder learns that one source type can have two nodes in two partitions. `rbs-scene` still depends on no adapter: it reads the pair from `NodeTypeTable`.
- The existing crossing code for this path (`picture_sound` with queue readers, `assign_queue_readers`) is replaced by the sound-node producer. No saved show refers to a reader id, so nothing in a document changes.
- The implementation must rewrite, not only trim: the scene-sources open question (the queue-reader line becomes the sound node, and the "(cameras with microphones)" parenthetical goes), the scene-model Wiring rule (a picture node has video outputs; its sound node is the mixer producer), the hold-scene paragraph (no readers in `hold_mixers`), and the audio-graph spec (the sound node and its block rule). No scene-format number is claimed here.
- A test shows the picture keeps playing while the sound node is not scheduled.
- `AudioOnVideoSource` stays in the tree until that implementation replaces it. Callers must still handle the problem.

## Alternatives considered

- **Keep the source silent and add sound in a later release.** Rejected: the strip's producer would change under saved shows (decision 7).
- **One node in render whose audio pin crosses on a partition queue, with the queue reader as the producer** (this ADR's first text). Rejected after review round 2: the render partition writes on the render cadence, not the audio block cadence, so sound gaps or overflows, a render stall becomes an audio gap, and lip sync depends on queue depth. It also needs reader ids from lowering in the binder.
- **Put the whole source in the audio partition and queue video back to render.** Moves every picture across a queue and adds a copy the compositor does not need.
- **Decode the file twice, once as a video source and once as an audio source.** Two file readers and two clocks to keep together.
- **Treat a camera microphone as this path.** ADR-0096 already gives it a companion.

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

Review round 2 (independent AI session, 2026-11-16, `/mnt/project-files/media-playback/review-round2.md`): changes requested, concerns 1–4.

Resolutions (author, 2026-11-16), written into decisions 1–9:

1. Render cadence vs block cadence: the source is two nodes, a picture node in render and a sound node in the audio partition, fed by one producer through two rings (decisions 1–3). No crossing; the sound node writes one block per audio tick.
2. Lip sync: every picture and block is stamped with master time by the producer; a TestClock check covers program compositor and program mixer within one block (decision 4).
3. Hold mixer: the sound node is the producer in every mixer that hears the source, program and hold (decision 2).
4. Spec text: the scene-sources parenthetical is dropped at implementation (Consequences).

Review round 3 (independent AI session, 2026-11-16, `/mnt/project-files/media-playback/review-round3.md`): changes requested, B1, B2, M1, M2.

Resolutions (author, 2026-11-16):

- B1: the producer never waits on the sound ring; it paces by master time and drops and counts when the ring is full; no audio partition means no sound node (decisions 1, 2; test in Consequences).
- B2: a producer on its own clock applies the audio input's time map and steering (decision 3).
- M1: blocks are cut on the `block_time` grid; the half-block rule (decision 3).
- M2: the spec lines to rewrite are listed; `NodeTypes` names the sound node type with a default; `replaced_sources` reports both nodes (decision 2, Consequences).

Review round 4 (independent AI session, 2026-11-16, `/mnt/project-files/media-playback/review-round4.md`): accept with minor fixes M1–M3, conditional on ADR-0108 B1 and B2.

Resolutions (author, 2026-11-16): M1 the pair is fixed whenever there is an audio partition (decision 5); M2 every stale block is dropped in one tick, and the first block is padded to the anchor (decision 3, ADR-0108 decision 5); M3 two `replaced_sources` entries per source (decision 2). ADR-0108 B1 and B2 are resolved in that ADR's decision 5.
