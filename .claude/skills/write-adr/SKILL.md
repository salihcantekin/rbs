---
name: write-adr
description: Write an Architecture Decision Record for RBS when a decision is hard to reverse, crosses crate boundaries, adds a core dependency or changes a contract.
---

# Write an ADR

1. Check `docs/adr/README.md` for an existing ADR on the topic. If one exists, decide which case you are in (owner decision D1, 2026-11-20):
   - **A factual error** in an accepted ADR (a wrong file, API name, number, platform fact or measurement; the decision itself stays): fix it in place and append a `## Review` record that says what was wrong, what it says now, who found it and the date. No new ADR.
   - **A changed decision** (anything a reviewer would check code against differently): write a short superseding ADR that names the decisions it replaces, set the old one's status line to `Superseded by ADR-NNNN`, and do not change its text otherwise. The new ADR needs the independent review of step 7 and the owner's approval.
   - When unsure which case it is, treat it as a changed decision.
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
