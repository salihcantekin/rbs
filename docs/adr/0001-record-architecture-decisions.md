# ADR-0001: Record architecture decisions

- Status: Accepted
- Date: 2026-11-02

## Context

RBS is meant to live for many years, with a changing set of contributors, many of them AI coding agents that start every task without memory of earlier discussions. Decisions that only live in chat history get re-argued or silently violated.

## Decision

We record every decision that is hard to reverse or crosses crate boundaries as an ADR in `docs/adr`, using [template.md](template.md). Accepted ADRs are immutable; changes happen through superseding ADRs.

## Consequences

- Contributors and agents read the relevant ADRs before changing an area; `CLAUDE.md` points to them.
- Reviews can reject a change by pointing to an ADR, or ask for a new one.

## Alternatives considered

- Wiki pages: drift away from the code and are not reviewed with it.
- Only code comments: cannot capture cross-crate reasoning or rejected options.
