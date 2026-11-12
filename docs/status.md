# Status

One page for what is true in this tree. A roadmap phase or a "planned" line in an index does not override it. When a change moves a row, update this file in the same pull request.

ADR status is the file header, not only the index. Where those two disagree, both are listed and the header wins until the owner records approval (ADR-0014).

| Item | State | Where |
| --- | --- | --- |
| Headless engine, one master clock, scheduled graph | Running | `rbs-core`, `rbs-graph`, `rbs-engine`. Only `Lifecycle::Active` nodes are scheduled |
| Ports and adapters, vendor names kept out of ports | Running | Overview layer table. `cargo xtask check-layers` enforces it |
| Desktop UI | Running | Slint, ADR-0022 Accepted. Tokens in `rbs-ui`, themes ADR-0057 Accepted |
| Control API | Partial | `rbs-api`, `rbs-api-ws`, `rbs-api-ipc`, served by `rbs-app` while remote control is on. Scene, mute, volume, record, stream, virtual camera, video and audio filters, flip/fit/align, replay. The window still has its own `studio_host::Command` for layout drags, looks and samples. See the control-api spec's implementation status |
| `audio.levels` | Partial | Sampled from source meters while a client is subscribed. `rms_db` is the root mean square since the previous sample. The master meter is not in the payload |
| JSON Schema, `stats`, `ui.*` routing, U20 command parity | Not started | Named in the control-api spec |
| Audio filters (gain, noise gate, ducker) | Running | ADR-0082 and ADR-0095 Accepted. Catalogue, API and Properties |
| Several outputs | Partial | ADR-0047 Accepted. Record and stream together, shared encoder when settings match. `MAX_OUTPUTS` is 4 in total and `MAX_OUTPUTS_PER_CANVAS` is 2 (ADR-0093 decision 6) |
| Default video encode | CPU readback | `YuvDownload` in `rbs-nodes` export, ADR-0038. The hardware check is SC-PC: Windows 11, RTX 4090 and the AMD iGPU in that machine (recording spec, 2026-11-03). It was off on 2026-11-10, so the native surface path is not in this tree |
| Second canvas | Partial, ADR Accepted | ADR-0093 Accepted 2026-11-10. Engine steps, add/remove, the Add vertical output dialog and `canvas.add_vertical` / `canvas.remove_vertical` (protocol 1.28) are in the tree. Deferred items in the review stay deferred. The Windows measurement is on SC-PC |
| Source transform commands | Running, ADR Accepted | ADR-0084 Accepted 2026-11-10. Transform, Arrange (incl. front/back, protocol 1.36), Visibility, Source and Remove as menu submenus; Properties icon row is 90° only (180° is two clicks). Locked sources disable transform UI. Multi-selection still deferred |
| Replay buffer | Partial, ADR Accepted | ADR-0088 Accepted 2026-11-10. Host and control API are in the tree |
| Crash isolation | Partial, ADR Accepted | `catch_unwind` around `Node::process`, `rbs-crash`. ADR-0055 Accepted 2026-11-10. Fault injection covers recording, stream start and end, and a canvas restart. A separate studio process is still follow-up work |
| Plugin SDK | Started | `rbs-plugin-sdk` parses the manifest. `rbs-plugin-host` reads installed manifests and loads nothing. No native, WASM or process loader |
| MCP | Started | `rbs-mcp` answers stdio while RBS is unreachable: the only tool is `studio_summary`. No IPC session yet |
| Compact spacing | Running | Appearance format 2, `density` `regular` or `compact`. Compact steps `space.*` and `size.control.*` down one token |
| Performance budgets in CI | Not enforced | ADR-0009 Accepted. CI does not run `cargo bench`. Idle-cost rows in the performance-budgets spec have no workflow |
| Browser source, ISO, markers, loudness, OBS importer, NDI | Not started | Later roadmap and Proposed ADRs 0086–0092 |
| Configurable glide (duration / easing) | Done | ADR-0100 Accepted. Collection defaults (Settings › General) and per-source override (Properties); scene format 12; protocol 1.35. Not on the Transition rail or panel |

Do not treat a Proposed ADR as shipped because code for an early step has merged.

## Owner backlog (2026-11-11)

Not scheduled. Each item already has a decision or a partial start. None of them is in the app yet, except where a row says so.

| Item | State | Where |
| --- | --- | --- |
| Separate files next to the master | Not started | ADR-0086 Proposed. One master file, plus each chosen source's own picture (the camera's full frame, the screen capture) and each chosen source's own audio, aligned on the master clock so an editor can recut. Nothing in the tree records a source separately |
| Say it will not fit, and check the disk, before Record or Go live | Not started | ADR-0087 Proposed (encoder sessions, throughput, disk space and write speed before the start). ADR-0090 Proposed (while a take runs: time left on disk, stop cleanly before the disk is full). The replay length estimate is unrelated |
| A different mix for the stream, the recording and the headphones | Partial | ADR-0097 Accepted. Headphones (monitor) are in the tree. A strip still has one mix for stream and recording. Buses, so an output can omit music from the recording while the stream keeps it, are not built |
| A failed piece does not end the show, and says why in one sentence | Partial | A node that panics is skipped (`catch_unwind`, ADR-0055). The window still dies with the studio (ADR-0099 Proposed, not built). A source fault is not yet one plain sentence on screen and on the control API |
