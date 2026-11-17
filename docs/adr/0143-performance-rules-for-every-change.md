# ADR-0143: Performance rules for every change

- Status: Proposed
- Date: 2026-11-17
- Deciders: Salih (owner); independent review pending (ADR-0014)

## Context

RBS promises to do more than reference tools with less CPU, GPU, memory and power ([vision](../vision.md#product-expectations-owner-2026-11-08), ADR-0009, ADR-0013). The existing rules cover parts of this: P6 (no allocation, locks or blocking on real-time paths), P10 (measure, do not assume), P13 (zero cost when unused) and P15 (hot paths get extra scrutiny).

The [November 2026 performance audit](../perf/audit-2026-11.md) found 148 places to improve. Most of the cost did not come from slow inner loops, which P6 and P15 already guard. It came from patterns that no rule covers today:

- **Work without a consumer.** A virtual camera converts and reads back every frame with no app reading it (R-02). The preview and the meter sink keep the render and audio graphs running while the window is minimised (R-04, A-11). A media worker wakes 500 times a second while paused (C-01).
- **The same work done several times.** Each output reads back the same canvas (R-01, E-04), and the replay buffer runs its own encoders (E-01).
- **Work at a larger size or rate than is used.** Displays are captured at native size and refresh rate on a 1080p60 canvas (C-02, C-05), and NV12 cameras are expanded to 16-bit float RGBA (R-11).
- **Edits that cost the whole world.** Every drag step rebuilds and re-sends every partition and writes the collection with fsync (G-01 to G-03). Every studio update re-runs the whole UI sync and forces a full-window redraw (U-04, U-05).
- **Polling.** Fixed-interval waits in the control thread (G-05), the browser supervisor (G-09) and encoders (E-11). The idle-cost checks compare two runs that share these wake-ups, so they cannot catch them.
- **Decisions without a cost statement.** Several accepted features say nothing about what they cost while active, so the cost was found only by this audit.

The owner decided on 2026-11-17 that numeric budgets are set later, from what the optimizations achieve. This ADR therefore sets rules and how they are checked, not new numbers.

## Decision

These rules apply to every new feature, node, adapter, UI surface and control-plane path, and to every change to an existing one. A change that breaks one needs an ADR that says why.

### R1. Every feature states its cost

Every spec or ADR for a feature has a **Cost** section with one row per lifecycle state (Installed, Standby, Active; ADR-0013). Each row lists threads, wake-ups per second, per-frame or per-block work (GPU passes, copies with their size at 1080p60), memory (RAM and VRAM at 1080p and 4K) and disk writes. Numbers are estimates until measured, and say so. A PR that changes the cost updates the section.

### R2. No consumer, no work

A node, thread or timer runs only while something consumes its result. A preview counts as a consumer only while it can be seen (not minimised, not fully covered, panel shown); meters only while drawn or read through the API; a virtual camera only while an app reads it; an off-air source only within the limits of its own ADR. The check happens where the graph is scheduled, not as a per-frame flag in a hot loop (P13).

### R3. Do the same work once

Two consumers that need the same result share one producer: one capture session per device or display, one conversion and readback per output shape (size, format, colour), one encode per identical encoder setting, one decode per media file. New output types reuse these shared producers instead of building their own chain.

### R4. Work at the size, rate and format that is used

Sources are captured, decoded and kept at the size and rate the canvas draws them, asking the OS to scale where it can. Pixel formats are the smallest that keep the quality the output needs (16-bit float only where blending, HDR or 10-bit needs it). Pools, rings and queues are sized from a stated need, not "large enough".

### R5. Skip work when nothing changed

Where the result of a stage cannot have changed (no input changed, the same picture again), the stage reuses its last result instead of recomputing it, unless its own ADR says why it cannot (for example a time-driven effect).

### R6. Wait for events, not time

Threads block on an event, a doorbell or a deadline they computed. A fixed-interval poll or sleep needs a written reason in the code and appears with its rate in the thread inventory. An idle app (nothing on air, window shown, no animation) has no periodic UI timer, and every periodic wake-up of any thread is listed in the inventory with its rate.

### R7. Edits cost what they change

A control-plane edit costs in proportion to what it changes, not to the size of the scene collection: it touches only the partitions, nodes and UI rows it affects. Continuous gestures (drags, faders) are coalesced. Writes to disk are debounced and flushed at the end of a gesture and on exit; no fsync per edit.

### R8. The UI updates what changed

UI code updates the rows and properties that changed instead of rebuilding models, and does not create new models for unchanged data. Anything that animates continuously (preview, meters) is drawn by a native layer where the platform allows (ADR-0034, ADR-0035), so the toolkit does not redraw the window for it.

### R9. Copies of pictures are visible

Every CPU copy and every GPU-to-CPU readback of a picture is counted in the metrics (P4) and listed in the feature's Cost section. A new copy on the default path needs an ADR. Zero copy is the target for every new path that hands frames between the GPU and an encoder, a capture or another process.

### R10. Quality per bit is performance

Encoder defaults are chosen for the best quality at the target bitrate that the machine can sustain, checked with an objective metric (VMAF or PSNR) when they change.

### R11. Measure, then budget

Performance claims carry numbers (P10). Each optimization wave ends with measured results written into [performance budgets](../specs/performance-budgets.md); those results become the budgets the next change must not exceed. The idle-cost checks gain an **absolute** check (thread count and wake-ups per thread of an idle app), so that a wake-up shared by baseline and loaded runs is visible.

### R12. The audit is kept current

The thread inventory (now in [engine-core.md](../perf/audit-2026-11/engine-core.md#thread-inventory)) moves to a maintained document, and a PR that adds a thread, timer or poll updates it. The audit is repeated after each large feature phase.

### How the rules are checked

- The `write-adr` and `add-graph-node` skills ask for the R1 Cost section, and the `hot-path-review` skill asks for R2 to R6 and R9.
- A short "Performance" checklist (R1 to R9) is added to the PR template for code changes.
- Later, where a rule can be counted (threads, wake-ups, copies per frame), it moves into an automatic check, in the way `cargo xtask check-ui` counts UI rules.

## Consequences

- New features carry a cost statement from the start, so reviewers can compare designs before code exists.
- Some designs get harder: shared producers (R3) and render-on-change (R5) need clear ownership and invalidation rules, and descheduling unseen work (R2) needs a fast warm-up so nothing visible changes when the user looks again.
- Existing features do not follow all rules yet. The audit's waves bring them in line; until then, a change to such a feature fixes the rule it touches or records why not.
- Skill and template updates (`write-adr`, `add-graph-node`, `hot-path-review`, PR template) and the maintained thread inventory are follow-up work once this ADR is accepted.

## Alternatives considered

- **Numeric budgets for every subsystem now.** Rejected by the owner for now: limits are set from what the optimizations achieve (R11).
- **Rely on P6, P13 and P15 alone.** They guard hot loops and installed-but-unused items, but the audit found the largest costs in active features doing unneeded, repeated or oversized work, which they do not cover.
- **Fix the audit findings without new rules.** New features would bring the same patterns back.

## Review

Pending: needs an independent review (a human, or a fresh AI session given only this ADR, the audit and the specs; ADR-0014) and the owner's approval before it becomes Accepted.
