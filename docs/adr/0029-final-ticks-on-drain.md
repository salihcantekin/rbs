# ADR-0029: Final ticks on drain

- Status: Proposed
- Date: 2026-11-03
- Deciders: Salih (owner)
- Amends: [ADR-0027](0027-engine-loop.md) Decision 7 (drains swap between ticks) and Decision 3 (the worker reads a stop flag).

## Context

ADR-0027 Decision 7 drains each partition at shutdown by sending it an empty schedule and waiting for the swap. A worker swaps between ticks, so:

- a cadence partition is drained without another tick: the encoder never reads the frame that render's last tick wrote, so that frame is missing from every recording or stream;
- an output's `PacketReader` passes at most `PER_TICK_EVENTS` (16) packets per tick and keeps the rest for the next tick, which never comes: up to a queue's worth of packets (256 by default) queued behind a slow output is dropped at release (`resync_drops`).

Forces: shutdown runs in pipeline order and every producer partition is fully drained before its consumers (ADR-0026 Decision 8); nodes may only run on their worker thread (`process` runs there, and a thread-bound runtime can only rely on that thread); the worker loop is a hot path (P6, P15) and must not gain per-iteration checks for an inactive feature (P13); tests use `TestClock`, which only moves when the test moves it.

## Decision

1. **Before the empty schedule is sent**, `Engine::shutdown` asks each running partition's worker, in pipeline order, for its final ticks and waits up to `EngineConfig::swap_timeout` for them. They run on the worker's own thread, on the schedule it already runs.
2. **Cadence worker:** exactly one final tick, at once, with the next free index (or the newest due one if the worker is late). Its time may be up to one cadence period ahead of the clock. It must not wait for the tick's time: an engine on a stopped clock (tests, a paused master clock) must still shut down.
3. **Doorbell worker:** one final tick, then one more for each ring left by a reader that still had packets, at most `MAX_DRAIN_TICKS` (4096) ticks. Because upstream partitions drained first, nothing else rings, and every packet in the queue when the output drains is passed on (packets a writer already dropped on overflow stay counted in `overflows`). In a cycle of partitions (ordered by id, ADR-0026) a producer may still run while its consumer drains; there the drain is best effort, bounded by `MAX_DRAIN_TICKS` and the timeout.
4. **After its final ticks** a worker runs no further tick but still swaps schedules on wake-up, so the empty schedule is swapped and its nodes cooled on the worker thread, as before.
5. **Final ticks that miss the timeout** are reported as `EngineError::DrainTimeout(partition)`. The worker is then joined (which waits for a node hung in `process`, as ADR-0027 already accepts) and the empty schedule is swapped on the control thread, so a hung partition costs one timeout plus the hang, not three timeouts. A worker thread that already died is not waited for.
6. **The worker's stop flag becomes a mode** (`run`, `drain`, `held`, `stop`) in one `AtomicU8`: still one atomic load per loop iteration on a cadence worker and two (before and after the wait) on a doorbell worker, as before. Changing `drain` to `held` and `run` to `drain` uses compare-exchange, so a stop is never overwritten. The worker reports the end of its final ticks through a one-slot channel created when the worker starts.

## Consequences

- The last frame render wrote is encoded and every queued packet reaches the output before release (tests `shutdown_encodes_the_last_frame_render_wrote`, `shutdown_passes_every_queued_packet_not_just_one_ticks_worth`, `a_drain_that_times_out_is_reported_and_still_releases_everything`; allocation guards for the run, park, drain and held paths in `worker.rs`).
- Render runs one more tick at shutdown than the clock asked for; its frame carries a timestamp on the cadence, up to one period ahead. Encoders see it as an ordinary last frame.
- Shutdown takes at most one extra tick per partition, plus the time a stalled output needs to pass its backlog (at most `swap_timeout`; past it, `DrainTimeout` and the join).
- **Not an encoder end-of-stream flush.** One more tick gives an encoder the last frame; an encoder that holds frames back (B-frames, lookahead) still needs an explicit flush at end of stream, which neither a tick nor `cool`/`release` can deliver. That is a separate, open question in the spec.
- Measured (`cargo bench -p rbs-engine --bench cross_thread`, main vs this change, same machine, back to back): doorbell hand-off p50 35.6 vs 31.2 us; render to output at 60 fps with a quarter-frame offset, mean 4347 vs 4343 us. No difference beyond noise; the loop does the same work per iteration.
- Graph edits that empty a partition while running are unchanged: they swap between ticks without a final tick (a live edit is not end-of-stream).
- Spec `docs/specs/engine.md` gains invariant 8a and the `DrainTimeout` error; the open question on final ticks is closed.

## Alternatives considered

- **Run the final ticks on the control thread** after joining the worker: simpler signalling, but `process` would run off its worker thread, breaking thread-bound runtimes and the real-time class of the thread.
- **Send the empty schedule first and tick before the swap:** `run_tick` swaps right after the first tick, so a doorbell worker could not pass more than one tick's packets.
- **Wait for the final cadence tick's time:** keeps ticks on the clock, but a stopped or test clock never reaches it and shutdown would hang until the timeout.
- **A separate drain flag:** a second atomic load per iteration; one mode value costs the same as the old stop flag.

## Review

Reviewer: independent AI session (given only the ADR, the diff, principles, specs and code)
Date: 2026-11-03
Verdict: changes requested, then resolved as below. Mode state machine, wake-ups, ring handling, shutdown/drop hang analysis and hot-loop cost were checked and held.
Concerns:
1. No before/after numbers for the hot path (P15) -> numbers added to Consequences and the PR. Early-phase merge policy (owner, 2026-11-02) allows merging on green CI; the human hot-path review follows in the owner's later review round.
2. The cadence allocation guard never parked; drain and held paths unmeasured -> added a 10 kHz real-clock run that parks before each tick, and drain-then-hold runs for both pacings (another thread stops the held worker).
3. `DrainTimeout` untested and a hung partition cost three timeouts -> after `DrainTimeout` the worker is joined and the partition drained inline (Decision 5); test added; spec 8a and thread rules corrected.
4. Overclaims fixing lost frames for encoders that hold frames back -> Consequences state that an end-of-stream flush is out of scope; open question added to the spec.
5. Overflowed packets are not "every queued packet" -> wording fixed (Decision 3).
6. Cycles break the "nothing else rings" premise -> Decision 3 states the drain is best effort there.
7. A dead worker costs the full timeout -> `drain` checks `JoinHandle::is_finished` first.
8. Skipped ticks not counted in the drain branch -> counted in `WorkerStats::late` like the run path.
9. Stall-based test could be flaky on a very slow CI -> stall raised to 2 s (128 frames take milliseconds); the post-condition holds either way.
10. `drain` reports an unreachable STOP as a timeout -> commented.
