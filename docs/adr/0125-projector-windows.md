# ADR-0125: Projector windows

- Status: Proposed
- Date: 2026-11-16
- Deciders: Salih (owner)
- Builds on: [ADR-0013](0013-zero-cost-when-unused.md), [ADR-0017](0017-flexible-panels-and-workspaces.md), [ADR-0028](0028-cross-process-preview-surfaces.md), [ADR-0034](0034-preview-as-native-layer.md), [ADR-0036](0036-windows-preview-layer.md), [ADR-0093](0093-vertical-second-canvas-from-the-same-sources.md)
- Owner decision: 2026-11-16, scope "step 1 + A + C" of the proposal (`projector/proposal.md` in the project files); the private capture-excluded overlay is not wanted.
- Spec: [gpu-compositor](../specs/gpu-compositor.md) (preview export), [ui-design-system](../specs/ui-design-system.md) (workspace file), [control-api](../specs/control-api.md) (`ui.*`), [shortcuts](../specs/shortcuts.md), [ui principles](../ui/principles.md) U19, [glossary](../ui/glossary.md) "Projector"

## Context

The virtual camera exists only on Windows (`rbs-vcam-mf`). On macOS a camera extension needs a signed, notarised system extension, which the owner has put aside for now. The way to send the program to Meet, Teams or Discord on macOS today is to share a window. The owner asked for a projector window for that (project thread, 2026-11-16).

What exists:

- U19 and the glossary define a Projector: "a full-screen or window view of a canvas, scene or source on any display", with no chrome, never stealing focus. ADR-0017 lets a workspace hold projectors (`kind = "projector"`, `content = "program?canvas=..."`) and places them by display fingerprint (`rbs_ui::display`). `rbs-app` never opens one: the variant is parsed, saved and ignored.
- Pictures are native layers fed by a `PreviewPublisher` (`rbs_nodes::PreviewNode`): a CALayer on macOS (ADR-0034), a DirectComposition visual on Windows (ADR-0036). A new frame changes the layer only; the toolkit does not redraw.
- Every publisher is scaled to `preview_size`: the longer side 1280 pixels. A publisher serves one UI at a time.
- The vertical canvas composes only while an output or its picture runs (`ExtraCanvas::preview`, ADR-0093).

How sharing apps capture a window, and what that means here:

- macOS (ScreenCaptureKit, used by Chrome, Safari, Teams, Discord, Zoom): captures the window's composited content, including Core Animation sublayers whose contents are IOSurfaces, and keeps capturing a window covered by other windows. A minimised window has no content and the share freezes or stops. The shared size is the window's backing size in pixels: on a Retina display (scale 2) a window of 960 x 540 points is 1920 x 1080 pixels.
- Windows (Windows.Graphics.Capture in Chromium, Teams): captures the DWM-composed window, including DirectComposition visuals. Older BitBlt or PrintWindow capture does not see composition content. Windows has the virtual camera, so the projector is the second choice there.
- No sharing app takes audio from a shared window on macOS. Audio for Meet stays the microphone the app opens itself; the picture passes through RBS and arrives a little after the voice (not measured). Only a virtual microphone with a delay fixes that (out of scope).

## Decision

