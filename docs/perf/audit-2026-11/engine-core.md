# Performance audit: engine core, threads, memory, control plane

Scope: rbs-core, rbs-engine, rbs-graph, rbs-scene, rbs-api, rbs-api-ipc/ws, rbs-app `studio_host.rs` (+ `user_session.rs`, `main.rs` glue), rbs-record `studio.rs`, rbs-crash, rbs-plugin-host, rbs-params, rbs-mcp, rbs-thread-os.
Method: reading code only (no build, no bench). All benefits are estimates; reasoning is given per item.
Note: no `tokio`, `rayon`, `tracing` or `log` crate is used anywhere; all threads are plain `std::thread`. The architecture overview's thread table still lists "Network: transports on a `tokio` runtime" (docs/architecture/overview.md:66), which no longer matches the code (doc drift, fix in a docs-only change).

## Thread inventory

"Idle" = app open, studio started, nothing on air, no remote control.

| Thread name | Spawned at | Exists when | Wakes how often | Needed? |
| --- | --- | --- | --- | --- |
| UI main (Slint event loop) | rbs-app `main.rs` | always | event-driven; `live_timer` 1 Hz only while live (main.rs:2125), `meter_timer` 33 ms only while Mixer shown and meters exist (main.rs:2301), settings `save_timer` single-shot debounce | yes |
| `rbs-studio` (control thread) | crates/rbs-app/src/studio_host.rs:3299 | always | **at least every 50 ms** (`MAX_WAIT`, studio_host.rs:102) even at idle: 20 wakes/s; plus each message | yes, but over-polls (G-05) |
| `rbs-user-data` (single writer) | crates/rbs-app/src/user_session.rs:133 | always (with a data dir) | condvar, event-driven; one write per queued save (no time debounce, G-03) | yes |
| `rbs-render` | crates/rbs-engine/src/worker.rs:402 (name from part.rs:150) | while render partition schedule is non-empty (a compositor has a consumer: preview shown, output, projector) | canvas cadence (30/60 Hz) | yes |
| `rbs-partition-N` (audio, encoders, outputs, replay, ISO; up to 19 ids, recording.rs:37-76) | worker.rs:402 | only while that partition has nodes; stopped when its schedule goes empty (part.rs:173) | audio: 100 Hz (480-frame blocks); encoders: frame cadence + offset; outputs: doorbell rings + 100 ms heartbeat (`DEFAULT_DOORBELL_TIMER`, worker.rs:53) | yes; good P13 |
| `rbs-api` | crates/rbs-api/src/thread.rs:159 | only when remote control / AI tools are on | event-driven (`wake.wait(next_deadline)`) | yes |
| `rbs-api-ws` | crates/rbs-api-ws/src/lib.rs:220 | when WebSocket transport on | poller, event-driven | yes |
| `rbs-api-ipc accept` | crates/rbs-api-ipc/src/lib.rs:137 | when IPC transport on | blocking accept | yes |
| `rbs-api-ipc read` / `write` | crates/rbs-api-ipc/src/conn.rs:237, :243 | **2 threads per IPC client** | blocking I/O | could be 0 extra (G-15) |
| `rbs-region-grab` | studio_host.rs:5452 | transient, per region-picker open | one-shot | yes |
| `rbs-thumbnail-0/1` | studio_host.rs:5497 | transient, while a target list loads | until queue empty | yes |
| `rbs-picker` | studio_host.rs:5542 | transient, while OS picker is up | blocking | yes |
| `rbs-text-measure` | studio_host.rs:15896 | transient, **one new thread per text measure** (add text, fit box) | one-shot | could reuse text worker (G-17) |
| `rbs-calibration*` | crates/rbs-app/src/studio_host/calibration.rs:267, :380, :643 | only during a sync calibration run | run-specific | yes |
| `rbs-replay-clip` | crates/rbs-app/src/studio_host/replay.rs:311 | transient, per saved clip | one-shot | yes |
| `rbs-preview-connect` / `rbs-preview` | crates/rbs-app/src/preview.rs:83, :142 | while a preview panel is shown | per frame + 250 ms recv timeout | yes (one `invoke` closure alloc per frame) |
| `rbs-projector-connect` / `rbs-projector` | crates/rbs-app/src/projector.rs:884, :937 | per open projector (macOS/Windows) | per frame + 250 ms | yes |
| app theme | crates/rbs-app/src/win/caption.rs | Windows, on the UI thread | `WM_SETTINGCHANGE` (`ImmersiveColorSet`, high contrast) | no extra thread |
| `rbs-vcam-setup` | crates/rbs-app/src/vcam_page.rs:321 | transient | one-shot | yes |
| reveal helper | crates/rbs-app/src/reveal.rs:20 | transient | one-shot | yes |
| `rbs-text-render` / `rbs-text-probe` | crates/rbs-sources/src/text_worker.rs:350, :372 | while text sources exist; stop 30 s after last use (`IDLE_STOP_AFTER`) | on request; on-air file polling every 500 ms (`POLL_INTERVAL`) | yes; file poll could be a watcher |
| `rbs-media` | crates/rbs-nodes/src/media/player.rs:250 | per open media source | decode pace | yes |
| `rbs-replay-reaper` | crates/rbs-nodes/src/replay_node.rs:283 | while replay buffer runs | on channel | yes |
| `rbs-browser-supervisor` | crates/rbs-browser-runner/src/lib.rs:304 | per browser source | **polls 2x page fps (120 Hz at 60 fps), 200 Hz with sound** (`SOUND_POLL` 5 ms, lib.rs:263) | over-polls (G-09) |
| `rbs-estimate-probe` | crates/rbs-record/src/estimate/probe.rs:170 | transient, on demand | one-shot | yes |
| protection probe | crates/rbs-record/src/protection/probe.rs:107 | only during a take | every 5 s | yes |
| `*-janitor` graveyards (sck, wgc, avf, media-mf, media-avf) | crates/rbs-capture/src/handoff/graveyard.rs:161 | only while handles wait for their grace; exits when empty | `sleep(period)` = 1 s | yes; good P13 |
| capture threads | crates/rbs-capture-pipewire/src/linux.rs:1750, crates/rbs-capture-sck/src/mac.rs:1570 | per active capture | device-driven | yes |
| audio I/O threads | crates/rbs-audio-io-pipewire/src/linux.rs:514, crates/rbs-audio-io-wasapi/src/win.rs:756, win/render.rs:159 | per open device / monitor | device period | yes |
| transport sessions | rbs-transport-rtmp session.rs:331, -srt session.rs:305, -file session.rs:303 | per output | I/O driven | yes |
| `rbs-vcam-mf` factory | crates/rbs-vcam-mf/src/win/factory.rs:306 | Windows vcam | | yes |
| `rbs-control` | crates/rbs-engine/src/handle.rs:119 | only via `EngineHandle`, which no crate outside rbs-engine uses | - | not used by the app |

