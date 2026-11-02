---
name: hot-path-review
description: Checklist for any RBS change that edits or is called from a hot path (audio thread, render loop, Node::process, pools, colour conversion, encoder hand-off, mux and transport send loops). Fill it in the PR; a separate reviewer verifies it.
---

# Hot-path review

Policy: principle P15, `docs/adr/0014-review-policy.md`. Paths: `docs/architecture/hot-paths.md`.

1. **Is it a hot path?** It is if the diff edits code inside, or called directly from, the functions and modules listed in `docs/architecture/hot-paths.md`. If a new module now runs per frame, block or packet, add it to that list and to `.github/CODEOWNERS` in the same PR.
2. **Measure** with the `measure-performance` skill. Put the before/after table in the PR. No numbers, no merge.
3. **Checklist** (answer each OK / Issue / N/A with one line):
   - No heap allocation per call (including hidden ones: `format!`, `to_string`, `Vec` growth, `Box`, `Arc::new`, closures that capture by value into boxes).
   - No locks that can be contended, no blocking calls, no I/O, no logging per call (use counters).
   - No new branches that test whether an inactive feature is enabled (P13).
   - Work per call is bounded; no loops over unbounded collections.
   - Data layout: no extra indirection or copies of frame or audio data; GPU frames stay on the GPU (P4).
   - Cache behaviour considered for tight loops (contiguous data, no per-item virtual calls where a batch call fits).
   - Clock reads and timestamp math do not drift or overflow.
   - Errors on the path are cheap (no allocation building error messages per frame).
4. **Tests:** allocation-guard test covers the changed function; benchmark exists for it (add one if not).
5. **Review:** the author fills this in; a different reviewer (human maintainer required, optionally plus an independent AI session) verifies it and approves. Self-review alone is never enough.
