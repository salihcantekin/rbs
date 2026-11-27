# ADR-0199: Background removal keeps the camera until its first mask, then cross-fades

- Status: Proposed
- Date: 2026-11-27
- Deciders: Salih (owner)
- Supersedes in part: [ADR-0179](0179-camera-background-removal.md) decision 4, the clause "until the first mask exists the filter publishes nothing"

## Context

ADR-0179 decision 4 lets the background removal filter publish nothing until its first person mask exists, so the unfiltered picture is never shown. The first mask needs at least the readback of a small picture and one inference after the filter joins the schedule: two to four ticks with a warm session (spec `person-mask-port.md`, "Filter"; about 67 ms at 30 fps measured on Fast, ADR-0179 Review). Turning the filter on therefore made the camera vanish for a moment and come back (Salih, Windows, 2026-11-27). Shortening the wait (the session is now opened and warmed in `prepare`, spec rule 2) cannot remove it, and a fade from nothing makes the return more visible, not less (tested on SC-PC 2026-11-27).

The picture shown in those ticks was on screen up to the click, so showing it for a few more ticks reveals nothing new. Later gaps are different: after a new aspect ratio, a lost session or a slow start, the plain picture would be news.

## Decision

1. Only when the user turns the filter on (or adds it) while a shown layer draws its source, before and after that edit, must it publish its input unchanged until its first mask is drawn, for at most 0.5 s of master-clock time from its first tick. The binder reports this edge (`BindReport::turned_on_filters`); the studio sets the node's flag (`CustomNode::turned_on`) before the apply, and the node takes it in `prepare`. A scene switch, an item shown again, a source restart or a restart after a fault is not this edge and shows nothing until the mask (owner, 2026-11-27: the room may be what the user hides). After 0.5 s without a mask, or after a size change, it publishes nothing, as before.
2. The first frame drawn with a mask must start a 0.2 s cross-fade, on master-clock time, from the plain picture to the filtered result (premultiplied mix). The person stays whole; only the background fades out.
3. Every other gap keeps ADR-0179 decision 4 and ADR-0053 decision 6: after the first mask, a new aspect ratio, a lost session or any later missing mask publishes nothing, never the plain picture, and the next mask fades the result in from nothing over 0.2 s.
4. No setting is added. The 0.5 s and 0.2 s are constants of the filter.

## Consequences