Idle summary: about 2-3 permanent threads (UI, `rbs-studio`, `rbs-user-data`) plus `rbs-render` only when a preview is shown. The Windows app theme arrives as `WM_SETTINGCHANGE` on the UI thread. The one P13 outlier at idle is the control thread's fixed 20 Hz wake (G-05).

## Findings (ordered by benefit / effort)

### G-01 Engine::apply re-sends every partition, and waits for each in turn, on every edit
- Location: crates/rbs-engine/src/engine.rs:509-512 (`for (p, part) in std::mem::take(&mut lowered.parts) { self.current.insert(p, part); self.dirty.insert(p); }`), :536-540 (phase 2 sends every dirty partition), :559-605 `send_current` (`handle.settle(timeout)` per partition, waits up to `swap_timeout` = 1 s); crates/rbs-graph/src/partition.rs:381-392 (`lower` emits a part for every partition that has nodes); crates/rbs-engine/src/part.rs:195-206 (`settle` = `worker.wake()` + `collect_timeout`).
- Problem: every `Studio::update` (every scene edit, including each step of an item drag, see G-02) lowers the whole graph and marks every non-empty partition dirty. Each one gets `Controller::apply`: `plan()` (HashMaps/BTreeSets, crates/rbs-graph/src/description.rs:486), a new `Schedule` (new slots, new audio buffers: G-12) sent to the worker, then a blocking `settle` that waits until that worker swaps. Partitions are settled one after another. A cadence worker that is in the middle of a tick only swaps after the tick ends, so the control thread waits for the rest of that tick. While streaming and recording there are about 5-8 partitions (render, audio, video and audio encoder per output, output, replay). With a software encoder a tick can take 5-15 ms, and a drag step can block the control thread for several such waits even though the edit changed nothing in that partition.
- Proposal: keep a per-partition fingerprint or revision of the lowered part (node ids + state + `Arc::as_ptr(descriptor)` + partition + links, or a revision counter that `GraphDescription` bumps on every mutating call) and skip `dirty.insert(p)` when the part is equal to the one sent last. Unchanged partitions then get no apply, no new schedule and no settle. Second step: settle the parts that did change together: `apply` + `wake` all of them first, then `collect_timeout` each. Total wait then becomes the slowest partition, not the sum.
- Benefit: estimate. For a transform-only edit while live, the partitions that are sent drop from about 6 to about 1 (render) or 0 when combined with G-02. Control-thread time per edit goes from roughly 0.5-20 ms (6 sequential settles, each about 50 µs wake latency up to one tick) to roughly 0.1-3 ms. RT threads also skip a useless schedule swap per edit.
- Effort: M
- Risk: Med. The fingerprint must include everything `Controller::apply` reads (faults or back-off gates: `restart_due` already forces dirty in `service`). Tests in rbs-engine cover apply semantics.
- Kind: structural; Hot path: no (control thread; the RT swap it avoids is in `Executor::maintain` but the change is on the control side)

