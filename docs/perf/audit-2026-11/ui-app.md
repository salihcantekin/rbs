# Performance audit: desktop app runtime and startup (rbs-app, rbs-ui, rbs-api*, Slint UI)

Scope: crates/rbs-app (main.rs, studio_host.rs, studio_ui.rs, native/, look), rbs-ui, crates/rbs-app/ui/**/*.slint, rbs-api / rbs-api-ws / rbs-api-ipc (event path), the Studio control thread's publish loop. Read-only audit at 2026-11-17; nothing was built or run. Line numbers were checked against the tree. Every benefit is an **estimate** unless marked **measured**. Measured numbers come from docs/specs/performance-budgets.md and ADR-0035.

Background facts that several findings rely on (checked in the Slint 1.18.1 sources under ~/.cargo/registry):
- `Property::set` compares with `PartialEq` and marks dependents dirty only when the value changed (i-slint-core properties.rs:1029-1051). Setting the same string or number again is free.
- `ModelRc`'s `PartialEq` compares pointers (i-slint-core model.rs:898). A new `ModelRc` is therefore *always* a change. Repeaters that use it reset, and any visible dependent requests a redraw.
- `VecModel::set_row_data` always notifies (no equality check). `set_vec` resets the whole repeater.
- The Skia renderer uses partial (dirty-region) rendering only on the software surface. It is also disabled whenever a rendering notifier is set, which the app always sets for preview placement (i-slint-renderer-skia lib.rs:338-346, 922-928, 1086). So **any dirty visible property means a full-window redraw** on Metal, D3D12 and wgpu, as ADR-0035 states.
- Every `studio_host::Update` goes through `deliver` → `Ctx::studio_update` → `Ctx::update` → a full `Ctx::sync()` on the next turn (main.rs:1621, 1663-1676, 1776, 2670).

---

### U-01 UI sync reads appearance.toml and every theme file from disk, up to twice per sync
- Location: crates/rbs-app/src/main.rs:2817 and 2820 (`menu_bar::values(&app.shown())`, `menu_bar::toolbar(&app.shown())`). `App::shown` is at main.rs:534-556, line 539: `themes: appearance_page::theme_menu_choices(&self.studio.user)`. appearance_page.rs:435-455: `user.data().appearance()` (reads and parses `appearance.toml`: rbs-user-data lib.rs:89-95) and `user.themes()` → `Library::list()` (read_dir plus `document::peek` of every theme file: rbs-user-data library.rs:277-292).
- Problem: synchronous file I/O and TOML parsing run on the UI thread on every `sync()`. That is twice per sync on macOS (menu bar, then toolbar) and once per sync elsewhere. Syncs happen on every studio Update: 4 Hz while any media source plays, 1 Hz while live (Stats), once per fader move event, on every hover or click callback that calls `ctx.update`, and on every resize step. `shown()` also calls `projector::displays()` (main.rs:551), which enumerates monitors through winit while a projector window exists. Cost: idle with looping media, live, and during interaction.
- Proposal: cache the theme menu choices in `App` (or `UserSession`) and refresh the cache only when appearance or themes change. Those changes already go through `appearance_page::reload` and `select_theme`. Build `Shown` once per sync and pass it to both `menu_bar::values` and `menu_bar::toolbar`. Cache the display names and refresh them on winit monitor-change events.
- Benefit: removes 2 + N file opens and parses (N = user themes) per sync, or 4 + 2N on macOS. Estimate: 30-100 µs per file on a warm SSD cache, and 0.3-2 ms per file on Windows with Defender real-time scanning. That is roughly 0.1-0.5 ms per sync on Mac and up to 5+ ms per sync on a weak Windows laptop. During a fader drag (60-120 syncs/s, U-04/U-13) the worst case is several percent of the UI thread plus frame hitches. Estimate, from typical open/read/parse costs.
- Effort: S
- Risk: Low. The values are cache invalidations on known change points. The risk is a stale menu after an external edit of the theme folder, which the existing reload path already handles.
- Kind: quick-win. Visual change: no.

