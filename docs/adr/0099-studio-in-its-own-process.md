# ADR-0099: Run the studio in its own process

- Status: Proposed
- Date: 2026-11-11
- Deciders: Salih (owner)
- Builds on: [ADR-0008](0008-ui-as-api-client.md), [ADR-0022](0022-native-ui-toolkit.md) decision 2, [ADR-0028](0028-cross-process-preview-surfaces.md), [ADR-0034](0034-preview-as-native-layer.md), [ADR-0036](0036-windows-preview-layer.md), [ADR-0044](0044-control-api-protocol-and-security.md), [ADR-0051](0051-user-data-profiles-and-scene-collections.md), [ADR-0055](0055-crash-resilience.md) (follow-up only), [ADR-0056](0056-keep-running-after-the-main-window-closes.md)

## Context

ADR-0008 makes every UI a client of the control API, and ADR-0022 decision 2 names two processes as the topology: the engine exports preview surfaces, the UI imports them. The app that ships is still one process. `rbs-app` links Slint and runs the studio on a control thread in that process (ADR-0041 decision 7). That is ADR-0022's optional shared-device mode.

ADR-0055 contains a panic whose unwind path is Rust, on the UI thread and on the studio's control thread. It does not contain a native fault in the toolkit, the window's GPU driver, or a library loaded into the UI process (class E), or a UI thread that stops returning (class G). Those take the recording and the stream with them, because they share the process. ADR-0055 lists "the studio in its own process" as follow-up work and says it is not part of that decision.

The pieces a split would use already exist. Local IPC for the control API is specified (ADR-0044: current-user socket or named pipe, same-user peer). Preview already crosses a process as native handles (ADR-0028 on macOS, ADR-0036 on Windows) and is shown as a native layer (ADR-0034). User data is saved atomically by one writer (ADR-0051). On macOS, Close and Quit are different, and Dock > Quit is routed through the app's own path (ADR-0055 decision 5, ADR-0056).

Forces: a fault or a hang in the UI must leave outputs running; the real-time paths stay in the process that owns the devices (P6); a second process is not free, so its cost is measured before it becomes the build that runs (P13, P15); the user sees one Dock icon and one Quit.

## Decision

1. **The split stays off until this ADR is Accepted.** While the status above is Proposed, the tree must not start a studio process, must not open an IPC endpoint for this split, and must not run a heartbeat. The in-process studio stays the implementation that ships. Remote-control IPC (ADR-0044) is a different endpoint and is unchanged. After this ADR is Accepted (review recorded, owner approves, ADR-0014), the split may be built, and it stays off in the default build until decision 8 is recorded.

