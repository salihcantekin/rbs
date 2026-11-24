# ADR-0183: Background removal draws each picture with its own mask (Exact), the newest picture stays an option (Fast)

- Status: Proposed
- Date: 2026-11-24
- Deciders: Salih (owner)
- Builds on: [ADR-0179](0179-camera-background-removal.md) decision 4 (the render thread uses the latest finished mask and never waits)

## Context

The first Mac test of the background removal filter (2026-11-24, Apple Vision, High) showed a white trail behind a moving hand: the full-size picture is drawn with the newest finished matte, which was made from a picture about two to four frames older (GPU readback, the Vision worker, the upload; plus the CPU smoothing against the matte before it). Where the hand was, the matte still keeps the wall; where the hand is now, the matte cuts it off. A faster level only shortens the trail. ADR-0179 decision 4 chose "newest picture, newest matte" so that the picture is never late; the test shows that is the wrong trade for the filter's main use.

## Decision

1. The filter has a setting *Picture and mask* with two values. **Exact** (default) draws a matte on the picture it was made from, found by timestamp in the node's last four input pictures; the output carries that picture's timestamp, so the picture is late by the mask's time (about 70 to 130 ms at 30 fps) and nothing trails. **Fast** is the behaviour of ADR-0179 decision 4: the newest picture with the newest matte.
2. Exact keeps four references (leases, not copies) to the source's input pictures; no extra GPU memory is allocated and nothing is read back beyond the small picture of ADR-0179 decision 3. A 4:2:0 input that the node decodes itself runs in Fast.
3. Output rate in Exact follows the rate of finished mattes: a picture is published when its matte arrives, and until the next matte the same picture stays (the output slot keeps its frame). A session that is slower than the source lowers the filter's frame rate; it never queues.
4. Exact delays only the picture. The source's sound is not delayed by the filter; keeping them in step uses the source's audio sync offset until a video latency report exists (open question below).
5. Smoothing defaults to 0.25 (was 0.5), because it also trails.

## Consequences

- No trail behind movement in the default setting; a delay of about a tenth of a second on that camera's picture, reported in the setting's help text.
- Four leases on the source's pictures: a capture source with a very small buffer pool could lose frames. Measured on the Mac (see the review record) before the default is kept.
- Open: a general video latency report so that the studio compensates a filter's delay for the source's sound as it does for audio filters.

## Alternatives considered

- **Only lower Smoothing and use a faster level.** Shortens the trail, never removes it.
- **Predict the matte forward with motion estimation.** Complex and wrong on sudden movement.
- **Copy each picture into the node's own textures.** Costs a full-size copy per frame and tens of megabytes; leases are enough.

## Review

Not reviewed yet. Needs an independent review and the owner's approval (rule 9, decision 4 of ADR-0179 is not removed: Fast keeps it).
