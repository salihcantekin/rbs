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
| Default video encode | CPU readback | `YuvDownload` in `rbs-nodes` export, ADR-0038. NVENC and AMF take system memory. VideoToolbox can take an IOSurface; the export node does not select it yet |
| Second canvas | Partial, ADR Proposed | ADR-0093 header is Proposed. Engine steps and add/remove of a vertical canvas are in the tree. Follow-up UI and API in that ADR are not a licence to keep building it before the owner accepts it |
| Source transform commands | In the tree, ADR Proposed | ADR-0084 header is Proposed. Flip, rotate, fit and align commands exist |
| Replay buffer | In the tree, ADR Proposed | ADR-0088 header is Proposed. Host and control API are in the tree |
| Crash isolation | Partial, status disagrees | `catch_unwind` around `Node::process`, `rbs-crash`. ADR-0055 header is Proposed. `docs/adr/README.md` says Accepted. The header wins until the owner adds an approval line |
| Plugin SDK, MCP | Not started | Specs are Draft. No `rbs-plugin-*` or `rbs-mcp` crate. ADR-0006 and ADR-0052 are the plans |
| Performance budgets in CI | Not enforced | ADR-0009 Accepted. CI does not run `cargo bench`. Idle-cost rows in the performance-budgets spec have no workflow |
| Browser source, ISO, markers, loudness, OBS importer, NDI | Not started | Later roadmap and Proposed ADRs 0086–0092 |

Do not treat a Proposed ADR as shipped because code for an early step has merged.
