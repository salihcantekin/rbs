---
name: add-backend
description: Add a new adapter (backend) for an RBS port such as capture, audio I/O, codec, mux or transport. Use for any new encoder, capture API, protocol or replacement of an existing library like FFmpeg.
---

# Add a backend adapter

Follow these steps in order. Spec: `docs/specs/backend-ports.md`. Decision: `docs/adr/0003-ports-and-adapters.md`.

1. **Identify the port.** Capture, audio I/O, codec, mux or transport. If the technology does not fit any port, stop and propose a spec change instead of bending a port.
2. **Check the port supports what you need.** If the adapter needs a capability key or trait method that does not exist, change the port first (separate PR, spec updated, semver respected).
3. **Create the crate** `crates/rbs-<port>-<name>` (for example `rbs-codec-nvenc`). Dependencies allowed: its port crate, `rbs-core`, vendor/binding crates. Never another adapter, never `rbs-graph`, `rbs-scene` or UI crates.
4. **Platform gating.** Use `#[cfg(target_os = ...)]` at the crate level so the crate builds (as an empty adapter that probes to nothing) on unsupported platforms.
5. **Implement `AdapterFactory`.**
   - `id()`: reverse-DNS, stable forever (`org.rbs.codec.nvenc`).
   - `probe()`: fast (< 50 ms), never panics, returns an empty set when hardware or libraries are missing. Declare only capabilities you will actually honour.
6. **Implement the port traits.** Typed errors, `is_recoverable()` correct, no panics. Real-time-marked methods do not allocate or block.
7. **Run the conformance suite** from the port crate in the adapter's tests (`tests/conformance.rs`). Hardware cases must skip cleanly without hardware.
8. **Register** the adapter in the composition root (`rbs-app`, `rbs-cli`) behind a cargo feature named after the adapter. Add it to the default preference order data only if it should be preferred.
9. **Docs.** Crate-level docs: what it wraps, platforms, licence of the vendor dependency, known limitations.
10. **Measure** with the `measure-performance` skill if it is on the frame path.
11. **Review** with the `risky-review` skill if there is any `unsafe` or FFI.

Done when: fmt, clippy, tests and conformance pass; no engine crate was changed; PR lists the capabilities declared.
