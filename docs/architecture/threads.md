# Threads

Every thread RBS starts, what it does, and what it may wait on. Read this before adding a thread, a channel receive, a join, a lock shared between threads, or a wait for the GPU.

Related: [hot paths](hot-paths.md) (what runs per frame or block), [principles](principles.md) P6 (real-time paths) and P13 (zero cost when unused), the [crash-resilience spec](../specs/crash-resilience.md) (panic boundaries, the ban on nested event loops and on blocking the UI thread, the stall watch), [ADR-0055](../adr/0055-crash-resilience.md), [ADR-0156](../adr/0156-ask-before-quitting-while-recording-and-stop-off-the-ui-thread.md) (shutdown off the UI thread).

## The waiting rule

A thread may only wait on something that is sure to answer within its own budget. Three threads are special:

1. **The UI thread** (the process's main thread while Slint's event loop runs) never joins a thread, never blocks on a channel (`recv`, `recv_timeout`, a `Condvar` wait), never takes a lock another thread can hold for longer than a copy, never waits for the GPU on the CPU (a fence wait, `device.poll(Wait)`, a blocking readback), never sleeps and never runs a nested event loop. Work that may wait goes to a thread of its own and comes back through `entry::invoke`. The steps after the loop has ended (`Ctx::after_loop`) may join: no window is answering then.
2. **CEF callbacks** in the browser helper (`rbs-browser-cef`: `OnPaint`, `OnAcceleratedPaint`, the audio handler, life-span and load handlers) run on CEF's own UI thread, which also handles the page's input. The same rule holds there: never wait on the CPU for the GPU, a keyed mutex, a fence or the studio ([ADR-0157](../adr/0157-browser-gpu-pages-sync-on-a-shared-fence.md)).
3. **Real-time threads** (audio device callbacks, engine workers inside a tick, capture callbacks) follow P6: no allocation, no blocking, no I/O, no contended lock.

### How it is checked

- `rbs_app::ui_thread::assert_off_ui(what)` is a `debug_assert!` at the top of helpers that wait on another thread: `StudioHost::drop` (joins the control thread), `Saver::drop` (joins the user-data writer) and the calibration lanes. In a debug build, calling one on the UI thread while the event loop runs panics inside the entry point's boundary, so the error bar shows it and the test or the developer sees it at once. Add the call to every new helper that joins, receives or waits on a condition.
- `main` runs the event loop through `rbs_app::ui_thread::run_loop`, which marks the UI thread right before `slint::run_event_loop_until_quit` and removes the mark right after it.
- The stall watch (crash-resilience spec, "UI thread waits and stalls") logs any UI entry that runs 100 ms or more, with where that entry was installed, and a UI thread that does not answer for 1 s or more while it still runs. Lines start with `rbs: ui:` on standard error (the diagnostics log of quality plan WP-21 takes them over when it exists).
- `crates/rbs-app/clippy.toml` bans `std::thread::sleep`, the blocking `rfd` dialogs and nested event loops in the app crate.

### Known waits on the UI thread

These are waits the rule does not allow yet. Each one is bounded, and each is a follow-up to move off the UI thread; the stall watch shows how long they take on a real machine.

