# Architecture principles

These rules apply to every change. A pull request that breaks one needs an ADR that explains why.

## P1. The core never names a vendor

Core crates (`rbs-core`, `rbs-graph`, `rbs-scene`, and every port crate) must not depend on FFmpeg, a GPU vendor SDK, an OS media API or a network protocol library. Those live only in adapter crates. The application crate is the only place where adapters are wired in.

Test: `cargo xtask check-layers` fails if a core or port crate depends on an adapter or a vendor crate, and if an engine crate depends on an adapter. The allowlists live in `xtask/src/main.rs` and match the layer table in [overview.md](overview.md).

## P2. Every external capability is a port with swappable adapters

Capture, audio I/O, video encode, video decode, audio encode, muxing and transport are each defined as a **port**: a Rust trait plus a capability description, in its own crate. Implementations are **adapters**. See [ADR-0003](../adr/0003-ports-and-adapters.md) and [backend ports spec](../specs/backend-ports.md).

- Adapters declare what they can do (formats, zero-copy, hardware, limits) as data.
- The core selects an adapter at runtime by matching required capabilities, not by name.
- Each port ships a conformance test suite. An adapter is accepted when it passes it.

Replacing FFmpeg, for example, means writing new adapters that pass the same suites, then changing the default preference order. No core code changes.

## P3. Choose by capability, not by vendor or generation

Code asks "can something encode AV1 10-bit from a GPU texture at 4K60?", never "is this an NVIDIA RTX 40 card?". New hardware is supported by adding or updating an adapter that reports new capabilities.

## P4. Frames stay where they are produced

Video frames are GPU textures from capture to encoder whenever the platform allows (D3D11/12 shared handles, IOSurface, DMA-BUF). Copies to system memory are an explicit, measured fallback and are visible in metrics. Nodes pass handles, not pixels.

## P5. One master clock

Every timestamp in the system is in master-clock time. Sources convert from their own clock at the edge. See [ADR-0004](../adr/0004-single-master-clock.md).

## P6. The real-time paths do not allocate, lock or block

The audio thread and the render loop must not allocate, take contended locks, do I/O or call into code with unbounded run time. Communication uses pre-allocated lock-free queues. This is enforced by tests (allocation guards in debug builds) and by review.

## P7. A failure is contained to the node that failed

A source, filter, plugin or output that errors, panics, stalls or exceeds its time budget is isolated: its last good frame or silence is used, it is restarted with back-off, and the user is notified. Other outputs keep running.

## P8. The core is headless

The engine runs without any UI. Every UI (desktop, web, CLI, Stream Deck) is a client of the same control API. No core crate depends on a UI toolkit.

## P9. Contracts are versioned

The plugin SDK, the control API and the scene document format are versioned with semver. Breaking changes only happen in major versions, and the host supports the previous major version for at least one release cycle.

## P10. Measure, do not assume

Performance claims require numbers. Hot-path changes come with a before/after benchmark. CI tracks the budgets in [performance budgets](../specs/performance-budgets.md) and fails on regressions beyond the allowed margin.

## P11. Safe Rust by default

`unsafe` is allowed only in adapter crates and in clearly marked low-level modules, each block with a `// SAFETY:` comment. Every crate that contains `unsafe` is run under Miri or sanitizers in CI where the platform allows.

## P12. Decisions are written down

Any decision that is hard to reverse or affects more than one crate is an ADR in [docs/adr](../adr/). Specs describe the current contract; ADRs describe why.

## P13. Zero cost when unused

Every feature, adapter, plugin and node is Installed, Standby or Active. Installed items cost nothing at runtime: no threads, polling, GPU resources, device handles or per-frame work, and plugin binaries are not loaded. Standby items are not scheduled and keep resources warm only when the user or scene asks for it, visibly. Only Active nodes are in the scheduled graph, so hot loops contain no per-iteration checks whose purpose is to test whether an inactive feature is enabled. See [ADR-0013](../adr/0013-zero-cost-when-unused.md).

Test: the idle-cost checks in [performance budgets](../specs/performance-budgets.md#idle-cost).

## P14. Architecture decisions get an independent review

New or superseding ADRs, new ports or port trait changes, changes to the plugin SDK, control API, scene format or crate layering, and new dependencies in core or port crates are reviewed by someone who did not write them: a human maintainer, or a fresh AI reviewer session that receives the proposal, specs and code but not the author's reasoning or conversation. The review is recorded in the ADR or the PR. See [ADR-0014](../adr/0014-review-policy.md).

## P15. Hot paths get extra scrutiny

Code edited inside, or called directly from, the functions and modules listed in [hot paths](hot-paths.md) needs, in addition to normal review: before/after measurements, the `hot-path-review` checklist, and approval from a human maintainer. An AI review can be added but never replaces the human approval.
