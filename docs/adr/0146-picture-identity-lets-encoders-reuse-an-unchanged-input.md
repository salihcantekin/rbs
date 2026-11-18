# ADR-0146: Picture identity on video frames, so encoders reuse an unchanged input instead of copying it

- Status: Proposed
- Date: 2026-11-18
- Deciders: Salih (owner)
- Builds on: [ADR-0004](0004-single-master-clock.md), [ADR-0025](0025-encoder-surface-export.md), [ADR-0031](0031-adapter-support-crates.md), [ADR-0038](0038-io-graph-nodes.md), [ADR-0093](0093-vertical-second-canvas-from-the-same-sources.md), [ADR-0117](0117-still-sources-as-images.md), [ADR-0143](0143-performance-rules-for-every-change.md) (R5)
- Spec: [media-types](../specs/media-types.md), [output-ports](../specs/output-ports.md), [gpu-compositor](../specs/gpu-compositor.md), [io-nodes](../specs/io-nodes.md)

## Context

Render on change (R-03, PR #534) stops drawing, converting and reading back a canvas that did not change. An encoder of a constant frame rate output still needs a frame every tick, so the export sends a *repeat*: `YuvDownload::submit_repeat` hands the previous frame's `SystemFrame` again, with the tick's timestamp and a new `sequence`. The bytes are shared (`Bytes` clone), no GPU work and no readback happen.

The encoder does not know that the picture is the same. Every video encoder session copies the whole frame into its own input buffer on every submit:

- `rbs-codec-hw` `Session::submit` calls `Engine::upload` (NVENC, QSV, VA-API: copy into the slot's input buffer; AMF: copy into a newly allocated host surface).
- VideoToolbox `copy_system` copies into a new pool `CVPixelBuffer`.

At 1080p NV12 that is 3.1 MB per frame, about 187 MB/s per encoder at 60 fps, on the encoder thread, for a picture the encoder already holds. Record and stream are two encoders; a vertical canvas (ADR-0093, 1080x1920) adds one more per output. The SC-PC static scene A/B after R-03 (1080p60, image and text) still used 6.2 % of one core on the RTX 4090 path; the input copy is the largest per-frame CPU cost left in it.

`VideoFrame::sequence` cannot carry this: the spec defines it as a per-producer counter whose gaps show drops, and a repeat must take a new value so stamps never go back. Comparing `Bytes` pointers is not safe: a freed buffer's address can be reused for a different picture.

## Decision

1. **`VideoFrame` gets a picture identity.** `rbs-core` adds `PictureId` (a `u64` newtype; `PictureId::NONE` is 0) and `VideoFrame::picture() -> PictureId` with `VideoFrame::with_picture(PictureId)`. Contract: two frames with the same non-`NONE` picture id have identical pixels and the same `VideoFormat`. `NONE` means "unknown" and promises nothing. New ids come from `next_generation()`, so they are unique in the process and never 0. Every existing constructor sets `NONE`; `share()`, `with_timestamp()` and `with_sequence()` keep it. Plugin and capture frames keep `NONE` unless a producer opts in. This is additive: no existing field changes meaning.
2. **Who sets it.** `YuvDownload` gives each frame it reads back a new id, and a repeat entry delivers the previous frame with that frame's id. A shared readback (R-01, PR #540, once merged) gives every export that joins it the same id, because the bytes are the same. The id is attached where the bytes are created and never moved to other bytes. A failed readback clears the last frame (as today), so no later repeat can carry a stale id. Producers other than `YuvDownload` may set ids later under the same contract; none is required to.
3. **Encoders may reuse, never must.** An encoder that sees a frame whose id is not `NONE` and equals the id of a picture it still holds unchanged in an input buffer it may use may encode from that buffer without copying. It must still encode a picture for that frame, with the frame's own timestamp, so the output stays constant frame rate and nothing downstream changes (mux, transports, keyframe requests, rate control).
4. **`rbs-codec-hw`.** `Session` remembers, per slot, the id of the picture last uploaded into it. `submit` skips `Engine::upload` when the frame's id is not `NONE` and equals that slot's id, and counts it in a new `SessionStats::reused` (not in `copied`). The slot id is cleared when an upload fails, on `flush`, on a reconfigure that changes the input, and whenever the engine reports an error. Skipping is allowed only for an engine whose `Engine::keeps_input()` returns `true` (new trait method, default `false`): the engine promises that a slot's input buffer still holds the uploaded pixels after its picture was encoded and read. With N slots, a static picture is copied at most N times and then never again.
   - NVENC, QSV and VA-API return `true` after a hardware test shows that an encode from a reused slot gives the same picture as a copied one.
   - AMF returns `false`: `SubmitInput` converts the host surface in place, so a submitted surface is never read again (output-ports spec). Reusing AMF input is a later, separate change.
5. **VideoToolbox.** The session keeps the `CVPixelBuffer` of the last system-memory copy together with its id and submits that same buffer again for a frame with the same id. VideoToolbox only reads input buffers, and the retained buffer leaves the pool only after its last release. The kept buffer is dropped on `flush`, on reconfigure and on any error. Native IOSurface frames are unchanged.
6. **The vertical canvas gets the same result.** A further canvas (ADR-0093) uses the same compositor node, the same export node and the same encoder sessions, so decisions 2 to 5 cover it without code of its own.
7. **Zero cost when not used, real-time rules.** The check is one integer compare per submitted frame on the encoder thread, which runs anyway; no allocation, no lock, no new thread (P6, P13). A frame with `NONE` takes today's path exactly.
8. **Measured before it merges (P15).** The encoder hand-off is a hot path. The implementing PR shows: a bench of `Session::submit` with repeated and fresh frames (fake engine); on SC-PC the R-03 static-scene A/B (1080p60, record + stream, then with a vertical canvas) for CPU per thread and `copied` / `reused` counts, with `RBS_ENCODER_COPY_ALWAYS=1` as the A/B switch for this change only; a short Mac run for VideoToolbox. It fills the `hot-path-review` checklist and needs Salih's sign-off.

## Consequences

- A static picture costs no CPU copy anywhere on the default path after R-03: no draw, no conversion, no readback, and now no encoder input copy. A half-static scene gains in proportion to its unchanged ticks.
- `rbs-core`'s `VideoFrame` grows by 8 bytes. `media-types.md` documents `picture()` beside `sequence()` and the identity contract; the port conformance suite checks that a frame with a non-`NONE` id never carries different bytes under that id.
- `output-ports.md` (`rbs-codec-hw` sessions, VideoToolbox) describes the reuse rule, `keeps_input`, `reused` and the invalidation points; `gpu-compositor.md` (System-memory download) says how `YuvDownload` assigns ids. These spec edits land with the implementation.
- New risk: an engine that claims `keeps_input` wrongly would encode a stale picture. Mitigations: opt-in per engine, a hardware test per engine, invalidation on every error path, and the A/B switch.
- The encoder still encodes every frame (GPU encoder time is unchanged, the bitstream of a static picture is already small). Not encoding repeats at all would need a variable frame rate output; see Alternatives.
- Follow-ups outside this ADR: virtual camera sender skipping its ring copy for a repeated id; AMF device-surface reuse; true zero copy (E-03 VideoToolbox, E-05 NVENC), which removes the copy for changing pictures too and makes this ADR matter only for the remaining system-memory paths.

## Alternatives considered

- **Reuse `sequence`.** It would change the meaning of an accepted field that the spec ties to drop detection, and repeats need new values so stamps never go back.
- **Compare `Bytes` pointers and lengths in the encoder.** No contract change, but an address can be reused by a new picture after the old buffer is freed (the buffers are pooled), which would encode a stale picture silently.
- **Hash the frame.** Hashing 3 MB costs about as much as copying it.
- **Do not send repeats; let the muxer or player hold the picture (variable frame rate).** Saves encoder GPU time too, but RTMP and many players and editors expect constant frame rate, and keyframe intervals and rate control are defined in frames. Could be a later option for recordings only.
- **One shared input buffer submitted for several frames in flight.** Removes even the first N copies, but some drivers write to or lock the input while encoding; the per-slot rule needs no such guarantee.

## Review

Pending: independent review per ADR-0014 (`architecture-review` skill).
