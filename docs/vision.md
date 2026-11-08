# Vision

## One sentence

RBS is a live production engine that lets a creator or a small team capture, compose, stream and record with broadcast-level reliability, on commodity hardware, with an architecture that can absorb new hardware, codecs and protocols for the next ten years.

## Who it is for

| User | What they need from RBS |
| --- | --- |
| Streamers and creators | Simple setup, low resource use, multi-platform and vertical + horizontal output at once |
| Small production teams | Remote guests, operator roles, reliable recording, remote control |
| Developers and integrators | A headless engine with a stable API and a plugin SDK in any language |
| Community contributors | Clear contracts, fast feedback, tests that tell them when they are done |

## What makes RBS different from OBS

These are features OBS either lacks or provides only through fragile plugins. RBS treats them as first-class because the core architecture supports them directly.

1. **Multiple canvases**: vertical and horizontal (or any number of) outputs from the same sources, each with its own resolution, frame rate and outputs.
2. **Unlimited outputs**: stream to several platforms with shared or separate encoders, plus local recording, all from one session.
3. **Crash isolation**: untrusted plugins run in a WASM sandbox or a separate process; a failing node is skipped and restarted, the stream continues.
4. **Stable plugin ABI**: a versioned SDK; plugins do not break on every application update.
5. **Free audio routing**: buses, sends, per-output mixes, sidechain; no fixed six tracks.
6. **One clock**: video, audio and every output stamped against a single master clock with drift compensation per device.
7. **Declarative scenes**: scene state is a document; UI, API, scripts and remote panels all issue the same commands, with undo and redo everywhere.
8. **Observability**: per-node latency, queue depth and drop reasons, exportable through `tracing` / OpenTelemetry.
9. **Colour-correct by design**: linear-light compositing, SDR and HDR (PQ/HLG) outputs.
10. **A modern, easy UI by rule**: binding UI rules with timed core tasks, one design system, accessibility and localisation from day one ([UI principles](ui/principles.md)).
11. **Remote guests** (post-1.0 plugin): browser guests over WebRTC, operator roles and moderation.

## Product expectations (owner, 2026-11-08)

- **Lean by default.** RBS must be easy on memory, CPU and GPU. Hot paths may change when a feature needs it, but only done very well: measured before and after, reviewed, approved (P10, P15). A feature that costs resources while unused is a defect (P13).
- **Beyond parity.** Do not copy how OBS (or any other tool) does a feature just because it does. Ask what that feature cannot do today, or does badly because of its technology or history, and design RBS to do it if it is technically possible. RBS is written from scratch, so it can pick designs older tools cannot change. Examples: scene transitions that move matching sources instead of cutting, a browser source that costs nothing while hidden.
- **Say it in the proposal.** A spec or ADR for a user-facing feature states what the reference tools do, where they fall short, and what RBS does better (or why it does not).

## Non-goals

- Feature parity with OBS on day one. We target the 80 % of daily use first, with a better foundation.
- Loading OBS (libobs) plugins. We provide importers for OBS scene collections and shader filters instead.
- A video editor. RBS is live production and recording.
- Our own codec implementations. We integrate existing encoders behind our ports.

## Success criteria for 1.0

- 1080p60 stream plus local recording on a mid-range machine with CPU and GPU usage at or below OBS for the same scene (see [performance budgets](specs/performance-budgets.md)).
- Zero dropped frames caused by RBS itself over a 4-hour soak test on each tier-1 platform.
- A third-party backend (for example, a second encoder) can be added without changes to any core crate.
- At least one plugin written in a language other than Rust runs through the WASM SDK.
