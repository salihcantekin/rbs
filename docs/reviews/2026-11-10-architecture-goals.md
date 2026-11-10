# Architecture goals review (2026-11-10)

Reviewed against `main` at `63e3919` (10 November 2026, 18:31 UTC). This is an audit of whether the tree serves the product goals, not a new ADR. Rows that moved after that commit are called out. Living status for agents is [status.md](../status.md).

The product is a headless live engine that should beat OBS on hardware cost, crash isolation, several outputs, a consistent customisable UI, and an API other programs can drive (a Stream Deck is the example). It is written incrementally by people and by agents, so a rule that is not in a spec and not enforced is not a rule.

## Verdict

The foundation matches the goals in the places that are hardest to add later: ports and adapters, a scheduled media graph, a real control API, and a token-based UI. Audio hand-off is a fixed lock-free ring (`crates/rbs-audio-io/src/ring.rs`). Only `Lifecycle::Active` nodes are scheduled (`GraphDescription::plan` in `crates/rbs-graph/src/description.rs`). Record and stream can run together and share an encoder when settings match (ADR-0047). The desktop UI is Slint over design tokens, with user themes, workspaces and shortcuts.

What is not true yet, despite the vision and several accepted ADRs, is GPU-resident encode on the default path, a plugin SDK, an MCP server, and "the UI is only a client of the control API." The export node reads every canvas frame back to the CPU (`YuvDownload` in `crates/rbs-nodes/src/export.rs`, ADR-0038). The app drives the studio through `studio_host::Command` in-process. `rbs-api::Command` is a second, smaller set. A Stream Deck can already switch scenes, mute, set volume, go live, record and start the virtual camera, once remote control is on. It cannot drag a source, apply a look, or read a meter. Meters are addressed in this change (`audio.levels`).

Since the first pass over an older branch, `main` has taken in audio filters and their UI (ADR-0082), the ducker (ADR-0095), flip/fit/align commands (ADR-0084, header still Proposed), the first vertical-canvas steps (ADR-0093, header still Proposed), the replay buffer (ADR-0088, header still Proposed), capture-audio companions (ADR-0096) and the UI interaction patterns (ADR-0094, Accepted). Those are not open work. Building them again would fork the tree.

## 1. Hardware friendliness

**What holds.** Hot paths are listed in `docs/architecture/hot-paths.md`. The audio producer never waits and never allocates. Packet and audio queues between partitions are bounded and preallocated. The compositor fills pre-reserved staging buffers and skips layers whose crop is empty. Colour conversion to NV12/P010 is a GPU pass. A still image is uploaded once. A video filter whose input and settings did not change skips its passes. Disabled filters are `Standby` and are not linked. An empty partition has no worker. Encoder and output steady-state ticks are tested to allocate nothing. Hardware encoder session logic is shared in `rbs-codec-hw`. Bitstream helpers are shared in `rbs-media-format`.

**What does not.** The default video encode path pays a CPU readback on every frame, then a second copy into the encoder, on Windows and Linux. ADR-0038 decision 3 is an explicit temporary exception to P4: `VideoExportNode` always uses `YuvDownload`. `rbs-codec-nvenc` states that input is system memory only. macOS has a native exporter (ADR-0025) that the node does not select, because a native frame cannot cross the partition queue yet. The exit criteria are written: native input, a queue that can move a single-consumer native frame, and a capability-based choice. They are follow-ups, not code. This change does not touch that path. It needs measurements on the machines named in the hot-path sign-off, and a human sign-off (P15), before it is implemented.

`rbs-gpu` and `rbs-filters` do not call `allocation_counter`. CI does not run `cargo bench` (ADR-0009). A layer at opacity 0 stays scheduled. A second canvas in the document is not free while ADR-0093's studio still builds a compositor for every canvas that has a program scene; the running studio's second canvas is the partial implementation already on `main`, and the ADR is still Proposed.

