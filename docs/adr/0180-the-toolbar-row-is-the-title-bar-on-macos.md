# ADR-0180: The toolbar row is the title bar on macOS too, as on Windows

- Status: Proposed
- Date: 2026-11-24
- Deciders: Salih (owner)
- Amends: [ADR-0071](0071-merged-title-bar-on-windows.md) decision 3 (macOS kept the OS title bar)

## Context

ADR-0071 made the main window's toolbar row the title bar on Windows, with the window buttons (minimize, maximize or restore, close) drawn by RBS at the row's right end. macOS kept its own title bar: a second row above the toolbar, with the close, minimize and zoom buttons on the left.

So the same window looks and behaves differently: on macOS the top edge has two rows, the window buttons sit on the other side, and the toolbar's start is pushed by nothing on one system and by an empty title row on the other. The owner decided on 2026-11-24: one title bar on every system, the Windows way, with RBS's own bar instead of the system's, and the buttons in that area organised so they work the same everywhere (project thread "Find app behaviour that differs by OS").

The projector window (ADR-0125) already hides the macOS title bar with winit's attributes (`with_titlebar_transparent`, `with_title_hidden`, `with_titlebar_buttons_hidden`, `with_fullsize_content_view`) and moves the window with `drag_window`, and it works on the owner's Mac.

## Decision

1. **The toolbar row is the title bar on Windows and macOS.** The row (`ToolbarRow`, ADR-0071 decision 1) is unchanged: RBS button and leading items, flexible space, end items, then the window buttons minimize, maximize or restore, and close at the right end, with the same tokens (`size.caption.w`, `color.caption.close-hover`, `color.caption.close-text`), icons and names ("Minimize", "Maximize", "Restore", "Close", glossary terms) on both systems. Linux keeps the OS title bar until Linux work starts (parked). Settings, Source settings, floating and other tool windows keep their native title bars; this ADR is about the main window.
2. **macOS: changed after the window exists, as on Windows.** Slint makes the main window's `NSWindow` once the event loop runs and again after ADR-0056 hides and shows it, so no creation hook is used. After every show (the same place as Windows' `title_bar::attach_after_show`), `rbs_app::native` (a low-level module, each `unsafe` block with its SAFETY comment) sets on the window: `styleMask |= NSWindowStyleMaskFullSizeContentView`, `titlebarAppearsTransparent = YES`, `titleVisibility = NSWindowTitleHidden`, the three `standardWindowButton:` views hidden, and `movable = NO`, so AppKit's own title band does not drag or zoom on top of RBS's handling. The window stays titled and resizable: shadow, rounded corners, edge resizing, Mission Control, Stage Manager and full screen stay the system's. It also observes `NSWindowDidExitFullScreenNotification` and `NSWindowDidBecomeKeyNotification` on that window to hide the standard buttons again (AppKit can show them after full screen); the observers are added when the window is changed and removed when it closes or is changed again. If any step fails, `custom-caption` stays false, the OS title bar stays and a `warn` line says why.
3. **macOS moving and double-click.** A press in the flexible space calls winit `drag_window` (`performWindowDragWithEvent:`, which works with `movable = NO`; part of the gate). While a popup is open, a press there closes the popup and does not drag (ADR-0071 hit-test rule 2). A double-click there does what "Double-click a window's title bar to" says: `AppleActionOnDoubleClick` "Maximize" zooms, "Minimize" minimizes, "None" does nothing, "Fill" (newer macOS) zooms; a missing value falls back to `AppleMiniaturizeOnDoubleClick` (1 minimizes) and otherwise zooms; any other value zooms. A pure function maps the values and is tested.
4. **macOS buttons.** Minimize sends `miniaturize:`, maximize and restore send `zoom:`, and the shown state follows `isZoomed`, read on each resize. Close is a close request, ADR-0056's close path. In full screen the minimize and maximize buttons are hidden (they do nothing there) and close stays.
5. **Window commands on every system.** New registry commands `ui.window.minimize`, `ui.window.zoom` and `ui.window.full_screen` (window-only, no control API method, reason in `command_path.rs`) in the RBS menu's Window menu and in the macOS system menu bar's Window and View menus. Default shortcuts on macOS: Cmd+M and Ctrl+Cmd+F (system conventions, shortcuts spec); none on Windows, where the system keys (Win+Down, Win+Up, Alt+Space) keep working. Cmd+W stays bound by the app (ADR-0056).
6. **The row covers the hidden title bar.** On macOS the row's height is at least the system's title bar height, read from the window (`frame` height minus `contentLayoutRect` height) and given to the UI as `Shell.caption-min-height`, so density, themes and the slim strip shown with the toolbar hidden never leave content under the transparent title band.
7. **One `WindowFrame` interface in `rbs-app`.** Methods: `attach(&slint::Window) -> Result<(), FrameError>` (after every show), `minimize`, `toggle_maximize`, `close`, `drag` and `double_click` (no-ops on Windows, where `HTCAPTION` lets the system do both), and `maximized() -> bool`. It lives on the UI thread only (not `Send`). One function `frame::current()` picks the implementation; it and the two implementation files are the only places with `cfg(target_os)` for the title bar. `title_bar.rs` and `Shell.caption(n)` call the interface. This is the first part of the platform shell contract proposed in the analysis "RBS: app behaviour that differs by OS" (2026-11-24, project thread).
8. **Zero cost (P13).** No polling and no timers: the window is changed on show, the buttons are hidden again on two window notifications, and the drag starts from a pointer press.

