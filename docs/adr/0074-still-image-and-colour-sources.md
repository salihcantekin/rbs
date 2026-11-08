# ADR-0074: Still sources: an image file and a solid colour or gradient, drawn once and costing nothing per frame

- Status: Accepted
- Date: 2026-11-08
- Deciders: Salih (owner)
- Builds on: ADR-0013 (zero cost when unused), ADR-0021 (scene collection model), ADR-0038 (I/O graph nodes), ADR-0039 (scene source types), ADR-0053 (item style and video filters)

## Context

The owner wants backgrounds, frames and overlays (logos, lower-third plates) without a plugin, as OBS has with its image and colour sources. Today every video source is a capture session (display, window, camera), so a background needs a workaround. A later text, browser or media source (see the planning thread on text and browser sources) can follow the same pattern; this ADR covers only pictures that do not change on their own.

Forces: P4 (no CPU readback), P6 (no allocation or I/O in `Node::process`), P13 (an unused feature costs nothing; an unchanging picture must not cost per-frame work), rule 7 and 8 (no panics at boundaries, typed errors), and licensing (GPL-3.0-or-later, ADR-0010: shipped sample pictures must be original).

## Decision

1. **Two new source types, always installed.** `org.rbs.source.image` (a picture file or a built-in sample) and `org.rbs.source.color` (a solid colour or a two-colour linear gradient). They need no adapter, so `SourceTypes::is_installed` is true without a probe. Their class is `SourceClass::Still`.
2. **One node type, `org.rbs.still`** in `rbs-nodes`, kind `Source`, output "video", render partition. The pixels are decoded or generated on the control thread (in `instantiate`), as straight-alpha `Rgba8` system frames, which the compositor already samples and premultiplies. The node uploads them with `SourceUpload` in `warm` (not real time, so `process` never allocates or touches the GPU, P6), publishes the texture in its first `process`, and does nothing on every later tick (the slot keeps the last frame, as for capture). It holds no thread, device or file handle. `cool` and `release` give the texture back; a later `warm` uploads again from the pixels the node keeps (decision 10).
3. **Settings (type `image`).** `path` (text, a file; PNG in this step), or `sample` (text, the id of a built-in sample picture, see decision 6). `path` wins when both are set. A missing or unreadable file is `SourceError::Image` at `instantiate`, shown by the app like a missing camera; it never panics. A file with a side over 8192 pixels, or whose decoded buffer (as the decoder reports it, before conversion to RGBA) is over 128 MiB, is refused before anything is allocated. The peak while decoding is the decoder buffer plus the RGBA copy, so at most about 2.3 times the limit, 300 MiB; it is released when `instantiate` returns, and the node keeps only the RGBA pixels. A picture the GPU cannot take (its texture limit can be below 8192 on some adapters) makes the node fail permanently at `warm` with a typed error; the source shows as failed like a missing camera.
4. **Settings (type `color`).** `color` (text `#rrggbb` or `#rrggbbaa`, default `#000000`), optional `color2` (same form; present means a gradient), `angle` (int degrees, 0 = top to bottom, default 0, only with `color2`), `width` and `height` (int, default 1920 x 1080). A gradient is generated in sRGB space per pixel, in the source's own size; the item scales it like any picture.
5. **Decoder: the `png` crate** (MIT or Apache-2.0, pure Rust, already in `Cargo.lock` through the UI stack) in `rbs-sources`, which is an engine-layer crate, not a core or port crate. JPEG and other formats are a later step. Decoding runs on the control thread and may block there; it never runs in `process`.
6. **Built-in sample pictures are generated in code**, not shipped as files: soft gradient backgrounds, a dotted pattern, and frames (a border ring with a transparent middle). They are original work of this repository (GPL-3.0-or-later) and add no binary asset or third-party licence. The set and ids are in the scene sources spec. Pictures from outside the repository are a separate licensing decision.
7. **Which thread decodes.** `instantiate` runs on the control thread, which in `rbs-app` is the host thread, not the UI thread. A large PNG can take seconds there, so the Library and Add source UI must show the source as pending, not block on it; moving decoding to a worker is a follow-up when measurements show a stall.
8. **Colour management.** PNG gamma, cHRM and ICC chunks are ignored and the pixels are treated as sRGB (the node declares `ColorSpace::SRGB`); 16-bit input is reduced to 8 bits and interlaced files are decoded to the same pixels. A wide-gamut PNG therefore looks different from tools that honour its profile.
9. **Security.** `path` is any local file the process can read, and symlinks are followed. A control API client (including the AI agent surface, ADR-0052) that may set source settings can therefore show any readable PNG on the host in the preview, a recording or a stream. This is the same accepted permission-system gap as vcam and stream start (the owner accepted it until a token scope system exists); `source.path` is added to the list to revisit then. `SourceError::Image` carries the decoder's or OS error text without the path.
10. **Memory while installed.** The node keeps the straight-alpha pixels for its whole life so that `warm` can run again after `cool`: a 3840 x 2160 picture costs about 33 MiB of CPU memory per instance, also in Standby or when not scheduled. This is accepted for pictures that are meant to stay; the GPU texture is held only while warm. Re-decoding on demand would remove the CPU copy at the price of file access in `warm`, and is not done.
11. **Health.** A still reports one frame and then none; the UI must not treat that as a stalled source (the UI follow-up tests it).
12. **Changing a setting rebuilds the node.** There is no parameter channel for a still; a settings edit replaces the node (as for capture sources) and the picture is uploaded again. This costs one upload per edit, nothing while idle.
13. **Layout and filters work unchanged.** A still source is an ordinary source: position, size, crop, shape, border (ADR-0053) and the filter chain apply to it. The picture's native size is the source size for `Layout::fit`.
14. **No format change.** Sources and settings already serialise as text, ints and bools in scene collection format 2 and later; older builds that do not know the type ids show an unknown-type source and keep the document (ADR-0051).