2. **The UI process starts the studio.** The process the user launches is the UI (the app bundle's main executable). On launch it connects to the studio endpoint in the user-private config directory. If the connect fails, it deletes a stale socket, starts the studio executable, and connects. If the connect succeeds, it attaches and must not start a second studio. One studio per OS user. The studio must not start a UI at its own launch. It is started so the UI process exiting does not signal it. The studio restarts the UI only in decisions 5 and 6, by `posix_spawn` of the UI executable path the UI passed at spawn, with an attach argument that names the endpoint.

3. **The studio process owns the engine, the outputs, the user-data writer, and the control API.** It runs the engine, every output (recording, stream, and the rest), the only writer of user data (ADR-0051), and the control API server, including remote control. It must not link a UI toolkit (no Slint, winit, or a windowing system). The UI process owns windows, menus, the toolkit, and the preview and meter layers. It must not open a capture or audio device, encode, mux, or write user data except by a control API request.

4. **The UI attaches over local IPC.** The studio listens on the endpoint from decision 2 (ADR-0044: mode 0600, current user, `session.hello` for a same-user peer). The UI is a client of the `rbs-api` types, serialized. The in-process shortcut (ADR-0044 decision 3) does not apply on this path. Preview and meters are not API messages. The UI subscribes to the preview surfaces that already exist (ADR-0028, ADR-0036) and shows them with the existing native layer (ADR-0034). A UI that attaches after the previous one died subscribes again once that subscription has ended (ADR-0028 decision 5). The service name and token are delivered on the control session, which is the product path ADR-0028 already names.

5. **A dead UI leaves outputs running.** The studio drops that control session. The preview subscription ends by the rule those surfaces already have (a dead port, or the kernel notice that the UI can hold no surface). The studio then starts the UI again (decision 2). The new UI attaches, reads the last saved scenes, and subscribes to the preview. Outputs are not stopped and not restarted because the UI died. Force Quit of the UI is this case, not decision 7.

6. **A hung UI is the heartbeat.** While its event loop is running, the UI sends `session.heartbeat` (no parameters, scope `read`) at least every 1 second. The studio's API thread, using the session state machine's existing deadline (ADR-0044 decision 5), treats that session as hung when 3 seconds pass with no request from it. The deadline exists only while the local UI session exists. There is no heartbeat thread, no wake while the split is off, and no heartbeat work on an audio or render thread. On a hang the studio ends the session, ends the preview subscription if the UI still holds one, terminates the hung UI process, and starts a UI (decision 2). A missed heartbeat must not stop an output.

7. **macOS: one app bundle, one Dock icon, Quit shuts the studio down.** The UI executable is `CFBundleExecutable`. The studio executable lives at `Contents/MacOS/rbs-studio` in that same bundle, is not its own app bundle, and must not activate an application or show a Dock icon. Dock badge and the way back from a hidden window stay on the UI (ADR-0056). Quit (Cmd+Q, Dock > Quit, logout) is delivered to the UI and keeps today's path: cancel system termination, ask while a stream is live, save, send the studio a shutdown request, wait until outputs are stopped and the recording is closed, then both processes exit. Close of the window stays ADR-0056 (the UI hides, the studio keeps running). Windows uses the same ownership and attach rules, with the preview from ADR-0036, and one taskbar button (the UI). Linux stays one process until its preview surface exists.

8. **Measure the cost before the split is turned on.** Before the default build runs two processes, a table in this ADR's Review records, on the reference machines in the performance budgets, the one-process build and the two-process build, idle and with one recording: CPU, resident memory, thread count, and the time from process start to the first preview frame. A human signs it (P15). Until that table is recorded, the default build is one process. The preview publish path is unchanged (ADR-0028 decision 4, ADR-0036 decision 4). This decision adds no per-frame work.

## Consequences

- Once the split is on, a native fault or a hang in the UI process leaves the recording and the stream running, and a new UI attaches. ADR-0055's in-process boundaries still apply inside the studio process; this split does not replace them.
- The UI binary can keep Slint. The studio binary's dependency tree has no UI toolkit, which `cargo tree` can check.
- Every command the UI sends is serialized JSON-RPC on the local socket. Decision 8 is what decides whether that cost is acceptable as the default.
- Quit must shut down two processes in order. A studio left running after a UI crash has no Dock icon until the UI is back; that gap is accepted.
- While this ADR is Proposed, decisions 2–7 must not be implemented. Decision 1 is in force now.
- Follow-up specs, after acceptance (not part of this ADR): [crash-resilience.md](../specs/crash-resilience.md) moves the studio process out of "Out of scope"; [control-api.md](../specs/control-api.md) gains `session.heartbeat`, the local UI session, and the endpoint hand-off; the GPU compositor spec states that the service name and token arrive on that session; a packaging note for `Contents/MacOS/rbs-studio` and the single Dock icon. Tests to add with the implementation: a killed UI leaves a recording open and a new UI attaches; three seconds without a request restarts a hung UI and does not stop the output; Quit stops the output; the default build starts no second process.

## Alternatives considered

- **Stay one process and rely on ADR-0055.** The boundaries that exist stay, and they are still required inside the studio. They do not cover a native fault or a hung UI thread. That is the gap this ADR is for.
- **The studio is the process the user launches, and it starts the UI.** The Dock icon would belong to a process with no windows, or the UI would be a second icon. One icon means the bundle the user opens is the UI.
- **A third supervisor process.** Two processes are enough: the UI starts the studio, the studio restarts the UI.
- **A heartbeat thread, or a timer that runs while the split is off.** ADR-0044 already sleeps until the next deadline, and only while a session exists. A thread that ticks anyway breaks P13.
- **Turn the split on before it is measured.** The cost of a second process and of serialized control is unknown. Decision 8 keeps the default at one process until the numbers are in the Review.
- **Only the recording writer in a helper** (an ADR-0055 follow-up). It protects the file sooner and leaves the engine and the stream in the UI process. It can be compared later; it is not this decision.

## Review

Independent review per ADR-0014. Record: reviewer (human handle, or "independent AI session"), date, verdict (approve / changes requested), main concerns and how each was resolved.
