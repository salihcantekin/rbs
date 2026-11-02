# ADR-0009: Performance budgets enforced in CI

- Status: Accepted
- Date: 2026-11-02

## Context

Efficiency with memory, CPU and GPU is a core product promise. Without measurement, performance degrades one reasonable-looking change at a time, especially with AI-generated code.

## Decision

- Budgets are defined in [performance budgets](../specs/performance-budgets.md) and are part of the spec.
- Micro-benchmarks (`criterion`) cover hot paths: frame pool, audio mixing, colour conversion, packet fan-out, mux.
- A headless scenario benchmark runs reference scenes and records CPU time, GPU time, peak memory, allocations on real-time threads and frame drops.
- CI fails when a tracked metric regresses beyond its allowed margin. Changes to hot paths must include before/after numbers in the PR.

## Consequences

- Contributors get objective feedback; reviewers have numbers.
- We need stable benchmark runners; GPU scenarios run on self-hosted machines, micro-benchmarks on hosted CI.

## Alternatives considered

- Manual profiling before releases: finds regressions too late.
