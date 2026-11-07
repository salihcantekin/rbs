# ADR-0057: Crop editing of sources: keep the scale, read the picture size from the capture node

- Status: Accepted
- Date: 2026-11-06
- Deciders: Salih (owner)
- Builds on: ADR-0048 (preview handles), ADR-0044 (control API), ADR-0053 (item style), ADR-0013 (zero cost when unused)

## Context

A user on a 3440x1440 display wants to show only a 1920x1080 part of it. The scene model already has a crop per item layout (`Layout::crop`, source pixels cut from each edge; [scene-model.md](../specs/scene-model.md)), the binder passes it to the compositor, and the compositor turns it into texture coordinates with the real texture size each frame ([gpu-compositor.md](../specs/gpu-compositor.md)). It is saved with the collection and returned by `source.list` (control API 1.3). Nothing can set it: the app's Properties panel has only X, Y, W and H, the preview handles (ADR-0048) only move and resize, and the control API has no setter. The capture port leaves cropping, including region capture, to the compositor ([capture-port.md](../specs/capture-port.md), out of scope).

`Layout::size` maps the unit square of the *cropped* source to the canvas. Changing only the crop therefore stretches what is left over the old box: cutting the right half of a picture doubles the width of the left half. Keeping the picture's scale needs the source's size in pixels, which the control thread does not know today: the studio host learns a display's size only from the target list when the source is added, a camera's actual mode is chosen by the adapter, a window can be resized at any time, and on HiDPI displays points and pixels differ.

A crop larger than the current picture (a window that shrank) gives texture coordinates with `u1 < u0`. Both compositor pipelines draw that as a mirrored strip: the plain vertex shader interpolates the reversed range, and the styled fragment shader's clamp uses the `min`/`max` of the crop, which keeps it.

## Decision

1. **Crop stays in source pixels; an edit keeps the scale.** The scene model does not change. A crop edit from the app or the API, unless told otherwise, also changes the box so that each source pixel keeps its size on the canvas and the part of the picture that stays does not move (rotation and flips included). This is a pure function in `rbs-scene`, `Layout::recrop(source: (u32, u32), crop: Crop) -> Option<Layout>`, which refuses (`None`) a crop that leaves less than one pixel in either direction or a source size of zero; if the current crop does not fit the source, the scale is taken from the whole picture and the box keeps its centre. `keep_scale = false` sets only the crop (the remaining picture fills the old box) and needs no source size. The crop is computed once, at edit time: when the picture later changes size (a window resized, or moved to a display with another scale), the box stays and the cropped picture is rescaled into it; the crop is not re-derived.
2. **The capture node reports the size of the picture it delivers.** `CaptureNodeStats` (now `#[non_exhaustive]`; only the node makes it) gains `picture_size() -> Option<(u32, u32)>`, one `AtomicU64` (width in the high half, height in the low half, 0 for unknown). `prepare` stores the session's negotiated format; `process` compares each new frame's `format().size` with a plain field of the node and stores the new size only when it differs (one comparison per new frame, one relaxed store per resize); `release` stores 0. The frame's size is the size of the texture the compositor sees: the importer and the upload path create textures of `format().size` (gpu-compositor spec). `rbs_record::Studio::picture_size(source)` reads it for a scheduled video source. The studio host remembers the last size it read per source, so a source that ran once in this session can still be cropped after it stopped; a crop that keeps the scale is refused only while no size was ever known ("no picture yet"). The app crops items of the program scene, whose sources run while anything shows or records the canvas (always on macOS and Windows, where the preview does); a preview scene (studio mode) gets the same rule when it exists.
3. **The compositor skips a layer whose crop leaves no picture.** `Compositor::render` does not draw a layer whose texture coordinates have `u1 <= u0` or `v1 <= v0` (or are NaN), next to the existing skip of stale textures, so every caller behaves the same and record and style indices stay in step; it counts them in `RenderStats::empty_crop_layers`. `UvRect` is ordered by contract. A window that shrinks under its crop shows nothing until it grows again or the crop is changed, never a mirrored strip.
4. **Properties.** A video source's Properties get Crop Left, Top, Right and Bottom fields in source pixels (each change is one undo step and keeps the scale) and a Region menu: *Full picture* (crop removed, scale kept), *Canvas size, centred* (the centre `min(canvas, source)` pixels, then fitted to the canvas with `Layout::fit`), *Left half* and *Right half* (also fitted). The source's picture size is shown next to the fields.
5. **Preview.** Holding Alt (Option on macOS) while dragging an edge or corner handle crops that side instead of resizing (as in other broadcast tools); the outline shows the cropped box during the drag, edges snap to the canvas's edges and centre unless Ctrl/Cmd is held, and a crop never leaves less than one source pixel. The geometry is pure code in `rbs_ui::transform`. A drag sends `SetSourceCrop` with the ADR-0048 flow control (one request in flight, sequence numbers, `CropPlaced { seq, ok }`), and all crops of one drag are one undo step.
6. **Undo.** Crop edits are scene edits on the shared stack, `EditKind::Crop` ("Crop <source>"); the control API's undo section lists them.
7. **Control API 1.4.** `item.set_crop {item, crop: [left, top, right, bottom], keep_scale?: bool = true}` (scope `control`) edits the item's layout on the studio's canvas: its override for that canvas when it has one, else its default layout (the `item.set_style` rule). Refusals: `not_found` for an unknown item; `invalid_params` for an item that places a scene, a crop that leaves less than one pixel of the known picture, or unknown keys; `failed` for `keep_scale` while no picture size was ever known. One undo step; publishes `source.changed`; setting the crop the item already has changes nothing.
8. **No region capture in the adapters now.** The capture port's scope does not change. A capture adapter still delivers the whole display or window; the compositor samples only the cropped part. If a measurement shows that capturing the whole surface costs too much (memory bandwidth of a large display at a high rate), a capture request region (`CaptureRequest::with_region`, ScreenCaptureKit `sourceRect`, a cropped WGC copy) is a later minor version of the capture port with its own ADR.

