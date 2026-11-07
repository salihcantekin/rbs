# ADR-0071: The toolbar row is the title bar on Windows, and the row leaves no gaps

- Status: Proposed
- Date: 2026-11-07
- Deciders: Salih (owner)
- Builds on: ADR-0049 (frame and areas), ADR-0050 (toolbar), ADR-0054 (preview stays visible), ADR-0057 (theme tokens), ADR-0069 (RBS menu)

## Context

The main window's frame has a toolbar row (ADR-0050) under the OS title bar. Two problems came up on 2026-11-07:

- When an item at the row's end is not shown, the row keeps its spacing, so the Settings button stops short of the corner. Each item handles its own spacing; nothing owns the row. (Whether the live indicator shows while idle is decided by U21 and the indicators work, PR #188; this ADR makes the row leave no gap either way.)
- On Windows, after ADR-0069 removed the menu bar row, the OS title bar is a second row that holds only the window title and the minimize, maximize and close buttons. The owner wants one tidy top edge, as VS Code has.

On a decision card on 2026-11-07, the owner chose to merge the toolbar row with the title bar on Windows, over keeping the native title bar, and was shown the mockup (`/mnt/project-files/title-bar/mockup.png` in the project).

## Decision

1. **One toolbar row component.** The frame's toolbar row is laid out by one component (`ToolbarRow`, contract in the design-system spec) with three parts: a leading group (the RBS button, then the toolbar items before `~`), the flexible space, and an end group (the items after `~`, then the live indicator wherever the spec shows it, then the window buttons on Windows). The in-row prompts (quit and end-stream confirms) and status texts ("Reachable from other computers", "Layout locked") belong to the leading group's end.
   - An item that is not shown takes no width and adds no spacing.
   - A separator is dropped when it would lead, end or double.
   - The last shown item of the end group sits `space.2` from the row's right edge, as the first item sits from its left edge. On Windows the window buttons end the row with no padding.
   - Spacing and sizes come from tokens (ADR-0057). The row's height (`size.control.md` + 2 × `space.1` + `size.border`, 37 px by default) is the caption height on Windows.
2. **Windows: the toolbar row is the title bar.** The main window keeps its native frame (`WS_OVERLAPPEDWINDOW`), so side and bottom resizing, snapping, shadows, the Alt+Space menu and maximize animations stay the OS's. Only the caption is removed, by a window subclass (`SetWindowSubclass`) in `rbs-app` `src/win/` (an existing low-level module, every `unsafe` block with its SAFETY comment).
   - **Lifecycle.** The HWND is read through `raw-window-handle` as `preview.rs` does. The subclass is installed when the main window's HWND is first seen and again whenever it changes (ADR-0056 hides and re-shows the window), followed by `SetWindowPos(SWP_FRAMECHANGED)`; it removes itself on `WM_NCDESTROY`. It runs on the UI thread, in front of winit's procedure, and passes every message it does not handle on.
   - **Caption.** `WM_NCCALCSIZE` makes the client area start at the window's top. While maximized, the client area is inset by the frame thickness (`GetSystemMetricsForDpi(SM_CYFRAME) + SM_CXPADDEDBORDER` at `GetDpiForWindow`), so nothing is drawn off screen, and the bottom edge leaves one pixel for an auto-hide taskbar. On Windows 10, `DwmExtendFrameIntoClientArea` with a one-pixel top margin keeps the native top border and shadow.
   - **Hit test.** `WM_NCHITTEST` answers in this order:
     1. the top resize band (the frame thickness above; none while maximized): `HTTOP`, `HTTOPLEFT`, `HTTOPRIGHT`; other edges get the OS's own answer;
     2. while any popup is open (`popups::open_in` not empty): `HTCLIENT`, so a click closes the popup instead of starting a drag;
     3. the maximize button: `HTMAXBUTTON`, so Windows 11 shows its snap layouts;
     4. any shown item of the row, prompts and texts included, and the minimize and close buttons: `HTCLIENT`;
     5. the rest of the row (the flexible space and the gaps): `HTCAPTION`, so dragging moves the window, a double-click maximizes it and a right-click opens the system menu;
     6. everything below the row: `HTCLIENT`.
   - **Rectangles.** The row publishes its items' rectangles in Slint logical pixels relative to the client area, from Rust, whenever the row's model or the window size changes (the row's geometry is computed by the `ToolbarRow` layout in `rbs-ui`, so Rust owns it and nothing is read back from Slint). They live in a fixed-size `thread_local` array (at most 48 rectangles; more items fall back to `HTCLIENT`). The hit test converts the point with `ScreenToClient` and the window's current scale factor, read on each message. The conversion, the maximized inset and the hit test are pure functions with unit tests.
   - **Window buttons.** Minimize, maximize or restore, and close are drawn by Slint at the row's right end, each `size.caption.w` logical pixels wide (scaled with the window's DPI like every token) and the row's full height, with new icons `window-minimize`, `window-maximize`, `window-restore` and `window-close` in the icon set. Hover uses `surface.overlay`; close hover uses `color.caption.close-hover` with `color.caption.close-text`. Minimize and close are ordinary Slint buttons. Maximize gets its mouse through the non-client messages, which the subclass handles (`WM_NCMOUSEMOVE`, `WM_NCMOUSELEAVE`, `WM_NCLBUTTONDOWN`, `WM_NCLBUTTONUP` over `HTMAXBUTTON`) and passes on to the UI as hover and pressed state. The actions are `WM_SYSCOMMAND` `SC_MINIMIZE`, `SC_MAXIMIZE` or `SC_RESTORE`, and `SC_CLOSE`; `SC_CLOSE` reaches winit as a close request, which is ADR-0056's close path.
   - **Accessibility.** The window buttons are not Tab stops, as native caption buttons are not; the keyboard keeps Alt+Space, Win+arrows and Alt+F4, which the subclass passes on (`SC_KEYMENU` included). Screen readers read them through Slint's accessibility tree as "Minimize", "Maximize" or "Restore" (bound to the window's real state) and "Close". With a Windows contrast theme on, RBS uses its high-contrast theme as it does today, and the drawn-pair contrast test (U7) covers the close-hover pair.
   - The window title is the accessible name and the taskbar text; it is not drawn.
