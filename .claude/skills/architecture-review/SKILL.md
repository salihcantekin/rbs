---
name: architecture-review
description: Independent review of an RBS architecture proposal (ADR, new or changed port, plugin SDK, control API, scene format, crate layering, new core dependency). Use as the reviewer, never on your own proposal in the same session.
---

# Independent architecture review

Policy: `docs/adr/0014-review-policy.md`, principle P14.

## Independence

- You must not be the author. If you wrote or discussed the proposal in this session, stop and ask for a fresh reviewer session.
- Inputs: the proposal (ADR or PR diff), `docs/architecture/principles.md`, the relevant specs and ADRs, and the code. Do not read the author's conversation or notes; judge what is written.
- When you start a reviewer as an AI agent, give it only those inputs and ask it to try to break the proposal, not to confirm it.

## Questions to answer

1. **Problem:** Is the context real and stated with evidence? Would doing nothing be acceptable?
2. **Principles:** Does it break any of P1-P15? Especially layering (P1), capabilities not vendors (P3), real-time rules (P6), zero cost when unused (P13).
3. **Alternatives:** Are the strongest alternatives considered fairly? Is there a simpler option that was missed?
4. **Swappability:** Does it make any external technology harder to replace later?
5. **Contracts:** Can the new or changed interface evolve without breaking plugins and adapters? Is semver impact stated?
6. **Performance:** What does it cost on hot paths and when idle? Is there a plan to measure it?
7. **Failure:** What happens when it fails at runtime? Is the failure contained to one node?
8. **Testability:** Can the decision be checked by a test, a conformance suite or CI?
9. **Reversibility:** What would it cost to undo in a year?

## Output

Write a `## Review` section (in the ADR, or as a PR comment for non-ADR changes):

```
Reviewer: <handle or "independent AI session">
Date: YYYY-MM-DD
Verdict: approve | changes requested
Concerns:
1. <concern> -> <resolution or open>
```

"Approve" with open concerns is allowed only for concerns explicitly accepted by the project owner.