### U-02 Meters push every row 30 times a second even when nothing moved
- Location: crates/rbs-app/src/studio_ui.rs:6935-6980 (`tick_meters`: `self.meters.set_row_data(i, row)` for every slot on every tick, and the same for `master_meter`). Driven by the 30 Hz `meter_timer` (main.rs:2304-2316, `METER_PERIOD` studio_ui.rs:66). Also crates/rbs-app/ui/studio.slint:3095-3099: the `GateBar` `Timer { interval: 33ms; running: true; ... root.levels = Studio.filter-levels(...) }`.
- Problem: `set_row_data` always notifies, so every tick dirties every meter row and the whole window redraws at 30 Hz while the Mixer is shown. This includes silence, muted sources and sources that are not running (rows with `live: false`). Measured whole-window meter redraw cost on the M4 (ADR-0035 / budgets): 8.4-9.2 % CPU with Slint meters against 2.5 % without, about 6-7 points for 16 meters. The inner loop is O(slots × rows) with a full `MeterRow` clone (4 SharedStrings) per probe.
- Proposal: (1) compute the painted row, then call `set_row_data` only when it differs from the current row (`MeterRow: PartialEq`). (2) Quantise the zone edges and the peak line to whole physical pixels of the bar (the bar's height in px is known from layout or tokens), so a noise floor that moves less than 1 px does not redraw. (3) Keep an id→row index map instead of the linear scan. (4) GateBar: compare `levels` before assigning, or drive it from the same Rust meter tick.
- Benefit: silent or steady meters (the common idle state: a mic in a quiet room, sources muted, music paused) make zero redraws instead of 30 full-window redraws per second. Estimate: up to the measured 6-7 % CPU of one core saved on the M4 while the Mixer shows and audio is quiet. It is more on iGPU and weak laptops, where a full-window redraw costs 2-5 ms. With live audio the saving falls to the share of ticks with sub-pixel motion, an estimated 20-50 %.
- Effort: S
- Risk: Low. The peak-hold decay still changes rows. Clip latching is unaffected.
- Kind: quick-win. Visual change: no (same pixels).

### U-03 Native meter layers (ADR-0035, Accepted) exist but the app does not use them
- Location: crates/rbs-app/src/native/meter_layer.rs (`MeterBridgeLayer`, macOS), exported in native/mod.rs:15. Its only users are examples/remote_preview.rs and tests/meter_layer_macos.rs. The app's Mixer still draws the moving parts in Slint: crates/rbs-app/ui/studio.slint:1228 and 1423 (`Rectangle { ... data.lo-from ... background: Theme.meter-low; }`). docs/status.md does not mention this gap.
- Problem: while the Mixer is visible with running audio, the window redraws fully at 30 Hz (see U-02). ADR-0035 measured the alternative: native layers 2.4-2.7 % against 8.4-9.2 % at 16 meters, and 3.0-3.1 % against 13.5-13.7 % at 128 meters (**measured**, reference Mac). Windows has no native meter layer yet (budgets: 7.6 % with 16 Slint meters against 1.3 % for the preview layer alone).
- Proposal: wire `MeterBridgeLayer` into the Mixer the same way `preview::place` places the preview layer. Pass geometry from layout through `set_rect`/`set_meters` before each redraw, `show(&[MeterView])` from `tick_meters`, and keep Slint for the static parts. Then build the Windows twin (DirectComposition visual, as ADR-0036 did for the preview). Record the gap in docs/status.md now.
- Benefit: **measured** −6 to −7 points of CPU on the Mac at 16 meters and −10.5 points at 128. Windows estimate: about −6 points (7.6 % → about 1.5-2 %), by analogy with ADR-0036.
- Effort: M (macOS), L (Windows)
- Risk: Med. Layer placement must follow popups, cutouts and scroll like the preview layer (ADR-0054). The macOS code and its tests already exist.
- Kind: structural. Visual change: no intended change (same tokens). Screenshots are still needed for owner sign-off because the drawing path changes.

### U-04 Every studio Update runs the whole UI sync, including periodic ones
- Location: main.rs:1776 (`studio_update` → `self.update(...)` → `sync()`, main.rs:2670-2960, then `sync_studio` main.rs:2139-2340 and `StudioUi::push` studio_ui.rs:865-1125, about 150 setters and string formats per window, repeated for main, every float and Settings). The periodic producers are `report_media` (studio_host.rs:10836-10869, `MEDIA_TICK` 250 ms, sent while any media source plays because the cursor is rounded to 250 ms), `report_stats` (studio_host.rs:8294-8315, 1 Hz while recording or streaming), and `Update::Sources`/`Heard` after each volume change (U-13).
- Problem: a looping background video, which is very common, makes the UI run a full `sync()` 4 times a second forever. That covers `view::window_view` layout, the toolbar and menu rebuilds, U-01's disk reads, `StudioUi::push` with about 60 new models (U-05) and a full-window redraw caused by U-05's new edge-tab models. Live streaming adds 1 Hz. The 0.0 % idle budget ("no redraw timer") is broken whenever media plays, even with the Media panel closed.
- Proposal: split `sync()` into targeted pushes. Handle `Update::Media` by updating only the rows of the sources whose media state changed (`set_row_data` on the Sources model and the Properties media fields). Handle `Update::Stats` by `set_rows` on the stats model only. Neither should schedule a full sync. Better still, let the UI subscribe: the host sends `Media` ticks only while a media control is visible (Properties of a media source, or a Sources row showing a time).
- Benefit: idle with looping media goes from 4 full syncs plus 4 full-window redraws per second to about 0 (or 4 small row updates while a media control is visible). Estimate 0.2-1 ms of UI-thread CPU per sync plus 1-5 ms GPU/CPU per full-window redraw, about 0.5-2 % of one core and 4 GPU wake-ups per second. Restores the "0.0 % idle" budget in the media case.
- Effort: M
- Risk: Med. `sync()` currently guarantees consistency. Targeted handlers must not miss derived state (for example, the toolbar enabling Restart). Gate with tests in `studio_ui`.
- Kind: structural. Visual change: no.

### U-05 New `ModelRc` objects on every sync force repeater resets and full redraws
- Location: main.rs:2814 `let edge_models = edges.map(|v| ModelRc::new(VecModel::from(v)));`, pushed at main.rs:2931-2933 (`set_left_tabs`/`right`/`bottom`). main.rs:2870 `live_model(...)` → `set_live_dests` (also main.rs:1759 in the 1 Hz `live_tick`). studio_ui.rs `StudioUi::push`: `strings(...)` and `choice_model(...)` (studio_ui.rs:7365, 7397) for `out_bands`, `out_extras`, `canvas_bands`, `canvas_extras`, `canvas_rates`, `settings_canvases`, `out_containers`, `out_pcm_widths`, `global_kind_names`, `global_options`, and in `push_remote` `rc_accesses`/`rc_access_hints`. About 62 such sites in main.rs, studio_ui.rs and studio_ui/*.rs.
- Problem: because `ModelRc` equality is pointer equality, each of these setters is a change on every sync, for each window the global lives in. The edge tabs and the live chip are in the visible main window, so every sync redraws the whole window and rebuilds those repeaters' item instances, even when nothing changed. The rest allocate and reset repeaters in an open Settings window, for example ComboBox lists.
- Proposal: keep one `Rc<VecModel<_>>` per list in `App`/`StudioUi` (as `MenuModels` and `toolbar_model` already do) and update it with the existing `set_rows` helper (main.rs:140). Static lists (`RATES`, `Access::ALL`, containers, PCM widths) are built once, as vcam_page.rs:247-258 already does. Push the same `ModelRc` each time; Slint then sees no change.
- Benefit: removes the guaranteed full-window redraw per sync. Estimate 1-5 ms of render per sync saved (more on iGPU), plus about 60 small allocations per window per sync. Combined with U-04 this makes no-op syncs (hover, focus) free on screen.
- Effort: S-M (mechanical, many sites)
- Risk: Low.
- Kind: quick-win. Visual change: no.

### U-06 Control thread rebuilds the whole published state 20 times a second while idle
- Location: studio_host.rs:3954 `let mut wait = serviced.wait.unwrap_or(MAX_WAIT).min(MAX_WAIT);` (MAX_WAIT = 50 ms, studio_host.rs:102). The round ends with `host.publish(notify)` (studio_host.rs:4083), and `publish` is at studio_host.rs:12635-12720. Each call runs `publish_save` (deep `PartialEq` of the whole `SceneCollection`, studio_host.rs:5686-5692), `refresh_sizes`, `loudness_view`, and `scene_list()` (all sources and scenes with cloned names, `sort_by_key(|s| s.name.to_lowercase())` at 11931 allocating per comparison, and `scenes.clone()` for hold scenes). It also runs `rows_of(scene)` + `global_rows()` (for Heard) + `rows()` (globals again + `rows_of(edited)`): 4 full `SourceRow` builds with name clones, `device_peers`, filter rows and an O(n²) `states.iter().find`. Finally it runs `history` clones and `publish_levels` (a Vec + BTreeSet + `RwLock::write` per round, studio_host.rs:12687-12699).
- Problem: all of this runs 20 times a second with no change, just to discover that nothing changed. It costs CPU and 20 wake-ups/s on the `rbs-studio` thread even with the window hidden, and it scales with collection size.
- Proposal: make `publish` change-driven. Set a `dirty` flag (or a generation counter) on every command, `apply`, API request and studio event that can change published state (`service()` reports source failures, recoveries and picture-size changes), and skip `publish` when it is clean. Cache `rows`/`heard`/`globals` once per publish (build global rows once and reuse them). Use `sort_by_cached_key`. Replace the collection deep compare with a revision counter on `SceneCollection` edits. Separately, ask the engine owner whether `service()` must run every 50 ms when no output, transition or failure is pending (the doc says the studio asks for "at most about 100 ms"); an idle 100-250 ms halves or better the wake-ups.
- Benefit: estimate 30-150 µs per round for a 20-40 source collection, so 0.06-0.3 % of one core and about 1,000-3,000 small allocations per second removed at idle. Wake-ups drop from 20/s to 10/s or fewer if the engine allows. Power matters most here on battery laptops.
- Effort: M
- Risk: Med. A missed dirty mark means a stale UI or API state. Keep a debug assertion mode that runs the full rebuild and compares, used in tests.
- Kind: structural. Visual change: no.

### U-07 Collection autosave writes and F_FULLFSYNCs the whole file on every edit, with no debounce
- Location: studio_host.rs:5686-5692 (`publish_save`: on any collection difference, `save.save(&self.collection)` and `self.saved = Some(self.collection.clone())`). Writer: user_session.rs:126-200 (one job per save; a waiting job is replaced, but a running write is not). rbs-user-data document.rs:510-531 `write_atomic`: `sync_all()` on the file and on the directory (on macOS Rust's `sync_all` is `F_FULLFSYNC`).
- Problem: each fader step, nudge, rename or toggle changes the collection. That costs 2 deep clones on the control thread, a full TOML serialisation, a temp-file write, a fsync and a directory fsync. During a fader drag the writer thread runs back to back. `F_FULLFSYNC` flushes the drive cache (about 5-20 ms each on Apple SSDs) and keeps the SSD from idling. While recording, these flushes compete with the recording's own writes.
- Proposal: debounce collection saves on the control thread, for example 1 s after the last change or at the end of a gesture (`gesture` ids already exist for box drags), and save at once on quit, collection switch and before risky operations. Keep the atomic write. Consider `sync_data` + rename without the directory fsync for autosaves, and full durability on explicit save or quit. This needs a spec check for ADR-0051's crash promise. Drop the second clone by storing a revision number instead of `saved: Option<SceneCollection>`.
- Benefit: during a 2 s fader drag, about 20-100 full writes with fsync become 1. Estimate: several ms of disk flush and 0.5-2 ms of serialisation per write avoided, and less SSD wear and power on laptops.
- Effort: S
- Risk: Med. A crash within the debounce window loses up to 1 s of edits. That is acceptable for UI tweaks, but the crash-notice text ("kept as last saved") must stay true.
- Kind: quick-win. Visual change: no.

### U-08 Sources list rows are always rewritten because each row carries a new filters model
- Location: studio_ui.rs:6552-6750 (`set_rows` builds `SourceData` for every row, then `set_rows(&self.sources, data)` at about 6746). studio_ui/filters.rs:319-323: `FilterUi::data` returns a new `ModelRc<FilterData>` per row, so `row_data(i) != row` is always true (pointer equality), and every row gets `set_row_data` on every `Update::Sources`.
- Problem: any change to any source rewrites all rows of the Sources list and the Properties bindings. That includes a volume step, which happens per pointer move (U-13), a rename, or a failure state. `SourceData` has about 90 fields, and each `set_row_data` re-evaluates that row's bindings and redraws. The rows' FilterField models (`field_data`) are rebuilt too.
- Proposal: keep a per-source `Rc<VecModel<FilterData>>` in `FilterUi`, update it with `set_rows`, and reuse the same `ModelRc` in `SourceData.filters`. Then the outer `set_rows` diff works, and only the changed row is touched.
- Benefit: with 20 sources, one volume step updates 1 row instead of 20. Estimate 0.1-0.5 ms of UI thread per Sources update saved, and less layout invalidation.
- Effort: S-M
- Risk: Low.
- Kind: quick-win. Visual change: no.

### U-09 The window keeps working while minimized or occluded
- Location: main.rs:2304-2316 starts the meter timer whenever the Mixer is in the layout (`mixer_shown && s.has_meters()`). It stops only when `app.hidden` is set (main.rs:2730-2733), which is the background mode, not minimize. preview.rs:141-170 shows every preview frame on the layer. A grep for minimize or occlusion handling in rbs-app/src finds only the Windows caption buttons.
- Problem: a minimized or fully covered RBS still runs 30 meter ticks/s with Slint model updates, receives and presents every preview frame (30-60 UI wake-ups/s plus compositor work), and the engine keeps rendering the preview picture. Streamers often minimize RBS during a stream.
- Proposal: watch `NSWindow.occlusionState` (macOS, `NSWindowDidChangeOcclusionStateNotification`) and `WM_SIZE SIZE_MINIMIZED` / DWM cloaking (Windows). When not visible, stop the meter timer, stop showing preview frames (drop them on the preview thread), and send `SetPreviewWanted(false)` so the engine skips the preview conversion and copy (as ADR-0093 decision 7 already does for the vertical picture). Resume on visible.
- Benefit: while minimized, estimate 2-4 % of one core (budgets: preview layer 1.3-2.0 % **measured**, plus meters) plus the engine's preview pass on the GPU. Equivalent to the "hidden" savings that ADR-0056 gives the background mode.
- Effort: M
- Risk: Low-Med. The first frame after restore must arrive quickly; keep the last frame on the layer.
- Kind: structural. Visual change: no.

### U-10 Startup probes every encoder and source before the GPU and the studio start
- Location: studio_host.rs:4101-4104: `let (mut adapters, mut notes) = adapters(); adapters.probe(); ... let gpu = gpu()?;` then `Studio::start_with_tree`. `RecordAdapters::probe` (rbs-record recording.rs:173-179) probes sources, all video encoders, audio encoders, muxers and transports in sequence. Then `adapters.sources.targets(Screen)` enumerates screens (studio_host.rs:4139).
- Problem: the first preview frame (budget < 1.5 s cold start, performance-budgets.md:54) waits for every hardware encoder probe: NVENC, AMF, QSV and Media Foundation on Windows, VideoToolbox on Mac. Those probes load drivers and open sessions, but the preview only needs sources and the GPU.
- Proposal: run `gpu()` and the source probe first and start the studio, then probe encoders, muxers and transports on a helper thread (or lazily on the first Record or Go live, or when the Settings encoder list opens) and send `Update::Encoders` when done. Alternatively, create the GPU context on its own thread in parallel with the probes.
- Benefit: estimate 50-150 ms on Mac and 150-600 ms on Windows with several GPU vendors. This is from typical driver-load and session-open times and is **to verify** with a startup trace.
- Effort: M
- Risk: Med. The UI must handle "encoders unknown yet" (Record disabled for a moment). The cannot-record message must wait for the probe.
- Kind: structural. Visual change: no (possibly a brief disabled state).

### U-11 Volume fader sends one command per pointer move, and each one republishes everything
- Location: studio_ui.rs:2469-2476 (`set_source_volume`: "Each pointer move sends one change"). Host: studio_host.rs:4499-4513 → `run_ui_api` → the round's `publish` → `Update::Sources` + `Update::Heard` → a full UI sync (U-04) + all Sources rows rewritten (U-08) + an autosave with fsync (U-07) + 2 disk reads of themes (U-01).
- Problem: a fader drag at 60-120 Hz pointer rate multiplies all of the above. Box drags are already coalesced (preview_edit.rs:68-269, one in flight plus latest), but volume is not.
- Proposal: coalesce like `preview_edit`: one volume command in flight per source, and the newest value replaces queued ones. Or apply the gain live through the existing lock-free mixer path and commit to the collection at gesture end (`answer_live` exists for that).
- Benefit: estimate a 5-20× reduction in control-thread and UI-thread work during fader drags, and smoother drags on weak laptops.
- Effort: S-M
- Risk: Low.
- Kind: quick-win. Visual change: no.

### U-12 Stats panel resets its repeater every second
- Location: studio_ui.rs:6411-6421 `self.stats.set_vec(rows);`
- Problem: `set_vec` resets the model, so every Stats row item is destroyed and recreated each second while recording or streaming, even if the Stats panel is a background tab. The `Update::Stats` that triggers it also forces a full sync (U-04).
- Proposal: use `set_rows` (main.rs:140 pattern) so only changed values update. Send stats only while the Stats panel (or a status-bar readout) is visible.
- Benefit: estimate 0.1-0.3 ms per second plus no item churn. Small, but it is part of making live mode quiet.
- Effort: S
- Risk: Low.
- Kind: quick-win. Visual change: no.

### U-13 Two unaligned 1 Hz wake-ups while live
- Location: main.rs:2119-2136 (`live_timer` 1 s → `live_tick`, main.rs:1710) and studio_host.rs:8294-8315 (`STATS_INTERVAL` 1 s, its own phase → full sync).
- Problem: two separate UI wake-ups and two redraws per second, at different phases.
- Proposal: drive the duration text from the Stats update (or the reverse): one 1 Hz tick that updates duration, chips and stats together.
- Benefit: estimate −1 full-window redraw per second while live, about 1-5 ms/s of render.
- Effort: S
- Risk: Low.
- Kind: quick-win. Visual change: no.

### U-14 Preview frames wake the UI thread once per frame
- Location: preview.rs:141-170 (the `rbs-preview` thread calls `crate::entry::invoke(move || show(which, frame))` per frame; `show` → `layer.show`, `retire`). The same pattern applies to projectors (projector.rs:938-960).
- Problem: with the preview visible (always, even when not live), the UI thread wakes and allocates a boxed closure 30-60 times a second, per picture (Program, Scene in studio mode, Vertical). The layer update itself is small.
- Proposal: macOS: set `CALayer.contents` / IOSurface from the preview thread inside an explicit `CATransaction` (Core Animation is thread-safe with explicit transactions), and keep only placement on the UI thread. Windows: DirectComposition `Commit` and swapchain `Present` can run on the preview thread. This makes the UI thread sleep when the UI is static.
- Benefit: estimate 30-180 UI wake-ups/s removed and 0.2-1 % CPU. Main gain is that the UI thread stays idle, which also helps input latency. **To verify** with the measure mode.
- Effort: L
- Risk: High. Thread-safety of layer placement against UI-driven `set_rect`/cutouts (ADR-0054) needs a careful design and an ADR amendment of ADR-0034/0036.
- Kind: architectural. Visual change: no.

### U-15 Preview, projector and API helper threads poll with timeouts
- Location: preview.rs:146 `sub.recv(Duration::from_millis(250))` (one thread per picture, up to 3), projector.rs:941 (same, per projector).
- Problem: each thread wakes 4 times a second when no frame comes, for example when the studio has stalled or the engine paused the preview. Small, but these are idle wake-ups.
- Proposal: block until a frame arrives or the connection closes (a close sends a sentinel or drops the sender), instead of polling `current()`.
- Benefit: estimate −4 wake-ups/s per open picture while frames are paused. Tiny CPU.
- Effort: S
- Risk: Low.
- Kind: quick-win. Visual change: no.

### U-16 Control API: events built before checking for subscribers in two places, and double JSON encoding
- Location: studio_host.rs `publish`: `let listed = api_scenes(&scenes);` and `let listed: Vec<api::SourceInfo> = heard.iter().map(api_source).collect();` are computed outside the lazy closure passed to `events.changed(...)`. rbs-api control.rs:209-224 already skips the payload when nobody listens. rbs-api codec.rs:164-176 `encode_event` goes `serde_json::to_value` → `json!` → `to_string` (two passes and a `Value` tree).
- Problem: allocations on every scene or source change with no API client. With clients, each event is encoded through an intermediate `Value`. It is encoded only once per event and shared by `Arc<str>` (session.rs:1309-1322), which is good.
- Proposal: move the `listed` computation into the closure. Encode with a borrowed wrapper struct (`#[derive(Serialize)] struct Notif<'a> { jsonrpc: &'static str, method: &'static str, params: &'a Event }`) and `serde_json::to_string` directly.
- Benefit: estimate about 30-50 % less encode time per event, and zero work without clients. Small absolute numbers (µs per event). It matters for `audio.levels` sampling at high rates.
- Effort: S
- Risk: Low.
- Kind: quick-win. Visual change: no.

### U-17 Text measuring spawns a thread per request
- Location: studio_host.rs:15890-15905 (`measure_text_wrapped`: `std::thread::Builder::new().name("rbs-text-measure")...` per call, waits up to 200 ms).
- Problem: one OS thread is created and destroyed per measurement (add text, Fit box to text). It is called on edits, so it is not hot, but each spawn costs about 20-60 µs and an 8 MB stack reservation.
- Proposal: one lazily started measuring worker with a channel, kept while text sources exist (P13).
- Benefit: tiny CPU. Fewer thread create/destroy events.
- Effort: S
- Risk: Low.
- Kind: quick-win. Visual change: no.

### U-18 Release profile does not size-optimise the 62 MB of generated Slint code
- Location: Cargo.toml:35-38 (`[profile.release] lto = "thin", codegen-units = 1, debug = "line-tables-only"`). Cargo.toml:46-62 tunes only the dev profile for the rbs-app-ui* crates.
- Problem: the generated UI code is large (about 62 MB of Rust according to the Cargo.toml comment, about 22.6 k lines of .slint). In release it is compiled at opt-level 3, which favours size-increasing inlining in code that runs rarely (callbacks, init). That means a bigger binary, more pages to fault in at startup and more i-cache pressure.
- Proposal: try `[profile.release.package.rbs-app-ui-main] opt-level = "s"` (and settings, dialogs, probes, base), and measure binary size, cold start and `RBS_MEASURE_UI` frame times. Consider `strip = "debuginfo"` for shipped builds, keeping symbols separately for crash reports.
- Benefit: estimate 15-35 % smaller UI crates' code and some ms off cold start (page-ins). Frame time should be unchanged. **To verify.**
- Effort: S
- Risk: Low (measure first).
- Kind: quick-win. Visual change: no.

### U-19 Small items in the per-sync and per-round paths
- `StudioUi::push` formats strings on every sync for values that rarely change (studio_ui.rs:1007-1060: `out_summary`, `out_kind`, `canvas_label`, `RATES` labels, `p.folder.display().to_string()`). Cache them when the profile changes. Estimate tens of µs per sync per window. S, Low.
- `sync_studio` runs from `relayout_now` on every resize step (main.rs:1703) and pushes the full `StudioUi::push` to every window. During a window resize only areas change, so split placement from data push. Estimate 0.2-0.5 ms per resize event. S, Low.
- `scene_list` `sort_by_key(|s| s.name.to_lowercase())` (studio_host.rs:11931) allocates 2 strings per comparison. Use `sort_by_cached_key`. Tiny. S, Low.
- `publish_levels` takes `RwLock::write` 20 times a second even when the feed is unchanged (levels.rs:33-39). Compare under a read lock first, or only after a dirty publish (U-06). Tiny. S, Low.
- `tick_meters` formats `take_report().english()` every 30 ticks per meter (studio_ui.rs:6947). This is fine, but with U-02 compare the text before setting. Tiny.
- Kind: quick-win. Visual change: no.

### U-20 (Cross-area note, rbs-audio, hot path) Meters keep a second accumulator bank for the API sampler even with no API client
- Location: crates/rbs-audio/src/meter.rs:56-88 (`record` and `record_block` update `peaks` and `shared` and both energy banks with CAS loops on every block).
- Problem: half of the meter atomics on the audio thread serve `audio.levels`, which is off unless remote control or AI tools run with a subscriber (P13).
- Proposal: an `AtomicBool observed` (set while a `LevelFeed` sampler is subscribed) that gates the shared bank. This is a hot-path change, so it needs numbers and a human reviewer (rule 12).
- Benefit: estimate a few ns per block per channel. Negligible CPU, but it is a principle (P13) item.
- Effort: S
- Risk: Med (hot path, review process).
- Kind: quick-win. Visual change: no.

---

## Already good (verified)
- Slint `Timer`s in .slint files are all gated by `running:` conditions (tooltips, submenu, repeat buttons, toasts, settings find). The only free-running one is `GateBar` (U-02), and it exists only while the gate section is open. No infinite animations; all `animate` blocks are short state transitions.
- The live timer exists only while live or recording (main.rs:2119-2136). The meter timer runs only while the Mixer is in the layout with meters, and it stops in the background mode (main.rs:2730-2733).
- Theme effects are created with `if Theme.effects-on` / `if root.fx` (components.slint:62-63, 486-505), so Calm creates no shadow elements (ADR-0120 decision 13 is measured per policy).
- The control API is event-driven: the WS server uses `polling` with no busy loops, `ACCEPT_BACKOFF` applies only after errors, and the `rbs-api` thread waits until the next deadline. `EventPublisher::changed` skips building payloads when no subscriber or local sink wants the topic, and events are encoded once and shared as `Arc<str>`.
- Box and crop drags are coalesced (one in flight plus latest, preview_edit.rs).
- Workspace saves are debounced by 400 ms (main.rs:69, 2959-2968). Menus are diffed (`MenuModels::update` with `shown`). The `set_rows` helper diffs rows. Virtual-camera page lists are built once.
- The studio and its GPU start on the `rbs-studio` thread, and the main window shows without waiting for them. The Settings and other windows are created on demand.
- Font discovery (fontique) runs only when a theme names a font family. Built-in tokens name none (tokens.toml:283).
- The vertical canvas picture is produced only while its panel is visible (main.rs:2290-2302, ADR-0093). Capture exclusion and the background badge are diffed.
- Meter level transport is lock-free (atomics, swap on read). The UI thread takes no lock shared with the engine. The only RwLock (`LevelFeed`) is between the control and API threads, with `try_read` on the reader side.

## Open questions / measurements needed (each ≤ 2 min on a device)
1. **Idle wake-ups with looping media (U-04, U-05):** a scene with one looping video, Mixer closed. macOS: `sudo powermetrics --samplers tasks --show-process-energy -i 1000 -n 10 | grep -i rbs` (wake-ups/s, CPU ms/s). Compare with the media paused. The expectation is that frames drop to about 0/s when paused, but they are already 30-60/s from the preview layer path. Better: run `RBS_MEASURE_UI=10` with media playing and count frames in the "idle" phase. Any frames > 0 there come from Slint redraws (U-04/U-05).
2. **Meter redraws in silence (U-02):** Mixer open, all mics muted. Use `RBS_MEASURE_UI=10` idle-phase frame count (expect 300 frames = 30 Hz today, 0 after U-02) and `top -pid <rbs> -stats cpu`.
3. **Per-sync disk I/O (U-01):** macOS `sudo fs_usage -w -f filesys <pid> | grep -E "appearance|themes"` while hovering toolbar buttons or dragging a fader. Windows: Process Monitor filter on `appearance.toml`.
4. **Autosave fsync rate (U-07):** `sudo fs_usage -w -f filesys <pid> | grep -E "fsync|F_FULLFSYNC"` during a 3 s fader drag.
5. **Control-thread idle cost (U-06):** `sample <pid> 5` and look at the `rbs-studio` thread share in `Host::publish`. Or `powermetrics` per-thread wake-ups (expect about 20/s).
6. **Cold start split (U-10):** add `eprintln!` timestamps (or use `RBS_LOG`) around `adapters.probe()`, `gpu()` and `Studio::start_with_tree`. A single launch on the Windows RTX 4090 + iGPU box gives the probe share of the time to first preview.
7. **Minimized cost (U-09):** minimize RBS with the Mixer and preview visible, then run `top`/Task Manager for 30 s. Today the CPU is expected to be about the same as visible.
8. U-18: `ls -l target/release/rbs` and `cargo bloat --release -p rbs-app --crates` (no full rebuild needed if the target exists) to size the generated UI share before trying opt-level "s".
9. Not examined in depth: `rbs-mcp` (separate process; its status says it uses "spaced retries"), `view::window_view` / `DockModels::update` diffing cost, and `sync_floats`. Workspace-switch time (U21, < 100 ms) was not measured here.