- Turning the filter on no longer blanks the layer; the room fades out over 0.2 s.
- On a slow first start (DirectML building its session past the 0.5 s limit), the layer is blank between 0.5 s and the first mask, as before.
- `OrtSession::warm` waits at most 0.5 s (was 1.5 s in the first draft) in `prepare` on the control thread, before the swap: a scene cut that brings in a camera with background removal can start up to 0.5 s late on Windows, and each join loads the model again (`release` closes it, P13), as on `main`, where the load ran on the render thread instead. Keeping the session warm across joins is a possible follow-up.
- Contract changes (rule 11): `BindReport::turned_on_filters` (rbs-scene, additive, `non_exhaustive`) and `CustomNode::turned_on` (rbs-filters).
- `process` publishes a second reference to its input frame in those ticks (no allocation, no copy; the source's lease is held one tick longer by the compositor, as without the filter).
- Spec `person-mask-port.md` rule 3 and the node's tests change with it (`tests/node.rs`: the plain picture is kept, kept 0.5 s at most, cross-fade).

## Alternatives considered

- **Keep hiding (ADR-0179 as is) with a fade from nothing:** tested, still reads as vanish and return.
- **A heavily blurred picture in the gap, then a fade:** hides the room, but adds a blur pass for a few ticks and a second visible change; the room was visible a moment earlier anyway.
- **Keep the old schedule running until the new filter has a mask:** needs a graph runtime change (a node that is scheduled but not yet linked); far larger for the same visible result.

## Review


Reviewer: independent AI session
Date: 2026-11-27
Verdict: changes requested
Concerns:
1. The plain picture is kept on every join, not only when the filter is turned on. The context argues that the picture "was on screen up to the click", but decision 1 also covers "its source starts drawing", and the code resets `Start` in `cool`, so every `prepare` + `warm` shows the plain picture for up to 0.5 s (about 67 ms plus a 0.2 s cross-fade with a warm session). The node rejoins the schedule, with the room not on screen just before, on: a scene switch or an item shown again (a chain that is linked nowhere is out of the plan; the binder sets Standby, ADR-0152 linger); a source that comes back after its linger or a restart; and the ADR-0024 restart after a fault. A lost session returns `NodeError::Backend` every tick, the executor bypasses the node after `failure_limit` (it publishes nothing, which is correct), and the restart then rejoins with a fresh `Start` and shows the plain picture. That contradicts decision 3 ("a lost session ... never the plain picture"). Fix: keep the plain picture only when the filter is inserted into a chain whose output was already drawn on the tick before (the enable edit), for example a flag the binder hands over through the parameter channel or the description, and publish nothing on every other join. Add tests that a rejoin without an enable (cool, then warm) and a restart after a lost session publish nothing and then fade in from nothing. -> open
2. The bounded wait (`OrtSession::warm`, `READY_WAIT` 1.5 s) now runs in `prepare`, and the controller prepares every joining node before the swap (`controller.rs`). The old schedule keeps rendering, so no frame is lost. But a scene cut that brings in a camera with background removal is held up to 1.5 s, and so is any other graph edit in that rebuild. Because `release` closes the session, each rejoin loads the model again and runs two warm-up pictures. The ADR does not mention this cost. State it under Consequences and have the owner accept it, or keep the wait off the swap path: keep the session through the source's linger, or let the node join without waiting. Moving the session out of the executor's `warm` (which must not block) is a correct fix of its own. -> open
3. Hot path (rule 12, P15): `Node::process` changes, and during each 0.2 s fade `Written` changes every tick, so `write_composite` (`queue.write_buffer` plus `blur::write`) runs about 12 times per fade-in, where before it ran only on a settings change. wgpu's `write_buffer` uses internal staging memory. No before/after numbers, `hot-path-review` checklist or human approval is recorded. -> open
4. ADR-0053 decision 6 says that on a size change the filter "must never output the unfiltered input". In the first 0.5 s, before the first mask, a size or aspect change still publishes the plain picture at the new size. Either end the plain picture on a size change, or state the exception and add a note to ADR-0053. -> open
5. Real-time rules in `process`: the plain path publishes `VideoFrame::share` (a reference count, no allocation and no copy). A 4:2:0 input publishes the node's own decoded RGB texture, which the compositor reads in the same tick. No lock or blocking was added. -> resolved
6. Composite: `mix(c * from_plain, result, t)` is premultiplied, and where the matte is 1 it gives `c` throughout, so the person stays whole as decision 2 says. The fade runs on tick time and starts one tick back, as the spec says. -> resolved
7. `wait_ready` and `set_ready`: the wait is bounded with `wait_timeout_while`, a poisoned lock returns false, an open failure wakes the waiter, and warm-up allocations happen on the worker, not the render thread. The new tests in `session.rs` use a 20 s bound only as a hang guard (ADR-0032). -> resolved
8. Spec rule 2 still says that the control thread hands the session over through the parameter channel, while the new note says the node opens it in `prepare`. Both run on the control thread, and P13 holds: the session lives from `prepare` to `release`, which is the Active span. Acceptable as an implementation note; consider rewording the rule so it does not describe two mechanisms. -> resolved (wording optional)
9. Tests cover keeping the plain picture, the 0.5 s cap and both cross-fades. Missing: an aspect change after the first mask (nothing, then a fade in from nothing), a lost session, and a rejoin (see 1). -> open

Author's changes after round 1 (2026-11-27), for the reviewer to check:
1. Plain picture only on a turn-on edge: binder `turned_on_filters` (source drawn before and after the sync), studio sets `CustomNode::turned_on` before the apply, node takes it in `prepare`; every other join (scene switch, linger return, fault restart) publishes nothing. Tests: `tests/binder/turned_on.rs` (turn-on listed; scene switch and turn-on while not drawn not listed), `tests/node.rs` `a_join_that_is_not_a_turn_on_shows_nothing_until_the_first_matte`. Owner chose this on 2026-11-27.
2. `READY_WAIT` lowered to 0.5 s; cost stated under Consequences for the owner.
3. Hot-path note: `/mnt/project-files/bg-removal-flicker/hot-path-review.md`; numbers on SC-PC pending, with Salih's approval.
4. A size change ends the plain picture (`a_size_change_before_the_first_matte_ends_the_plain_picture`).
9. Rejoin covered (see 1); a lost-session restart is a rejoin without the flag.
