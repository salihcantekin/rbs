---
name: measure-performance
description: Measure the CPU, GPU, memory and allocation impact of an RBS change against main. Use when touching a hot path or making any performance claim.
---

# Measure performance

Budgets: `docs/specs/performance-budgets.md`. Decision: ADR-0009.

1. **Identify the affected benchmarks**: micro-benchmarks under `benches/` in the touched crates, and the scenario benchmark if the change is on the frame or audio path.
2. **Baseline on main:** in a separate worktree of `main`, run `cargo bench -p <crate> -- --save-baseline main`.
3. **Run on the branch:** `cargo bench -p <crate> -- --baseline main`.
4. **Allocation check:** run the crate's allocation-guard tests (`cargo test -p <crate> --features alloc-guard`, once available).
5. **Report** in the PR as a table: benchmark, main, branch, change %, verdict against budget. Include machine and OS.
6. A regression over 5 % on a micro-benchmark is not acceptable without a written reason in the PR and reviewer agreement.
7. Never claim "faster" or "more efficient" without these numbers.

If a benchmark does not exist for the hot path you touched, add one in the same PR.
