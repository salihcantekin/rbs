# ADR-0012: Spec-first, AI-assisted development workflow

- Status: Accepted
- Date: 2026-11-02

## Context

Much of RBS will be written by AI coding agents under human direction. Agents are fast and consistent with written rules but start each task without memory, may invent APIs, and are weakest at platform-specific `unsafe` FFI and GPU synchronisation.

## Decision

1. **Spec first.** No new crate, port or public API without a spec in `docs/specs` (or a spec change in the same PR). Agents implement against specs, not against chat descriptions.
2. **Small tasks.** Work is split into tasks that fit one PR and one crate where possible, each with acceptance criteria.
3. **Repository knowledge.** `CLAUDE.md` holds the rules every agent must follow. Repeatable procedures live as skills in `.claude/skills` (add a backend, add a graph node, write an ADR, measure performance, review risky code).
4. **Machine-checkable gates.** `fmt`, `clippy -D warnings`, tests, conformance suites, docs, dependency layer check and performance budgets run in CI. A task is done when CI is green and the acceptance criteria are met, not when the agent says so.
5. **Human review where agents are weak.** Changes touching `unsafe`, FFI, GPU interop, real-time threads, the plugin ABI or the control API schema require a human reviewer and the `risky-review` checklist.

## Consequences

- Up-front cost in specs and tooling; repaid by fewer rewrites and safer parallel work.
- Specs must be kept current; a PR that changes a contract without updating the spec is incomplete.

## Alternatives considered

- Free-form agent sessions without specs: fast initially, produces inconsistent architecture.
