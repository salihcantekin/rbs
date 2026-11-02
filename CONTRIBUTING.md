# Contributing to RBS

Thank you for helping. RBS is designed so that contributions can be checked by machines first and reviewed by people second.

## Before you start

1. Read [docs/architecture/principles.md](docs/architecture/principles.md). Every change must respect these rules.
2. Find the spec for the area you are changing in [docs/specs](docs/specs/). If there is none and you are adding a public API, write or extend a spec first.
3. If your change is hard to reverse or crosses crate boundaries, propose an ADR ([docs/adr](docs/adr/)).

## Workflow

1. Open or pick an issue with clear acceptance criteria.
2. Create a branch, keep the change to one concern.
3. Run the local checks:

   ```sh
   cargo fmt --all -- --check
   cargo clippy --workspace --all-targets -- -D warnings
   cargo test --workspace
   cargo doc --workspace --no-deps
   ```

4. If you touched a [hot path](docs/architecture/hot-paths.md), include before/after numbers (see [performance budgets](docs/specs/performance-budgets.md)) and the `hot-path-review` checklist. A maintainer must approve.
5. If you added a feature, make sure it costs nothing while inactive (principle P13).
6. ADRs, port changes and public contract changes need an independent review before they are accepted ([ADR-0014](docs/adr/0014-review-policy.md)).
7. Open a pull request using the template.

## Adding a backend (adapter)

Follow [backend ports](docs/specs/backend-ports.md): a new crate depending only on its port and `rbs-core`, capabilities declared in `probe`, the port's conformance suite passing. No changes to the engine should be needed; if they are, the port is missing something and that is a spec discussion.

## Language

All code, comments, docs, commits and PRs are in English ([ADR-0011](docs/adr/0011-english-only-repository.md)).

## AI-assisted contributions

Welcome, under the same rules. Agents must follow [CLAUDE.md](CLAUDE.md). The human who opens the PR is responsible for it, and changes touching `unsafe`, FFI, GPU interop, real-time threads or public contracts need human review ([ADR-0012](docs/adr/0012-ai-assisted-development.md)).
