# ADR-0099: Run the studio in its own process

- Status: Proposed
- Date: 2026-11-11 (revised 2026-11-21)
- Deciders: Salih (owner)
- Builds on: [ADR-0008](0008-ui-as-api-client.md), [ADR-0022](0022-native-ui-toolkit.md) decision 2, [ADR-0024](0024-node-restart-with-back-off.md), [ADR-0028](0028-cross-process-preview-surfaces.md), [ADR-0034](0034-preview-as-native-layer.md), [ADR-0035](0035-meters-as-native-layers.md), [ADR-0036](0036-windows-preview-layer.md), [ADR-0044](0044-control-api-protocol-and-security.md), [ADR-0051](0051-user-data-profiles-and-scene-collections.md), [ADR-0055](0055-crash-resilience.md), [ADR-0056](0056-keep-running-after-the-main-window-closes.md), [ADR-0134](0134-ai-tools-settings-page.md), [ADR-0156](0156-ask-before-quitting-while-recording-and-stop-off-the-ui-thread.md), [ADR-0159](0159-studio-host-in-its-own-crate.md), [ADR-0160](0160-one-platform-adapter-list.md), [ADR-0162](0162-extension-ready-registries-and-plugin-order.md)
- Amends (once Accepted, and only where the split is on): ADR-0044 decision 3 (the in-process shortcut), ADR-0035 (meters as in-process shared objects), ADR-0056 decision 6 (who owns the second-launch signal), ADR-0156 decision 5 (the shutdown runs in the studio process)

## Context

