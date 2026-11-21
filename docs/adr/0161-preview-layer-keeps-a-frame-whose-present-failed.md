# ADR-0161: The Windows preview layer keeps a frame whose present failed and presents it again

- Status: Proposed
- Date: 2026-11-21
- Deciders: Salih (owner)
- Supersedes: [ADR-0036](0036-windows-preview-layer.md) decision 7, only the clause "or a failed `Present`" (a failed present counted as a lost presentation manager). The rest of decision 7 stays: replaced frames go back by `IsAvailable`, the lost event is a loss, a lost manager releases every frame.
- Builds on: [ADR-0034](0034-preview-as-native-layer.md), [ADR-0036](0036-windows-preview-layer.md), [ADR-0125](0125-projector-windows.md), [ADR-0013](0013-zero-cost-when-unused.md); stability plan WP-H (Faz 2.6, "the last frame is never lost")

## Context

Every native preview on Windows (Program, Preview, projectors, Interact windows) shows engine frames through `rbs_gpu::share::PreviewLayer`: `SetBuffer` and `Present` on a composition swapchain, on the UI thread.

ADR-0036 decision 7 says a failed `Present` means the presentation manager is lost, and every frame goes back. The code stopped doing that (before 2026-11-21, without an ADR change): with the browser GPU path a busy compositor failed a present now and then, and marking the layer lost froze the Interact window for good. The code now drops the frame whose present failed and keeps the picture already showing.

Dropping is wrong for a page that paints only on change. An Interact window paints on each mouse move; when the mouse stops, the last paint is the last frame the engine sends. If that one present fails, the window shows the state before it until the next change, possibly forever. The stability plan's rule (Faz 2.6) is that a place that drops frames keeps the newest one and tries again, and counts what it could not show. The accepted decision and the code disagree, so either has to change; this ADR changes the decision.

## Decision