| Wait | Where | Bound | Follow-up |
| --- | --- | --- | --- |
| `UserSession::flush` and `UserSession::run`: wait until the writer thread has written every queued save, then run a task there and receive its result | `user_session.rs`; called on profile and collection switch, load, export and import, theme import and export, OBS import | One disk write of the queued saves (milliseconds on a local disk, longer on a slow or network folder) | Make these asynchronous: queue the task, answer through `entry::invoke` |
| Short locks on state shared with another thread (the stream key store, the AI tools' recent-actions ring, the browser runtime slot) | `stream_key.rs`, `ai_tools.rs`, `browser_tree.rs` | The other side holds each lock only for a copy | None while that stays true |
| Small file reads and writes (settings files, themes, the appearance file) | `look.rs`, the `*_settings.rs` files | One small file | Moved with the WP-21 and settings work where a read is on a slow folder |

## Thread map

Names are the OS thread names (`std::thread::Builder::name`), so they show in a debugger, a crash report and the OS tools. A new thread gets a name with the `rbs-` prefix and a row here in the same change.

### Desktop app process (`rbs`)

| Thread | Started by | Does | May wait on | Never |
| --- | --- | --- | --- | --- |
| Main (UI thread) | `main` | Slint's event loop: windows, input, layout, rendering, every `entry::*` callback, the preview layers on macOS and Windows | Nothing that can stall (rule above); known waits listed above | Join, receive, contended lock, GPU wait, sleep, nested loop |
| `rbs-ui-watch` | `ui_thread::loop_started` | Logs a UI thread that has not returned from an entry for 1 s, then each time the wait doubles | Parked while the UI is idle; a timed park while an entry runs | Anything that touches UI state |
| `rbs-log` | `rbs_log::start` (not wired into the app yet) | Writes the diagnostics log: redacts, writes, rotates and prunes files, keeps the last 500 lines ([ADR-0158](../adr/0158-diagnostics-log.md)) | Its queue with no timeout when idle (60 s while a repeat count is pending), the disk, a copy of the secrets list | Being waited on by the UI thread (`Logger::flush` is off the UI thread only) |
| `rbs-studio` | `StudioHost::start` | The studio's control thread: commands from the UI and the control API, studio service, outputs, snapshots ([ADR-0041](../adr/0041-studio-recording-on-a-running-engine.md) decision 7) | Its message channel, parked with no timeout when idle ([ADR-0154](../adr/0154-wake-the-control-thread-on-bypass.md)); the engine's answers; helper start-up with deadlines | Waiting on the UI thread |
| `rbs-shutdown` | `shutdown::start` (Quit) | Drops remote control, the studio host and the user-data saver in order, each within a budget ([ADR-0156](../adr/0156-ask-before-quitting-while-recording-and-stop-off-the-ui-thread.md)) | Joins of the threads it stops | Waiting on the UI thread |
| `rbs-user-data` | `Saver::start` | Writes profiles, scene collections and themes ([ADR-0051](../adr/0051-user-data-profiles-and-scene-collections.md)) | Its job queue (`Condvar`), the disk | |
| `rbs-preview-connect`, `rbs-projector-connect`, `rbs-interact-connect` | `preview.rs`, `projector.rs`, `interact.rs` | Connect to the studio's frame endpoint off the UI thread, then hand the layer to the UI thread | The endpoint, with a timeout | |
| `rbs-preview`, `rbs-projector`, `rbs-interact` | the same files | Receive frames and keep only the newest for the UI thread | The frame endpoint | Waiting on the UI thread |
| `rbs-text-measure` | `studio_host/text_measure.rs`, `studio_host.rs` | Measures a text block without blocking the control thread (a late answer is dropped with its thread) | The text renderer | |
| `rbs-calibration`, `rbs-calibration-lane-<n>`, `rbs-calibration-reaper` | `studio_host/calibration.rs` | Encoder calibration runs and their encoder sessions | Lane answers; reaper joins | Running on the UI thread |
| `rbs-replay-clip`, `rbs-stinger-look`, `rbs-add-on-install`, `rbs-region-grab`, `rbs-thumbnail-<n>`, `rbs-permission`, `rbs-picker`, `rbs-vcam-setup`, `rbs-profile-sweep`, `rbs-profile-remove` | the `studio_host` files, `vcam_page.rs`, `browser_profiles.rs` | One-off jobs that read files, ask the OS or write a clip, then report back | Their own work | Waiting on the UI thread |
| Unnamed child waits | `reveal.rs`, `privacy.rs` | Wait for an opened system app to end so it does not stay a zombie | The child process | |
| `rbs-api`, `rbs-api-ipc accept`, `rbs-api-ipc read`, `rbs-api-ipc write`, `rbs-api-ws` | `rbs-api`, `rbs-api-ipc`, `rbs-api-ws` | The control API server while remote control is on (off by default) | Sockets, pipes, its inbox | Waiting on the studio's answers for longer than a request's deadline |

### Engine and media (in the app and `rbs-cli`)

| Thread | Started by | Does | May wait on | Never |
| --- | --- | --- | --- | --- |
| `rbs-control` | `rbs-engine` `EngineHandle` | The engine's command loop: graph changes, start and stop | Its command channel | |
| Engine workers (one per partition, named by the partition) | `rbs-engine/src/worker.rs` | Ticks: `Executor::run_tick`, swap of pending graphs (hot path) | The pacing clock or the doorbell between ticks | Blocking inside a tick (P6) |
| `rbs-media`, `rbs-media-reaper`, `rbs-replay-reaper` | `rbs-nodes` | Media file decode workers and the threads that join stopped ones | Decoder, file, rings | |
| `rbs-text-render`, `rbs-text-probe` | `rbs-sources/src/text_worker.rs` | Text rasterising for text sources | Its request slot | |
| Capture janitors (named per capture type) | `rbs-capture/src/handoff/graveyard.rs` | Release capture frames that are still in use when a session ends | The frames' release | |
| `rbs-estimate-probe`, `rbs-protection-probe` | `rbs-record` | Encoder probes for the recording estimate and protection | The encoder | |
| `rbs-file-transport`, `rbs-rtmp-transport`, `rbs-srt-transport` | `rbs-transport-*` | Write or send packets of a recording or stream | The disk, the network, their packet queue | |
| Audio device threads (`rbs-wasapi-audio`, `rbs-wasapi-render`, `rbs-pw-audio`; CoreAudio's own callback threads) | `rbs-audio-io-*` | Device callbacks and the sample rings (hot path) | The device | Anything P6 forbids |
| Capture threads (`rbs-pipewire`, `rbs-sck-cursor`; ScreenCaptureKit, AVFoundation and Media Foundation callback queues) | `rbs-capture-*` | Frame callbacks and the hand-off ring (hot path) | The OS capture API | Anything P6 forbids |
| `rbs-vcam-mf` | `rbs-vcam-mf` | The Windows virtual camera's media source | The camera frame server | |
| `rbs-browser-supervisor` | `rbs-browser-runner` | One per browser source: forwards control messages to the helper, publishes its frames and sound, checks the helper's heartbeat | A timed poll (twice per page frame; slower while hidden) | Waiting on the helper without a deadline |

### Browser helper process (`rbs-browser-cef`)

| Thread | Does | May wait on | Never |
| --- | --- | --- | --- |
| CEF UI thread | Page life cycle, input, `OnPaint`, `OnAcceleratedPaint` | Nothing (rule 2 above) | CPU waits on the GPU, a keyed mutex, a fence or the studio |
| `rbs-browser-stdin` | Reads control messages from the studio and posts them to CEF | Standard input | Touching CEF objects directly |
| CEF's own threads (IO, GPU process, renderer processes) | Chromium | Chromium's own rules | |

## Adding a thread

1. Name it (`rbs-<what>`), and add its row to the map above in the same change.
2. Say what it may wait on, and make every wait end: a channel that disconnects when its owner drops, a deadline, or a stop flag with an unpark. A thread that parks with no timeout needs a wake on every way to stop it ([ADR-0154](../adr/0154-wake-the-control-thread-on-bypass.md)).
3. Start it only when its feature is Active (P13), and stop it when the feature is no longer used.
4. If the UI thread ever joins or drops it, move that to the shutdown thread or a detached stop instead, and put `assert_off_ui` in the helper that waits.
