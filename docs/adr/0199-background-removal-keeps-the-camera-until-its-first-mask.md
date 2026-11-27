# ADR-0199: Background removal keeps the camera until its first mask, then cross-fades

- Status: Proposed
- Date: 2026-11-27
- Deciders: Salih (owner)
- Supersedes in part: [ADR-0179](0179-camera-background-removal.md) decision 4, the clause "until the first mask exists the filter publishes nothing"

## Context

ADR-0179 decision 4 lets the background removal filter publish nothing until its first person mask exists, so the unfiltered picture is never shown. The first mask needs at least the readback of a small picture and one inference after the filter joins the schedule: two to four ticks with a warm session (spec `person-mask-port.md`, "Filter"; about 67 ms at 30 fps measured on Fast, ADR-0179 Review). Turning the filter on therefore made the camera vanish for a moment and come back (Salih, Windows, 2026-11-27). Shortening the wait (the session is now opened and warmed in `prepare`, spec rule 2) cannot remove it, and a fade from nothing makes the return more visible, not less (tested on SC-PC 2026-11-27).

The picture shown in those ticks was on screen up to the click, so showing it for a few more ticks reveals nothing new. Later gaps are different: after a new aspect ratio, a lost session or a slow start, the plain picture would be news.

## Decision

1. When the filter joins the schedule (turned on, or its source starts drawing), it must publish its input unchanged until its first mask is drawn, for at most 0.5 s of master-clock time from its first tick. After 0.5 s without a mask it publishes nothing, as before.
2. The first frame drawn with a mask must start a 0.2 s cross-fade, on master-clock time, from the plain picture to the filtered result (premultiplied mix). The person stays whole; only the background fades out.
3. Every other gap keeps ADR-0179 decision 4 and ADR-0053 decision 6: after the first mask, a new aspect ratio, a lost session or any later missing mask publishes nothing, never the plain picture, and the next mask fades the result in from nothing over 0.2 s.
4. No setting is added. The 0.5 s and 0.2 s are constants of the filter.

## Consequences

- Turning the filter on no longer blanks the layer; the room fades out over 0.2 s.
- On a slow first start (DirectML building its session past the 0.5 s limit), the layer is blank between 0.5 s and the first mask, as before.
- `process` publishes a second reference to its input frame in those ticks (no allocation, no copy; the source's lease is held one tick longer by the compositor, as without the filter).
- Spec `person-mask-port.md` rule 3 and the node's tests change with it (`tests/node.rs`: the plain picture is kept, kept 0.5 s at most, cross-fade).

## Alternatives considered

- **Keep hiding (ADR-0179 as is) with a fade from nothing:** tested, still reads as vanish and return.
- **A heavily blurred picture in the gap, then a fade:** hides the room, but adds a blur pass for a few ticks and a second visible change; the room was visible a moment earlier anyway.
- **Keep the old schedule running until the new filter has a mask:** needs a graph runtime change (a node that is scheduled but not yet linked); far larger for the same visible result.

## Review