The app that ships is one process. `rbs-app` links Slint and runs the studio on its control thread (`rbs-studio`, ADR-0041 decision 7). Since ADR-0159 the studio host lives in the toolkit-free crate `rbs-studio-host`; since ADR-0160 the adapter list is one function in `rbs-platform`. The window talks to the host with `studio_host::Command` (into `ControlOut`'s UI queue) and hears `studio_host::Update` through a `notify` closure.

What is contained today (ADR-0055), checked on `main` 2026-11-21:

- A panic in a UI callback is caught at the entry point and shown in the error bar.
- A panic in a studio round is caught. After 3 in a row the studio counts as failed: every output ends cleanly, the window stays open and says "The studio stopped: …" (`Update::Failed`). There is no way back except quitting and starting RBS again.

What is not contained: a native fault (a GPU driver, an encoder SDK, a capture framework or a native plugin), an abort (out of memory, a panic during unwinding, a stack overflow) or a hang. The browser source already runs its engine in a helper process (ADR-0109), so a page cannot do this. Everything else in the studio can, and so can the toolkit. In one process each such fault ends both the window and the outputs. Two directions matter:

- **The studio dies.** Today the window disappears with it. The owner asked on 2026-11-21, before a public demo, that the window survive this.
- **The UI dies or hangs.** Today the recording and the stream end with it.

The pieces a split would use exist in part: local IPC transports (`rbs-api-ipc`, ADR-0044), cross-process preview surfaces (ADR-0028 macOS, ADR-0036 Windows) shown as a native layer (ADR-0034), atomic user-data saves by one writer (ADR-0051), a bounded shutdown off the UI thread (ADR-0156), and a host with no toolkit (ADR-0159). Missing: meters across a process (ADR-0035 hands the UI `Arc<LevelMeter>`), a wire form for `Command` and `Update`, and a second executable.

**Rule until Accepted.** While this ADR is Proposed, the tree must not start a studio process, open an endpoint for this split, or run anything decisions 2 to 9 describe. The in-process studio stays the build that ships. The "Restart the studio" action inside the one process ([crash-resilience.md](../specs/crash-resilience.md), 2026-11-21) needs nothing from this ADR.

Forces: a fault in either process must leave the other running; real-time paths stay in the process that owns the devices (P6); a second process is not free and is measured before it becomes the default (P13, P15); the user sees one Dock icon, one taskbar button and one Quit; nothing new runs while nothing fails.

## Decision

1. **Two executables in one product.** The executable the user launches is the UI (`rbs`, the app bundle's `CFBundleExecutable` on macOS). The studio is a second executable, `rbs-studio`, built by a new composition-root crate `rbs-studio` that links `rbs-studio-host`, `rbs-platform` and the control API server, and must not link a UI toolkit (no Slint, winit or windowing crate; `cargo xtask check-layers` checks its dependency tree). On macOS it lives at `Contents/MacOS/rbs-studio` in the same bundle and is not an app bundle of its own; on Windows it sits next to `rbs.exe`. Plugins and add-ons run in the studio process; their settings reach the UI as `rbs-params` schemas, which already cross a process (ADR-0162).

2. **Ownership.** The studio process owns the engine, every capture and audio device, every output (recording, stream, replay buffer, virtual camera), the only user-data writer (ADR-0051), the control API server with remote control and AI tools (ADR-0044, ADR-0134), the stream-key store, and the browser-runner helpers. The UI process owns windows, menus, the tray, the toolkit, and the preview and meter layers. It must not open a capture or audio device, encode, mux or write user data, except by a message on the link (decision 4). Each process has its own crash marker and reports (ADR-0055 decisions 4 and 5).

3. **Who starts whom, and the only way in.** One studio per OS user.
   - The studio holds an OS file lock on `<config>/studio.lock` for its lifetime. Only the process holding it may create, delete or replace the studio's endpoints. The UI never deletes an endpoint.
   - A launch of the UI first signals the running studio's **show door**: on Windows the per-user event of ADR-0056 decision 6, now owned by the studio; on macOS and Linux a socket `<runtime dir>/show.sock` (mode 0600 in a user-private directory) that accepts one fixed byte and nothing else. The show door carries no scope and no state. When a studio answers, the launched process exits; the studio brings its UI forward, or starts one if it has none.
   - When no studio answers, the UI starts one. It creates a connected anonymous pair (`socketpair` on Unix, an anonymous duplex pipe on Windows), spawns `rbs-studio` with one end inherited and no other handle, and keeps the other end. The studio is started in its own session and process group (`setsid`; on Windows `CREATE_NEW_PROCESS_GROUP`, and `CREATE_BREAKAWAY_FROM_JOB` when the job allows it, else the case is logged), so the UI ending does not signal it.
   - Two launches at once: both fail to reach the door and both spawn a studio. The second studio cannot take the lock, writes "busy" on its link and exits; that UI tries the door again for up to 2 s, signals it and exits.
   - The studio starts a UI (decisions 5 and 7) by spawning the UI executable it was started from (the path the UI passed on the link), with a new inherited link end and an attach argument. The studio never starts a UI on its own launch.

   The link is the only channel with all scopes, and it exists only as an inherited handle: no other process can open it. The control API's IPC endpoint (ADR-0044, ADR-0134) keeps its own rules unchanged.

4. **The link protocol is private to one build.** The link carries the window's `studio_host::Command` (including ADR-0163's `Command::Api` once that ADR is Accepted) and the host's `Update`, length-prefixed and serialized with an encoding chosen in phase 1 and measured in decision 9. It is not a public contract and is not versioned: the first message is the build id, and a mismatch (a new UI after an update while an old studio still runs) shows "Quit RBS and open it again to finish the update" and sends nothing else.
   - Link messages enter `ControlOut`'s UI queue, not an API session: the local user still goes first and no remote limit applies (ADR-0044 decision 5 unchanged; decision 3's in-process shortcut is replaced by the link on this path only).
   - Every `Command` and `Update` variant has a link form. Variants that hold in-process objects get one listed in the studio-host spec before phase 1: preview endpoints use the service name and token their surfaces already have (ADR-0028, ADR-0036); meters become a read-only shared-memory block of the same atomics `LevelMeter` writes, mapped by the UI's meter layer (amends ADR-0035 on this path; cost measured in decision 9); a seam that is a closure or a factory stays inside the studio process. An exhaustive `match` in a test fails when a new variant has no link form.

5. **The studio dies: the window stays.** The UI learns it from the link closing and the child process ending (process handle or `waitpid` notification; no polling). It keeps every window open and shows, in the existing failure notice, "The studio stopped unexpectedly." plus what ended (the recording and its file, the stream), and a **Restart the studio** action (the same action the one-process build has, crash-resilience spec). It also restarts the studio on its own, with back-off as ADR-0024 (1 s, then 2 s, then 4 s), at most 3 times in 5 minutes; after that it waits for the action. A new studio reads the last saved scenes and the UI subscribes to its preview again. A Matroska recording stays playable up to the fault; an MP4 being written is lost, as today.

