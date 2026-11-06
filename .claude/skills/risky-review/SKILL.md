---
name: risky-review
description: Fresh-eyes checklist for RBS changes touching unsafe code, FFI, GPU interop, real-time threads, cross-thread state, the plugin ABI or public contracts. Run before finishing such a change.
---

# Risky change review

Read the diff as a reviewer who did not write it. Report each item as OK / Issue / N/A with a one-line reason.

## unsafe and FFI
- Every `unsafe` block has a `// SAFETY:` comment stating the invariant and why it holds.
- Pointer lifetimes: nothing outlives the foreign object; no use after the vendor `release`/`destroy` call.
- Ownership across FFI is explicit (who frees what); `Drop` releases every handle exactly once.
- No panics unwind across `extern "C"`: every function or block an OS or vendor library calls wraps its body in `catch_unwind` and returns a neutral value (silence, no frame, an error status). Since Rust 1.81 a panic there aborts the process (ADR-0055 decision 7).
- Thread affinity of the vendor API is respected (COM apartments, Metal command queues, CUDA contexts).

## GPU interop
- Synchronisation between producer and consumer (fences, keyed mutexes, semaphores) is correct on both sides.
- Texture formats and colour spaces match the negotiated link format.
- Pooled textures are not reused while an encoder still reads them.

## Real-time threads
- No allocation, locking, I/O, logging or unbounded loops in audio callbacks or `Node::process`.
- Queues are bounded; full-queue behaviour is defined and counted.

## Crash resilience (ADR-0055)
- Closures handed to Slint go through `rbs-app/src/entry.rs` (clippy enforces it for the Slint functions); a generated `on_*` callback goes through `on!` or starts with `entry::guard` (`tests/entry_points.rs` checks it).
- A new stream secret or URL is passed to `rbs_crash::add_secret` before use.
- Nothing on the UI thread runs a nested event loop or blocks (modal dialogs, `sleep`, `recv`, `join`).
- A new control thread calls `rbs_crash::report_panics_on_this_thread()`; a real-time thread never does.
- A step on the studio's control thread that can panic half-way leaves state `Host::recover` can reconcile.

## Concurrency
- Shared state uses the narrowest primitive; no lock held across an await or a callback into user code.
- Atomic orderings are justified.

## Contracts
- Public API, port traits, plugin ABI, control API or scene format changed? Spec updated, semver impact stated, compatibility tested.

## Tests
- Miri or sanitizer run for new `unsafe` where the platform allows.
- Conformance suite still passes; new failure modes have tests.

Finish by listing remaining risks for the human reviewer.
