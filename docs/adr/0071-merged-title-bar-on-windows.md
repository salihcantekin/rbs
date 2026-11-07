# ADR-0071: The toolbar row is the title bar on Windows, and the row leaves no gaps

- Status: Proposed
- Date: 2026-11-07
- Deciders: Salih (owner)
- Builds on: ADR-0049 (frame and areas), ADR-0050 (toolbar), ADR-0054 (preview stays visible), ADR-0057 (theme tokens), ADR-0069 (RBS menu)

## Context

The main window's frame has a toolbar row (ADR-0050) under the OS title bar. Two problems came up on 2026-11-07:

- When an item at the row's end is hidden (the REC/LIVE badge while idle), the row keeps its spacing, so the Settings button stops short of the corner. Each item handles its own spacing; nothing owns the row.
- On Windows, after ADR-0069 removed the menu bar row, the OS title bar is a second row that holds only the window title and the minimize, maximize and close buttons. The owner wants one tidy top edge, as VS Code has.

On a decision card on 2026-11-07, the owner chose to merge the toolbar row with the title bar on Windows, over keeping the native title bar.

## Decision

1. **One toolbar row component.** The frame's toolbar row is laid out by one component (`ToolbarRow`) with three parts: a leading group (the RBS button, then the toolbar items before `~`), the flexible space, and an end group (the items after `~`, then the REC/LIVE badge while something runs, then the window buttons on Windows).
   - A hidden item takes no width and adds no spacing.
   - A separator is dropped when it would lead, end or double.
   - The last visible item of the end group sits `space.2` from the row's right edge, as the first sits from its left edge. On Windows the window buttons end the row with no padding.
   - Spacing and sizes come from tokens (ADR-0057).
2. **Windows: the toolbar row is the title bar.** The main window keeps its native frame (`WS_OVERLAPPEDWINDOW`), so resizing, snapping, shadows, the Alt+Space menu and maximize animations stay the OS's. Only the caption is removed:
   - `WM_NCCALCSIZE` makes the client area cover the caption. While maximized, the client area is inset by the frame thickness, so nothing is drawn off screen.
   - `WM_NCHITTEST` answers from rectangles the UI publishes each layout:
     - `HTCAPTION` for the flexible space and the gaps between items, so dragging moves the window, a double-click maximizes it, and a right-click opens the system menu;
     - `HTMINBUTTON`, `HTMAXBUTTON` and `HTCLOSE` for the window buttons, so Windows 11 shows its snap layouts on the maximize button;
     - the OS's own answer for the resize edges;
     - `HTCLIENT` everywhere else.
   - The window buttons (minimize, maximize or restore, close) are drawn by Slint at the row's right end. Each is `size.caption.w` wide and the row's full height. Hover uses `surface.overlay`; close hover uses `color.caption.close-hover` with `text.on-live`. Their accessible names are "Minimize", "Maximize" or "Restore" and "Close". Clicks go through the same close path as the OS close button (ADR-0056).
   - The window title is the accessible name and the taskbar text; it is not drawn.
   - The subclass lives in `rbs-app` `src/win/` (an existing low-level module). Every `unsafe` block has its SAFETY comment.
3. **Other platforms keep the OS title bar.** macOS keeps its title bar and system menu bar (ADR-0069). Linux is unchanged (parked). Floating windows keep the native title bar on every platform.
4. **Preview.** The preview layer (ADR-0054) is below the toolbar row and never under the hit-test rectangles. Moving and resizing are the OS's own loops, as they are today, so the preview stays visible during them.
5. **Zero cost.** The hit-test rectangles change only when the row's layout changes. No polling and no timers.

## Consequences

- Windows gains one row of height and a tidy top edge. The cost is a window procedure subclass: about 150 lines of Win32 code, and a hand check on Windows 10 and 11 (snap layouts, dragging between monitors with different DPI, maximize on a second monitor, high contrast mode).
- New tokens: `size.caption.w` (46) and `color.caption.close-hover` (the OS's close red, in every built-in theme), both not themeable.
- The design-system spec's Frame and Toolbar sections and the themes spec's not-themeable list are updated.
- Tests: the row's layout (hidden items, separators, end padding) is unit-tested in `rbs-ui`; the hit-test function (point and rectangles to an `HT*` value) is a pure function with tests; the Win32 glue is checked by hand.

## Alternatives considered

- **Keep the native title bar and only fix the row** (recommended on the card): least code, but leaves two rows on Windows. The owner chose the merge.
- **Draw the whole frame ourselves (`no-frame`)**: loses OS resizing, snapping, shadows and snap layouts, and every one of them would need rewriting.
- **winit `drag_window` from Slint pointer events**: gives dragging but not snap layouts, double-click maximize or the system menu.