## Consequences

- A background, a logo or a frame is one source and costs one upload when it joins the schedule and nothing per frame afterwards (P13). Removing it from the schedule returns its texture.
- `SourceInstance` gains a `Still` variant, so the composition roots that match it (`rbs-record`, `rbs-app` through `rbs-record`) handle it; the enum is `non_exhaustive`.
- `rbs-sources` gains the `png` dependency (justification above). Core and port crates are untouched.
- The pictures' pixels stay in CPU memory for the life of the node (decision 10); the GPU texture only while warm.
- Animated pictures (GIF, APNG, video), SVG, text and browser content are not covered. They need a source that produces frames over time and are decided separately.

## Related

ADR-0075 (looks and the Library panel) lists these samples by id in the Library; the ids live in `rbs_sources::still::SAMPLES` only.

## Alternatives considered

- **Generate the gradient on the GPU in a shader.** No upload, but a new pipeline and a per-frame draw for a picture that never changes; a one-time upload is cheaper overall (P13) and reuses tested code.
- **Treat a picture as a one-frame capture adapter.** Reuses the capture port, but a "device" with no device pollutes the port and its conformance suite (P1, P2).
- **Ship sample pictures as PNG files.** Simpler to author, but adds binary assets with licence questions; generated pictures avoid both.
- **Use the `image` crate.** Many formats, but a large dependency tree for what this step needs; `png` alone is enough now.

## Review

Reviewer: independent AI session (round 1)
Date: 2026-11-08
Verdict: changes requested
Concerns:
1. ADR said the upload happens in the first `process`; code and spec upload in `warm`. -> Resolved: decision 2 says `warm` uploads and `process` only publishes.
2. "Pixels held only until upload" was false (the node keeps them for `warm` after `cool`). -> Resolved: decision 10 states the CPU memory held for the node's life and why; Consequences corrected.
3. Decode peak memory about twice the stated limit, and the limit compared the wrong size. -> Resolved: the limit is the decoder's own buffer size (128 MiB, checked before allocating) and the peak is stated (decision 3); test `pictures_over_the_size_limits_are_refused_before_decoding`.
4. A `path` setting lets a control token show any readable PNG. -> Resolved: decision 9 (Security): same accepted permission-system gap, symlinks followed, no path in the error text, `source.path` to revisit with the permission system.
5. `scene-sources.md` settings rows were split off the table, `width` and `height` listed twice. -> Resolved: one table, one `width, height` row naming the types.
6. `colour` and `color` naming. -> Resolved: `color` for ids and keys.
7. Colour management not stated. -> Resolved: decision 8.
8. Which thread decodes. -> Resolved: decision 7 (control thread, not the UI thread; a worker is a follow-up if measured).
9. A still must not show as stalled. -> Recorded: decision 11; tested with the UI follow-up.
10. Missing tests for Standby and back, and for the size limits. -> Resolved: `a_still_is_uploaded_once_and_stays_on_the_canvas` now cycles Standby and Active and checks one more upload and publication; the size-limit test.
11. GPU texture limit. -> Resolved: a picture the GPU cannot take fails the node at `warm` with a typed error (decision 3).