### G-02 An item move, resize or crop rebuilds the whole studio (binder sync, graph apply, publish, autosave) for every drag step
- Location: crates/rbs-app/src/studio_host.rs:13940-13957 (`C::ItemSetBox` -> `host.apply(notify)`), 4931-4975 (UI `SetSourceBox` -> `run_ui_api`), 12566-12585 (`apply` -> `studio.update` -> `publish`); crates/rbs-record/src/studio.rs:2214-2290 (`update_with`: `with_followers` clone, `sync_cued`, `self.collection = collection.clone()`), 2806 (`self.engine.apply(&self.graph)` always runs); UI throttle: crates/rbs-app/src/preview_edit.rs:68 (`in_flight`, one request at a time).
- Problem: the compositor already receives its layers through a latest-value param channel (`LayerSender = ParamSender<Vec<NodeLayer>>`, crates/rbs-gpu/src/node.rs:209; ADR-0023). Even so, a pure transform edit takes the whole path: run-collection copy, binder `sync` over every source, item and scene (BTreeMap and BTreeSet building, settings deep compares per source, crates/rbs-scene/src/binder.rs:907+), `engine.apply` over every partition (G-01), and then `publish` (G-08) plus `publish_save` (G-03/G-04). The UI keeps one box request in flight, so the drag update rate equals 1 / (cost of this path). The control thread runs at about 100 % during a drag.
- Proposal: add a "layers only" fast path. Some commands only change an item's transform, crop, style or visibility when the source is already drawn: `ItemSetBox`, `SetSourceCrop`, `TransformSource`, `ItemSetStyle`, and visibility toggles where the source stays wanted. For these, recompute only the affected compositor's `Vec<NodeLayer>` (reuse the buffer through `ParamSender::recycle`), send it, and mark the collection dirty for publish and save. Skip `studio.update`. Keep the full path for anything that changes graph shape (wanted sources, filters, scene membership). Before any apply, the binder can also report `graph_changed == false` cheaply, so the engine is not touched.
- Benefit: estimate. Per drag step, control-thread work drops from about 1-20 ms (full path, depends on collection size and partitions) to about 20-100 µs (one layer list for one canvas). Drag feedback rises toward display rate even while live, and control-thread CPU during drags falls by about 90 %+.
- Effort: L (spec touch: docs/specs for scene binding / ADR-0023 usage; careful with held or transition copies, ADR-0063/0128)
- Risk: Med. Held scenes, transitions and glide (ADR-0100) also produce layers, and the fast path must give the same layers the binder would. A property test can check this: fast path result == full rebind result.
- Kind: architectural; Hot path: no

### G-03 Scene collection autosave writes and fsyncs on every edit, with no time debounce
- Location: crates/rbs-app/src/studio_host.rs:5686-5693 (`publish_save`), crates/rbs-app/src/user_session.rs:699-702 (`save` pushes `collection.clone()`), :103-113 (coalesces only with the job at the back of the queue), crates/rbs-user-data/src/document.rs:509-538 (`write_atomic`: create tmp, write, `sync_all`, rename, directory `sync_all`).
- Problem: UI settings are debounced (`save_timer`, `SAVE_DELAY`, main.rs:2958), but the collection is not. During a drag, nudge or slider gesture the writer thread is always busy: TOML serialisation of the whole collection, then a file fsync and a directory fsync for each write. Back-to-back writes, each with 2 fsyncs, mean constant disk I/O, SSD wear, wakeups, and on Windows antivirus scans of each new temp file.
- Proposal: in `CollectionSaver`, or in the writer thread, add a trailing debounce (for example 1 s after the last change, and at most every 5 s during long gestures). Flush at once on gesture end (`gesture` ids are already known), on collection switch and on exit (`Saver::drop` already writes what is waiting). Keep the crash-safety contract: the worst case loses less than 1 s of edits.
- Benefit: estimate. Collection writes during a 5 s drag drop from about 50-500 (bounded by fsync latency 1-10 ms on NVMe/SSD) to 1-5, so about 99 % fewer fsyncs. Writer-thread CPU for TOML serialisation drops by the same factor.
- Effort: S
- Risk: Low. Crash window up to the debounce; ADR-0051 decision 8 (single writer) is unchanged. Check the spec's durability wording (docs/specs, user data) and update it in the same change.
- Kind: quick-win; Hot path: no

