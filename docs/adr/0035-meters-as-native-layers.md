# ADR-0035: Draw the moving parts of audio meters as native compositor layers

- Status: Proposed
- Date: 2026-11-03
- Deciders: Salih (owner)
- Builds on: ADR-0034 (preview as a native layer), ADR-0022 (Slint), ADR-0013 (zero cost when unused), ADR-0015 (UI design rules)
- Supersedes in part: ADR-0022 decision 3, first tier, for audio meters on macOS (meters drawn with the toolkit's vector primitives), and ADR-0034 decision 7 (meters stay in the toolkit)

## Context

After ADR-0034 the UI met its CPU budget (at most 10 % of one core with the preview and mixer meters animating) with a thin margin: 8.3-8.8 % at 720p and 9.2 % at 1080p on the reference Mac, against 2.0 % for the preview alone. The rest is Slint redrawing the whole window at the meter rate: Slint's Skia renderer gets a buffer age, and so redraws only dirty regions, only on OpenGL; on its `wgpu` and Metal paths every change redraws everything. A real mixer has more meters than the demo's 16 and other panels in the same window, so the margin would not hold, and any other toolkit on the GPU path has the same whole-window redraw (ADR-0022 Review, 2026-11-03).

The meter itself was not specified beyond rule U16 (dBFS scale, peak hold, clip indicator). This ADR also adds the AudioMeter contract to the design system spec.

Measured on the reference Mac (MacBook Pro M4 Pro, macOS 26, release build of `remote_preview` with `preview_engine`, preview as a layer per ADR-0034, 16 meters at 30 Hz with their accessible values at 1 Hz, 21 s windows, process CPU of the UI from `getrusage`), runs alternated in one sitting:

| Meters drawn by | 720p preview | 1080p preview |
| --- | --- | --- |
| Slint (`RBS_METER_MODE=slint`) | 8.4, 8.5, 8.6, 9.1 % | 9.2 % |
| Layers (`RBS_METER_MODE=layer`, this ADR) | 2.4, 2.4, 2.4, 2.7 % | 2.5 % |
| No meters (preview only) | 1.9, 2.0 % | |

Every run showed 60.0 fps; frames more than 25 ms apart in delivery to the UI ranged from 0 to 3 per run in both meter modes. Peak resident memory was 103-104 MiB in every run. The 1080p figures are single runs.

## Decision

1. **AudioMeter contract.** Scale, zones, peak hold, clip latch, orientation, rates and accessibility are specified in the [design system](../specs/ui-design-system.md#audiometer) and implemented once, toolkit-neutral, in `rbs_ui::meter` (`Meter`, `MeterView`, `MeterView::frames`). Time is passed in by the caller.
2. **Split by rate.** The toolkit draws a meter's static parts (bar background, scale and labels, the clip indicator's outline) and owns its accessible element. The parts that move at the meter rate (lit zones, peak line, filled clip indicator) are drawn by a native compositor layer where the platform has one. A meter update is then one compositor transaction and no toolkit redraw.
3. **macOS layer.** `rbs_app::native::MeterBridgeLayer` holds one container `CALayer` above the toolkit's layer per meter bridge (a mixer, or any group of meters laid out together), with five solid-colour sublayers per meter: three zones, the peak line and the clip fill. `show(&[MeterView])` changes frames and visibility in one transaction without implicit animations, skips meters whose view did not change and never allocates. Geometry comes from the toolkit's layout through `set_rect` (the bridge, window coordinates, top-left origin) and `set_meters` (each meter relative to the bridge), called before every toolkit redraw, as for the preview layer (ADR-0034 decision 2). Colours come from the theme's tokens (`MeterColors::from_tokens`); a theme change calls `set_colors`.
4. **Overlap and stacking** follow ADR-0034 decision 4: toolkit content drawn over a bridge hides the bridge (`set_hidden`) while it overlaps.
5. **Accessibility.** The accessible value (level in dBFS, and "clipped") is updated at most once per second, not at the meter rate.
6. **Fallback.** Where no layer backend exists (Windows and Linux today), or attaching fails, the toolkit draws the moving parts from the same `MeterView::frames`. Windows (DirectComposition) and Linux (Wayland subsurface) backends follow the preview layer backends of ADR-0034 decision 6.
7. **Zero cost when unused (P13).** A bridge exists only while its meters are visible; it adds no thread and no timer of its own. Updates come from the meter data the UI already receives, at 30 Hz, and stop while the audio is inactive.
8. **Where the code lives.** The layer is UI code, so it lives in the UI crate: `rbs-app` gains a library target (`src/lib.rs`, module `native`, a marked low-level module per P11) that the shell and its examples share, with macOS-only dependencies on `objc2`, `objc2-quartz-core`, `objc2-core-foundation` and `objc2-core-graphics` (versions already in the lockfile through `wgpu-hal` and `rbs-gpu`). No engine, port or core crate changes.

## Consequences

- Meter updates no longer redraw the window on macOS; the toolkit redraws for layout, input and text at 1 Hz. The UI with preview and meters goes from about 8.5 % to about 2.5 % of one core, 0.5 points above the preview alone: the budget now has a wide margin.
- Meters must be laid out so the shell can tell the bridge where each meter is. In the demo, Rust and the Slint markup share the layout constants; the shell's AudioStrip component must expose each meter's rectangle (for example with a callback on geometry change) when it is built.
- Two drawing paths for the same meter (layer and toolkit fallback). Both take their geometry from `MeterView::frames`, so they differ only in how a rectangle becomes pixels; the conformance test `crates/rbs-app/tests/meter_layer_macos.rs` checks the layer against it.
- A screenshot of the window through the toolkit no longer contains the meters' moving parts on macOS (as for the preview).
- Colours: the layer's colours are sRGB `CGColor`s, which Core Animation colour-manages; Slint's Metal layer carries no colour space, so its token colours reach the wide-gamut display unmanaged and look a little more saturated. Side by side the meter fill is slightly less saturated than the same token drawn by Slint. The right fix is to tag the toolkit's layer as sRGB (tokens and contrast checks are sRGB), which changes every colour of the UI and is follow-up work, not part of this ADR.
- Edges are snapped to device pixels (`contentsScale` of the parent), as the toolkit draws them; unsnapped half-pixel edges showed as soft 1 px columns on the Retina display.
- The design system's requirement 8 now names native layers for graphics that move at the meter rate or faster.

## Alternatives considered

- **Keep meters in Slint, fewer redraws** (lower meter rate, coalescing with the status text): halves the cost at best and makes meters less useful; still a whole-window redraw.
- **Meters on a GPU texture shown by Slint** (ADR-0022 decision 3, second tier): the texture is cheap, but Slint must still redraw the window to show a new texture, so it buys nothing for the redraw cost.
- **Meters as one IOSurface layer drawn by a UI-side `wgpu` pass**: per-pixel freedom (for gradients, spectrum, scopes) but GPU work and a ring of surfaces per update, where solid rectangles need neither. Kept for dense visualisations (spectrum, scopes), where the five-layers-per-meter approach does not scale.
- **Slint on OpenGL for partial redraw**: Slint's Skia renderer gets a buffer age only on OpenGL, but OpenGL is deprecated on macOS and would lose the shared `wgpu` device that the preview's texture fallback uses (ADR-0022 decision 2).

## Review

Pending: independent review per ADR-0014.
