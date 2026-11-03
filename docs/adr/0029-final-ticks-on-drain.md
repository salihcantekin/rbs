# ADR-0029: Final ticks on drain

- Status: Proposed
- Date: 2026-11-03
- Deciders: Salih (owner)
- Amends: [ADR-0027](0027-engine-loop.md) Decision 7 (drains swap between ticks) and Decision 3 (the worker reads a stop flag).

## Context

ADR-0027 Decision 7 drains each partition at shutdown by sending it an empty schedule and waiting for the swap. A worker swaps between ticks, so:

- a cadence partition is drained without another tick: the encoder never reads the frame that render's last tick wrote, and the last frame of every recording or stream is lost;
- an output's `PacketReader` passes at most `PER_TICK_EVENTS` (16) packets per tick and keeps the rest for the next tick, which never comes: up to a queue's worth of packets (256 by default) queued behind a slow output is dropped at release (`resync_drops`).

Forces: shutdown runs in pipeline order and every producer partition is fully drained before its consumers (ADR-0026 Decision 8); nodes may only run on their worker thread (`process` runs there, and a thread-bound runtime can only rely on that thread); the worker loop is a hot path (P6, P15) and must not gain per-iteration checks for an inactive feature (P13); tests use `TestClock`, which only moves when the test moves it.

## Decision

1. **Before the empty schedule is sent**, `Engine::shutdown` asks each running partition's worker, in pipeline order, for its final ticks and waits up to `EngineConfig::swap_timeout` for them. They run on the worker's own thread, on the schedule it already runs.
2. **Cadence worker:** exactly one final tick, at once, with the next free index (or the newest due one if the worker is late). Its time may be up to one cadence period ahead of the clock. It must not wait for the tick's time: an engine on a stopped clock (tests, a paused master clock) must still shut down.
3. **Doorbell worker:** one final tick, then one more for each ring left by a reader that still had packets, at most `MAX_DRAIN_TICKS` (4096) ticks. Because upstream partitions drained first, nothing else rings; every packet queued before the output's drain is passed on.
4. **After its final ticks** a worker runs no further tick but still swaps schedules on wake-up, so the empty schedule is swapped and its nodes cooled on the worker thread, as before.
5. **Final ticks that miss the timeout** are reported as `EngineError::DrainTimeout(partition)` and the drain continues as in ADR-0027 (a node hung in `process` still blocks the join).
6. **The worker's stop flag becomes a mode** (`run`, `drain`, `held`, `stop`) in one `AtomicU8`: still one atomic load per loop iteration on a cadence worker and two (before and after the wait) on a doorbell worker, as before. Changing `drain` to `held` and `run` to `drain` uses compare-exchange, so a stop is never overwritten. The worker reports the end of its final ticks through a one-slot channel created when the worker starts.

## Consequences

- The last frame render wrote is encoded and every queued packet reaches the output before release (tests `shutdown_encodes_the_last_frame_render_wrote`, `shutdown_passes_every_queued_packet_not_just_one_ticks_worth`).
- Render runs one more tick at shutdown than the clock asked for; its frame carries a timestamp on the cadence, up to one period ahead. Encoders see it as an ordinary last frame.
- Shutdown takes at most one extra tick per partition, plus the time a stalled output needs to pass its backlog (bounded by `swap_timeout`).
- Graph edits that empty a partition while running are unchanged: they swap between ticks without a final tick (a live edit is not end-of-stream).
- Spec `docs/specs/engine.md` gains invariant 8a and the `DrainTimeout` error; the open question on final ticks is closed.

## Alternatives considered

- **Run the final ticks on the control thread** after joining the worker: simpler signalling, but `process` would run off its worker thread, breaking thread-bound runtimes and the real-time class of the thread.
- **Send the empty schedule first and tick before the swap:** `run_tick` swaps right after the first tick, so a doorbell worker could not pass more than one tick's packets.
- **Wait for the final cadence tick's time:** keeps ticks on the clock, but a stopped or test clock never reaches it and shutdown would hang until the timeout.
- **A separate drain flag:** a second atomic load per iteration; one mode value costs the same as the old stop flag.

## Review

Pending: independent review per ADR-0014.