1. **A failed present is not a loss.** Only the presentation manager's lost event (checked before every present, zero timeout) is a loss. A failed `SetBuffer` or `Present` must leave the shown picture and every other frame as they are.
2. **The newest frame whose present failed is kept.** The layer holds at most one such frame. `show` of a newer frame replaces it, and the replaced one retires by `IsAvailable` like any replaced frame (its buffer may be the surface's current one).
3. **It is presented again at each `retire`.** `next_retire` returns a time while a frame is kept, so the UI's existing one-shot retire timer (`DEFAULT_RETIRE_POLL`, 4 ms) drives the retries; no new timer or thread. A retry first checks the lost event, then calls `SetBuffer` and `Present` again on the same buffer. A layer attached to another window while a frame is kept retries it there.
4. **Bounded, and no idle poll.** A kept frame whose present still fails one second (`RETRY_FOR`) after its first failure goes to the retiring frames, and so does a kept frame at `detach`. The layer remembers the slot whose `SetBuffer` succeeded last (the surface's current buffer). A retiring frame on that slot may not report `IsAvailable` until another buffer is set, so `next_retire` does not poll for it: it is checked again at the next `show`, as ADR-0036 holds the frame kept at `detach`. Once no other frame waits, the timer stops (P13). A loss drops every frame at once.
5. **Counted.** The layer counts failed presents (`present_failures`) and kept frames never shown because a newer frame replaced them, the retry window ended or the layer was detached (`frames_not_presented`). A frame dropped by a loss is not counted there; `lost()` reports the loss. Showing the counts in diagnostics is a follow-up.
6. **Never waits, never allocates.** `show` and `retire` keep their rules: no wait, and the retiring list's capacity (the ring size) is enough. This relies on ADR-0036's rule that the engine sends at most one frame per surface: the shown, detached, kept and retiring frames are then always different slots.
7. **Testable without Windows.** The rule lives in a platform-independent `PresentRetry` (`rbs-gpu` `share/present_retry.rs`) with unit tests on every platform. The Windows layer calls it.

macOS's layer has no failing present (`setContents` cannot fail), so nothing changes there.

## Consequences

- An Interact window, projector or Program preview whose last present hit a busy compositor shows that frame within a few retries instead of staying on an older one.
- While a frame is kept the layer can hold one surface more than `surfaces_needed` counts (the shown frame, the one it replaced, the kept frame and a kept frame just replaced, plus the engine's frame being written and one in flight: 6 against 5 at 60 fps). The engine then skips a frame (`busy`), never stalls. `ring_needed` is unchanged; if the SC-PC run shows `busy` from this, the ring gets one more surface (at most `MAX_PREVIEW_SURFACES`).
- A frame given up or detached whose buffer is still the surface's current one holds one surface until the next `show`, like the frame kept at `detach` today.
- Retries cost one `SetBuffer` and `Present` every 4 ms while a frame is kept, at most one second per frame. No cost when presents succeed.
- Spec: [gpu-compositor](../specs/gpu-compositor.md) "Lifetime with the layer" and the API block say this. When this ADR is accepted, ADR-0036's status line names the superseded clause.
- The callers are `rbs-app` `preview.rs` (Program and Preview), `projector.rs` and `interact.rs`; each drives `retire` from its one-shot timer and needs no change.
- The layer is UI-side and not on the engine's hot path ([hot paths](../architecture/hot-paths.md)). Its own orchestration (replace, detach and loss while a frame is kept) is not tested off Windows: the SC-PC run with an Interact sweep reads the counts, and a `device-check hardware` test with injected present failures is a follow-up. That run also checks how `IsAvailable` behaves for a buffer that was set but never presented.
- Not covered here: the UI thread's own frame delivery is WP-J's latest-only slot. That includes `entry::invoke` per frame, `try_borrow_mut` in each caller's `show` (which drops a frame on a re-entrant borrow), and each caller's `retire`, which returns without re-arming its timer when the borrow fails. A retry chain cut that way resumes at the next `show`, and the one-second bound then counts from the frame's first failure.

## Alternatives considered

- **Keep dropping the frame.** Loses the last state of a page that paints on change; the defect this ADR fixes.
- **Count a failed present as lost again (ADR-0036 as written).** Froze Interact after the first busy second on SC-PC.
- **Retry inside `show` in a loop.** Waits on the UI thread (P6, UI responsiveness); the compositor clears at its next composition, which a loop does not wait for without sleeping.
- **Retry forever.** A present that fails for a second is not going to succeed for that frame; a poll that never stops costs a timer wake every 4 ms for nothing (P13).
- **Wait for the next engine frame instead of a timer.** A still page sends no next frame; that is the case to fix.

## Review

Round 1: independent AI session (this ADR, ADR-0034, ADR-0036, ADR-0014, the principles, hot-path list, gpu-compositor spec and the code), 2026-11-21. Verdict: changes requested; the decision itself (a failed present is not a loss; keep the newest, bounded retry) is sound. Concerns:
1. A kept frame whose `SetBuffer` succeeded may never report `IsAvailable` after give-up or detach, so the retire timer could poll every 4 ms forever (P13). -> The layer tracks the slot of the last successful `SetBuffer`; `next_retire` does not poll for a retiring frame on it; it is checked at the next `show` (decision 4).
2. The streaming worst case holds one surface more than `surfaces_needed`. -> Stated in Consequences (engine `busy`, never a stall; one more surface if SC-PC shows it).
3. The counters missed the lost-path frame. -> Decision 5 says a loss is reported by `lost()`, not counted.
4. A caller's `retire` that fails its `try_borrow_mut` does not re-arm the timer. -> Named in Consequences as WP-J's; the chain resumes at the next `show`.
5. No layer-level test off Windows; spec omits `preview.rs`; decision 3 should say the retry sets the buffer again; re-attach undocumented. -> Consequences and decision 3 updated; device test is a follow-up.

Still open before acceptance: a confirming independent check of these resolutions, the Windows build and an SC-PC run, and the owner's approval.
