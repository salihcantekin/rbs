# ADR-0031: Adapter support crates

- Status: Accepted
- Date: 2026-11-03
- Deciders: Salih Cantekin (owner; early-phase policy lets threads accept ADRs), independent AI session (review)
- Amends: ADR-0003 (adapters may also depend on adapter support crates of their port)

## Context

ADR-0003 lets an adapter depend only on its port, `rbs-core` and vendor crates; ADR-0019 added the format-helper crate `rbs-media-format`. The hardware video encoder adapters (NVENC merged; Intel QSV, AMD AMF and VA-API next) share about 1,300 lines of vendor-neutral logic: the slot ring and the encoder port contract (ordering, keyframes, `Busy`, refusals, error handling), the capability set derived from a probed device, and the configuration checks. Copying it into each adapter would mean four copies to keep in step and four places for the same contract bug. The logic is not a port (the engine never sees it) and not a format helper (it is about driving an encoder, not about bitstream formats).

## Decision

1. An **adapter support crate** holds code that several adapters of one port share and that contains no vendor code. It is named after its port (`rbs-codec-hw` for hardware video encoders).
2. It may depend on its port, `rbs-core` and the format helpers only. It never depends on a vendor library, an SDK binding, an OS media API or an adapter, and it holds no `unsafe` FFI.
3. Adapters of that port may depend on it. Nothing else does: not the engine, not other ports, not the composition root (which still sees only the adapters' factories).
4. Its API is internal to the workspace's adapters; it is not a stable contract for third-party plugins and may change together with its adapters. Plugin adapters get the port contract only (ADR-0006).
5. Its behaviour that adapters rely on is specified in the port's spec (`docs/specs/output-ports.md`, "Hardware encoder sessions"), and the port's conformance suite runs against it (over a fake engine) on every CI runner.
6. A new adapter support crate, or a new dependency of one, is a crate layering change and needs an independent review (ADR-0014). Its hot-path code follows P15 like adapter code.

## Consequences

- The QSV, AMF and VA-API adapters keep only their loader, bindings and engine; a contract fix lands once.
- The port contract of every hardware encoder is tested on every CI runner, without hardware.
- One more layer in the overview table ("Adapter support"), with its dependency rule.
- A change in the support crate can affect every adapter at once; its tests and the conformance suite over the fake engine guard that, and hardware runs per vendor remain needed.

## Alternatives considered

- **Copy the logic into each adapter.** Simple, no new layer, but four copies of a subtle contract (the `Busy`/drain rule, IDR after flush) drift apart.
- **Put it in the port crate `rbs-codec`.** The port would then carry implementation helpers for one family of adapters and change whenever they do; ports stay contracts only.
- **Make NVENC the shared crate and let other adapters depend on it.** Adapters never depend on each other (ADR-0003); it would also pull NVIDIA bindings into every adapter.

## Review

- Reviewer: independent AI session (fresh context, given the change, specs and code only), 2026-11-03.
- Verdict: approve with changes.
- Concerns and resolution:
  - Layering needs an ADR amending ADR-0003, as ADR-0019 did: this ADR.
  - Session code is on the encoder hot path: measured before and after (PR #56, `cargo bench -p rbs-codec-hw --bench session`; bookkeeping per frame 258-287 ns after vs 274-362 ns before, within noise; the 1080p row copy dominates at about 0.3 ms); human sign-off pending in the owner's big review per the early-phase merge policy.
  - A compile-time slot count does not fit engines that pick their queue depth at run time: `Engine::slots()` is read when the session is created.
  - Queue-based engines (AMF, QSV) were not described: the `Engine` docs and the spec now say how they map.
  - The profile set could hold only 8 profiles: widened to 32.
  - AV1 and native-surface input are not in the shared plan: stated as known limits in the spec and crate docs; they are added here when the first adapter needs them.
  - Test devices were compiled into runtime builds: moved behind the `testing` feature.
  - `flush` with nothing in flight did not re-arm the IDR, and `copied` counts a retried frame twice: the first is fixed, the second is documented.

- Record, 2026-11-03: the owner (Salih) signed off the human hot-path review (P15) for this decision; see the sign-off table in `docs/architecture/hot-paths.md`.