## Consequences

- `rbs-nodes` and `rbs-gpu` are 0.0.x workspace crates: `CaptureNodeStats` becomes `#[non_exhaustive]` and `RenderStats` gains a field; no code outside the node builds either.
- One `AtomicU64` and one comparison per new frame in `CaptureSourceNode::process`, and one comparison per layer per frame in `Compositor::render`: both on hot paths, so the PR measures them (`measure-performance`) and needs the owner's P15 sign-off.
- The host needs no target enumeration to crop, and the size is right for cameras (the chosen mode), windows (after resizes) and HiDPI displays (pixels, not points).
- `Studio::picture_size` exists only for scheduled sources: a source that is not running cannot be cropped from the app or the API (its stored crop still applies when it runs).
- Spec updates in the same change: scene-model (recrop), gpu-compositor (empty crop skipped), capture-port (`CaptureNodeStats::picture_size` is node side, the port unchanged), control-api (1.4, `item.set_crop`), ui-design-system (crop fields, Region menu, Alt-drag).
- Alt-drag is often taken by the window manager on Linux; a crop mode toggle on the picture panel (and a command in the registry) is the follow-up there, as Linux work is parked.
- Follow-ups: region capture in adapters if measured to matter (decision 8); a "pick region" tool that drags a rectangle on the preview to crop and fit in one step.

## Alternatives considered

- **Crop only, no scale keeping.** Simple, but every crop distorts the picture until the user fixes the box by hand.
- **Size from the target list on the control thread.** No hot-path change, but enumeration can take tens of milliseconds (ScreenCaptureKit), knows nothing about a camera's chosen mode, goes stale when a window resizes, and reports points on some systems.
- **Store the crop as fractions of the source.** Keeping the scale would then need no source size (`size' = size · f'/f`), so neither the capture-node change nor the scheduled-source rule would be needed, and the crop would follow resizes and HiDPI changes. Rejected: it changes the scene format (a new version and migration) and the meaning of a saved field, the fields users type and the API speak in pixels ("1920x1080 of my display"), so a size is needed anyway to show and convert them, and a resized window would silently show a different part of itself.
- **Clamp the crop in the compositor instead of skipping the layer.** A zero-width texture rectangle still draws one column stretched over the box.
- **Region capture in the adapters first.** Saves bandwidth, but needs a capture port change, three adapters and hardware tests on each platform, and a crop on the compositor is needed anyway for cameras and for changing the region without restarting the capture.

## Review

Independent review per ADR-0014.

- Reviewer: independent AI session (given only the ADR, principles, specs and code). Date: 2026-11-06. Verdict: changes requested; all resolved in this revision.
- Concerns and resolutions:
  1. The styled pipeline mirrors a reversed crop too: Context corrected.
  2. Skip in `CompositorNode::process` would leave other `Compositor` callers and the record indices out of step: moved into `Compositor::render` (decision 3), counted separately, `UvRect` ordered by contract; test `a_crop_that_leaves_no_picture_draws_nothing_plain_or_styled` covers plain and styled layers.
  3. Which size is reported, and stale after release: the frame's size, equal to the texture's by the import and upload contract; reset to 0 in `release` (decision 2); test `the_capture_node_reports_its_picture_size_and_follows_a_resize`.
  4. Semver of `CaptureNodeStats`: made `#[non_exhaustive]`, stated in Consequences.
  5. `item.set_crop` layout rule and `keep_scale = false` without a size: both stated (decisions 1 and 7).
  6. Fractional crop alternative missing: added with the reason it was rejected.
  7. Only scheduled sources could be cropped: the host remembers the last known size per source; the program scene's sources are all scheduled (decision 2).
  8. Meaning of the crop when the picture changes size: the box stays and the picture rescales (decision 1).
- Optional suggestions taken: separate counter for crop-skipped layers, `EditKind::Crop`, a Linux toggle as follow-up.
- Owner: Salih chose this scope (numeric crop, Alt-drag, Region presets, undo, `item.set_crop`) on 2026-11-06; the hot-path parts still need his P15 sign-off with the measurements in the PR.
