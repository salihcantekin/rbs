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
- [ ] UI changed? `ui-review` checklist done, dark and light screenshots attached (high-contrast too when surfaces, borders or outlines changed), owner approved the result
- [ ] UI exceptions added (ADR-0115): none | list (`cargo xtask check-ui --against origin/main`)
- [ ] ADR, port or public contract changed? Independent review recorded (`architecture-review`, ADR-0014)
- [ ] Touches `unsafe`, FFI, GPU interop, real-time threads or public API? `risky-review` checklist done and human reviewer requested
