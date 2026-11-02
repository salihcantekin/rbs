## Summary

What changes and why.

## Specs and ADRs

- Spec(s) touched or followed:
- ADR(s) relevant / added:

## Checklist

- [ ] Follows [architecture principles](../docs/architecture/principles.md)
- [ ] Spec updated if a contract changed
- [ ] Tests added or updated (conformance suite for adapters)
- [ ] Inactive features add no cost (P13): no threads, polling, resources or hot-loop checks when unused
- [ ] Hot path touched ([list](../docs/architecture/hot-paths.md))? Before/after numbers and `hot-path-review` checklist included; human maintainer review requested
- [ ] ADR, port or public contract changed? Independent review recorded (`architecture-review`, ADR-0014)
- [ ] Touches `unsafe`, FFI, GPU interop, real-time threads or public API? `risky-review` checklist done and human reviewer requested
