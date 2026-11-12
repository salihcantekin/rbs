# ADR-0102: Output size distinct from the canvas

- Status: Accepted (macOS done; Windows remains)
- Date: 2026-11-12
- Deciders: Salih (owner)
- Refines: [ADR-0047](0047-several-outputs-per-canvas.md) decision 3 (encoder sharing also matches output size), [ADR-0066](0066-virtual-camera.md) decision 4 (the scaled converter's kernel; the 1:1 path and letterbox stay)

## Context

A recording and a stream are encoded at the canvas size. The canvas is both the scene's pixel grid and the encoded picture: the glossary calls it an output picture size, and the export node writes NV12 or P010 at that size through the 1:1 `textureLoad` path (`YuvDownload::with_tone`). The only scaled output is the virtual camera (ADR-0066): `YuvConverter::scaled` fits the canvas, keeps aspect, letterboxes, and samples with bilinear from the linear `Rgba16Float` canvas. Preview scaling fits the UI panel and is not encoded.

OBS has one "Output (Scaled) Resolution", plus a second rescale in advanced output mode. Those steps usually run on a gamma-coded 8-bit frame through one global filter. Lanczos upscale rings; bilinear upscale stays soft. Changing that scaled resolution is not a separate size on each output, taken once from a linear canvas, and it is not kept from moving the scene the way a canvas change does.

Forces: one readback per export (ADR-0038, P4) must not gain a second copy; an equal size must not sample (P13); a kernel that is not in use must not be a per-frame branch (the colour-mode rule); a vertical program is a second canvas (ADR-0093), not a scale of the horizontal one; the scene collection holds no output settings (ADR-0051).

## Decision

1. **Each recording and each stream has its own output size, stored on the output profile, not in the scene collection.** The sections are `recording`, `stream`, `recording-vertical` and `stream-vertical`. The value is absent or `output_size = { width, height }`. Absent means the same as **that** output's canvas. `format_version` stays 1. An explicit size is two even sides. A half-specified size (one side only) is refused. While `recording-vertical` is absent, encoding settings still fall back to `recording` (protocol 1.29), but the output size does not: the vertical recording stays at the vertical canvas. The first saved copy of a vertical section must omit `output_size` unless the user set one for that canvas.

2. **Changing the output size must not restart the studio and must not move scene items.** It applies the next time that output starts. An output that has already started keeps the size it started with. `settings.set` of a recording profile stays `conflict` while a recording uses those settings, and `settings.set` of `stream` stays `conflict` while that stream runs, including when only the size changed. A canvas change still restarts the studio. A stored size is not rewritten when the canvas changes. The next start refuses it when it no longer matches that canvas (decision 4) or the limits (decision 7).

3. **Equal size uses the existing 1:1 path.** Resolved size equal to that output's canvas builds `YuvDownload::with_tone` (and `with_color` when the colour is not the default): `textureLoad`, no scaler, no extra sampling. A different size scales **once** from that output's linear `Rgba16Float` canvas, inside the existing luma and chroma passes, before encode. There must be no intermediate scaled canvas and no second YUV conversion. Two outputs scale from the same canvas texture. One must not scale from the other's planes. The replay buffer is unchanged: it still exports the canvas.

4. **Output aspect matches that output's canvas.** A vertical program stays the second canvas (ADR-0093). It must not be produced by scaling a horizontal canvas. An explicit size is the fit of an even long side onto the canvas:

   - Long side `L` is `max(width, height)` and lies on the same axis as the canvas's long side (a square canvas stays square).
   - The short side is the even integer closest to `L * canvas_short / canvas_long`. With `num = L * canvas_short` and `den = canvas_long`, the nearest integer is `(2 * num + den) / (2 * den)` (a half rounds away from zero). An even result is the short side. An odd result takes the even neighbour closer to `num / den`, and a tie keeps the smaller one.
   - Both sides must be in 2..=7680. Otherwise that long side is not a legal output size.

   UI presets are the long sides 1280, 1920, 2560 and 3840, each fitted that way, skipping a result equal to the canvas or outside 2..=7680. The default row is "Same as canvas" (stored absent). A preset with fewer pixels than the canvas (`width * height`) is labelled downscale; one with more is labelled upscale. The API may store any long side that passes this fit, not only the four presets.

5. **The scale kernel is chosen when the converter is created, and only that kernel is built** (the colour-mode rule: no per-frame branch on an unused mode). One shader module may hold every entry point; the pipeline's entry point is the one pair below, including the open-shadows or sRGB pair when that colour mode is the one being built. No Lanczos entry point is added.

   | Scale of the fitted picture | Kernel | Entry points (video; `_open` and `_srgb` infix the same way) |
   | --- | --- | --- |
   | Sizes equal | Existing `textureLoad`, unchanged | `fs_luma`, `fs_chroma` |
   | Integer downscale: canvas width and height are `N` times the fitted picture, integer `N >= 2` | Box average, equal weights, in linear light, then the same OETF and matrix | `fs_luma_box`, `fs_chroma_box` |
   | Fitted picture larger than the canvas on both axes | Catmull-Rom (Keys cubic, `a = -0.5`), in linear light, samples clamped to the canvas edge, then the same OETF and matrix | `fs_luma_catmull`, `fs_chroma_catmull` |
   | Any other downscale | Existing bilinear in linear light, unchanged | `fs_luma_scaled`, `fs_chroma_scaled` |

   For a recording or a stream the fitted picture is the whole output (decision 4), so there are no bars. Chroma stays centre-sited: the mean of the four luma-pixel centres of its 2×2 block after the luma kernel. `N` and the fit rectangle are pipeline constants, not a per-frame branch.

6. **The virtual camera uses this same scaled converter.** A camera size that differs from the canvas gets the kernel in decision 5. Its 1:1 path stays `textureLoad`. ADR-0066 decision 4's fit stays when the camera aspect differs: aspect kept, centred, black outside the fit (Y 16, C 128 on the video path). The kernel runs on that fitted rectangle.

7. **Limits.** An explicit size must have even sides, each from 2 to 7680 inclusive, must match decision 4, and the resolved size (the canvas, when the size is absent) must lie in the encoder's declared `codec.video.width` and `codec.video.height` when that encoder declares them. Otherwise refuse. `settings.set` uses `invalid_params` and writes nothing. `start_output` returns `RecordError::OutputSize { width, height }` before any adapter is chosen when an explicit size fails the even, range or aspect checks. A candidate whose declared range excludes the resolved size is skipped; if none remain, the start is `NoAdapter`. An encoder that declares no range is not refused for range. Absent size skips the 2..=7680 check: the studio's canvas is already a legal canvas, including a vertical canvas up to 8192 (ADR-0093).

8. **"Main display" is the framebuffer in physical pixels.** The canvas must not be multiplied or divided by the scale. The Video settings row reads `{width} x {height} (main display, {percent}%)`, percent included at 100% as well. Percent is the integer closest to `display_scale * 100` on macOS and to `effective_dpi / 96 * 100` on Windows (halves away from zero). macOS already has `CGDisplayMode` pixel size and `display_scale`; the pixel size is the canvas size, and the scale is only the label. Windows must read `EnumDisplaySettings` (`ENUM_CURRENT_SETTINGS`, `dmPelsWidth` / `dmPelsHeight`) and the monitor's effective DPI. When that pixel mode disagrees with `GetMonitorInfo`, the pixel mode is the canvas size. A pure function with no OS calls chooses the size: the pixel mode when it was read, otherwise the monitor rectangle, and never a size scaled by the percent. Checks: 3840×2160 at scale 2 is `3840 x 2160 (main display, 200%)`, not 1920×1080 and not 7680×4320; a Windows pixel mode of 1920×1080, a monitor rectangle of 1280×720 and 144 DPI is `1920 x 1080 (main display, 150%)`.

9. **Control API protocol 1.39.** `settings.get`, `settings.set` and `settings.changed` gain `output_size` on each main recording profile and on `stream`: `{ "width", "height" }`, or omitted when it is the same as that output's canvas (`null` is the same as omitted). Old clients ignore the new field. An old profile file with no `output_size` reads as the same as that canvas. A `settings.set` that omits it stores the same as canvas. Sides that fail decision 4 or 7 are `invalid_params`. Vertical settings stay out of these methods until their own minor version (protocol 1.29); the profile sections still store the size (decision 1).

10. **Preview scaling is unchanged** and is not the encoded output. The panel fit stays the preview publisher's bilinear pass.

What OBS does not do, and this decision requires: one pass per output from the linear 16-bit canvas, a box filter for integer downscale, Catmull-Rom for upscale, a size per output, and an output size that does not rescale the scene.

## Consequences

- A 4K recording and a 1080p stream of one canvas are two exports and two encoders, each scaled from that canvas. Matching size and matching video settings still share one export and one encoder (ADR-0047). Audio sharing is unchanged.
- The 1:1 converter and its entry points stay. New entry points are the colour-conversion hot path ([hot-paths.md](../architecture/hot-paths.md)). The implementation needs before/after numbers, the `hot-path-review` checklist and a human sign-off (P15). The 1:1 path must measure unchanged.
- Tests (ADR-0032, `TestClock`, no sleeps): equal size is the 1:1 frame; a 2× downscale is the box mean of known flat blocks; an upscale keeps a small edge sharper than bilinear; fitted sizes are even and share the canvas aspect; a 1080p stream and a canvas-sized recording open two encoders; equal output sizes share one; an old profile loads as the canvas size; the pure function in decision 8 picks the framebuffer and the label at 200% and at 150%.
- Settings copy and screenshots are a later UI change (rule 16). It must use the glossary terms Canvas and Output size.
- This ADR does not supersede ADR-0047 or ADR-0066. It does not change ADR-0093.

## Alternatives considered

- **One global scaled resolution, and a second rescale in advanced mode (OBS).** Two outputs cannot differ without scaling from each other, and the scale usually leaves linear 16-bit. Rejected.
- **Lanczos.** Upscale rings; a downscale Lanczos is unmeasured. Not in this change.
- **Bilinear for every scale, including integer downscale and upscale.** Integer downscale blurs a flat block that a box mean keeps; upscale stays soft. Rejected for those two cases. Non-integer downscale keeps bilinear.
- **Store the size on the canvas or the scene collection.** A canvas change restarts the studio and moves items, and both outputs of one canvas would share it. Rejected.
- **Multiply the main-display canvas by the OS scale.** A 200% framebuffer would be counted twice, or a physical mode would be replaced by logical points. Rejected.
- **A per-frame branch on the kernel.** An unused mode would still be tested on the hot path. Rejected; the colour-mode rule already forbids it.

## Review

- 2026-11-12: Owner (Salih) accepts this ADR. The macOS path is done: each recording and stream stores its own output size, protocol 1.39, the scale kernels, and the settings rows. The Windows side remains. Status: Accepted.
