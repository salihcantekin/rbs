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
| Second canvas | Partial, ADR Accepted | ADR-0093 Accepted 2026-11-10. Engine steps, add/remove, the Add vertical output dialog and `canvas.add_vertical` / `canvas.remove_vertical` (protocol 1.28) are in the tree. ADR-0111 Accepted 2026-11-13: Record and Go live start and stop the vertical outputs too ("Main + vertical" by default), `canvas: "all"` and `output.set_with_vertical` (protocol 1.42), and the registry entries are in the tree; the chip, notice and hint are built and wait for the owner's screenshot approval. Deferred items in the review stay deferred. The Windows measurement is on SC-PC |
| Source transform commands | Running, ADR Accepted | ADR-0084 Accepted 2026-11-10. Transform, Arrange (incl. front/back, protocol 1.36), Visibility, Source and Remove as menu submenus; Properties icon row is 90° only (180° is two clicks). Locked sources disable transform UI. Multi-selection still deferred |
| Replay buffer | Partial, ADR Accepted | ADR-0088 Accepted 2026-11-10. Host and control API are in the tree |
| Crash isolation | Partial, ADR Accepted | `catch_unwind` around `Node::process`, `rbs-crash`. ADR-0055 Accepted 2026-11-10. Fault injection covers recording, stream start and end, and a canvas restart. A separate studio process is still follow-up work |
| Plugin SDK | Started | `rbs-plugin-sdk` parses the manifest. `rbs-plugin-host` reads installed manifests and loads nothing. No native, WASM or process loader |
| MCP | Started | `rbs-mcp` answers stdio while RBS is unreachable: the only tool is `studio_summary`. No IPC session yet |
| Compact spacing | Running | Appearance format 3, `density` `regular` or `compact`, `compact_steps` 1 to 3. Compact steps `space.*`, `size.control.*` and the panel gaps; floors hold |
| Performance budgets in CI | Not enforced | ADR-0009 Accepted. CI does not run `cargo bench`. Idle-cost rows in the performance-budgets spec have no workflow |
| Browser source, ISO, markers, loudness, OBS importer, NDI | Partial | Browser type and node are in the tree (ADR-0109 Proposed); the studio adopts the node and does not link CEF. Media-file type and node are in the tree (ADR-0108 Proposed); the studio adopts the node and does not link FFmpeg. ISO has started (ADR-0086 Accepted): the source flag, the start plan, the start manifest, and separate audio files (partitions 17 and 18, not an output slot). AAC in the master's container, PCM in WAV, and FLAC in a native FLAC file (ADR-0085 Accepted) can be opened. PCM and FLAC separate files take `bit_depth` the same way as the main recording; absent is 16-bit. Video files are named and not opened. Mov, finalize-on-stop and the encoder option schema are later slices of ADR-0085. Markers, loudness, the OBS importer, and NDI stay later roadmap; ADRs 0087 and 0089–0092 are Proposed |
| Configurable glide (duration / easing) | Done | ADR-0100 Accepted. Collection defaults (Settings › General) and per-source override (Properties); scene format 12; protocol 1.35. Not on the Transition rail or panel |
| Mixer orientation | Running, ADR Accepted | ADR-0083 Accepted 2026-11-12. `auto`, rows and columns, saved in the workspace `panel_options`. The Layout menu and `ui.mixer.set_orientation` are in the tree |
| Mixer columns | Running, ADR Accepted | ADR-0101 Accepted 2026-11-12. Column strips keep `size.mixer.strip`, clusters sit side by side, and the row scrolls horizontally |
| Output size | Partial, ADR Accepted | ADR-0102 Accepted 2026-11-12. Each recording and stream stores its own size; equal size stays the 1:1 download. macOS is done. The Windows side remains |
| Audio-only recording | Started | The main recording can omit video. Settings Format is Video or Audio, then the container. Audio is WAV or FLAC (protocol 1.45): WAV is PCM, 16-bit by default, 24-bit and 32-bit float; FLAC is 16-bit or 24-bit integer, with no picture and no size patch. Salih approved the WAV control on 2026-11-13. A computer with no video encoder can still record WAV or FLAC. The new Format card is not built. WAV write and PCM submit are signed off the same day (about 2.4 µs per 10 ms stereo block, 12 ns per WAV packet; see hot paths). Record, the status and the program picture say so. One 4096-sample stereo frame measured 118 µs; that sign-off is still open |

Do not treat a Proposed ADR as shipped because code for an early step has merged.

## Owner backlog (2026-11-11)

Not scheduled. Each item already has a decision or a partial start. None of them is in the app yet, except where a row says so.

| Item | State | Where |
| --- | --- | --- |
| Separate files next to the master | Started | ADR-0086 Accepted 2026-11-13. A source can be marked to record separately (scene format 16, only when set). `Studio::start_iso_audio` opens one audio file per audio flag, on partitions 17 and 18, tapped before the fader. AAC in the master's container, PCM in WAV, and FLAC are registered, and PCM and FLAC pass `bit_depth` (absent is 16-bit). Video files, off-air sources, the cost warning and the UI are not in this tree |
| Say it will not fit, and check the disk, before Record or Go live | Not started | ADR-0087 Proposed (encoder sessions, throughput, disk space and write speed before the start). ADR-0090 Proposed (while a take runs: time left on disk, stop cleanly before the disk is full). The replay length estimate is unrelated |
| A different mix for the stream, the recording and the headphones | Partial | ADR-0097 Accepted. Headphones (monitor) are in the tree. A strip still has one mix for stream and recording. Buses, so an output can omit music from the recording while the stream keeps it, are not built |
| A failed piece does not end the show, and says why in one sentence | Partial | A node that panics is skipped (`catch_unwind`, ADR-0055). The window still dies with the studio (ADR-0099 Proposed, not built). A source fault is not yet one plain sentence on screen and on the control API |
| Picture and sound on one source | Partial | ADR-0106 Proposed. Binder `picture_sound` and `assign_queue_readers` are in the tree; the studio reconciles readers after apply. Ship waits on acceptance |
| Temporal sources sleep when hidden | Partial | ADR-0107 Proposed. Media and browser are Active only while a shown scene draws them (or a timed-show cue). Capture and stills stay wanted while placed. The ADR is not accepted |
| A local media file as a source | Partial | ADR-0108 Proposed. Type and node in the tree with RGBA `SourceUpload`. FFmpeg is not linked; `rbs-app` always registers `ScriptFactory` until an FFmpeg adapter ships. Add Source → Media file opens a path picker |
| A browser page as a source | Partial | ADR-0109 Proposed. Type and node in the tree. `rbs-app` starts `rbs-browser-helper` (bright synthetic checker, no CEF). Add Source → Browser places a page. Cap 2 helpers; no audio pin, no JS, no device prompts |
| Source region (not item crop) | Partial | ADR-0103 Proposed. Source settings: region_grab preview + drag, Apply also Cover-fits the item on the canvas |
| Display chrome (hide taskbar / Dock) | Partial | ADR-0104 Proposed. Source settings: one edge + thickness UI (stores four insets); Reset from work_rect; Fit |
| Timed show (show for N ms, no undo) | Partial | ADR-0105 Proposed. Properties Show now + duration; Sources bolt while hidden; eye is lasting visibility only |