3. **Other platforms keep the OS title bar.** macOS keeps its title bar and system menu bar (ADR-0069). Linux is unchanged (parked). Floating windows keep the native title bar on every platform.
4. **Preview.** The preview layer (ADR-0054) is below the toolbar row and never under the hit-test rectangles. Its DirectComposition visual is placed in client coordinates, which now start at the window's top (and at the inset while maximized); this is part of the hand check. Moving and resizing are the OS's own loops, as today, so the preview stays visible during them.
5. **Zero cost (P13).** The rectangles change only when the row's model or the window size changes. The hit test runs per mouse message over the fixed array, without allocation. No polling and no timers.

## Consequences

- Windows gains one row of height and a tidy top edge. The cost is a window subclass of about 250 lines and a hand check on Windows 10 and 11: snap layouts, dragging between monitors with different DPI, maximize on a second monitor, the auto-hide taskbar, top-edge resizing, Alt+Space, Narrator, a Windows contrast theme, and the preview position windowed and maximized.
- New tokens: `size.caption.w` (46), `color.caption.close-hover` (the OS's close red) and `color.caption.close-text` (white), in every built-in theme, not themeable.
- New `windows-sys` features in `rbs-app`: `Win32_UI_Shell`, `Win32_UI_HiDpi`, `Win32_Graphics_Dwm`, `Win32_Graphics_Gdi`. They are features of a crate already used; no new crate is added.
- The design-system spec's Frame and Toolbar sections (including the outdated sentence that the menu bar is drawn on Windows and Linux) and the themes spec's not-themeable list are updated in the same change.
- Tests: the row's layout (items not shown, separators, end padding) is unit-tested in `rbs-ui`; the hit test, the coordinate conversion and the maximized inset are pure functions with tests; the Win32 glue is checked by hand.

## Alternatives considered

- **Keep the native title bar and only fix the row** (recommended on the card): least code, but leaves two rows on Windows. The owner chose the merge.
- **Let DWM draw the caption buttons** (`DwmExtendFrameIntoClientArea` over the whole caption and `DwmDefWindowProc`): snap layouts, accessibility and localisation for free, but the buttons take the OS's colours and not the RBS theme (light, dark, high contrast and user themes), and on Windows 11 they need a system backdrop under the row. The mockup the owner saw draws them in our theme.
- **Draw the whole frame ourselves (`no-frame`)**: loses OS resizing, snapping, shadows and snap layouts, and every one of them would need rewriting.
- **winit `drag_window` from Slint pointer events**: gives dragging but not snap layouts, double-click maximize or the system menu.

## Review

- 2026-11-07, independent AI session (proposal, specs and code): changes requested, 13 findings. Resolved in this text: the live indicator rule is left to U21 and PR #188 (the row works either way); minimize and close are client buttons and only maximize is `HTMAXBUTTON`, with its non-client mouse messages handled and actions sent as `WM_SYSCOMMAND`; top-edge resizing, the Windows 10 top border and the auto-hide taskbar; units, DPI and pure conversion functions; Rust owns the rectangles; popups and in-row prompts answer `HTCLIENT`; the HWND lifecycle, `SetWindowSubclass` and the `windows-sys` features; accessibility (no Tab stops, Alt+Space, screen readers, contrast); the DWM alternative; separate close-text token and window icons; preview position in the hand check; spec updates; the fixed-size array.
