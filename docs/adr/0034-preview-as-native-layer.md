# ADR-0034: Show the engine preview as a native compositor layer, not through the toolkit's redraw

- Status: Proposed
- Date: 2026-11-03
- Deciders: Salih (owner)
- Builds on: ADR-0022 (Slint, decision 2), ADR-0028 (preview surfaces shared as native handles), ADR-0013 (zero cost when unused)

## Context

The desktop UI owns a budget of at most 10 % of one CPU core while the preview and the mixer meters animate ([performance budgets](../specs/performance-budgets.md#desktop-ui)). ADR-0022 decision 2 hands the imported preview surfaces to Slint as textures, so every preview frame makes Slint redraw the whole window: Slint's Skia renderer on `wgpu` gets no buffer age from the swapchain and cannot redraw only the changed region. The UI was above budget on every measurement (ADR-0022 Consequences; 15 % for `remote_preview` before this change, 20-27 % for the Skia prototype at 1080p).

Profile and measurements on the reference Mac (MacBook Pro M4 Pro, macOS 26, release build of `remote_preview` with `preview_engine`, 720p preview at 60 fps, 21 s windows, process CPU from `getrusage`):

| UI path | Preview only | Preview + 16 meters at 30 Hz |
| --- | --- | --- |
| Slint draws the preview (`RBS_PREVIEW_MODE=slint`) | 10.7-11.7 % | 12.8-15.0 % |
| Layer (`RBS_PREVIEW_MODE=layer`, this ADR) | 1.4-1.5 % | 6.1-8.0 % |

`sample` of the Slint path: about 70 % of the main thread's busy time is the per-frame redraw (Skia item drawing, `wgpu` surface acquire, encode, submit, present); no single hot spot. With the layer, a preview frame costs one Core Animation transaction (about 12 % of the main thread's busy time); what remains is Slint redrawing the meters and the status line. 60 fps, 0 to 7 late frames per run in both paths, engine never blocked, no frame dropped.

## Decision

1. **On macOS the UI shows the engine's preview surfaces as the contents of a Core Animation layer** (`rbs_gpu::share::PreviewLayer`) placed over the preview area of the window, above the toolkit's own layer. A new frame sets the layer's `contents` to that frame's IOSurface inside an explicit transaction without implicit animations. The toolkit does not redraw for preview frames; the window server composites the surface. No copy, no UI-side GPU work.
2. **Placement follows the toolkit's layout.** The app passes the preview area's rectangle (window coordinates, top-left origin) to `PreviewLayer::set_rect` before every toolkit redraw (Slint's `BeforeRendering` notifier). Every layout change redraws, so the layer follows; an unchanged rectangle costs nothing.
3. **Release rule.** Core Animation reports no point at which the window server stops reading old contents, and IOSurface use counts and `IOSurfaceIsInUse` do not track layer contents (measured: counts stay 0; the in-use flag stays set). A commit takes effect at the next display refresh, so a replaced frame goes back to the engine a fixed retire time after it was replaced: 1.5 refresh periods of the slowest display, 25 ms by default. Time comes from the caller (an injected clock; tests use `TestClock`). The next frame's `show` retires due frames; a timer runs only while every surface but the shown one is retiring (`engine_may_starve`), until the oldest is due. At 60 fps this holds the shown frame plus at most one or two replaced ones, so the engine's ring must have at least 4 surfaces.
4. **Overlap.** The layer is above the toolkit's content, so toolkit content drawn over the preview area (in-window menus, popups, toasts, dialogs) would be hidden. While such content intersects the preview area the shell hides the layer (`set_hidden`) and the area shows its toolkit background. A mask layer that cuts the overlay out of the preview is a later refinement.
5. **The toolkit path stays.** Showing the surfaces as Slint textures (ADR-0022 decision 2, ADR-0028 decision 6) remains for platforms without a layer backend, for screenshots of the window by the toolkit, and as a fallback when attaching the layer fails. It is no longer the default on macOS.
6. **Other platforms.** Each platform gets its own layer backend with the same API when its preview export exists: a DirectComposition visual (Windows) and a Wayland subsurface (Linux). Until then they keep the toolkit path and the preview stays macOS-only (ADR-0028 decision 8).
7. **Meters and text stay in the toolkit at the declared rates** (meters 30 Hz, live duration and stats 1 Hz, [design system](../specs/ui-design-system.md#panel-model)). Their redraw is now the UI's main cost.
8. **Zero cost when unused (P13).** A `PreviewLayer` exists only while a preview panel shows a subscribed preview; it adds no thread, and its timer runs only in the case of decision 3.

## Consequences

- The UI meets its CPU budget for the preview-and-meters scenario on the reference Mac (6.1-8.0 % against 10 %), and the preview alone costs 1.4-1.5 %.
- `rbs-gpu` gains macOS dependencies on `objc2` and `objc2-quartz-core` (same versions `wgpu-hal` and `raw-window-metal` already build) and the `CFCGTypes` feature of `objc2-core-foundation`. `share::PreviewLayer` is part of the marked low-level module (P11).
- The UI's frame pacing for the preview is now the window server's; the UI's own late-frame counter measures delivery, not presentation. Presented-frame timing for the preview needs Core Animation instrumentation (Instruments) if it is ever in doubt.
- The retire time is a timing rule, not a completion signal. A display slower than 40 Hz needs a longer time (`set_retire_after`). Visual checks found no tearing at 60 Hz.
- Window server CPU could not be separated from its baseline on a busy desktop (about 48 % with and without the preview); a quiet-desktop measurement is follow-up work.
- The shell's Program/Preview panel must use `PreviewLayer` when it is wired to the engine, and its overlays must hide the layer (decision 4).
- Slint calls `getenv` on every animation update (seen in the profile); small, upstream.

## Alternatives considered

- **Partial redraw in Slint** (`SLINT_SKIA_PARTIAL_RENDERING`): the `wgpu` surface reports no buffer age, so every frame is still a full redraw. Not usable on the GPU path.
- **Own `CAMetalLayer` drawn with `wgpu`**: a blit per frame plus drawable acquire on the main thread (which can block); more CPU than setting `contents`, and no benefit while the engine already produces display-ready surfaces.
- **Layer below a transparent toolkit window with a hole**: correct stacking for overlays, but the whole window becomes non-opaque (blending and shadow work in the window server, Slint transparency handling on macOS). Kept as a fallback if hiding the layer for overlays proves awkward.
- **Lower preview rate in the UI**: hides frames from the operator; the preview is a monitoring view and must run at the program rate.

## Review

Independent review per ADR-0014: pending.