**This change.** `LevelMeter::record` also updates a second atomic peak so the control API can read levels without clearing the Mixer's peaks. That is one extra `fetch_max` per channel per block, with no "is anyone subscribed" branch (P13). `record` and `observe` are covered by the existing allocation test. A before/after mixer benchmark still belongs in the pull request before merge (P15).

## 2. Documents agents can extend the system with

`CLAUDE.md`, the specs and the skills are enough for an agent to add an adapter or a node without inventing a port. They were not enough to stop an agent treating a Proposed ADR as shipped, or a README line that still said "planned" as the truth.

Fixed in this change:

- [status.md](../status.md) is the page that says what is running, partial, or not started, and which ADR header disagrees with the index.
- `docs/specs/README.md` no longer says the control API and `rbs-ui-tokens` are planned. Those crates exist (`rbs-ui` holds the tokens).
- `docs/roadmap.md` no longer says Slint is pending a GPU run. ADR-0022 is Accepted.
- `CLAUDE.md` tells an agent to stop on a Proposed ADR, to prefer a spec's implementation status over an index, and not to add another CPU readback beside `YuvDownload`.
- `cargo xtask check-layers` is the P1 test that was labelled planned. It is in CI.

Still open, and not guessed here:

- ADR-0055's header says Proposed and `docs/adr/README.md` says Accepted. The header wins until the owner adds an approval line. This change does not flip it.
- ADR-0084, ADR-0088 and ADR-0093 have code on `main` and headers that are still Proposed. Same rule.
- JSON Schema generation, the U20 test that every UI command is an API method, and `stats` sampling are still "not yet" in the control-api spec.

## 3. Maintainability

The crate graph is maintainable. Ports do not depend on adapters, and the new check fails the build if a future change adds that edge. `rbs-codec-hw` and `rbs-media-format` already hold the shared encoder and bitstream code. Errors on the graph are typed.

`crates/rbs-app/src/studio_host.rs` is still the module that owns both command enums, the studio lifecycle, recording, streaming, the virtual camera and the control-API target. `studio_host::Command` still has layout drags, looks and samples that `rbs_api::Command` does not. Splitting the file before those commands are one path would freeze the duplication. That unification is the right refactor and it is large enough to be its own change, with the control-api spec updated method by method. It is not done here.

Audio filters are no longer a third path. `studio_host/filters.rs` looks up video and audio catalogues, and the API edits both.

## 4. API, plugins, and a Stream Deck

A pad can already switch scenes, mute, set volume, record, go live, start the virtual camera, edit video and audio filters, and flip, fit or align a source, with remote control on. IPC is served only while remote control is on. That matches ADR-0013. There is no unauthenticated socket.

Still UI-only: preview drags (`SetSourceBox`, `MoveSource`), looks (`ApplyLook`) and sample stills (`AddSample`). `ui.*` routing and generated JSON Schema are not implemented. There is no plugin crate. A Stream Deck does not need a plugin ABI. It needs the missing layout methods, levels, and a generated client. Levels are the part this change adds.

`audio.levels` (control-api spec): while a client is subscribed, the API thread reads each running audio source's meter with `LevelMeter::observe`. Peaks are in dBFS, silence is −120, and `rms_db` repeats `peak_db` until an RMS accumulator exists. The master meter is not in the payload. Nobody subscribed means the sampler is not called.

Do not start the plugin SDK or MCP until the UI and the API share one command path. ADR-0006 and ADR-0052 stay the plans.

## 5. UI

The UI system serves "one design system, customisable, no hard-coded visuals" better than the encode path serves P4. Tokens, themes (colours, type, motion, font), workspaces, panel visibility, collapse, auto-hide and shortcut rebinding are real. A test rejects stray `px` and hex colours in `.slint`.

Not styling gaps: several canvases, and transitions that move sources. The rail's transition is Cut. Windows meters are still drawn by Slint; native meter layers exist on macOS only. That is a visible change. Rule 16 says it is proposed to the owner, with screenshots in dark, light and high contrast, before it is implemented. This change does not restyle anything.

