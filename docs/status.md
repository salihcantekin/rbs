# Status

One page for what is true in this tree. A roadmap phase or a "planned" line in an index does not override it. When a change moves a row, update this file in the same pull request.

ADR status is the file header, not only the index. Where those two disagree, both are listed and the header wins until the owner records approval (ADR-0014).

| Item | State | Where |
| --- | --- | --- |
| Headless engine, one master clock, scheduled graph | Running | `rbs-core`, `rbs-graph`, `rbs-engine`. Only `Lifecycle::Active` nodes are scheduled |
| Ports and adapters, vendor names kept out of ports | Running | Overview layer table. `cargo xtask check-layers` enforces it |
| Desktop UI | Running | Slint, ADR-0022 Accepted. Tokens in `rbs-ui`, themes ADR-0057 Accepted |
| Control API | Partial | `rbs-api`, `rbs-api-ws`, `rbs-api-ipc`, served by `rbs-app` while remote control is on. Scene, mute, volume, record, stream, virtual camera, video and audio filters, flip/fit/align, replay. The window still has its own `studio_host::Command` for layout drags, looks and samples. See the control-api spec's implementation status |
| `audio.levels` | Partial | Sampled from source meters while a client is subscribed. `rms_db` repeats the peak. The master meter is not in the payload |
| JSON Schema, `stats`, `ui.*` routing, U20 command parity | Not started | Named in the control-api spec |
| Audio filters (gain, noise gate, ducker) | Running | ADR-0082 and ADR-0095 Accepted. Catalogue, API and Properties |
| Several outputs | Partial | ADR-0047 Accepted. Record and stream together, shared encoder when settings match. `MAX_OUTPUTS` is 2 per the current cap |
| Default video encode | CPU readback | `YuvDownload` in `rbs-nodes` export, ADR-0038. The hardware check is SC-PC: Windows 11, RTX 4090 and the AMD iGPU in that machine (recording spec, 2026-11-03). It was off on 2026-11-10, so the native surface path is not in this tree |
| Second canvas | Partial, ADR Accepted | ADR-0093 Accepted 2026-11-10. Engine steps and add/remove of a vertical canvas are in the tree. Deferred items in the review stay deferred. The Windows measurement is on SC-PC |
| Source transform commands | Partial, ADR Accepted | ADR-0084 Accepted 2026-11-10. Flip, rotate, fit and align exist. Remaining menu categories do not |
| Replay buffer | Partial, ADR Accepted | ADR-0088 Accepted 2026-11-10. Host and control API are in the tree |
| Crash isolation | Partial, ADR Accepted | `catch_unwind` around `Node::process`, `rbs-crash`. ADR-0055 Accepted 2026-11-10. Follow-ups in the review are still follow-ups |
| Plugin SDK | Started | `rbs-plugin-sdk` parses the manifest. `rbs-plugin-host` reads installed manifests and loads nothing. No native, WASM or process loader |
| MCP | Started | `rbs-mcp` answers stdio while RBS is unreachable: the only tool is `studio_summary`. No IPC session yet |
| Compact spacing | Running | Appearance format 2, `density` `regular` or `compact`. Compact steps `space.*` and `size.control.*` down one token |
| Performance budgets in CI | Not enforced | ADR-0009 Accepted. CI does not run `cargo bench`. Idle-cost rows in the performance-budgets spec have no workflow |
| Browser source, ISO, markers, loudness, OBS importer, NDI | Not started | Later roadmap and Proposed ADRs 0086–0092 |

Do not treat a Proposed ADR as shipped because code for an early step has merged.
