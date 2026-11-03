# ADR-0043: Final ticks are marked, and the video encoder flushes on them

- Status: Proposed
- Date: 2026-11-03
- Deciders: Salih (owner)
- Amends: [ADR-0029](0029-final-ticks-on-drain.md) (closes its "not an encoder end-of-stream flush" consequence)

## Context

ADR-0029 gives every draining partition its final tick(s), so the video encoder submits the last frame render wrote. Hardware encoders (NVENC, AMF, QSV, Media Foundation) are asynchronous: the packet of a frame is ready a frame or more after `submit`. On the final tick the node drains, finds nothing for the last frame yet, and the session is flushed only in `release`, where nothing can pass the packet on. On SC-PC (2026-11-03, PR #96) every desktop-app recording ended with `flush_lost` > 0: the last frame was missing. Encoders with B-frames or look-ahead would lose more.

Nothing tells a node that a tick is its last: `cool` and `release` run after the schedule swap, without outputs, and `release` runs on the control thread. The engine spec and `io-nodes.md` list this as an open question.

Forces: `Node::process` is a hot path (P6, P15); P13 forbids per-iteration checks for inactive features in hot loops; the worker loop must stay as it is on the run path; the node contract (ADR-0020) is public and should change as little as possible; tests use `TestClock` and tick indices, never time (ADR-0032).

## Decision

1. **`Tick` gains `draining: bool`.** `Tick::new` and `Tick::of` set it to `false`; `Tick::draining(self)` returns the tick marked. The worker builds marked ticks only in its drain branch (the one cadence final tick, and every doorbell final tick); the run path builds the same unmarked tick as before. `Tick` is `#[non_exhaustive]`, so adding the field is not a breaking change.
2. **Meaning:** a marked tick is a final tick of a partition that drains (shutdown, or `apply_draining` emptying it). Partitions feeding it have already drained (pipeline order), so for its nodes the streams end with this tick, and the nodes leave the schedule after it (then `cool`, `release`). A live `apply` that removes nodes runs no final tick and marks nothing (unchanged).
3. **The video encoder node** on a marked tick: passes carried packets, submits the frame as usual, then calls `VideoEncoder::flush` instead of `drain`, passes every packet (carrying what the queue cannot take now), counts `EncoderNodeStats::flushes` and submits nothing on later ticks (a doorbell partition may run several). `prepare` clears that state. `release` still flushes; what it gets there (no final tick ran: a live edit or a drain that timed out) and the carry-over are `flush_lost`, as before.
4. **Blocking on the final tick is allowed** for this flush: it waits at most for the encoder to finish frames already submitted, a few milliseconds for hardware encoders. Final ticks are already bounded by `EngineConfig::swap_timeout` (ADR-0029 Decision 5), and no later tick of that partition can be late. This is the only exception to P6 here, and it applies only to marked ticks.
5. **Other nodes ignore the flag.** The audio encoder node does not flush yet: its doorbell partition may run several final ticks while blocks are still queued, and a flush pads a partial frame with silence, so flushing on the first marked tick would put silence in the middle of the tail. It stays an open question in `io-nodes.md` (the tail it drops is under one AAC frame plus the codec's delay).

## Consequences

- The last frame (and any frame held back) reaches the file at stop and at shutdown. Tests: `the_final_tick_flushes_frames_the_encoder_held_back`, `without_a_final_tick_held_frames_are_counted_as_lost` (rbs-nodes, a fake encoder that holds two frames), and the engine tests `shutdown_encodes_the_last_frame_render_wrote` and `a_branch_removed_with_apply_draining_drains_while_render_keeps_running` now check that exactly the final ticks are marked.
- `Tick` grows from 16 to 24 bytes; it is built once per tick per partition and copied out of the context by the nodes that read it. Measured on the cloud container (Intel Xeon 2.8 GHz, 4 vCPU, Linux), `main` vs this change, alternating runs: `executor_tick` 6/20/68 nodes 151/562/1924 ns vs 152/565/2019 ns, 171/667/2350 vs 146/573/2024, 154/553/1917 vs 156/563/1923 (noise in both directions); `cross_thread` doorbell wake p50 59.1 vs 60.4 us, render to output at 60 fps with a quarter-frame offset mean 4471 vs 4495 us. No change beyond noise.
- Hardware run still to do: a desktop-app recording on SC-PC must show `flush_lost = 0` and the last frame in the file.
- Node authors get one documented way to see the end of a stream without a new trait method.

## Alternatives considered

- **A new `Node::end_of_stream` method called on the worker before the swap:** a second call path through the executor and the worker, a breaking trait change for every node and plugin, and the same information as a flag on the tick the node already gets.
- **Flush in `cool`:** it runs after the schedule swap, without outputs to push to.
- **Flush on every tick when the input frame is unchanged:** costs a flush (and a pipeline stall) on every repeated frame; also wrong for live sources that repeat frames.
- **Have the output node wait in `release` for more packets:** the encoder has already been released by then (pipeline order), so the packets no longer exist.

## Review

Independent review per ADR-0014: pending.