### G-04 `publish_save` deep-compares the whole collection on every publish and clones it twice per change
- Location: crates/rbs-app/src/studio_host.rs:5686-5693 (`self.saved.as_ref() != Some(&self.collection)` then `save.save(&self.collection)` (clone 1, user_session.rs:701) and `self.saved = Some(self.collection.clone())` (clone 2)); `SceneCollection: PartialEq` is structural (crates/rbs-scene/src/collection.rs:391-399), with `Settings = BTreeMap<String, SettingValue>` per source (model.rs:79).
- Problem: `publish` runs after every command and apply (50 direct call sites plus 86 `apply` sites). When nothing changed, the comparison walks every canvas, source (settings maps, filters) and scene item until the end. Equal collections are the worst case of `PartialEq`. On a change, two full deep clones follow, each about one allocation per String or map node. `Studio::update_with` adds 1-2 more (G-06).
- Proposal: give `SceneCollection` a `revision: u64` that every `&mut self` mutator bumps (it has a narrow mutation API already). Compare revisions instead of contents. Hold the saved snapshot as `Arc<SceneCollection>` and send the same `Arc` to the writer (one clone per change, zero when unchanged). Longer term: make sources and scenes `Arc<Source>` / `Arc<Scene>` (copy on write via `Arc::make_mut`), so a clone is O(number of entries) pointer copies and equality can short-circuit on `Arc::ptr_eq`.
- Benefit: estimate. For a 50-source / 20-scene collection (about 1-3k heap nodes), each publish saves about 20-60 µs of compare and each change about 2 x 30-100 µs of clone. That is small per call but adds to every drag step (G-02). Allocator churn on the control thread drops noticeably, about 2-4k allocations fewer per change.
- Effort: M (the revision is S; the Arc-sharing model is M, in rbs-scene; check the scene-format spec, since serialisation is unchanged)
- Risk: Low (revision) / Med (Arc model: every mutator must bump; a test can assert `revision` changes iff `PartialEq` changes)
- Kind: structural; Hot path: no

