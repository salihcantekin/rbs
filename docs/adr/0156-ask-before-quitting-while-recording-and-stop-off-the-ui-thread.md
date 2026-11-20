# ADR-0156: Ask before quitting while recording, and stop the studio off the UI thread

- Status: Accepted
- Date: 2026-11-20
- Deciders: Salih (owner); independent review recorded below; owner approved 2026-11-20
- Supersedes: [ADR-0056](0056-keep-running-after-the-main-window-closes.md) decision 3, the *Quit* clause "while a stream runs it shows the main window and asks 'End the stream and quit?'", so that a running recording also asks, and the words name what ends. The rest of ADR-0056 stays, including "Close with Keep RBS running never asks".

## Context

Quitting RBS (ADR-0056 decision 3) asks first only while a stream runs. A running recording is stopped without a question, although U4 lists "stop recording" among the actions that cannot be undone and need a confirmation while live. The owner decided on 2026-11-20 (stability plan, decision D7) that quitting while recording or streaming asks first, then closes.

The quit itself had two problems found in the 2026-11-20 review of the `browser-gpu-texture` work:

- `entry::quit_loop` ends the event loop from a new thread after `sleep(200 ms)` (commits 2feaabf, 8760f77). The sleep is a timing guess. The hang it was added for had another cause, which 8760f77 also fixed: an idle control thread parks with no timeout, did not notice that its channel closed, and `StudioHost::drop`'s join waited for ever, so the process stayed with its window hidden. In Slint 1.18.1 `slint::quit_event_loop` only posts a user event (`CustomEvent::Exit`) that winit 0.30.13 handles later on a clean stack, also when it is called inside `WM_CLOSE`.
- After the event loop ends, `main` hides only the main window and then stops the studio on the UI thread: `app.studio.host = None` runs `StudioHost::drop`, which joins the control thread. That thread closes the recording (up to `CLOSE_TAIL_GUARD`, 1 s, of tails), ends the outputs, stops helper processes and shuts the studio down. Then `UserSession::flush` waits for the user-data writer. Neither wait has a deadline. While it lasts, the UI thread pumps no events: on macOS a window ordered out without a run loop turn can stay on screen, and AppKit can show the app as not responding.

## Decision

1. **Quit asks while a stream or a recording runs.** `background::on_request` takes the live state (`streaming`, `recording`) instead of one `stream_live` flag. A stream counts from Go live until it has ended, a recording from its start until it has stopped, each for the main and the vertical output. The question is a card under Go live and Record, in the place and style of the End stream card (the toolbar row has no room for it; owner, 2026-11-20): a title, one line on what happens, and two buttons, the ending one with `kind: "live"` and the keep one with the focus; Escape answers keep. No new tokens. Its words name what ends:
   - stream only: "End the stream and quit?", "Viewers see it end.", "End stream and quit", "Keep streaming";
   - recording only: "Stop recording and quit?", "The recording is saved up to now.", "Stop recording and quit", "Keep recording";
   - both: "End the stream and the recording, and quit?", "Viewers see the stream end. The recording is saved up to now.", "End both and quit", "Keep both".
   The virtual camera and an armed replay buffer do not ask: nothing a viewer sees ends and no take is cut. Close with Keep RBS running still never asks. When the app state is borrowed by a callback in progress, the request is retried once from a zero-delay `entry::single_shot` and decided then; if it is still borrowed, it is dropped and logged, never turned into a quit without asking. The decision uses the UI's live state; a take started by a control API client in the last moment before the quit may not be seen yet (known limit). Windows logoff and shutdown (`WM_QUERYENDSESSION`) are unchanged by this ADR.
