# ADR-0155: One noise-suppression state until each channel is selected

- Status: Accepted
- Date: 2026-11-19
- Deciders: Salih (owner); independent review recorded below; owner approved 2026-11-19
- Supersedes: [ADR-0130](0130-voice-filters-noise-suppression-compressor-limiter-eq.md) decision 5, the sentence that `make` builds one RNNoise state per channel so switching `mode` in `set` allocates nothing, and that `mono` uses the first of those states. The rest of ADR-0130 stays.

## Context

ADR-0130 decision 5 builds one RNNoise state per channel when the filter is created, including the default `mono` mode, which denoises the mean of the channels once and writes that pass to every channel. The unused states exist so `set` can switch to `separate` without allocating. Each state plans an FFT. A stereo microphone in the default mode therefore plans a second FFT it never runs.

`AudioFilterNode` applies `set` on the executor thread, before the block's samples. A state built in `set` is ready for that block. `process` must stay free of allocation (P6).

The owner approved this change on 2026-11-19.

## Decision

1. **`make` builds only the states the starting mode uses.** `mono` builds one RNNoise state. `separate` builds one per channel. The default mode must not plan an unused channel's FFT.
2. **`set` builds a missing state before it returns.** Switching to `separate` builds every missing state before `set` returns, so the following `process` uses them. `process` and `reset` must not build or free a state. Switching back to `mono` keeps states already built.
3. **A failed build stays on `mono`.** If a state cannot be created, `set` leaves the mode at `mono` and uses the states that exist.
4. **One allocation on the first switch is accepted.** That allocation is the FFT plan, on the thread that runs `set` (today the executor thread). A steady block, a later `set` that does not need a new state, and `process` must not allocate.
5. **The heard signal is unchanged.** `mono` still denoises the mean once and writes it to every channel. `separate` still runs one state per channel. A mode change still resets every state.

## Consequences

- A default stereo instance plans one FFT, not two. The per-block `mono` path does not gain a branch that tests whether a second state exists; it uses the one state.
- The first switch to `separate` allocates once on the executor thread, before that block's samples. ADR-0130's " `set` allocates nothing" no longer holds for that switch.
- `docs/specs/audio-filters.md` states this. ADR-0130's status line records that decision 5 is superseded in that sentence. The decision text is not rewritten.
- Tests: one denoiser in the default stereo mode; `set` builds the missing one and later blocks allocate nothing; channels stay apart after the switch (`rbs-audio-filters` `tests/noise.rs`).

## Alternatives considered

- **Keep one state per channel from `make`** (ADR-0130 as written). `set` stays allocation-free. Every default stereo filter plans an FFT it never runs. The owner rejected this on 2026-11-19.
- **Build the second state on the control thread and only then deliver `set`.** That keeps the executor free of the plan. `AudioFilterNode` applies `set` itself on the executor, so this needs a new hand-off. Out of scope; decision 4 accepts the one allocation where `set` already runs.

## Review

Reviewer: independent AI session
Date: 2026-11-19
Verdict: approve
Concerns:
1. Default stereo `make` builds one `Denoiser` (`built = 1`). The mono arm of `process` sums into `frames_in[0]` only. `run_frame` sets `passes = 1` and calls `process_frame` once. That path never reads `states.len()`, so it does not grow a branch that asks whether a second state exists, and it does not run a second FFT. The `separate` flag is the mode choice ADR-0130 already required. -> Matches decision 1.
2. `process` and `reset` do not call `denoiser` or `build_missing_denoisers`, and they do not grow `states` or the rings. A missing state is built only from `set`. -> Matches decision 2. Steady-block allocation stays inside `Denoiser::process_frame`, which ADR-0130 already requires to be allocation-free after `new`.
3. If `Denoiser::new` fails, `build_missing_denoisers` returns false and drops that state, and `set` stores `separate = false`. A stereo failure adds no state. For more than two channels a mid-loop failure keeps states already pushed, still stores mono, and mono still runs only pass 0, so those extra plans stay idle until a later `set` finishes them. `make` in `separate` still returns `Err` if any state fails (ADR-0130 pass-through), which is a different path from this fallback. The stored control value can still say `separate` while the DSP runs mono. -> Matches decision 3, which chooses that over dropping the filter.
4. `make` with `separate` sets `built` to the channel count and constructs that many states. A one-channel graph never grows a second state, because `states.len() >= channels` returns before `reserve`. -> Matches decision 1.
5. Once the missing states exist, `build_missing_denoisers` returns before `reserve` or `Denoiser::new`. Switching to `mono` does not pop them, so the next `separate` does not allocate. The allocating `set` is the first one that still needs a state. -> Matches decisions 2 and 4. `Denoiser::new` (FFT plan and that state's tables) runs on the executor thread; decision 4 accepts that one switch and rejects a new hand-off. A failed attempt can reserve capacity and, on a later success, allocate the states still missing; a `set` that needs nothing new does not.
6. No other ADR-0130 decision is contradicted. Latency is still fixed in `make` and does not change in `set` (decision 2). A mode change still resets every state, and `reset` itself does not allocate, so the allocation-free clause on that reset sentence still holds. The 48 kHz refusal, the mean-versus-per-channel signal on the success path, the meter and the steady-state budgets (decision 11) are unchanged. The only "set allocates nothing" rule is the sentence this ADR supersedes.
