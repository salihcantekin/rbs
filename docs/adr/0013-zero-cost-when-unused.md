# ADR-0013: Zero cost when unused

- Status: Proposed
- Date: 2026-11-02

## Context

RBS is meant to grow through many features, adapters and plugins. In long-lived media applications, each addition tends to add a little background work (a polling thread, a per-frame check, an always-allocated buffer) even when nobody uses it. Over years this makes the base system slower for everyone. Efficient use of CPU, GPU and memory is a core promise of RBS (ADR-0009).

## Decision

### Lifecycle states

Every feature, adapter, plugin and node is in exactly one state:

| State | Meaning | Allowed cost |
| --- | --- | --- |
| Installed | Present on disk or compiled in, not used by any scene, output or setting | Manifest and static metadata only; plugin binaries are not loaded; at most one probe at start-up |
| Standby | Used by the configuration (for example a camera in a scene that is not on air) but not producing output | Not scheduled by the render tick or audio graph. May keep resources warm (open device, GPU pipeline) only when the scene or user policy asks for it; that cost is shown per item in diagnostics and the UI |
| Active | Producing or consuming media | Its normal cost, measured by per-node metrics |

### Rules

1. **Installed** items have no threads, timers, polling, GPU resources, device handles, open files or per-frame work. Native plugin libraries are loaded and WASM modules compiled only on first transition out of Installed.
2. Device change detection uses one shared watcher owned by the registry, started at start-up. Individual adapters do not run their own watchers. A re-probe happens only on a watcher event.
3. Only **Active** nodes are in the scheduled media graph. Hot loops never contain per-iteration checks whose purpose is to test whether an inactive feature is enabled. Branches required by the graph itself (bypass of a failing node per P7, transition state) are allowed.
4. Optional subsystems that need background work (network services, hot-plug for a specific device class) start lazily on first use and stop when their last user goes away.
5. Deactivation returns threads, memory and GPU resources to their level before activation.
6. Compile-time features (cargo features) are used for build size and licence reasons, not as a substitute for runtime zero cost.
7. The idle-cost checks in the performance budgets spec enforce rules 1, 2 and 5.

## Consequences

- Users pay only for what they use; adding features does not slow down simple setups.
- Feature authors must implement explicit transitions between the three states. The media graph and plugin SDK specs define these transitions.
- Leaving Installed or Standby can add latency on first use; warm-up before graph swaps (media graph spec) hides it for scene switches, and Standby with "keep warm" exists for cases where that is not enough.

## Alternatives considered

- Enable flags checked in hot loops: simple, but adds per-iteration work and keeps resources alive.
- Compile-time features only: users would need different builds for different setups, and plugins could not follow the rule.

## Review

- Reviewer: independent AI session (fresh context; given only this ADR, the principles, related ADRs, specs and code)
- Date: 2026-11-02
- Verdict: approve after changes
- Concerns: (1) specs lacked the lifecycle -> media graph and plugin SDK specs updated; (2) "active" undefined -> Installed / Standby / Active states; (3) idle-cost test not measurable -> same binary, deterministic checks with tolerances, runner per check; (4) device watcher contradicted "no threads" -> one shared registry watcher, lazy plugin loading; (5) "no branches" too absolute -> only per-iteration enable checks banned.
- Owner approval: pending
