# ADR-0003: Every external technology behind a port with swappable adapters

- Status: Accepted (amendment proposed in ADR-0019: adapters may also depend on the format-helper layer)
- Date: 2026-11-02

## Context

Media software lives on top of fast-moving external technology: OS capture APIs, GPU vendor encoders, FFmpeg, streaming protocols. Hardware changes every year. We want to replace any of these (for example, FFmpeg) with little effort and let the community add new ones without touching the core.

## Decision

1. Each kind of external technology is a **port**: a crate containing a trait, a capability model expressed as data, error types, and a conformance test suite. Ports: capture, audio I/O, video encode, video decode, audio encode, mux, transport.
2. Implementations are **adapters** in their own crates, depending only on their port, `rbs-core` and their vendor crates.
3. Adapters register with a **registry** at start-up. The engine requests an adapter by **required capabilities** and the registry returns the best match according to a preference order that users and distributions can change.
4. An adapter is accepted when it passes its port's conformance suite on the platforms it claims.
5. Plugins can provide adapters through the same traits (see ADR-0006).

Contract details: [backend ports spec](../specs/backend-ports.md).

## Consequences

- Swapping FFmpeg for native encoders is a configuration change once the new adapters pass conformance.
- New hardware support is a new adapter or a capability update, not a core change.
- More crates and more boilerplate; mitigated by the `add-backend` skill and templates.
- Port traits must be designed carefully because they are hard to change; they follow semver like the plugin SDK.

## Alternatives considered

- Direct use of FFmpeg everywhere (common in Rust media apps): fastest start, but locks the project to FFmpeg's model and licence.
- Compile-time feature flags only: makes swapping possible but not runtime selection, fallback or third-party adapters.