A density token (compact / regular) that only changes existing space and control-size tokens would help power users. It is a visual change and needs the same approval. It is not implemented here.

## 6. Zero cost when unused

The lifecycle is real. Disabled filters are Standby. Capture sessions exist between `prepare` and `release`. Remote control off constructs no API thread and no socket. The virtual camera is off until the user starts sending.

Not yet a guarantee: the idle-cost rows in `docs/specs/performance-budgets.md` have no CI job. Opacity 0 does not remove a source from the tick. A canvas that is only data in the file can still get an active compositor when it has a program scene; ADR-0093 is the specified fix and is Proposed.

`audio.levels` adds no thread and no timer. The API thread samples only when a subscriber's interval is due. The extra atomic in `LevelMeter::record` runs on every block whether or not a client is connected, because a branch in that function would be the per-iteration feature check P13 forbids.

## 7. Beyond OBS

| Vision item | State on `63e3919` |
| --- | --- |
| One master clock, leased frames, scheduled graph | Running |
| Several outputs, shared encoder when settings match | Partial, cap of two, ADR-0047 Accepted |
| Several canvases, sources opened once | Partial. Model supports many canvases. A vertical second canvas can be added and removed. ADR-0093 is Proposed |
| Crash isolation of a bad node | Partial. A UI-process crash or a native fault still ends the stream. ADR-0055 header is Proposed |
| Stable control API | Partial. See section 4 |
| Plugin ABI / WASM / MCP | Not started |
| Audio filters, ducker, capture-audio companions | Running |
| Replay buffer | In the tree, ADR-0088 Proposed |
| Browser source that costs nothing while hidden | Not started |
| ISO, markers, loudness, adaptive bitrate, OBS importer | Not started (Proposed ADRs or roadmap) |

Order that matches the vision, after this change:

1. Native encode hand-off (ADR-0038 exit), measured, with a human sign-off. More outputs on top of a CPU readback multiply the wrong cost.
2. One command path, so every window action is an API method, then generated JSON Schema. That is the integration promise.
3. ADR-0093 only after its header can say Accepted. The early steps already on `main` should not grow while the ADR is Proposed.
4. Then a browser source that is Standby while hidden, and a plugin host whose first test is "installed manifests load nothing."

## Done in the change that added this file

- This review, plus `docs/status.md`.
- `cargo xtask check-layers` in CI.
- Stale "planned" and "Slint pending" lines corrected.
- Stop rules in `CLAUDE.md` and the node and hot-path skills.
- `audio.levels` from the source meters, without clearing the Mixer.

## Not done, and why

| Item | Why it is not in this change |
| --- | --- |
| Native GPU encode (NVENC, AMF, VA-API) | Hot path (P15). Needs before/after `frames_cpu_copied` on the Windows machine in the sign-off table, and the owner's approval. This environment cannot measure that machine |
| Folding `studio_host::Command` into `rbs_api::Command` | The right P0, and a behaviour change across every window action. It should land as its own reviewed change, spec first, not beside the layer check |
| Accepting ADR-0055, ADR-0084, ADR-0088, ADR-0093 | ADR-0014: the owner records approval. Code and headers disagree; this file does not pick a winner |
| Remaining ADR-0093 UI (panels, guided command) | The ADR is Proposed, and the UI parts are visible (rule 16) |
| Windows native meter layers, density token | Visible UI. Rule 16: propose, then screenshots, then the owner's approval |
| `cargo bench` as a 5% CI gate | ADR-0009. Criterion on shared runners is noisy. A gate that fails without a code change blocks the tree (ADR-0032). Needs a runner decision |
| Plugin SDK and MCP | The audit's own order: after API parity. No crate until then |
| JSON Schema and a TypeScript client | Spec'd, and the natural follow-up to the single command path |