6. **The studio hangs: the UI asks, it never kills on its own.** The studio's control thread writes a round stamp to a small shared-memory block: the time a round started, or zero while it is parked (one relaxed store at the start and at the end of each round, on the control thread, not a hot path). The UI reads it only while a link request has waited more than 2 s for its answer; nothing reads it otherwise. A round older than 5 s shows "The studio is not responding." with **Wait** and **Restart the studio**. Restart ends the studio process (`kill`/`TerminateProcess`) and then follows decision 5. The studio is never ended without that choice, because the encoder and the outputs run on other threads and may still be writing.

7. **The UI dies: the outputs keep running.** The studio learns it from the link closing. It drops that link and lets the preview subscriptions end by their own rules (ADR-0028 decision 5). Then:
   - If no output runs, the studio saves (ADR-0051) and exits. Nothing needs protecting, and Force Quit of the UI then stops RBS as the user expects.
   - If an output runs, the studio starts a UI again (decision 3) with the same back-off and limit as decision 5. The new UI shows once "RBS's window closed unexpectedly. The recording and the stream kept running." Past the limit the studio stops starting UIs and keeps the outputs running; a launch through the show door starts one.
   - A hung UI is not detected by the studio in this ADR: the outputs are safe in the studio, and the user's Force Quit of the UI is this decision's case. A UI heartbeat is left out (Alternatives).

8. **Quit, close and the platform.**
   - Quit (Cmd+Q, Dock > Quit, logout, the tray) arrives at the UI and keeps ADR-0156: the prompt while live or recording, the quitting state, everything hidden first. Then the UI sends `Shutdown` on the link, and the studio runs ADR-0156's steps with the same budgets (remote 2 s, studio 8 s, flush 2 s) and the same `rbs_crash::exit_unfinished` past them. The UI waits for the studio process to end, bounded by those budgets plus 1 s, then exits.
   - SIGTERM or a console close to the studio runs the same shutdown (the handler only sets a flag the control thread reads).
   - Close of the window stays ADR-0056: the UI hides and the studio keeps running. Hidden, the UI keeps no timer and reads no shared block (ADR-0056 decision 5).
   - macOS: the studio is a plain executable and never becomes an `NSApplication`, so it shows no Dock icon; the Dock badge and the way back stay on the UI. Which process TCC (camera, microphone, screen recording) and the Keychain attribute to the studio is checked on a Mac in phase 2 before anything else is built on it; if TCC does not attribute a child of the bundle to the bundle, this ADR returns to review.
   - Windows: one taskbar button (the UI). Linux stays one process until its preview surface exists.

9. **Off until measured.** Through phase 3 the split runs only with a developer switch (`rbs --split`) and the default build is one process. Before the default changes, a table in this ADR's Review records, on the reference machines of the performance budgets, the one-process and the two-process build, idle and with one recording plus a stream: CPU, resident memory of both processes together, thread count, idle wake-ups per second, time from launch to the first preview frame, command round trip, a box drag's round trip at the display rate, and meter latency. A human signs it (P15). The preview publish path is unchanged (ADR-0028 decision 4, ADR-0036 decision 4); nothing in this ADR adds per-frame work.

