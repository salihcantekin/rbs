# ADR-0074: Still sources: an image file and a solid colour or gradient, drawn once and costing nothing per frame

- Status: Proposed
- Date: 2026-11-08
- Deciders: Salih (owner)
- Builds on: ADR-0013 (zero cost when unused), ADR-0021 (scene collection model), ADR-0038 (I/O graph nodes), ADR-0039 (scene source types), ADR-0053 (item style and video filters)

## Context

The owner wants backgrounds, frames and overlays (logos, lower-third plates) without a plugin, as OBS has with its image and colour sources. Today every video source is a capture session (display, window, camera), so a background needs a workaround. A later text, browser or media source (see the planning thread on text and browser sources) can follow the same pattern; this ADR covers only pictures that do not change on their own.

Forces: P4 (no CPU readback), P6 (no allocation or I/O in `Node::process`), P13 (an unused feature costs nothing; an unchanging picture must not cost per-frame work), rule 7 and 8 (no panics at boundaries, typed errors), and licensing (GPL-3.0-or-later, ADR-0010: shipped sample pictures must be original).

## Decision

1. **Two new source types, always installed.** `org.rbs.source.image` (a picture file or a built-in sample) and `org.rbs.source.color` (a solid colour or a two-colour linear gradient). They need no adapter, so `SourceTypes::is_installed` is true without a probe. Their class is `SourceClass::Still`.
2. **One node type, `org.rbs.still`** in `rbs-nodes`, kind `Source`, output "video", render partition. The pixels are decoded or generated on the control thread (in `instantiate`), as straight-alpha `Rgba8` system frames, which the compositor already samples and premultiplies. The node uploads them with `SourceUpload` on its first `process`, publishes the texture, and does nothing on every later tick (the slot keeps the last frame, as for capture). It holds no thread, device or file handle. `release` drops the pool textures.
3. **Settings (type `image`).** `path` (text, a file; PNG in this step), or `sample` (text, the id of a built-in sample picture, see decision 6). `path` wins when both are set. A missing or unreadable file is `SourceError::Image` at `instantiate`, shown by the app like a missing camera; it never panics. Files larger than 8192 x 8192 pixels or 256 MiB decoded are refused.
4. **Settings (type `colour`).** `color` (text `#rrggbb` or `#rrggbbaa`, default `#000000`), optional `color2` (same form; present means a gradient), `angle` (int degrees, 0 = top to bottom, default 0, only with `color2`), `width` and `height` (int, default 1920 x 1080). A gradient is generated in sRGB space per pixel, in the source's own size; the item scales it like any picture.
5. **Decoder: the `png` crate** (MIT or Apache-2.0, pure Rust, already in `Cargo.lock` through the UI stack) in `rbs-sources`, which is an engine-layer crate, not a core or port crate. JPEG and other formats are a later step. Decoding runs on the control thread and may block there; it never runs in `process`.
6. **Built-in sample pictures are generated in code**, not shipped as files: soft gradient backgrounds, a dotted pattern, and frames (a border ring with a transparent middle). They are original work of this repository (GPL-3.0-or-later) and add no binary asset or third-party licence. The set and ids are in the scene sources spec. Pictures from outside the repository are a separate licensing decision.
7. **Changing a setting rebuilds the node.** There is no parameter channel for a still; a settings edit replaces the node (as for capture sources) and the picture is uploaded again. This costs one upload per edit, nothing while idle.
8. **Layout and filters work unchanged.** A still source is an ordinary source: position, size, crop, shape, border (ADR-0053) and the filter chain apply to it. The picture's native size is the source size for `Layout::fit`.
9. **No format change.** Sources and settings already serialise as text, ints and bools in scene collection format 2 and later; older builds that do not know the type ids show an unknown-type source and keep the document (ADR-0051).

## Consequences

- A background, a logo or a frame is one source and costs one upload when it joins the schedule and nothing per frame afterwards (P13). Removing it from the schedule returns its texture.
- `SourceInstance` gains a `Still` variant, so the composition roots that match it (`rbs-record`, `rbs-app` through `rbs-record`) handle it; the enum is `non_exhaustive`.
- `rbs-sources` gains the `png` dependency (justification above). Core and port crates are untouched.
- A large picture is held in memory only until its upload, then dropped; the GPU keeps the texture.
- Animated pictures (GIF, APNG, video), SVG, text and browser content are not covered. They need a source that produces frames over time and are decided separately.

## Alternatives considered

- **Generate the gradient on the GPU in a shader.** No upload, but a new pipeline and a per-frame draw for a picture that never changes; a one-time upload is cheaper overall (P13) and reuses tested code.
- **Treat a picture as a one-frame capture adapter.** Reuses the capture port, but a "device" with no device pollutes the port and its conformance suite (P1, P2).
- **Ship sample pictures as PNG files.** Simpler to author, but adds binary assets with licence questions; generated pictures avoid both.
- **Use the `image` crate.** Many formats, but a large dependency tree for what this step needs; `png` alone is enough now.
