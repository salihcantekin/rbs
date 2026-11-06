# ADR-0056: Keep running after the main window closes

- Status: Accepted
- Date: 2026-11-06
- Deciders: Salih (owner)
- Builds on: P13 / ADR-0013 (zero cost when unused), ADR-0015 (UI rules: U4, U5, U7, U9, U17, U18), ADR-0022 (Slint), ADR-0041 decision 7 (studio on its own control thread), ADR-0044 (control API), ADR-0051 (user data, read-only second instance), ADR-0055 decision 5 and PR #148 (Quit routed through the app's close path on macOS)

## Context

Today closing the main window quits RBS: the close handler ends Slint's event loop, `main` stops the studio and the process ends. While a stream runs, the close button first asks "End the stream and quit?" in the window frame.

The owner asked (2026-11-06) for a setting so that the close button keeps RBS running instead of quitting. Reasons:

- A recording or a stream should not end because the window was closed to get it out of the way.
- Remote control (Stream Deck, panels, scripts and agents through the control API, ADR-0044 and ADR-0052) works only while the app runs. A producer who drives RBS from a Stream Deck does not need the window.
- The platforms have their own conventions. On macOS the red close button closes a window and the app stays in the Dock until Quit (Cmd+Q, Dock > Quit). On Windows an app that runs without a window shows an icon in the notification area ("system tray"); OBS does this with its "Minimize to system tray" options.

Forces:

- Quitting and closing a window become two different actions. The question "End the stream and quit?" belongs to quitting, which ends the stream; closing the window to the background does not end anything.
- U4: the live state must be visible at all times, not by colour alone. With no window, the Dock icon or the tray icon must carry it. U9/U18: a problem must reach the user, also while no window shows.
- U5 and P13: a hidden window does no rendering and no polling. The feature costs nothing while it is off: no tray icon, no thread, no OS object.
- Slint 1.18 has no tray or status item API. `main.run()` ends the event loop when the last window is hidden.
- winit's application delegate on macOS does not answer `applicationShouldHandleReopen:hasVisibleWindows:`, so a click on the Dock icon does not bring a hidden window back. Cmd+Tab to an app with no window does not send reopen.
- A second launch today opens a second, read-only instance (ADR-0051). With a hidden first instance, that would leave the user with two copies of RBS.
- P1: core and port crates name no toolkit or OS API. The app (`rbs-app`, a composition root) may; this feature lives there.

## Decision

1. **One setting.** Settings window, new **General** section (the first section): "When I close the main window", a SegmentedControl with "Quit RBS" and "Keep RBS running", and a one-line hint that names the platform's default and how to quit ("RBS stays in the Dock. Quit with Cmd+Q. Default on Mac: Keep RBS running." / "RBS stays in the notification area. Quit from its icon. Default on Windows: Quit RBS."). Level `simple` (U2). Kept in `app.toml` in the config directory (`format_version = 1`, `on_close = "quit" | "keep_running"`). A missing, unreadable or invalid file, or a value the platform does not support (`keep_running` on Linux), means the platform default; the file is not rewritten until the user changes the setting. A change takes effect at once. Changing it back is one interaction; it joins settings undo with the other sections when settings undo exists (U17). On Linux the control shows disabled at Quit RBS, with the reason in the hint ("Not available on Linux yet."), so the section is not empty. All texts in this ADR (setting, hints, tray menu, tooltips, Dock badge, notices) go through the app's translation catalog (U8).

2. **Defaults per platform.** macOS: Keep RBS running (the platform's convention). Windows and Linux: Quit RBS (today's behaviour; a tray icon on first close would surprise users who did not choose it).

