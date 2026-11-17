# ADR-0143: Performance rules for every change

- Status: Proposed
- Date: 2026-11-17
- Deciders: Salih (owner); independent review recorded below (ADR-0014)

## Context

RBS promises to do more than reference tools with less CPU, GPU, memory and power ([vision](../vision.md#product-expectations-owner-2026-11-08), ADR-0009, ADR-0013). Several rules already cover parts of this:

- P6: no allocation, locks or blocking on real-time paths.
- P10: measure, do not assume.
- P13: zero cost when unused.
- P15: hot paths get extra scrutiny.

The [November 2026 performance audit](../perf/audit-2026-11.md) found 149 places to improve. On the owner's Mac the app used 26.9 % CPU at idle, and the window redrew about 30 times a second with nothing changing. Most of the cost did not come from slow inner loops, which P6 and P15 already guard. It came from patterns that no rule covers today:

- **Work without a consumer.** A virtual camera converts and reads back every frame with no app reading it (R-02). The preview and the meter sink keep the render and audio graphs running while the window is minimised (R-04, A-11). A media worker wakes 500 times a second while paused (C-01).
- **The same work done several times.** Each output reads back the same canvas (R-01, E-04), and the replay buffer runs its own encoders (E-01).
- **Work at a larger size or rate than is used.** Displays are captured at native size and refresh rate on a 1080p60 canvas (C-02, C-05).
- **Small edits that cost as much as rebuilding everything.** Every drag step re-sends every partition (G-01, G-02). Every studio update re-runs the whole UI sync and forces a full-window redraw (U-04, U-05).
- **Polling.** The control thread (G-05), the browser supervisor (G-09) and encoders (E-11) wait on fixed intervals. The idle-cost checks compare two runs that share these wake-ups, so they cannot catch them.
- **Decisions without a cost statement.** Several accepted features say nothing about what they cost while active.

How this ADR relates to the existing rules:

- R2 extends ADR-0013 rule 4 from optional subsystems to Active nodes.
- R8, R9 and R11 cite ADR-0034/0035, P4 and P10, and add only what is new.

The owner decided on 2026-11-17 that numeric budgets are set later, from what the optimizations achieve. This ADR therefore sets rules and how they are checked, not new numbers.

## Decision

### Scope

These rules apply to:

- new features, nodes, adapters, UI surfaces and control-plane paths;
- any change that adds or changes runtime work.

A change that adds a new breach of a rule needs an ADR that says why. Code that already breaks a rule may be changed, but the breach must not get worse: the change either fixes it or names the audit finding ID in the PR. Docs-only changes and changes with no runtime work are exempt.

### R1. Every feature states its cost

Every spec for a feature that adds runtime work has a **Cost** section.

- The section has one row per lifecycle state the feature has (Installed, Standby, Active; ADR-0013).
- Each row lists threads, periodic wake-ups per second, per-frame or per-block work, memory and disk writes.
- Picture-sized items are given at 1080p60 and 4K60. Items that do not apply say "none".
- Numbers are estimates until measured, and say so.

An ADR for such a feature states the estimate, and the spec carries the current figures. A PR that changes the cost updates the spec, never an accepted ADR.

### R2. No consumer, no work

A node, thread or timer is Active only while something consumes its result.

- A preview counts only while it can be seen: window not minimised, panel shown, and not covered, where the platform reports occlusion.
- Meters count only while drawn or subscribed through the API.
- A virtual camera counts only while an app reads it.
- Source activation follows ADR-0013 and ADR-0107; R2 does not change them.
- Keeping resources warm in Standby (ADR-0013) is not running.

Consumers are detected from events where the platform has them. Otherwise a low-rate check is used, and it is listed in the thread inventory (R12). Work starts and stops at lifecycle or scheduling transitions, never through a per-frame flag in a hot loop (P13).

### R3. Do the same work once

When two consumers need the same result, they share one producer:

- one capture session per device or display and configuration, sized for the largest consumer (R4);
- one conversion and readback per output shape: size, format and colour (ADR-0102 decision 3);
- one encode per identical encoder setting;
- one decode per media file and playback state.

New output types reuse these shared producers instead of building their own chain.

### R4. Work at the size, rate and format that is used

- Sources are captured, decoded and kept at no more than the largest size and rate at which they are drawn on a canvas by a scene shown under ADR-0107 decision 6, or used by an output. Canvas pixels count, not the preview panel's on-screen size. The OS is asked to scale where it can.
- A stated step or hysteresis applies, so a resize does not reopen the device on every step.
- A new texture, pool or buffer uses the smallest pixel format that keeps the quality the output needs.
- Existing format contracts stay until an ADR supersedes them: `Rgba16Float` canvases and filter outputs (ADR-0062, ADR-0086, ADR-0128).
- Pools, rings and queues are sized from a stated need, written next to the constant.

### R5. Say whether unchanged input is reused

A new or changed per-frame stage says in its Cost section whether it reuses its last result when no input changed. If it does not, it says why, for example a time-driven effect. An encoder that needs a frame anyway gets the last frame again with a new timestamp, not a recomputed one.

### R6. Wait for events, not time

Threads that RBS creates block on an event, a doorbell or a deadline they computed. A fixed-interval poll or sleep needs two things:

- a `// POLL:` comment that gives the reason and the rate;
- an entry in the thread inventory (R12).

An idle app has no periodic UI timer ([performance budgets](../specs/performance-budgets.md#desktop-ui), Desktop UI).

### R7. Edits touch only what they change

- A control-plane edit touches only the partitions, nodes and UI rows it affects. It does not deep-clone or re-send the whole scene collection.
- Continuous gestures (drags, faders) are coalesced.
- Disk writes follow ADR-0051 decision 8: one save per committed edit, a gesture is one edit at its end, and a burst of edits is one save. Each save keeps ADR-0051's atomic, flushed write.

### R8. The UI updates what changed

UI code updates the rows and properties that changed. It does not rebuild models, or create new models, for unchanged data.

The preview and the meters animate continuously. They use the native layers of ADR-0034 and ADR-0035 where those exist on the platform. Other UI animation declares its rate per panel (design system panel model).

### R9. Picture copies are counted

- GPU-to-CPU readbacks are counted in `frames_cpu_copied` (P4). Other per-frame CPU copies of a picture are counted in a separate metric defined in the gpu-compositor spec (spec first). Both are listed in the Cost section.
- One-time copies, such as a still upload, are listed but not counted per frame.
- No new per-frame copy is added to the default path without a new ADR that allows it for that path (CLAUDE.md, ADR-0038, as ADR-0117 did). R9 adds no exception.
- A new path that hands frames between the GPU and an encoder, a capture or another process states how it reaches zero copy, or why it cannot.

### R10. Encoder defaults are measured for quality per bit

A PR that changes encoder defaults reports, before and after:

- an objective metric (VMAF, or PSNR where VMAF is not available) at equal bitrate, on a stated reference clip;
- the encode time.

A default that adds latency needs the owner's approval.

### R11. Measured results become budgets

The measured results of each optimization wave are written into [performance budgets](../specs/performance-budgets.md). They then become budgets under that spec's regression policy, and its margins apply.

The idle-cost checks gain an **absolute** check. It compares the threads and periodic wake-ups of an idle app with the thread inventory (R12). It fails on an unlisted thread, or on a wake-up rate above the listed one.

### R12. The thread inventory is maintained

- The thread inventory moves to `docs/perf/threads.md`. It starts from the one in [engine-core.md](../perf/audit-2026-11/engine-core.md#thread-inventory).
- A PR that adds or removes a thread, timer or poll, or changes its rate, updates the inventory in the same change.
- Third-party threads seen in idle runs (OS capture queues, display links, driver threads) are listed as observed.
- The audit is repeated at the end of each roadmap phase, or sooner when the owner asks.

### How the rules are checked

| Where | Asks for |
| --- | --- |
| `write-adr` skill | R1 |
| `add-graph-node` skill | R1 |
| `add-backend` skill | R1, R3, R9 |
| `hot-path-review` skill | R2 to R6, R9 |
| `ui-review` skill | R8 |

The PR template gets one Performance line for code changes: "Cost section updated (R1); thread inventory updated if a thread, timer or poll changed (R12); new per-frame copy: none | ADR".

Later, rules that can be counted move into automatic checks, the way `cargo xtask check-ui` counts UI rules. The first is R11's absolute idle check against the inventory.

## Consequences

- New features carry a cost statement from the start, so reviewers can compare designs before code exists.
- Some designs get harder:
  - shared producers (R3) need clear ownership;
  - reusing unchanged results (R5) needs correct invalidation;
  - descheduling unseen work (R2) needs a fast warm-up, so nothing visible changes when the user looks again.
- Existing features do not follow every rule yet. The audit's waves bring them in line ([work plan](../perf/plan-2026-11.md)), and the Scope section says what a change to such code must do meanwhile.
- Follow-up work once accepted:
  - the skill and PR template lines above;
  - `docs/perf/threads.md`;
  - the absolute idle check and its text in `docs/specs/performance-budgets.md` (spec first, CLAUDE.md rule 2);
  - the per-frame CPU copy metric in the gpu-compositor spec (R9);
  - a one-line pointer from `docs/architecture/principles.md` and CLAUDE.md to this ADR, so contributors find it.

## Alternatives considered

- **Numeric budgets for every subsystem now.** The owner declined this for now: limits are set from what the optimizations achieve (R11).
- **Rely on P6, P13 and P15 alone.** They guard hot loops and installed-but-unused items. The audit found the largest costs in active features doing unneeded, repeated or oversized work, which those rules do not cover.
- **Fix the audit findings without new rules.** New features would bring the same patterns back.

## Review

**Round 1: independent AI session, 2026-11-17. Verdict: changes requested.** The review covered the ADR, the audit, principles, ADR-0013/0014/0022/0051/0062/0086/0102/0107/0128, performance budgets, hot paths and the PR template. It confirmed the problem is real and backed by measurements, that the rules fill gaps beyond P6, P13 and P15, and that leaving out numbers matches the owner's decision. It raised 15 concerns, all about wording. All were applied in this revision:

1. The scope contradicted the Consequences section. Added a Scope section: new breaches need an ADR, existing breaches must not get worse.
2. R1 implied editing accepted ADRs (CLAUDE.md rule 9). The cost now lives in the spec, and items that do not apply say "none".
3. R3 said "one decode per media file". Now "per media file and playback state", and capture is per configuration.
4. R4 conflicted with the `Rgba16Float` contracts. It now keeps existing contracts, uses "largest shown consumer" with hysteresis, and requires sizing comments.
5. R7 conflicted with ADR-0051. It now follows decision 8 and keeps atomic, flushed saves.
6. R2's edges were unclear. Now: occlusion only where reported, ADR-0107 unchanged, Standby is not running, event-based detection.
7. R5 could not be checked. It is now a Cost section statement, with last-frame resend for encoders.
8. R6 was unbounded. It now covers RBS threads, adds the `// POLL:` comment and lists third-party threads as observed.
9. R9 was looser than CLAUDE.md. It now allows no new per-frame copy without a superseding ADR.
10. R10 could not be checked. It now asks for VMAF or PSNR at equal bitrate on a stated clip, and latency changes need the owner.
11. R11 lacked margins and a pass condition. It now uses the regression policy, and the absolute check compares against the inventory.
12. R12 named no path or schedule. It now names `docs/perf/threads.md` and the end of each roadmap phase.
13. Enforcement missed rules and skills. It now covers `add-backend`, `ui-review` and the PR template line.
14. Overlaps with existing rules were repeated. They are now named in Context and only cited.
15. The finding count disagreed with the audit (149) and some wording was idiomatic. Both fixed.

**Round 2: independent AI session, 2026-11-17. Verdict: approve with changes.** All 15 round-1 resolutions are confirmed in the text. No conflict was found with P1-P15, ADR-0013, 0034, 0035, 0038, 0051, 0062, 0086, 0102, 0107 or 0128. Applied: R4's "shown" now means ADR-0107 decision 6 scenes plus outputs, at canvas pixels. R9 points to ADR-0038 for new copies and keeps `frames_cpu_copied` for readbacks only. The owner confirms the revised text before acceptance.

Owner approval: Salih approved accepting the ADR on 2026-11-17 ("adr ları kabul edelim"), before the review rounds changed the text. Confirmation of the revised text: pending. Record the name and date here before setting Status to Accepted.
