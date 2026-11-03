# ADR-0004: Single master clock for all media

- Status: Accepted
- Date: 2026-11-02

## Context

Audio/video drift and "audio buffering" issues are among the most reported OBS problems. They come from multiple timing paths and incomplete compensation for device clock drift.

## Decision

- The engine has one monotonic master clock (QPC on Windows, `mach_absolute_time` on macOS, `CLOCK_MONOTONIC` on Linux), represented as `rbs_core::time::Timestamp` in nanoseconds.
- Every frame, audio block and packet carries a master-clock timestamp. Sources convert from their own clock at the edge and nowhere else.
- Each audio device has a drift estimator; the audio graph compensates with small-ratio resampling.
- The render loop of each canvas ticks on the master clock at the canvas frame rate. A late source contributes its previous frame; it never blocks the tick.

## Consequences

- One place to reason about sync; per-source latency offsets become simple.
- Every source adapter must implement clock conversion; the capture and audio conformance suites test it.

## Alternatives considered

- Audio clock as master (common in players): breaks with several unrelated audio devices.
- Per-output clocks: makes multi-output sync impossible to reason about.

## Review

- Record, 2026-11-03: the per-device drift estimator of the decision is superseded in part by [ADR-0037](0037-audio-graph.md) decision 5: adapters already deliver master-clock timestamps, and the audio graph steers its resampler against them instead. Drift is still corrected by small-ratio resampling against the master clock.
