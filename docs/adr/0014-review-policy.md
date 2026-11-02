# ADR-0014: Independent review for architecture, extra review for hot paths

- Status: Accepted
- Date: 2026-11-02
- Amends: ADR-0001 (allows appending a review record to accepted ADRs)

## Context

Much of RBS will be written by AI agents (ADR-0012). An author, human or AI, tends to confirm their own reasoning; an agent asked to review its own proposal in the same session usually agrees with itself. Architecture mistakes are expensive to undo, and hot-path regressions hurt every user. Both need a review that is not anchored on the author's view.

The project currently has one human maintainer, the owner. The policy must work in that situation and scale when more maintainers join.

## Decision

### Scope of architecture review (principle P14)

New or superseding ADRs; new ports or changes to port traits; changes to the plugin SDK, control API or scene document format; changes to crate layering; new dependencies in core or port crates.

### Architecture review

1. The reviewer must be independent of the author: a human maintainer who did not write it, or a fresh AI reviewer session given only the proposal, the relevant specs, ADRs and principles, and the code. It must not see the author's conversation or notes.
2. The reviewer follows the `architecture-review` skill. The outcome (reviewer, date, verdict, concerns and how each was resolved) is recorded in the ADR's `## Review

- Reviewer: independent AI session (fresh context; given only this ADR, the principles, related ADRs, specs and code)
- Date: 2026-11-02
- Verdict: approve after changes
- Concerns: (1) unworkable with one maintainer -> `Owner approval:` path, manual enforcement stated; (2) hot-path scope covered nearly everything -> limited to code edited inside or called directly from listed paths, CODEOWNERS coarseness accepted; (3) human vs AI reviewer contradiction -> AI review never replaces the human for hot paths; (4) silent change to ADR-0001 -> amendment stated, ADR-0001 status points here; (5) scope lists differed between documents -> aligned; (6) merge vs acceptance unclear -> Accepted requires review plus owner approval.
- Owner approval: Salih, 2026-11-02 (project thread)