### G-05 The control thread wakes 20 times a second at idle
- Location: crates/rbs-app/src/studio_host.rs:102 (`const MAX_WAIT: Duration = Duration::from_millis(50)`), :3791 (`rx.recv_timeout(wait)`), :3949 (`wait = serviced.wait.unwrap_or(MAX_WAIT).min(MAX_WAIT)`); crates/rbs-engine/src/engine.rs:732-745 (engine asks for `control_interval` 250 ms only while a worker runs, `None` otherwise).
- Problem: with nothing running (no worker, no output), the engine asks for no wake-up at all, but the host caps the wait at 50 ms. Every 50 ms it runs a full `round`: `serve_api`, `feed_text_live`, `studio.service` (`pipeline_order` Vec alloc, collect per partition, media/browser controls), around 15 `drive_*`/`report_*` checks. This breaks the P13 idea (no polling while unused) and keeps a laptop CPU from deep C-states. The idle-cost CI check (performance-budgets.md:27) compares loaded vs baseline, so it cannot catch this fixed cost.
- Proposal: use `MAX_WAIT` only while something that is really polled is active: an output, a transition or T-bar, a hold, a held switch, calibration, protection, or link reporting. Each of those `drive_*` already returns its own deadline, so keep `min()` over them. With none active, wait for `None` = block on `recv()`, or on the engine's own deadline. Also check that nothing else relies on the 50 ms tick, such as `report_link` and `check_destinations`, which return no deadline but only run while streaming.
- Benefit: estimate. Idle control-thread wakeups drop from 20/s to 0/s (no preview) or 4/s (engine `control_interval` while the preview's render worker runs). That is 72k fewer wakeups per idle hour, worth roughly 0.1-0.5 W on laptops through C-state residency (typical per-wake cost about 10-50 µs CPU plus a C-state exit).
- Effort: S-M
- Risk: Med. A driver that silently relies on the periodic round would stall; make every driver return its deadline and add a test with `TestClock` that the round asks for no wake at idle.
- Kind: quick-win; Hot path: no

### G-06 Studio::update clones the collection 2-3 times per call even when nothing needs a copy
- Location: crates/rbs-record/src/studio.rs:5113-5129 (`with_followers` returns `collection.clone()` even when `shown.is_empty()`), :2220 (called on every update), :1804/:1877/:1935 (called again for checks, with `self.last_follow.clone()`), :2288 (`self.collection = collection.clone()`).
- Problem: one or two deep clones of the whole collection per update that a borrow could avoid. In the common case (no followed programs) `with_followers` could return the input.
- Proposal: return `Cow<'_, SceneCollection>` from `with_followers` (borrowed when `shown.is_empty()`). Store `self.collection` as `Arc<SceneCollection>` shared with the host (combine with G-04), or keep only the parts the studio reads (mix, master, browser claims are already derived).
- Benefit: estimate. 1-2 deep clones fewer per update, about 30-200 µs and about 1-3k allocations per edit for a mid-size collection.
- Effort: S
- Risk: Low
- Kind: quick-win; Hot path: no

### G-07 Settle partitions in parallel instead of one after another
- Location: crates/rbs-engine/src/engine.rs:536-540 + :559-605 (apply then settle per partition, in a loop).
- Problem: even with G-01, a structural edit (add source, start recording) waits for each partition's swap in turn. The worst case is the sum of the remaining tick times.
- Proposal: split `send_current` into `send` (apply + wake) and `finish` (collect). Phase 2 sends all parts first, in the required consumers-first order (invariant 4a needs only that a consumer's schedule is *sent* before its producer's), and then collects them. Check invariant 4a: "its worker runs the new schedule before the partitions feeding it prepare theirs" may need per-edge ordering. Where it does, settle only those pairs in order.
- Benefit: estimate. Structural-edit latency while live goes from the sum to the max of per-partition waits, about 2-5x faster with 5-8 partitions (for example 20 ms to 5 ms with a software encoder).
- Effort: M
- Risk: Med (ADR-0029/0041 ordering invariants; needs an independent review as an engine contract change)
- Kind: structural; Hot path: no

### G-08 `publish()` rebuilds scene list, rows and mixer rows from scratch on every call
- Location: crates/rbs-app/src/studio_host.rs:12635-12735 (`publish`), 11918-11960 (`scene_list`: every source, `sources.sort_by_key(|s| s.name.to_lowercase())`, `scenes.clone()`), 12737+ (`rows`, `rows_of` called 3 times: `heard`, `rows`, plus `global_rows` twice).
- Problem: each publish allocates Vecs of rows with cloned Strings and Arcs for every source and scene, then compares them with the previous ones. `sort_by_key` with `to_lowercase()` allocates O(n log n) Strings (the key function runs on every comparison).
- Proposal: (a) quick: `sort_by_cached_key` (n allocations instead of n log n). (b) gate the whole block on `collection.revision` (G-04) plus a studio state revision (source states, previews): when neither changed, skip the rebuild.
- Benefit: estimate. (a) about 5x fewer allocations in `scene_list` for 50 sources (~280 to ~50). (b) skips about 10-50 µs per publish when unchanged.
- Effort: S (a) / M (b)
- Risk: Low
- Kind: quick-win; Hot path: no

### G-09 Browser source supervisor polls at 120-200 Hz per source
- Location: crates/rbs-browser-runner/src/lib.rs:297-323 (`period = 1 s / fps / 2`, `min(SOUND_POLL)` where `SOUND_POLL` = 5 ms, lib.rs:263; loop ends in `park_timeout(period)`).
- Problem: each browser source keeps a thread waking 120 times a second (60 fps page), or 200 times with sound, even when the page is static or not on air. Ten overlay pages make 1,200-2,000 wakeups/s.
- Proposal: make the helper signal frames and sound through the shared memory's futex, eventfd or Windows event, and have the supervisor block on it with a long heartbeat timeout (for example 250 ms for `check_heartbeat`). At least drop to 1x fps and stop polling while the page has no consumer (ADR-0109 budget).
- Benefit: estimate. About 95 % fewer supervisor wakeups for static overlays: from 120-200/s to about 4/s per source plus 1 per real frame.
- Effort: M (shm protocol change: rbs-browser-shm and rbs-browser-cef; probably overlaps the browser auditor)
- Risk: Med (cross-process signalling on 3 OSes)
- Kind: structural; Hot path: no (supervisor), but the polling feeds frame publish

### G-10 Control API events go through a `serde_json::Value` tree; requests are parsed twice
- Location: crates/rbs-api/src/codec.rs:164-177 (`encode_event`: `to_value(event)` then `json!{..}` then `.to_string()`), :72-141 (`decode`: `from_str` to `Value`, then `json!({"method", "params"})` then `from_value`), crates/rbs-api/src/session.rs:1310-1324 (encodes before checking whether any session subscribes to the topic).
- Problem: each event allocates a full `Value` tree (map per object, String per key) and then serialises it. Sampled topics (`audio.levels`, stats) do this several times a second per topic. Each request is parsed into a `Value` and deserialised a second time.
- Proposal: encode with a borrowed wrapper struct `#[derive(Serialize)] struct Notif<'a> { jsonrpc: &'static str, method: &'static str, params: &'a Event }` and `serde_json::to_string`. In `events()`, compute `conns` first and skip the encode when it is empty. For decode, deserialise straight into an envelope `struct Envelope<'a> { jsonrpc, id, method: &'a str, #[serde(borrow)] params: Option<&'a RawValue> }`, then `Request` from the method + params.
- Benefit: estimate. About 2-3x faster event encoding and about 50-70 % fewer allocations per event. Small in absolute terms (µs per event) but on the API thread for every client.
- Effort: S
- Risk: Low (wire format unchanged; the conformance tests in rbs-api pin it)
- Kind: quick-win; Hot path: no

### G-11 Engine service: per-round allocation and a 250 ms poll only to see bypasses
- Location: crates/rbs-engine/src/engine.rs:697 (`let order = self.pipeline_order();` allocates a Vec each round), :734-741 (`control_interval` while any worker runs: comment at :28 says it exists only so bypasses are seen, ADR-0024 decision 9), crates/rbs-engine/src/part.rs:244 (`has_faults` builds and sorts a Vec via `faulted()`), crates/rbs-graph/src/runtime/controller.rs:304.
- Problem: small allocations on every control round (20/s, G-05), and 4 wakes/s while any worker runs, even when nothing failed.
- Proposal: cache `pipeline_order` and recompute it only when `crossings` changes (in `apply_inner`). Add `Controller::has_faults()` -> `!self.faults.is_empty()`. Let the executor wake the control thread on a bypass: it already bumps `bypasses` (executor.rs:224, :287); add an optional waker, an `Arc` the engine passes, called from the swap or bypass path with `try_send` or an unpark (no alloc, no block). Then drop `control_interval` polling.
- Benefit: estimate. 4 wakes/s fewer while anything runs. A few allocations fewer per round (negligible CPU, cleaner idle profile).
- Effort: S (cache, `has_faults`) / M (waker: an executor change, so it needs the hot-path review because it touches `process_all`'s failure branch, which is on the RT thread)
- Risk: Low / Med
- Kind: quick-win; Hot path: yes for the waker part (executor failure branch), no for the rest

### G-12 Audio output slots are sized for 8 channels and reallocated on every schedule build
- Location: crates/rbs-graph/src/runtime/schedule.rs:41-49 (`vec![0.0; capacity * MAX_CHANNELS]`, `MAX_CHANNELS` = 8, crates/rbs-core/src/media/audio.rs:18), controller.rs:663-667 (`Slot::for_kind` per output pin at each build), executor.rs:263-271 (`carry_slots` carries only `Slot::Video`).
- Problem: each audio output pin holds 480 x 8 x 4 B = 15 KiB, while almost every pin carries stereo (3.75 KiB used). Every apply (G-01/G-02) allocates and zeroes all of them again, frees the old ones on the control thread, and gives the audio RT thread cold cache lines and possibly page faults on the first tick after a swap.
- Proposal: carry audio slots across swaps like video slots (swap the boxed buffers for nodes that stay). Size the slot by the pin's channel layout when the descriptor declares it, falling back to 8.
- Benefit: estimate. About 11 KiB less resident memory per stereo audio pin (for example 20 pins = 225 KiB). No page-fault or cold-cache spike on the first audio tick after an edit (a few µs of RT jitter avoided per swap).
- Effort: S-M
- Risk: Low-Med (slot format must still allow a format change; keep the 8-channel fallback)
- Kind: quick-win; Hot path: yes (executor swap on the audio RT thread)

### G-13 System allocator on a control plane that allocates heavily; glibc arenas with many threads
- Location: no `#[global_allocator]` in any non-test target (only in `tests/alloc_per_*.rs`); rbs-app binary `crates/rbs-app/src/main.rs`.
- Problem: the control thread allocates heavily (G-02/G-04/G-06/G-08), and about 10-20 threads allocate at least a little. glibc malloc creates per-thread arenas (up to 8 x cores) that keep freed memory as RSS. Windows' default heap is slower for many small allocations.
- Proposal: measure first. Try `mimalloc` (or `MALLOC_ARENA_MAX=2` on Linux) as `#[global_allocator]` in the composition roots only (`rbs-app`, `rbs-cli`), so P1 is respected. Keep the per-test counting allocators as they are.
- Benefit: estimate, to verify: control-plane allocation-heavy paths 1.3-2x faster; idle RSS often 5-20 MB lower on glibc with many threads. Must be measured with `measure-performance`.
- Effort: S
- Risk: Low-Med (new dependency in the composition root; needs a PR justification; check macOS/Windows parity)
- Kind: quick-win; Hot path: no (the RT paths do not allocate by design)

### G-14 LevelMeter updates a second "shared" bank on the audio thread even when no API reader exists
- Location: crates/rbs-audio/src/meter.rs:55-63, :70-87 (`record` and `record_block` update both `peaks`/`energy` and `shared`/`shared_energy`); the shared bank is only read by `observe()` for the control API `audio.levels` sampler (meter.rs:105-111).
- Problem: per block, per channel: 2 `fetch_max` (CAS loops on x86), 2 f64 CAS-add loops and 2 `fetch_add`. Half of that serves a reader that exists only when remote control is on. This is a small P13 violation in the audio thread.
- Proposal: the audio thread keeps one bank. The UI and the API sampler read from a single "since last read" bank through per-reader sequence snapshots: a monotonic max-hold with an epoch counter, or each reader stores its last-seen (sum, count) and takes deltas. That leaves one write bank, and the API costs nothing when off.
- Benefit: estimate. About 50 % fewer locked atomic RMWs in metering (about 6 to 3 per channel per block). Absolute: about 0.2-0.5 µs per source per block. Tiny, but it is in the RT callback.
- Effort: S-M
- Risk: Low-Med (max-hold semantics per reader must stay)
- Kind: quick-win; Hot path: yes (audio callback / mixer)

### G-15 IPC transport uses 2 OS threads per client; WebSocket uses one poller for all
- Location: crates/rbs-api-ipc/src/conn.rs:237, :243 (read and write thread per connection), crates/rbs-api-ipc/src/lib.rs:137 (accept thread); compare crates/rbs-api-ws/src/lib.rs:220 (single poller thread).
- Problem: each local client (rbs-cli, rbs-mcp, Stream Deck-like tools) costs 2 threads (about 2 x 64 KiB-8 MiB reserved stack, plus scheduler entries). It is minor, but it grows with AI tools that keep sessions open.
- Proposal: reuse the WS crate's poller design (`polling`/epoll/IOCP) for IPC: one thread for accept + all connections.
- Benefit: estimate. Per IPC client 2 threads fewer, so with 3 clients 6 threads fewer and about 6 x 2 MiB fewer reserved stacks (committed memory is small).
- Effort: M
- Risk: Med (Windows named pipes with IOCP are fiddly)
- Kind: structural; Hot path: no

### G-16 Doorbell::wait clones the current Thread handle on every wait
- Location: crates/rbs-graph/src/runtime/queue.rs:173-179 (`let me = std::thread::current();` each wait; `Thread` is an `Arc`, so it is an atomic inc and dec, plus the registration lock).
- Problem: output workers call `wait` once per packet or ring. The clone is 2 atomic RMWs, and the mutex is taken on every wait even though the worker thread rarely changes.
- Proposal: cache the registered `ThreadId` in the worker loop (worker.rs `Pacing::Doorbell` branch). Register once per worker start (and again after a move), and make `wait` lock-free on the common path: fence, then check `rung`, then park.
- Benefit: estimate. About 30-60 ns per wait fewer, so at about 200 waits/s per output, negligible CPU. Mainly it removes a lock from the hot loop (P6 wording).
- Effort: S
- Risk: Med (the ring/wait fence protocol is subtle, see the comment at queue.rs:149-156; needs a loom or stress test)
- Kind: quick-win; Hot path: yes

### G-17 Text measure spawns a new OS thread per call
- Location: crates/rbs-app/src/studio_host.rs:15889-15906 (`measure_text_wrapped`: `thread::Builder::new()...spawn` + `recv_timeout(200 ms)`).
- Problem: about 20-60 µs thread creation and teardown per measure, plus a control-thread block of up to 200 ms. It only runs on add-text and fit-box, so it is rare.
- Proposal: send the measure to the existing `rbs-text-render` worker (text_worker.rs already handles hangs with `HANG_AFTER`) instead of a fresh thread.
- Benefit: estimate. Tiny (rare path); it mostly removes an ad-hoc thread kind from the inventory.
- Effort: S
- Risk: Low
- Kind: quick-win; Hot path: no

## Already good (verified)

- Executor (crates/rbs-graph/src/runtime/executor.rs): no allocation or lock in `run_tick`/`swap`. Node instances and video slots are carried across swaps, warm and cool run between ticks, and `catch_unwind` per node costs nothing when nothing panics. Health counters are plain fields owned by the RT thread.
- Partition workers stop when their schedule goes empty (part.rs:173-177). Doorbell outputs tick on rings with a 100 ms heartbeat. Cadence workers use `park_timeout` to the next due time, with no busy wait (worker.rs:519-528).
- Parameters go RT-side through a latest-value channel with a single atomic load when nothing is new, `try_lock` only, and drops on the sender's thread (crates/rbs-graph/src/runtime/param.rs).
- `WorkerStats`: relaxed atomics written by one thread and read at about 1 Hz, so no false-sharing hotspot. Transports already use `repr(align(128))` padding where producer and consumer counters are hot (rtmp io.rs:66, srt io.rs:96, file session.rs:42).
- `SeqCst` use is minimal in this area: only the two documented store/load fences in `Doorbell` (queue.rs:156, :182), which are needed.
- Binder keeps node ids stable and diffs settings per source. `live_settings`/`live_keys` avoid node replacement (binder.rs:964-985). Undo uses reverse records, not snapshots (edit_history.rs, `MAX_STEPS` = 100).
- Events to API clients: topic mask gates publishing at the source (control.rs:99-119, `EventPublisher::changed` takes a closure). Each event is encoded once and shared as `Arc<str>` across sessions (session.rs:1312).
- UI timers are P13-clean: meters, live timer and settings save run only when needed (main.rs:2120-2136, :2300-2312, :2958). UI updates coalesce into one `sync` per event-loop turn (main.rs:1664-1676).
- Remote control / API threads start only when turned on (main.rs:5921 comment; remote_control.rs).
- Plugin host only parses manifests: no library load, no WASM compile, no process (rbs-plugin-host lib.rs header). rbs-params `Values` is a fixed array that moves through the param channel without allocating.
- Crash handling is a thread-local flag plus `catch_unwind` per control round, with no extra thread and no periodic I/O (rbs-crash lib.rs:42-56).
- No `tracing`/`log` macros, so there is no hidden formatting cost on hot paths. App `log(&format!(..))` calls are on error and connect paths only.
- Graveyard janitor threads exist only while graves wait, and exit when empty.

## Open questions / measurements needed (each about 2 min on a device)

1. Drag cost today (G-01/G-02): while streaming + recording with x264, drag a source for 5 s. Measure `rbs-studio` CPU % (`top -H -p <pid>` / Activity Monitor threads) and the count of `BoxPlaced` answers per second (or temporary `Instant` timing around `host.apply`). This sets the real benefit.
2. Autosave rate (G-03): during the same 5 s drag, `strace -f -e trace=fsync,rename -p <pid> -c` (Linux) or Process Monitor filtered on the collection file (Windows). Count the fsyncs.
3. Idle wakeups (G-05): app idle with the preview hidden, `perf stat -e sched:sched_wakeup -p <pid> -- sleep 10` or `pidstat -w -t 1 10`. Expect about 20/s on `rbs-studio`.
4. Browser polling (G-09): one static browser overlay, `pidstat -w -t -p <pid> 1 5`. Expect about 120 cswch/s on `rbs-browser-supervisor`.
5. Allocator (G-13): RSS at idle and after 10 min of live with the system allocator vs `MALLOC_ARENA_MAX=2` (Linux env var only, no code change). This shows whether the arena effect matters before adding mimalloc.
6. Does any control-loop driver rely on the fixed 50 ms tick (G-05)? Check transitions and T-bar (`step_transitions`, `manual_transition`), `report_link` and `check_destinations` while streaming. Code reading suggests only output-time drivers do.
7. G-07 ordering: confirm with the engine owner whether invariant 4a ("consumers first") needs a completed swap or only a sent schedule before the producer prepares.
8. Doc drift: docs/architecture/overview.md:66 says transports run on `tokio`, but no crate depends on `tokio`. Update the table (docs-only).