## Consequences

- One top edge, one button order and one place for the window buttons on Windows and macOS.
- macOS loses the coloured buttons, the green button's full screen and tiling menu, and Option-click zoom; full screen stays in the View menu and Ctrl+Cmd+F, tiling stays by dragging to a screen edge and through the system's Window menu. VoiceOver reads RBS's three buttons; the hidden system buttons are not offered.
- Gate before merge, on the owner's Mac: clicks on toolbar items in the top band reach the toolbar; dragging the flexible space moves the window; a double-click zooms once (not twice); the window resizes from every edge, tiles, enters and leaves full screen with the buttons right afterwards; VoiceOver reads the three buttons; Cmd+M, Cmd+W, Ctrl+Cmd+F work; the preview layer (ADR-0054) and the meter layers sit in their places; hide and show through ADR-0056 (Dock, Cmd+Tab) keeps the bar; Stage Manager; a second display with a different scale; the saved window frame (`forget_saved_frame`); compact density and the toolbar hidden. If clicks in the top band are taken by the system, the implementation stops and the choice goes back to the owner.
- Visual change: a proposal and dark and light screenshots on macOS go to the owner before merge (rule 16).
- ADR-0071 gets an "Amended by ADR-0180" line. The design-system spec's Frame and Toolbar sections, the themes spec's note on the Windows close button, the shortcuts spec (new window commands) and the "on Windows" comments in `CaptionButton`, `FrameBar` and `title_bar.rs` are updated in the implementation.
- Tests: the double-click mapping; the window commands in the registry and menus; the `WindowFrame` calls are checked by hand on the Mac and SC-PC.

## Alternatives considered

- **Keep the macOS title bar** (ADR-0071 decision 3): the systems stay different, which the owner does not want.
- **Keep the macOS buttons on the left, moved into the row:** the button order would still differ between systems.
- **A borderless macOS window (no `NSWindowStyleMaskTitled`):** loses rounded corners, tiling and full screen, each of which would need rewriting.

## Review

- 2026-11-24, independent AI session (ADR, specs, code): changes requested, 6 blocking and 5 minor. Resolved in this text: the window is changed after it exists, on every show, not through the projector's creation hook (decision 2); the AppKit notifications that hide the standard buttons again (decision 2); `movable = NO` so the system's title band does not drag or zoom too, and a single zoom in the gate (decisions 2 and 3); the system menu shortcuts did not exist, so window commands are added (decision 5); the minimum height comes from the window, for every density and the slim strip (decision 6); the interface is defined, with thread, reattach, failure, the no-ops on Windows and where `cfg` lives (decision 7); the popup rule (decision 3); full screen (decision 4); double-click values (decision 3); names; ADR-0071 line, comments and spec updates; the longer hand check.
- Round 2: pending.
