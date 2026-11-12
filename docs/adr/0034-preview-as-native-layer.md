# ADR-0034: Show the engine preview as a native compositor layer, not through the toolkit's redraw

- Status: Accepted
- Date: 2026-11-03
- Deciders: Salih (owner)
- Builds on: ADR-0028 (preview surfaces shared as native handles), ADR-0013 (zero cost when unused)
- Supersedes in part: ADR-0022 decision 2 (Slint textures as the way the UI shows the preview) and ADR-0028 decisions 5 and 6 (release after the UI's own GPU work; Slint textures by default), on macOS

## Context

The desktop UI owns a budget of at most 10 % of one CPU core while the preview and the mixer meters animate ([performance budgets](../specs/performance-budgets.md#desktop-ui)). ADR-0022 decision 2 hands the imported preview surfaces to Slint as textures, so every preview frame makes Slint redraw the whole window: Slint's Skia renderer on `wgpu` gets no buffer age from the swapchain and cannot redraw only the changed region. The UI was above budget on every measurement (ADR-0022 Consequences; 15 % for `remote_preview` before this change, 20-27 % for the Skia prototype at 1080p).

Profile and measurements on the reference Mac (MacBook Pro M4 Pro, built-in ProMotion display up to 120 Hz, macOS 26, release build of `remote_preview` with `preview_engine`, 720p preview at 60 fps unless noted, 21 s windows, process CPU of the UI from `getrusage`). Final run (ring of 6, 34 ms retire time), all in one sitting:

| UI path | Preview only | Preview + 16 meters at 30 Hz |
| --- | --- | --- |
| Slint draws the preview (`RBS_PREVIEW_MODE=slint`) | 12.2 % | 15.3 % |
| Layer (`RBS_PREVIEW_MODE=layer`, this ADR) | 2.0 % (twice) | 8.3-8.8 %; 9.2 % at 1080p |

Every layer run showed 60.0 fps in the UI's 21 s measurement window; by the engine's own counters over its whole run (about 1,190 converted frames, `dropped` 0), it skipped 1 to 4 frames per run for want of a free surface (`ExportBusy`); a ring of 8 made it 1 and changed nothing else. The 1080p figure is a single run (n = 1); the 720p figures are two runs each. Earlier sittings gave the same picture (Slint 10.7-14.3 % and 12.8-19.2 %; layer 1.4-1.8 % and 6.1-9.3 %); the machine's background load moves both paths by a few points, so compare paths within one sitting.

`sample` of the Slint path: about 70 % of the main thread's busy time is the per-frame redraw (Skia item drawing, `wgpu` surface acquire, encode, submit, present); no single hot spot. With the layer, a preview frame costs one Core Animation transaction (about 12 % of the main thread's busy time); what remains is Slint redrawing the meters and the status line at the display rate.

## Decision

1. **On macOS the UI shows the engine's preview surfaces as the contents of a Core Animation layer** (`rbs_gpu::share::PreviewLayer`) placed over the preview area of the window, above the toolkit's own layer. A new frame sets the layer's `contents` to that frame's IOSurface inside an explicit transaction without implicit animations. The toolkit does not redraw for preview frames; the window server composites the surface. No copy, no UI-side GPU work.
2. **Placement follows the toolkit's layout.** The app passes the preview area's rectangle (window coordinates, top-left origin) to `PreviewLayer::set_rect` before every toolkit redraw (Slint's `BeforeRendering` notifier). Every layout change redraws, so the layer follows; an unchanged rectangle costs nothing.
3. **Release rule (best effort).** Core Animation reports no point at which the window server stops reading old contents, and IOSurface use counts and `IOSurfaceIsInUse` do not track layer contents (measured: counts stay 0; the in-use flag stays set even for a layer outside any window). `show` sends each change to the window server at once (`CATransaction::flush`); it is applied at the next display refresh, or the one after if the window server misses it, and until then the old surface is read or scanned out. So a replaced frame goes back to the engine a retire time after it was replaced: two refresh periods of the slowest display the window is on, 34 ms by default (60 Hz and faster). Time comes from the caller (an injected clock; tests use `TestClock`). If the time is too short, the worst case is one torn or wrong monitoring frame on screen, never a memory hazard: the UI keeps every surface alive. The UI retires due frames on every `show` and with a one-shot timer at the oldest frame's due time (`next_retire`) while any frame waits: at most one wake-up per frame. Waiting for the next frame instead held surfaces up to a frame longer and, measured, made a ring of 6 run out on one frame in five (48-51 fps).
   The ring must hold the shown frame, the frames replaced within the retire time, one being written and one in flight: `PreviewLayer::surfaces_needed(frame_interval)`, 6 surfaces at 60 fps with 34 ms. A smaller ring makes the engine drop frames (`ExportBusy`), never deadlock; measured: a ring of 4 with a 25 ms retire time halved the preview to 30 fps.
4. **Overlap.** The layer is above the toolkit's content, so toolkit content drawn over the preview area (in-window menus, popups, toasts, dialogs) would be hidden. While such content intersects the preview area the shell hides the layer (`set_hidden`) and the area shows its toolkit background. A mask layer that cuts the overlay out of the preview is a later refinement.
5. **The toolkit path stays.** Showing the surfaces as Slint textures (ADR-0022 decision 2, ADR-0028 decision 6) remains for platforms without a layer backend, for screenshots of the window by the toolkit, and as a fallback when attaching the layer fails. It is no longer the default on macOS.
6. **Other platforms.** Each platform gets its own layer backend with the same API when its preview export exists: a DirectComposition visual (Windows) and a Wayland subsurface (Linux). Until then they keep the toolkit path and the preview stays macOS-only (ADR-0028 decision 8).
7. **Meters and text stay in the toolkit at the declared rates** (meters 30 Hz, live duration and stats 1 Hz, [design system](../specs/ui-design-system.md#panel-model)). Their redraw is now the UI's main cost.
8. **Zero cost when unused (P13).** A `PreviewLayer` exists only while a preview panel shows a subscribed preview; it adds no thread, and its timer runs only in the case of decision 3.
9. **UI code stays out of the engine.** `PreviewLayer` is behind the `rbs-gpu` feature `preview-layer`, off by default and turned on only by the UI crate (today by `rbs-app`'s dev-dependencies for the demo; the shell turns it on in its normal dependencies when it wires the preview panel), so engine binaries carry no window code (P8). When the preview panel closes while the window stays open, the UI calls `detach` and retires the remaining frames by the same rule instead of dropping them at once.

## Consequences

- The UI meets its CPU budget for the preview-and-meters scenario on the reference Mac (8.3-8.8 % at 720p, 9.2 % in one 1080p run, against 10 %), and the preview alone costs 2 %. The margin is thin: the remaining cost is Slint redrawing the whole window for the meters at the display rate, so the next gains are in the toolkit (meters on a GPU texture per ADR-0022 decision 3, fewer redraws), not in the preview.
- `rbs-gpu` gains optional macOS dependencies on `objc2` and `objc2-quartz-core` behind `preview-layer` (same versions `wgpu-hal` and `raw-window-metal` already build, so the lockfile gains no crate) and the `CFCGTypes` feature of `objc2-core-foundation`. `share::PreviewLayer` is part of the marked low-level module (P11); it is UI-process code, not on the engine's hot path. Its test-only `layer()` and `attach_to_layer` accessors are hidden from the documented API.
- The engine's preview ring grows from 4 to 6 surfaces at 60 fps (about 8 MiB per extra surface at 1080p).
- The UI's frame pacing for the preview is now the window server's. The demo's late-frame counter measures delivery to the UI (frames more than 25 ms apart), not presentation, so the budget row "0 late frames over 60 s" cannot be checked on the layer path with it; presented-frame timing needs Core Animation instrumentation (Instruments) and stays open in the budget spec.
- The retire time is a timing rule, not a completion signal (decision 3). A display slower than 60 Hz needs a longer time (`set_retire_after`) and a larger ring; nothing reads the display's refresh rate yet (follow-up: derive it from `NSScreen.minimumRefreshInterval`). Visual checks (screenshots during 60 fps runs and resizes) on the reference Mac's built-in ProMotion display found no tearing or misplacement; a 60 Hz external display is not checked yet.
- Window server CPU could not be separated from its baseline on a busy desktop (about 48 % with and without the preview); a quiet-desktop measurement is follow-up work.
- The shell's Program/Preview panel must use `PreviewLayer` when it is wired to the engine, and its overlays must hide the layer (decision 4).
- Slint calls `getenv` on every animation update (seen in the profile); small, upstream.

## Alternatives considered

- **Partial redraw in Slint** (`SLINT_SKIA_PARTIAL_RENDERING`): the `wgpu` surface reports no buffer age, so every frame is still a full redraw. Not usable on the GPU path.
- **Own `CAMetalLayer` drawn with `wgpu`**: a blit per frame plus drawable acquire on the main thread (which can block); more CPU than setting `contents`. Its `MTLDrawable` presented handler would report when the copy is on screen, but the engine's surface is free as soon as the blit completes anyway, so the signal buys nothing over the copy it costs.
- **`AVSampleBufferDisplayLayer` with IOSurface-backed pixel buffers**: releases buffers when it is done with them, but it is a video pipeline (timebases, enqueue, flush on gaps) with its own buffering latency and more work per frame; worth revisiting if the timing rule proves wrong on some display.
- **Counting display refreshes** (`CVDisplayLink` or `NSView.displayLink`) instead of wall time: exact for the refresh count, but adds a wake-up on every refresh while the preview shows; the measured CPU budget left little room, and the timing rule with a flushed commit gives the same bound at 60 Hz and faster.
- **Layer below a transparent toolkit window with a hole**: correct stacking for overlays, but the whole window becomes non-opaque (blending and shadow work in the window server, Slint transparency handling on macOS). Kept as a fallback if hiding the layer for overlays proves awkward.
- **Lower preview rate in the UI**: hides frames from the operator; the preview is a monitoring view and must run at the program rate.

## Review

- 2026-11-03, independent AI session (fresh context: proposal, specs and code only), round 1: changes requested. Main concerns and resolution:
  1. The release rule read as a guarantee and 25 ms did not cover a missed refresh. Response: decision 3 states it as best effort with the worst case (one torn monitoring frame, no memory hazard); `show` flushes the transaction; 34 ms (two refreshes at 60 Hz); ring size from `surfaces_needed`; alternatives that report release (presented handler, `AVSampleBufferDisplayLayer`, display-link counting) recorded with reasons.
  2. Accepted ADR-0022 and ADR-0028 were changed without saying so. Response: "Supersedes in part" header; pointers appended to their Review sections.
  3. The late-frame budget row could no longer be checked. Response: marked open in the budget spec; Consequences say why.
  4. `DEFAULT_RETIRE_AFTER` not re-exported; `set_rect` cache ignored the superlayer's height and attach; Drop released frames possibly on screen; opaque layer with letterbox bars; UI code built into engine binaries. Response: all fixed (re-export, cache on rectangle + height + flip with an autoresizing mask, `detach`, not opaque, `preview-layer` feature off by default).
  5. Tests missed the 60 fps ring and placement. Response: `tests/share_layer_metal.rs` covers them with `TestClock`.
- Round 1 Mac runs then showed the retire time costs frames: 25 ms with a ring of 4 gave 30 fps, 34 ms with a ring of 6 gave 48-51 fps while retirement waited for the next frame. Response: a one-shot timer at `next_retire` (decision 3); final run 60 fps.
- 2026-11-03, independent AI session (fresh context), round 2: changes requested (docs gave two timer rules; measurement wording; jitter and no-op tests; example input errors), then approved after commit be96f94. No hot-path code changed (P15 not triggered: `layer.rs` is UI-side).
- Owner: under the early-phase ADR policy (2026-11-03) threads may mark an ADR Accepted after its independent review. Status: Accepted, 2026-11-03.
- 2026-11-03: [ADR-0035](0035-meters-as-native-layers.md) supersedes decision 7 in part: on macOS the meters' moving parts are native layers too; the toolkit keeps their static parts and accessible elements.
- 2026-11-06: decision 4 is now implemented as written (it hid the layer for every open popup before): the layer hides only while an in-window popup, toast or the error bar overlaps the picture or a selection mark; popups are read from Slint's own list (`rbs_app::popups`). ADR-0054 (proposed) addresses covers that do overlap.
- 2026-11-11: the studio host had hard-coded a ring of 3. Decision 3's count is `ring_needed` with the 34 ms hold: 5 surfaces at 30 fps (`ceil(34 ms / 33.333 ms) = 2` retiring, plus shown, written and in flight) and 6 at 60 fps. A ring of 3 is only the hold-nothing case, so both rates drop frames (`ExportBusy`); raising the canvas to 60 fps makes that worse, not smoother. The host now sets `PreviewDesc.surfaces` from that arithmetic, capped at `MAX_PREVIEW_SURFACES` (8), including when the canvas rate changes. Windows uses ADR-0036's hold (one frame plus the 4 ms poll): 5 at 30 fps and 5 at 60 fps. No live busy/dropped counter was taken in this change; the proof is the arithmetic, asserted in `share::geometry` and the host.