2. **No sleep in the quit.** The `sleep` and the `rbs-quit` thread go; the last shutdown step's report ends the loop with a direct `slint::quit_event_loop` (decision 4), which never runs inside `WM_CLOSE`. The hardware check covers the toolbar Close, Alt+F4 and the tray menu's Quit RBS on Windows, and Cmd+Q, Dock > Quit and the close button on macOS, each while recording.
3. **Quitting is a state.** Once Quit is decided the app is *quitting* until the process ends. While quitting: `request` answers `KeepWindowShown` and does nothing else (a second Cmd+Q, Close or tray Quit is ignored); show requests (Dock click, reopen, Cmd+Tab activation, tray, second-launch signal) are ignored; studio updates are dropped except the step reports of decision 4, so no preview, Interact window, panel, error or layout change happens after the last save; UI saves are not attempted. On macOS the app holds a user-initiated `NSProcessInfo` activity from Quit until exit, so App Nap does not throttle the shutdown.
4. **Everything visible goes first, inside the event loop.** On Quit (and after the prompt is answered), the app saves the layout, hides every RBS window (`hide_all`: Settings, floating windows, the main window), closes the preview, projector and Interact layers, removes the tray icon and the single-instance event (ADR-0056 decision 6), without arming the hidden-window observers (`native::set_hidden`), and only then starts the shutdown. The event loop keeps running during the shutdown, so the windows really leave the screen and the app answers the OS.
5. **The studio stops on a shutdown thread.** The app moves the remote-control server (`StudioUi.remote_server`), the studio host and the user-data writer (`UserSession::into_saver`, a new call that hands over the `Saver`) to one thread named `rbs-shutdown`. Only `Send` values move (a compile-time assertion checks it); every `Rc` and Slint handle stays on the UI thread. In this order, which a pure function names and a test pins: remote control detached (`detach_api`, server stopped), studio host dropped (its control thread closes the recording and ends the outputs), user data flushed and the `Saver` dropped (its writer joined, the writer lock released). The thread sends the end of each step on an `mpsc` channel owned by a step watcher and then wakes the UI thread with `entry::invoke`; the watcher, never the invoke, decides. After the last step the UI thread calls `slint::quit_event_loop`. The UI thread never joins or waits. Each step has its own budget so a slow step does not starve the next: remote control 2 s, the studio 8 s, the user-data flush 2 s. The step-order function returns (step, budget) pairs, so no step can lack one. If the `rbs-shutdown` thread cannot be spawned, the steps run after the event loop has returned, as before this ADR, and the app logs it. A single-shot timer on the UI thread checks the budget of the running step; it drains the channel (`try_recv`) before it decides, so a step that finished wins over its budget.
6. **Past a budget the exit is not clean.** The UI thread writes and flushes one log line naming the unfinished step, and calls a new `rbs_crash::exit_unfinished(step)`. In `rbs-crash`'s marked low-level module, that call writes a report of a new kind, "shutdown did not finish: <step>", to `crash-reports/` (it is not limited by the per-process report cap; `Unclean` gains a `kind`, additive since it is `non_exhaustive`), keeps the running marker, and ends the process without running exit handlers or static destructors (`_exit` on Unix, `TerminateProcess(GetCurrentProcess(), 1)` on Windows, declared in `extern` blocks of that module, no new dependency), because a thread that is still stuck may hold a lock those handlers need. The next start shows the notice for that report: "RBS did not finish closing last time. The last recording may be cut short; a Matroska recording stays playable up to that moment." User data is written with a temporary file and a rename (ADR-0051), so a cut-off write leaves the previous file whole.
7. **A second launch while RBS is closing.** On Windows it finds no single-instance event (decision 4) and starts. On macOS a Finder or Spotlight launch arrives as a reopen, which decision 3 ignores. A new instance that starts before the closing process released the user-data lock opens read only for its whole session, as any second instance does (ADR-0051); the window for this is at most the budgets of decision 5 (known limit). Logout during the shutdown is still cancelled by the app, as in ADR-0055 decision 5 (known limit; answering `TerminateLater` is a follow-up).
8. **Tests.** The decision function covers streaming × recording × request × setting × way back. The step watcher is a small type fed by a channel the test controls and an injected clock (`TestClock`): a step that finished before its budget is done, also when the budget is already over at the moment it is checked (done wins); a step that never finishes is over budget; no test sleeps (ADR-0032). The step order is a pure function with its own test, which also checks that every step has a non-zero budget.

## Consequences