1. **Content.** A projector shows the program of a canvas: the main canvas (`program`), or the vertical canvas while the collection has one (`program?canvas=vertical`), the content strings of ADR-0017. Scene and source projectors (U19) are a later step; a workspace that names one keeps it in the file and does not open it, as today.
2. **Pictures in the studio.** `rbs_record::Studio` gets one general way to add a picture to a running canvas, replacing the single slot of `set_canvas_picture` (which becomes a thin wrapper, so the Vertical panel keeps its behaviour):
   - `add_picture(collection, sources, canvas, PreviewDesc) -> Result<PictureId, RecordError>` links a new `PreviewNode` to that canvas's program compositor; `remove_picture(id)` removes the node and its links; `picture(id) -> Option<PreviewEndpoint>` and `picture_stats(id)`.
   - Any number of pictures per canvas, the main canvas included. A further canvas composes while it has at least one picture or one output (ADR-0093's rule, now counted over all pictures).
   - `add_picture` applies at once: when the graph update fails, nothing is kept (unlike `set_canvas_picture`, whose request is kept for the next update), the call returns the error and no `PictureId` exists. A `PictureId` returned is a running picture.
   - Pictures added this way are **not essential**: a failure to start (surface allocation, Mach `check_in`, pipe creation) fails the call; a later fault of the node is reported in the studio's per-node faults (`Studio::service`, as for any non-essential node), and the host then removes that picture and closes its projector with the reason (decision 11, `ui.projector.changed` with `reason`). The studio keeps running. The main canvas's start picture stays essential.
   - Spec: `docs/specs/recording.md`, "Further canvases" and a new "Pictures" subsection.
3. **Projectors are host state.** `StudioHost` keeps the list of open projectors `{id, content, size}` (like it keeps the vertical canvas), not the `Studio`. On every studio start or restart (collection switch, canvas size change, engine restart) it adds their pictures again with the current canvas size and sends the UI the new endpoints; the window shows black until it has subscribed again. A projector whose canvas no longer exists (the vertical canvas is removed, or the new collection has none) becomes **suspended**: it stays in the session's workspace (so a save keeps it) with no window and no picture, does not count toward the limits of decision 4, and opens again when its canvas comes back. Removing the vertical canvas is not refused because of a projector. The same holds for a projector whose display is missing when a workspace is applied.
4. **Size of the picture and limits.** A projector's picture is the canvas size (not `preview_size`), with the ring of `preview_surfaces(fps)`. Projector pictures together may hold at most `MAX_PROJECTOR_SURFACE_BYTES` = 512 MiB of surfaces, counted as width × height × 4 × `MAX_PREVIEW_SURFACES` (8) per projector picture whatever ring it really uses, so the limit is the same on every platform (the Program, studio Preview and Vertical pictures do not count): a 1080p projector counts about 63 MiB, a 2160p one about 253 MiB, so two 2160p projectors fit and a third is refused. Opening one beyond that, or a fifth projector, is refused with a message ("Too many projectors are open. Close one first."). With no projector open nothing of them exists: no node, surface, link, window, timer or thread (P13).
5. **The window has no chrome and draws nothing over the picture.** A sharing app captures everything in the window, so the window holds only the picture: the native layer of the Program picture (`PreviewLayer` on macOS, the DirectComposition layer on Windows), resize-aspect on black. No Slint content is drawn in it, ever: no caption, no toolbar, no cover (ADR-0054 does not apply to it). This follows U19 ("no chrome"):
   - macOS: a titled, resizable `NSWindow` with a transparent, hidden title bar and full-size content (`titlebarAppearsTransparent`, `titleVisibility = hidden`, the three buttons hidden); dragging the picture moves the window, edges resize it. Its title (used by sharing apps' window lists and Mission Control, never drawn) is "RBS Projector – Program" or "RBS Projector – Vertical".
   - Windows: a borderless window (`WS_POPUP | WS_THICKFRAME`, no caption) with the same title; dragging moves it, edges resize it.
   - It does not take focus when it opens from a menu or the API (macOS `orderFront` without making it key; Windows `SW_SHOWNOACTIVATE`), and runs no app shortcut (shortcuts spec); Esc closes it while it has focus.
   - The window settings above that Slint does not offer are made on the native window reached through its raw handle, as `preview.rs` does for the `NSView`, in `rbs-app`'s platform modules (macOS through `objc2` message sends, already a dependency; `objc2-app-kit`, already in the build through winit, may become a direct dependency at the same version; Windows through `windows-sys`, already a dependency), with `// SAFETY:` comments and the `risky-review` checklist.
   - The pointer hides after 2 s without movement over it (sharing apps may draw the pointer).
6. **Its menu is a native OS menu.** Right click (or the Menu key, Shift+F10) on the projector opens the app's existing `CommandMenu` (ADR-0050 decision 4, context `projector`), a Slint `ContextMenuArea` that Slint 1.18 shows as a native menu (muda): the OS draws it in its own window, outside the shared window, and the element itself draws nothing, so decision 5 holds. On Windows the projector window gets `win::redraw::keep_visible`, so building the menu does not drop the window out of the compositor (a missing frame in the share). Items: a disabled first line "Shared at 1920 × 1080" (decision 8), Window, Exact pixels, Full screen › (one item per display, by its name), and Close projector. The same items are in the main window's View › Projectors submenu, one group per open projector, so a projector can be resized or closed without touching it. New registry entries get icons from the icon set; the visible change (menus, the window) is shown to the owner with dark and light screenshots before merge (rule 16).
7. **It cannot be minimised by the app's own controls.** The window has no minimise button and refuses minimise requests (macOS: no `miniaturizable` style, so Cmd+M does nothing; Windows: no `WS_MINIMIZEBOX`, `SC_MINIMIZE` ignored), because a minimised window stops sharing. What the OS does on its own stays: Cmd+H hides it with the app, and Win+D or Win+M can still put it away. The picture keeps running while the projector is open, whether or not its window is visible, so the share resumes at once when it comes back; closing the projector is what stops the cost.
8. **Sizes.** Three sizes; the shared size is the window's content size in physical pixels, letterboxed to the canvas aspect.
   - **Window**: any size; the default.
   - **Exact pixels**: the content size in physical pixels is `canvas / k`, with `k` the smallest whole number (1, 2, 3, ...) for which that fits the display's visible area; the window is resized to it in physical pixels (`slint::PhysicalSize`), so the OS does any point rounding. While the size is Exact pixels it is computed again when the window's scale factor changes (moved to another display, scale changed, display unplugged and the OS moved the window). Resizing the window by hand turns the size back to Window. Menu and list read "Shared at 960 × 540 (half the canvas)" when `k > 1`.
   - **Full screen** on a display: a borderless window covering that display (macOS: window level above the menu bar, not a fullscreen Space, so sharing apps still list it; Windows: covering the monitor rect, topmost). It is stored as `placement.fullscreen = true` (ADR-0017), not in the content string. When its display goes away the projector stays open at Window size on the main display, and its saved placement (display and full screen) is kept unchanged until the user changes the projector's size or moves it, so the next save does not lose it.
9. **Opening.** The Program picture's context menu and View › Projectors list "Open projector: Program" and, with a vertical canvas, "Open projector: Vertical". Each is a registry command (`ui.projector.open?content=...`) with a menu entry, an optional shortcut and an API path. Opening a content that is already open brings that projector to the front and returns its id; it does not count again toward the limit.
10. **Workspace.** Open projectors are saved in the workspace with their placement in the existing format 3 (ADR-0017, no new key): Exact pixels is the content-string parameter `size=exact` (`program?size=exact`, `program?canvas=vertical&size=exact`); Full screen is `placement.fullscreen`; neither means Window. When both say something (a file edited by hand with `size=exact` and `fullscreen = true`), full screen wins. Parameters the shell does not know are kept as written. Older builds keep projector windows in the file and open nothing, as today. Applying a workspace closes projectors it does not hold and opens those it does; a projector whose display is missing is suspended (decision 3; ADR-0017 does not open it).
11. **Control API.** Client-scoped and routed like every `ui.*` method, scope `control`:
    - `ui.projector.open {content, size?, display?}` → `{id}`. `size` is `window`, `exact` or `fullscreen`; `display` is a display's EDID fingerprint as in the workspace file (`ui.projector.displays` lists `{fingerprint, name, size: [w, h] physical pixels, scale}`), needed for `fullscreen`, else the display of the main window.
    - `ui.projector.set_size {id, size, display?}`, `ui.projector.close {id}`, `ui.projector.list` → `[{id, content, state, size, display, shared: [w, h]}]` (`state` is `open` or `suspended`; `shared` in physical pixels, absent while suspended).
    - Topic `ui.projector.changed` with the same list, sent when a projector opens, closes, is suspended or comes back, or changes size, with `reason` on a close or suspension that the user did not ask for (`canvas_removed`, `display_missing`, `picture_failed`).
    - `invalid_params` for an unknown content, size or a `fullscreen` without `display`; `not_found` for an unknown id or display; `conflict` beyond the limits of decision 4 (a third 2160p projector is refused on every platform; a fifth 1080p one is refused by count). Protocol version: the next free one at merge.
12. **Hot path.** A projector adds one `PreviewNode::process` and one `PreviewPublisher::publish` per frame while it is open (hot paths: render loop). No hot-path code changes; the render tick is measured (render tick time and GPU memory at 0, 1 and 4 projectors, 1080p and 2160p canvases) with the `hot-path-review` checklist and the maintainer's sign-off before merge (P15).
13. **Tests.**
    - Studio (`rbs-record`): `add_picture` adds exactly one node and link at the given size; `remove_picture` leaves no node, link or publisher; with no picture the plan equals today's; a further canvas stops composing after its last picture and output go; a failing picture leaves the studio running.
    - Host (`rbs-app`, demo adapters): projectors are added again after a restart and a collection switch with the new endpoint; a projector of the vertical canvas is suspended (reason `canvas_removed`) when the canvas is removed and reopens when it comes back; the limits of decision 4 (a third 2160p projector is refused on every platform; a fifth 1080p one is refused by count).
    - Exact pixels size function over scales 1, 1.25, 1.5, 2 and canvases 1920 × 1080, 1080 × 1920, 3840 × 2160 against small visible areas; shared-size text.
    - Suspended projectors: kept through a save, reopened when the canvas or display comes back.
    - Workspace: content-string parsing, `size=exact` round trip, unknown parameters kept, fullscreen wins, a workspace switch opens and closes the right projectors, a missing display opens nothing.
    - Shortcuts: a key bound in the app runs nothing in a projector; Esc closes it.
    - No test opens a real OS window in `cargo test`; window behaviour (no minimise, native menu, chrome) is a manual check on the Mac and on Windows.
14. **Specs changed with the implementation.** `recording.md` (Pictures), `control-api.md` (`ui.projector.*`), `ui-design-system.md` (content-string grammar and a "Projector window" section), `shortcuts.md` (projectors: Esc only), the glossary entry "Projector" (Program of a canvas, sizes), and `status.md`.

## Consequences

- macOS users can send the program to Meet, Teams and Discord at full canvas resolution without a virtual camera; with Exact pixels a 1080p share takes a quarter of a Retina screen.
- Each open projector costs one scaled render pass per frame at canvas size and its surfaces, only while open; the memory cap keeps four 2160p projectors from taking a gigabyte.
- The operator never sees a status on the projector itself; the shared size lives in the native menu, View › Projectors and the API.
- The workspace format does not change.
- The in-app help (the (i) next to View › Projectors) states the limits: a minimised or hidden window stops the share, the shared size is the window's pixels, the picture is a little behind the voice because the sharing app takes the microphone directly (not measured; one to three frames is the expected order from capture plus composition).
- Performance claims (UI CPU with a projector open, render tick) are measured on the Mac before they are made.

## Alternatives considered

- **Reuse the Program picture's publisher.** One UI per publisher, and it is capped at 1280 pixels: a projector would share 720p. Rejected.
- **Draw the projector with Slint from an image.** Redraws the toolkit every frame (the cost ADR-0034 removed). Rejected.
- **A fullscreen Space on macOS.** Sharing apps list fullscreen windows inconsistently, and the Space takes over a display. A borderless window covering the display is listed like any window.
- **Render small, report large.** A window cannot be shared at a size larger than its pixels; any trick (offscreen windows, windows outside the screen) is refused by macOS or breaks with capture changes. Rejected; Exact pixels is the honest way to keep a share large and the window small.
- **A caption or status over the projector's picture.** Sharing apps capture the whole window, so viewers would see it; a capture-excluded overlay window was proposed and the owner declined it (2026-11-16).
- **Virtual camera on macOS (camera extension).** The real fix, independent of windows; needs a signed, notarised system extension. Deferred by the owner, 2026-11-16.

## Later (not part of this decision)

- Scene and source projectors (U19), and a projector of a scene that is not on air (needs its own compositor while open).

## Review

- Round 1, 2026-11-16, independent AI session (got the ADR, specs and code only): changes requested. Concerns and resolution:
  1. Title bar and caption broke U19 "no chrome". Resolved: decision 5, no chrome and nothing drawn in the window.
  2. A caption over the picture is captured by sharing apps; ADR-0054 covers are for the main window. Resolved: decisions 5 and 6, shared size only in the native menu, View › Projectors and the API.
  3. Projectors lost on studio restart, canvas resize, vertical canvas removal. Resolved: decision 3, host state, re-added on restart, closed with their canvas.
  4. Single-slot `set_canvas_picture`, failure containment, recording spec. Resolved: decision 2.
  5. Hot path and 4K memory. Resolved: decisions 4 (512 MiB cap) and 12 (measurements, checklist, sign-off).
  6. `size=fullscreen` duplicated `placement.fullscreen`; unknown parameters dropped. Resolved: decisions 8 and 10.
  7. Exact pixels rounding, scale changes, unplugged displays, Windows DPI. Resolved: decision 8 (physical pixels, recomputed on scale change).
  8. Behaviour while hidden, Win+D and Win+M. Resolved: decision 7.
  9. API display format, change event, reopen id. Resolved: decisions 9 and 11.
  10. Missing tests and spec list. Resolved: decisions 13 and 14.
- Round 2, 2026-11-16, same independent session: changes requested; round 1 concerns 1-10 resolved. New concerns and resolution:
  - N1 native menu: use the existing `CommandMenu` (Slint native menu) and `win::redraw::keep_visible`. Resolved: decision 6.
  - N2 projectors closed for a missing canvas or display were lost on the next save. Resolved: suspended state (decisions 3, 8, 10, 11).
  - N3 `add_picture` apply-failure and fault semantics. Resolved: decision 2.
  - N4 no-focus and hidden title bar mechanism. Resolved: decision 5.
  - N5 cap constant, scope and test. Resolved: decisions 4 and 13.
  - N6 icons and rule 16 approval. Resolved: decision 6.
- Round 3, 2026-11-16, same independent session: N1-N6 resolved. B1 (the cap example depended on the platform's ring size, 5 on Windows and 6 on macOS at 60 fps): resolved, the cap counts 8 surfaces per projector (decision 4). M1 (a test said "closes" where decision 3 says "suspended"): resolved (decision 13). Reviewer: approve once both are fixed; both fixed.