3. **Close and Quit are separate actions.**
   - *Close*: the main window's close button, Alt+F4 on Windows, and Cmd+W on macOS, which the app binds to Close on the main window (Slint does not provide it). With **Quit RBS** set, Close is Quit.
   - *Close* with **Keep RBS running** hides every RBS window and keeps the process, the studio, its outputs and remote control running. It never asks: nothing ends. An open Settings window goes through the same path as its own close (`settings_closed`: unconfirmed fields are confirmed or dropped as when it closes), then hides. Floating panel windows hide and come back with the main window; neither counts as a layout change (no layout undo entry, no workspace save).
   - The first Close to the background on this computer shows, once, in the window frame before it hides: "RBS keeps running in the Dock. Quit with Cmd+Q." (Windows: "RBS keeps running in the notification area (under the ^ arrow until you pin it). Quit it from the RBS icon there.") with OK, which has the focus. OK hides the window; `app.toml` remembers `background_notice_shown = true`.
   - *Quit* (macOS: Cmd+Q, Dock > Quit, logout, routed by PR #148; Windows: the tray menu's Quit RBS, or Close with Quit RBS set) is today's path: while a stream runs it shows the main window and asks "End the stream and quit?"; then it saves, stops the studio (a recording is closed first) and ends the process. Logout while live is held by that question (macOS reports that RBS cancelled the logout): intended, since ending a stream cannot be undone (U4).
   - The choice is a pure function of the setting, the request, the live state and whether a way back exists (`rbs_app::background::on_request`), unit-tested without a window.

4. **The event loop outlives hidden windows.** The app runs Slint with `slint::run_event_loop_until_quit` instead of `MainWindow::run`, so hiding the last window keeps the loop. Quit is the only path that calls `slint::quit_event_loop`.

5. **Hidden means idle where the UI is concerned (U5, P13).** When the main window is hidden, the preview layer is closed (its subscription to the studio's preview ends) and the meters stop, as for hidden panels. Showing the window connects the preview again. The studio keeps running: its outputs and remote control are the reason to stay, so its compositor keeps rendering at the canvas rate even when no output runs. That cost is measured in the hardware check (process CPU over 60 s, nothing live, remote control off, window shown versus hidden): hidden must cost less than shown. Putting the compositor in Standby while it has no consumer is a follow-up, needed before Keep RBS running can claim to cost nothing while idle. The tray tooltip's 1 Hz update runs only while the window is hidden and something is live.

6. **Getting the window back.**
   - macOS: the app adds `applicationShouldHandleReopen:hasVisibleWindows:` (Dock click, Finder or Spotlight launch) to winit's delegate class, the same way PR #148 adds `applicationShouldTerminate:` (`class_addMethod`, which never replaces an existing method), in the marked low-level module `rbs_app::native`. For Cmd+Tab (keyboard users), it observes `NSApplicationDidBecomeActiveNotification`; that observer shows the window only on the first activation after the app was deactivated while hidden by Close, so system dialogs, file pickers and notification clicks do not bring back a window the user just hid. Either shows the main window while it is hidden.
   - Windows: the tray icon. A click, or Enter / Space on the focused icon (Win+B, arrows), shows the main window; Shift+F10 or the context-menu key opens its menu: a status line (disabled item: "Not live", "Recording 00:12:03", "Live on <host>", or the last problem, see decision 8), Show RBS, and Quit RBS. Keyboard and Narrator use of the icon is part of the hardware check.
   - Windows, second launch: while Keep RBS running is set, the running instance owns a named per-user event (`Local\RBS.Show.<user SID>`) and waits on it with `RegisterWaitForSingleObject` (the system's wait pool, no thread of our own; the wait is unregistered when the setting is turned off or at Quit). A second launch that finds the event calls `AllowSetForegroundWindow(ASFW_ANY)` so the first may take the foreground, signals the event and exits; the first shows its main window and brings it to the front. While the setting is on, this also applies when the window is shown: a second launch brings the running instance forward instead of opening a read-only one (intended). With Quit RBS set no event exists and a second launch behaves as today (ADR-0051).
   - **One rule for failure:** when the way back cannot be set up (macOS: the reopen method or the activation observer could not be added, for example because a later winit defines it; Windows: the tray icon could not be created, for example Explorer is not running), Close means Quit, the Settings hint says why, and the app logs it. A test pins the assumption that winit's delegate class does not answer the two selectors, so a winit upgrade that changes it fails CI instead of silently quitting. The tray icon is added again after Explorer restarts (`TaskbarCreated`); the hardware check confirms it.

7. **The live state without a window (U4).**
   - macOS: the Dock tile's badge (`NSDockTile setBadgeLabel:`) shows "LIVE" while a stream runs (with or without a recording) and "REC" while only a recording runs, whether or not the window shows; nothing otherwise. Text, not colour alone.
   - Windows: the tray icon has three variants that differ in shape, not only colour: the RBS icon (not live), the RBS icon with a recording dot (recording) and the RBS icon with a broadcast glyph (live). The tooltip says the state in words ("RBS: live on <host>, 00:12:03"). Windows 11 places new tray icons in the overflow area until the user pins them; the icon is then one click away rather than always visible. This is accepted as a known limit of U4 for the hidden state; the first-close notice (decision 3) says where the icon is.

8. **Problems while hidden (U9, U18).** A problem that goes to the error bar while the window is hidden (the stream stopped after its reconnect attempts, the recording failed, the disk is full, a caught panic) still goes there, and also: on macOS the Dock badge adds "!" to the live state ("!", "REC !", "LIVE !") and the app calls `requestUserAttention` (informational; best effort, since macOS bounces the Dock icon only while RBS is not the active app); on Windows the tray icon adds a warning mark (a triangle) to its variant, so the recording dot or broadcast glyph stays, and the status line and tooltip name both the live state and the problem. The live state is never replaced by a warning (U4). Showing the window shows the error bar as usual and clears the badge or warning variant. The decision function's tests cover these cases.

9. **Tray icon library behind an app-local interface.** The Windows tray uses the `tray-icon` crate, pinned to 0.24, the release line that uses `muda` 0.19, which Slint's menu bar already brings (one copy of `muda`, one menu event channel; checked with `cargo tree -d` in the implementation PR). It is used only in `rbs_app::background::tray`, behind a small trait (`TrayHost`: `set_state`, `on_show`, `on_quit`) so the decision logic is tested without an OS icon and another library can replace it. The tray icon exists only while the setting is Keep RBS running (P13: with Quit RBS set, nothing is created); it is created when the setting is turned on, or at start when it is on, and dropped when it is turned off. Its events arrive through `tray-icon`'s callback on the UI thread's message loop and are forwarded with `slint::invoke_from_event_loop`; no thread and no polling. On macOS the Dock replaces the tray; no menu bar status item is added. Linux: not built.

10. **Crash resilience is unchanged.** The quit paths still end with `Session::end_clean` (or the exit handler of ADR-0055 decision 5). A hidden app is a running session: a crash while hidden shows the usual notice at the next start.

## Consequences

- Closing the window no longer ends a recording or a stream when Keep RBS running is set, and remote control keeps working with no window.
- "End the stream and quit?" now belongs to Quit only; the spec's live controls text changes to say so.
- Four OS integrations to maintain: the macOS delegate methods and the Dock tile badge and attention request (same technique and module as PR #148, listed in the native module's scope and checked with `risky-review`), and on Windows the tray icon and the named show event. All need a manual hardware check; CI covers the decision function, `app.toml`, the delegate-class assumption on macOS and the build.
- New dependency in `rbs-app` only (`tray-icon` 0.24, Windows target). No core or port crate changes.
- `docs/specs/ui-design-system.md` gains the General section, the tray icon, the Dock badge, the first-close notice and the Close/Quit distinction; the glossary gains "Keep running" and "notification area".
- Tests: the decision function (setting x request x live state x way back x problem), `app.toml` round trip and fallbacks (missing, invalid, unsupported on this platform), that with Quit RBS set no tray host is created (a fake `TrayHost`), and that hiding Settings goes through `settings_closed`.

## Alternatives considered

- **No setting, platform convention only** (macOS keeps running, Windows quits). The owner asked for a setting, and Windows users who drive RBS from a Stream Deck need the background mode.
- **Minimize to the taskbar instead of hiding.** Keeps a taskbar button, which is what Close is used to remove; a minimized window also still costs a little, while hidden costs nothing.
- **Tray icon on macOS as well.** Duplicates the Dock and adds a second OS object; can be added later as its own option.
- **Ask on every close ("Quit or keep running?").** A question on every close is in the way (U1); the setting decides once, and the first-close notice says how to quit.
- **Write our own Win32 tray code** (`Shell_NotifyIconW`, a message-only window). Smaller dependency tree but more `unsafe` in the app; `tray-icon` is maintained, used by Tauri and shares `muda` with Slint.
- **Second launch through the control API's IPC** (ADR-0044) instead of a named event. IPC exists only while remote control is on and needs a token; a named event needs neither.

## Review

Reviewer: independent AI session
Date: 2026-11-06
Verdict: changes requested
Concerns:
1. Windows second launch would open a second, read-only instance next to the hidden one. -> Resolved: decision 6, named per-user show event while the setting is on.
2. U4 on Windows: same red dot for recording and live, tooltip-only words, Windows 11 overflow area. -> Resolved: decision 7, three shape-distinct variants; overflow accepted as a known limit; first-close notice (decision 3).
3. Problems while hidden reach nobody. -> Resolved: decision 8 (Dock "!" and attention request, tray warning variant), with tests.
4. Failure modes incomplete and the macOS fallback contradictory; a winit upgrade could silently restore quitting. -> Resolved: decision 6, one rule (Close means Quit, hint and log), a test pinning the delegate-class assumption, `TaskbarCreated` in the hardware check.
5. Keyboard: Cmd+Tab sends no reopen; Cmd+W not provided by Slint; tray keyboard access. -> Resolved: decision 6 adds `applicationDidBecomeActive:`, decision 3 binds Cmd+W, tray keys and Narrator in the hardware check.
6. Settings hidden with unconfirmed input; floating panels on show. -> Resolved: decision 3.
7. The compositor keeps rendering while hidden with nothing live. -> Resolved: decision 5 states and measures the cost; compositor Standby without consumers is a required follow-up; tooltip updates only while hidden and live.
8. Linux empty section, unsupported values, i18n, undo. -> Resolved: decision 1.
9. Dock badge is a further OS integration and its states were unspecified. -> Resolved: decision 7 and Consequences.
10. `tray-icon` version must share `muda` 0.19; logout while live. -> Resolved: decision 9 pins 0.24; decision 3 records logout behaviour.

Reviewer: independent AI session (same reviewer, second round)
Date: 2026-11-06
Verdict: approve
Concerns:
1. Windows foreground lock on show. -> Resolved: second launch calls `AllowSetForegroundWindow(ASFW_ANY)` (decision 6); in the hardware check.
2. A blocked thread cannot be stopped; second launch while shown. -> Resolved: `RegisterWaitForSingleObject`, no own thread; second launch while shown brings the running instance forward, stated as intended.
3. A warning hid the live state. -> Resolved: decision 8 adds the warning to the live state, never replaces it.
4. `requestUserAttention` does nothing while RBS is active. -> Resolved: best effort, the badge carries it.
5. `applicationDidBecomeActive:` too broad. -> Resolved: notification observer, only the first activation after a deactivation while hidden; the failure rule needs both.
6. Windows 11 overflow wording in the notice. -> Resolved: decision 3.
7. `cargo tree -d` cannot be checked before the dependency is added. -> Open for the implementation PR.
8. Idle cost worded two ways. -> Resolved: Standby follow-up is required (decision 5, first-round line 7).
9. Process: the first-round resolution lines were written by the author. -> Noted; this second-round entry is the reviewer's verdict. Owner approval per ADR-0014 and the project's ADR policy.