- Quitting while only recording now takes one more click; it can no longer lose a take by accident.
- Every window disappears as soon as Quit is decided; the time the studio needs to close its files is no longer seen as a frozen app.
- A shutdown that hangs past its budget ends the process and is reported at the next start, saying the recording may be cut short, instead of leaving an invisible process behind.
- `docs/specs/ui-design-system.md` (Go live and Record) changes its quit sentence to the three texts above; `docs/specs/crash-resilience.md` gains the report kind and its notice text.
- Measuring: Quit to the last window gone under 100 ms on macOS and Windows; a quit while recording leaves an intact file.

## Alternatives considered

- **Keep `sleep(200 ms)`.** A guess, and the hang it was meant for had another cause.
- **Stop routing the toolbar Close through `SC_CLOSE`.** Loses the OS close path that ADR-0056 relies on, and is not needed once the parked thread is woken.
- **Wait for the studio on the UI thread after the loop with a deadline.** Simpler, but no events are pumped: on macOS the windows may stay and the app is reported as not responding.
- **Exit at once without waiting for the studio.** Loses the end of the recording and the last user-data write.
- **`std::process::exit` past the deadline.** Runs exit handlers and DLL detach while a stuck thread may hold their locks, so it can hang itself.
- **Ask on every quit.** In the way when nothing is live (U1, ADR-0056 alternatives).

## Review

- Reviewer: independent AI session (architecture-review skill), 2026-11-20. Verdict: changes requested.
- Concerns and resolutions:
  1. Decision 2 had no proven root cause; a timer adds nothing because `quit_event_loop` already posts an event. -> Root cause stated in Context (parked control thread, fixed by 8760f77); `quit_event_loop` is called directly; Windows and macOS hardware checks listed.
  2. Windows hidden after the loop may stay on screen on macOS, and the app reads as not responding; only the main window was hidden. -> Decision 3: every window, layer and the tray go inside the event loop; decision 4 keeps the loop running during shutdown.
  3. The forced exit was unspecified; `exit` and `ExitProcess` can deadlock with a stuck thread. -> Decision 5: `rbs_crash::exit_unfinished`, `_exit` / `TerminateProcess`.
  4. "Closed unexpectedly" hides a cut take. -> Decision 5: own report and notice text.
  5. A second launch while closing was not covered. -> Decision 6.
  6. One deadline starves the save; are writes atomic? -> Decision 4: per-step budgets; decision 5: ADR-0051 atomic writes.
  7. The ask rule was incomplete (borrow failure, virtual camera, replay buffer, logoff, UI lag). -> Decision 1 decides each case and passes the live state into `on_request`.
  8. "Done at zero deadline" races. -> Decision 7: done wins, controlled channel and `TestClock`.
  9. Hygiene: `Saver::drop` vs `flush`, ADR-0055 decision 6 wording. -> Decision 4 names `UserSession::into_saver`; Context reworded.
- Second round: independent AI session, 2026-11-20. Verdict: changes requested.
  1. Windows come back during shutdown (show, reopen, a second Quit). -> Decision 3, the quitting state.
  2. Studio updates keep changing UI state after the save. -> Decision 3 drops them.
  3. Step reports via `invoke` race the budget timer. -> Decision 5: channel owned by the watcher, drained first.
  4. The notice cannot be told apart; the report cap can block it; how the exit calls are declared. -> Decision 6.
  5. Name what moves (remote server), keep `Rc`/Slint on the UI thread, drop the `Saver`. -> Decision 5.
  6. App Nap. -> Decision 3.
  7. Unbounded borrow retry, read-only second instance, logout, decision 2 wording. -> Decisions 1, 2 and 7.
- Third round: independent AI session, 2026-11-20. Verdict: changes requested, one blocking concern; all round-1 and round-2 resolutions checked against the code and hold.
  1. The remote-control step had no budget, and dropping `RemoteServer` joins its transports. -> Decision 5: every step has a budget (remote control 2 s) from (step, budget) pairs; decision 8 tests it; the report names the step.
  - Not blocking: a failed spawn of `rbs-shutdown`. -> Decision 5: the steps run after the loop as before, logged.
- After round 3 only the blocking concern remained and it is resolved as the reviewer proposed. Owner approved the ADR and the new notice text on 2026-11-20. Before merge the owner moved the question from the toolbar row to a card under Go live and Record (decision 1).