10. **Phases, after acceptance.** Each phase is its own change with its own tests.
    1. Link forms for `Command` and `Update`, the codec, the exhaustive test, and a loopback test that runs the host behind the link inside one process.
    2. `rbs-studio`, the spawn and the show door, on macOS and Windows behind `--split`; the TCC and Keychain check; decision 5 (the owner's request) first.
    3. Meters in shared memory, preview re-subscription, decision 6.
    4. Decision 7.
    5. Decision 9's table, then the owner decides whether the default changes.

## Consequences

- With the split on, a native fault, an abort or a hang in the studio leaves the window open with a way back, and a fault in the UI leaves the recording and the stream running. ADR-0055's in-process boundaries still apply inside each process.
- The UI binary keeps Slint; the studio's tree has no toolkit, checked by `check-layers`.
- `studio_host::Command` and `Update` gain a serializable link form; a new variant needs one. That is a cost on every future host change, kept small because ADR-0163 moves most window commands to `rbs_api` types, which are already serializable.
- Quit stops two processes in order. A studio left running after a UI crash has no Dock icon until a UI is back; that gap is accepted.
- Follow-up specs with phase 1: [crash-resilience.md](../specs/crash-resilience.md) moves the studio process out of "Out of scope"; the studio-host spec gains the link table; the GPU compositor spec says the preview service name and token arrive on the link; a packaging note for `Contents/MacOS/rbs-studio`. Tests with the phases: a killed studio leaves the window open and a new studio attaches; a killed UI with a recording leaves the file growing and a new UI attaches; a killed UI with nothing running ends the studio; a stuck round shows the question and nothing is ended without the answer; Quit ends both processes inside the budgets; the default build starts no second process. Process tests use injected clocks and wait for conditions, never sleeps (ADR-0032).
- Native crash dumps (`crash-handler` with an out-of-process `minidumper`, ADR-0055 follow-up) become possible: each process can be the other's dump writer.

## Alternatives considered

- **Stay one process and rely on ADR-0055 and the in-process restart.** Restart in-process covers a studio that failed through caught panics. It cannot cover a native fault, an abort or a hang. That is the gap this ADR is for.
- **The studio is the process the user launches and starts the UI.** The Dock icon would belong to a process with no windows, or the UI would be a second icon.
- **A third supervisor process.** Two are enough: the UI starts and restarts the studio, the studio restarts the UI.
- **A listening endpoint for the UI with `session.hello`.** Any same-user program could take all scopes on it, against ADR-0134's stance. An inherited handle cannot be opened by anyone else.
- **The UI as a plain `rbs-api` client now.** Waits for full U20 parity (ADR-0163's ratchet at zero) and turns every `Update` into events first. The private link lets the split be built and measured sooner; ADR-0163 shrinks it over time.
- **A UI heartbeat.** Needs a timer in a UI that ADR-0056 wants idle when hidden, and App Nap would make a healthy UI look hung. The outputs are already safe from a hung UI once they live in the studio.
- **Killing a hung studio automatically.** A stuck control round does not mean the encoder stopped; killing could end a recording that was still fine.
- **Only the recording writer in a helper.** Protects the file sooner, leaves the stream and the window in one process. It does not answer the owner's request.

## Review

Independent review per ADR-0014. Record: reviewer (human handle, or "independent AI session"), date, verdict (approve / changes requested), main concerns and how each was resolved.

Round 1. Reviewer: independent AI session. Date: 2026-11-17. Verdict: changes requested. The main decision held (the studio process owns devices, outputs and the user-data writer; the split stays off until measured). Blocking concerns:
1. The UI endpoint would give all scopes to any same-user program on a bare `session.hello`, against ADR-0044 decision 15 and the stance of ADR-0134; "the local UI" is not defined, and a second launch has no rule. -> Resolved in the 2026-11-21 revision: the link is an inherited handle only (decision 3); a second launch uses a scope-free show door and exits; one studio per user by `studio.lock`.
2. Decision 4 silently changed ADR-0044 decisions 3 and 5 and ADR-0035; meters and command parity did not exist. -> Resolved: the header names each amendment; link messages use the UI queue with no remote limits (decision 4); meters cross as shared memory, measured (decisions 4, 9); the link carries the host's own types, so U20 parity is not a precondition; WP-09, WP-12 and WP-13 are done (ADR-0160, ADR-0159).
3. The heartbeat relied on a per-session deadline that does not exist, needed the API thread, and conflicted with ADR-0056 decision 5 and App Nap. -> Resolved: no UI heartbeat (decision 7, Alternatives); the studio-hang check is a shared round stamp read only while a request waits (decision 6).
4. Restarts had no back-off or limit; Force Quit could not stop RBS; the UI side of a dead or hung studio was undefined. -> Resolved: back-off 1, 2, 4 s, at most 3 in 5 minutes (decisions 5, 7); with nothing running a dead UI ends the studio (decision 7); a dead or hung studio is decisions 5 and 6.
Follow-ups from round 1: endpoint location and stale sockets -> decision 3 (lock owner only, the UI never deletes). TCC, Keychain, activation identity and Windows job breakaway -> decision 8 and phase 2. Bounded quit and SIGTERM -> decision 8. The crash notice with two processes -> decisions 2, 5, 7. Layering -> decision 1. Decision 1 moved to Context ("Rule until Accepted"). Decision 9 now measures drag round trips, meter latency and idle wake-ups. Exiting the UI process while hidden is not taken: ADR-0056 already keeps a hidden UI idle, and the tray needs the process.

Status stays Proposed until a review of this revision approves it and the owner accepts it.
