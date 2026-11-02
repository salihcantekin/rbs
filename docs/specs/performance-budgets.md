# Spec: Performance budgets

- Status: Draft
- Owner: whole project; enforced in CI
- Related ADRs: [0009](../adr/0009-performance-budgets-in-ci.md)

## Purpose

Turn "use memory, CPU and GPU efficiently" into numbers that CI can check.

## Hard rules (always enforced)

| Rule | How it is checked |
| --- | --- |
| No heap allocation on the audio thread or inside `Node::process` | Allocation guard in debug/test builds fails the test |
| No CPU readback of video frames on the default path when zero-copy is available | Metric `frames_cpu_copied` must be 0 in scenario tests |
| No blocking I/O or contended locks on real-time threads | Review checklist; lock contention metrics |
| Encoded packets fan out to many outputs without copying payloads | Micro-benchmark asserts zero payload copies |

## Scenario budgets (initial targets, to be calibrated in Phase 0)

Measured on reference machines that will be listed here. Each target is compared with OBS on the same machine and scene.

| Scenario | Metric | Target |
| --- | --- | --- |
| 1080p60 display capture, one camera, two images, text, stream + record, hardware encoder | Engine CPU time | at or below OBS |
| Same | GPU time per frame | at or below OBS |
| Same | Resident memory after 10 min | at or below OBS |
| Same | Frames dropped by RBS (render or encode lag) over 4 h | 0 |
| Audio path, 8 sources, 4 buses | Audio thread load per 10 ms block | < 10 % of block time |
| Glass-to-encoder latency, display capture | Latency | < 2 frame intervals |
| Cold start to first preview frame | Time | < 1.5 s |

## Regression policy

- Micro-benchmarks: CI fails on > 5 % regression with statistical confidence (criterion comparison against `main`).
- Scenario benchmarks: nightly on self-hosted GPU runners; > 3 % regression opens an issue automatically and blocks release.
- A PR that intentionally trades performance must say so and link the measurement.
