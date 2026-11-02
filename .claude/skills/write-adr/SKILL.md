---
name: write-adr
description: Write an Architecture Decision Record for RBS when a decision is hard to reverse, crosses crate boundaries, adds a core dependency or changes a contract.
---

# Write an ADR

1. Check `docs/adr/README.md` for an existing ADR on the topic. If one exists and you disagree, write a superseding ADR; never edit an accepted one (only its status line).
2. Copy `docs/adr/template.md` to `docs/adr/NNNN-short-kebab-title.md` with the next free number.
3. Fill in:
   - **Context:** the forces and constraints, with links to specs, issues and measurements. No solution here.
   - **Decision:** stated so a reviewer can check code against it. Use "must" / "must not".
   - **Consequences:** both positive and negative, plus the follow-up work (tests, tooling, spec updates).
   - **Alternatives considered:** each with the reason it lost.
4. Status `Proposed`; date today (YYYY-MM-DD).
5. Add the row to the index in `docs/adr/README.md`.
6. Update affected specs in the same PR, or list them as follow-ups in Consequences.
7. Leave the `## Review` section for the reviewer. Request an independent review (`architecture-review` skill, ADR-0014): a human maintainer or a fresh AI session that gets only the ADR, specs, principles and code, not this conversation. Never review your own ADR.
8. Keep it short: one to two pages. English only.
