# CLAUDE.md

Instructions for AI coding agents working in the RBS repository. Read this file fully before any change.

## What RBS is

A headless live production engine in Rust (capture, compose, encode, stream, record) with swappable backends and a plugin system. Start with [docs/architecture/overview.md](docs/architecture/overview.md) and [docs/architecture/principles.md](docs/architecture/principles.md).

## Non-negotiable rules

1. **English only.** Code, identifiers, comments, docs, commit messages and PR text are in English, even when the conversation is in another language ([ADR-0011](docs/adr/0011-english-only-repository.md)).
2. **Spec first.** Before implementing a public API, port, node type or crate, find its spec in `docs/specs`. If it is missing or wrong, update the spec in the same change and say so. Do not invent contracts in code alone.
3. **Respect the layers.** Core and port crates never depend on adapters, vendor SDKs, FFmpeg, OS media APIs or UI toolkits (principle P1). Only `rbs-app` / `rbs-cli` wire adapters together.
4. **Capabilities, not vendors.** Never branch on vendor or device model names in engine code. Add a capability instead (P3).
5. **Real-time paths.** No allocation, blocking, I/O or contended locks in audio callbacks or `Node::process` (P6). Use pre-allocated buffers and lock-free queues.
6. **Zero-copy by default.** Pass GPU texture handles; never read frames back to the CPU on the default path (P4).
7. **`unsafe` discipline.** Only in adapter crates or marked low-level modules, every block with a `// SAFETY:` comment explaining the invariant. Run the `risky-review` skill.
8. **No panics across boundaries.** Port and plugin calls return typed errors. No `unwrap()`/`expect()` in library code except for proven invariants with a comment.
9. **Do not change accepted ADRs.** Write a new ADR that supersedes it.

## Workspace layout

- `crates/rbs-core`: time and clocks, capability model, adapter registry. Foundation for everything.
- Planned crates and layers: see the table in [overview.md](docs/architecture/overview.md#layers).
- `docs/adr`: decisions. `docs/specs`: contracts. `docs/roadmap.md`: phases.
- `.claude/skills`: repeatable procedures. Use them.

## Commands

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo doc --workspace --no-deps
```

A task is done when all four pass, the acceptance criteria are met, and specs are in sync. Saying it works is not evidence; show the command output.

## Skills

| Skill | Use when |
| --- | --- |
| `add-backend` | Adding an adapter for any port (encoder, capture, transport, ...) |
| `add-graph-node` | Adding a source, filter, transition or other node type |
| `write-adr` | A decision is hard to reverse or crosses crates |
| `measure-performance` | Touching a hot path or claiming a performance change |
| `risky-review` | Before finishing any change with `unsafe`, FFI, GPU interop, threads or public API changes |

## How to work

- Keep each change to one crate and one concern where possible. Prefer several small PRs over one large one.
- Read the relevant spec and ADRs before writing code; cite them in the PR.
- Write tests first for contracts: conformance tests for ports, property tests for pure logic.
- Use the controllable `TestClock` from `rbs-core` in tests; never sleep for timing.
- When a requirement is unclear or a spec seems wrong, stop and ask the human instead of guessing.
- Prefer well-maintained crates already listed in specs. Adding a new dependency to a core or port crate needs a justification in the PR.
