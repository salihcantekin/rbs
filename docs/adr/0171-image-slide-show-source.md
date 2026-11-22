# ADR-0171: Image slide show source

- Status: Accepted
- Date: 2026-11-22
- Deciders: Salih Cantekin (owner)

## Context

People show a folder of pictures in turn: sponsors between segments, a photo wall, a "be right back" loop, slides exported as pictures. OBS has an Image Slide Show source for this. RBS has a one-picture image source (`org.rbs.source.image`, ADR-0074, PNG only) and a media file source that refuses slide shows (ADR-0108 decision 1). The OBS importer lists `slideshow` as unsupported.

Weak spots of the OBS slide show this design avoids:

- It creates an image source per file and loads them all, so memory grows with the folder (it warns past a memory limit), and changing the list reloads everything.
- Its output size is the largest picture unless a size is typed, so a mixed folder moves and rescales the item.

The owner asked for this source on 2026-11-22. Rules that apply: zero cost when unused (ADR-0013), extension-ready types and schema settings (ADR-0162 decision 8), source activity (ADR-0152), no per-frame CPU copy (P4, ADR-0038), hot paths (P15).

## Decision

1. **Scene type `org.rbs.source.slideshow`, node `org.rbs.slideshow`.** `SourceClass` gains `Slideshow` and `rbs_api::SourceKind` gains `slideshow` (the closed lists ADR-0162 decision 8 allows extending until the source registry exists). The node is a render-partition source with one video output, in `rbs-nodes`, listed in `node_types()`.

2. **Pictures come from one local folder.** Setting `folder` (text, local path, checked like every other path, ADR-0052). Regular files directly in the folder, not subfolders, whose extension is `png`, `jpg` or `jpeg` (case-insensitive). Hidden files (name starting with `.`) and symbolic links are skipped. Files are sorted by natural name order (`2.png` before `10.png`), case-insensitive, ties broken by the exact name. The cap of 1000 files applies after sorting; the rest are left out and the source shows a warning. A file list (pick single files) is a later addition with its own setting key, not part of this decision.

3. **JPEG decoding joins PNG.** `rbs-sources` decodes JPEG with `zune-jpeg` (pure Rust, MIT/Apache-2.0/Zlib, already in `Cargo.lock` through Slint), on the control side only, with the same limits as PNG (`still::MAX_SIDE`, `still::MAX_BYTES`). The EXIF orientation tag is applied when present. `zune-jpeg` gives the raw EXIF bytes only, so `rbs-sources` reads the one tag with a small bounded parser of its own (at most 64 KiB read, at most 256 entries, every offset checked); it takes untrusted input and gets a limit test and a property test over random bytes. The image source accepts JPEG through the same function in the same change.

4. **Settings are an `rbs-params` schema.** The type declares:

   | Key | Kind | Default |
   | --- | --- | --- |
   | `advance` | choice `auto` / `manual` | `auto` |
   | `interval_s` | number 1..3600 s, step 0.5 | 5 |
   | `order` | choice `sequential` / `shuffle` | `sequential` |
   | `loop` | toggle | true |
   | `transition` | choice `cut` / `fade` / `slide` / `swipe` | `fade` |
   | `transition_ms` | number 0..5000 ms, step 50 | 700 |
   | `fit` | choice `contain` / `cover` / `stretch` | `contain` |
   | `watch` | toggle (pick up added and removed files) | false |

   Properties draws these rows with the same field view the filters use. `folder` is not in the schema: it is a host-owned path row (like a media file's path), because rbs-params has no path kind and a path needs the host's local-path check. Every key except `folder`, `transition` and the size keys of decision 5 is a live key (ADR-0108 decision 12): changing it never replaces the node. `transition` is not live because the blend pass exists only when it is not `cut` (decision 7); changing it replaces the source, which starts again at the first picture. A transition never lasts longer than half the interval: when `transition_ms` is more than half of `interval_s`, the transition is shortened to that, so a change cannot start while one is still running. With `advance = manual` there is no interval and no cap. Live `fit`: the decode thread re-fits the pictures it holds and the current one is replaced at once with a cut. Live `order`: takes effect after the current picture (sequential to shuffle starts a new shuffled pass; shuffle to sequential continues from the current picture's sorted place).

5. **The output has a fixed size, stored in the settings.** The source stores `width` and `height` (the existing size keys, `types::keys::WIDTH` and `HEIGHT`). The host writes the main canvas's size into them when the source is added; a source without them uses `InstanceContext::canvas`, else `still::DEFAULT_SIZE` (1920 x 1080). They are ordinary bind settings: changing them replaces the node, and the binder needs no new rule. A later canvas resize does not change them; the item scales like any other source, and other canvases draw it scaled too. There is one node per source, not per canvas. So the item box never changes when the picture does. Each picture is fitted on the decode thread, once, when it is decoded: resampled with a bilinear filter in premultiplied linear light (no dark fringes at alpha edges or `contain` bars) and converted back to the node's `Rgba8` sRGB format. `contain` leaves transparent bars, `cover` crops the centre, `stretch` fills. A source setting for another output size is a later addition.

6. **At most three decoded pictures exist per source: previous, current, next.** A cold decode thread, shared by every slide show, starts when the first one turns Active and exits when the last one leaves Active (Standby has no threads, P13). It decodes the next picture ahead and keeps the previous one for Back. The fitted pixels are an `Arc` the thread's cache and the node share. The mailbox item is a `SystemFrame` the decode thread builds around that `Arc` (`Bytes::from_owner`). The node never drops one. The mailbox is the parameter channel (ADR-0023), whose receiver hands a displaced value back to the sender, so that item drops on the decode thread. The render thread never frees picture memory, whatever the eviction order. A test counts frees on the render thread (none). Nothing is decoded or held in Installed or Standby. When the source turns Active its output stays empty (transparent) until the current picture is decoded; that first picture appears with no transition. "On return, continue" keeps only the index across Standby, not pixels.

   The folder is listed when the source turns Active. With `watch`, the same thread re-lists it every 2 seconds while Active: one directory read compared by the list of names and, for each name, size and modification time (no per-file open; no file system watcher dependency). A new file joins the order (at its sorted place, or at the end of the current shuffle pass); a removed file leaves it. If the current picture's file is removed or changed, it stays on screen until the next change; if the next one is, the thread decodes the new next. Shuffle draws a new order per pass, never showing the same picture twice in a row.

7. **Drawing.** The node follows the text node's upload pattern (ADR-0129 decision 5, `rbs-nodes` `text.rs`), not the still node's warm-only upload:
   - In `warm` it reserves its picture textures with `ReservedUpload` at the output size: two (current and incoming). Only when `transition` is not `cut` it also reserves two transition output textures and a two-layer blend pass (pipeline and one uniform buffer). `cool` releases all of them. Nothing is created in `process`.
   - `ReservedUpload` textures are copy targets without `RENDER_ATTACHMENT`, so the transition textures need a new `rbs-gpu` type: `ReservedTargets`, a pair of pool textures in the node's output format (`Rgba8` sRGB, the same as the published picture, not the canvas's `Rgba16Float`) with render-attachment, sampled and copy-source usage with `free_target()`, which returns the texture no reader leases, or none. It is a new public item of `rbs-gpu`, an addition with no change to existing items; `rbs-gpu` is an internal crate, not part of the plugin contract.
   - Every write follows the lease rule of `ReservedUpload`: the node writes only into a texture no reader leases. A transition tick draws into the transition texture that is not leased; when both are leased it skips that tick's draw and the slot keeps the last frame.
   - A new picture arrives through a lock-free mailbox from the decode thread. `process` writes it into the free picture texture only when `upload_budget.try_take` grants its bytes for the tick, else it waits for a later tick; each upload gets a new `next_generation()` sequence (ADR-0117 decision 4).
   - Between changes the output slot keeps the frame and `process` does nothing.
   - During a transition the node draws the two picture textures into the transition output, each tick for the transition's length (opacity for `fade`, offset for `slide` and `swipe`), then publishes the incoming texture and returns to the steady state. `cut` and `transition_ms` 0 publish the incoming texture at once.
   - The node does not upload while a transition runs. A Next, Previous or timed change that comes during a transition waits until it ends (one waiting change; a newer one replaces it), so neither picture texture is overwritten while it is drawn.
   - Live keys go through the control handle (decision 8); `transition` is not live (decision 4).
   - Cost: Installed and Standby nothing; Active between changes one compare per tick and 2 textures of the output size (4 when `transition` is not `cut`); during a transition one draw of two layers per tick.
   This pass is new hot-path code: it is measured and signed off before merge (P15).

8. **A control handle connects the host, the node and the decode thread.** As for media (ADR-0108 decisions 10 and 12), `SourceInstance::Slideshow` carries a `SlideshowControl` handle; `BindReport::live_changed` and the `media.*` methods reach the source through it, never by re-warming the node.
   - The handle holds atomics only: `paused`, the live keys as numbers, a settings generation, and one pending command (`next`, `previous`, `restart`) with its own generation. The host writes them on the control side; the node reads them in `process` with no lock.
   - The node owns the timer. It compares the tick time with the next change time; on a change it takes the incoming picture from the mailbox and stores the shown index in an atomic.
   - The decode thread owns the order and the index. It never waits on the node: it sleeps with a timeout (half the interval, at most the 2-second watch period) and on wake refills the mailbox when the shown index moved. The control side unparks it after writing a command or a live key, so a Next is decoded at once. `SourceTypes` owns the shared thread behind a control-side lock (started with the first Active slide show, joined after the last); each handle holds an `Arc` of the thread's waker, set by that registry when the source turns Active, so the control side always wakes the current thread. The render thread makes no system call to wake it.
   - `advance = manual` never changes by itself. With `loop` off the last picture stays. The index survives Standby in the handle (decision 6).

9. **Control uses the media methods.** One protocol minor, the next free number at merge, holds all of this:
   - `rbs_api::SourceKind` gains `slideshow`. A client on an older minor sees it as the unknown kind, as for every `#[non_exhaustive]` kind addition.
   - `media.play` and `media.pause` start and stop the timer; with `advance = manual`, `play` only clears pause and nothing advances by itself. `media.restart` goes to the first picture of the order (a new shuffle pass). `media.seek` on a slide show is `invalid_params`.
   - `media.state` for a slide show: `cursor_ms` is the time the current picture has been shown, `duration_ms` is the interval (absent with `manual`), `ended` is true on the last picture with `loop` off, `cued` keeps its ADR-0108 meaning (a source seen only in Preview does not advance). `state` is `paused` when paused and otherwise `playing`, for `manual` too. New optional fields `index` (0-based) and `count` on `MediaPlayState`; older clients ignore unknown fields, as the `rbs-api` decoder note already says for added fields.
   - New methods `media.next` and `media.previous` (scope `control`, effect `edit`) move one picture with the source's transition and restart the interval. They accept only a slide show; any other source is `invalid_params`. A later ADR may widen them.
   - Shortcuts get Next and Previous per source beside the existing media ones.

10. **Activity.** Default policy (ADR-0152), declared as a `SLIDESHOW_ACTIVITY` constant and a `class_of` arm in `rbs-sources` `activity.rs`, read through `NodeTypes::activity` (ADR-0152 decision 5), since types without one default to `placed`: run when shown; on return, continue where it was. The type is not still (ADR-0117 decision 2) whatever its settings, `advance = manual` included, so a separate recording is always a video track. That is declared in `rbs_sources::is_still`, which returns false for `SourceClass::Slideshow` without looking at its settings.

11. **OBS import.** `slideshow` and `slideshow_v2` map to this type with the folder their files share. When the files do not share one folder, the first file's folder is used; when the OBS list is not every matching file of that folder, the folder is used anyway. Both cases put a warning in the import report.

## Consequences

- One new node, one closed-list entry per list, one schema, a decode thread that exists only while a slide show is Active, and a transition pass that runs only during a change. A folder of any size costs three pictures of memory.
- The image source gains JPEG.
- The media control methods become "playable source" methods. Their spec and the MCP tool list change in the same change.
- New tests: folder listing and natural order, shuffle without repeats, EXIF orientation, fit modes (pure functions); node steady state allocates nothing and the transition ends on the right tick (`TestClock`); live keys do not replace the node; API refusals.
- Specs updated in the same change: `scene-sources.md`, `io-nodes.md` (the node's process contract, stats and generation rule), `control-api.md`, the MCP tool list in `mcp-server.md`, `shortcuts.md`, `obs-import.md`, `hot-paths.md` (the transition pass).
- New tests also cover: the decode thread exits with the last Active slide show; the first picture after Active has no transition; a transition longer than half the interval is shortened; the EXIF parser's limits.

## Alternatives considered

- **Load every picture at start (OBS).** Simple, but memory grows with the folder and a change stalls the studio.
- **Make it a mode of the media file source.** ADR-0108 keeps media for one file that changes over time; a picture list has different settings and no decoder port.
- **Do the transition with two compositor layers (no pass in the node).** Needs a source to own two layers, which the scene model does not have; the pass in the node keeps the source one layer.
- **A file system watcher crate.** A 2-second metadata check while Active is enough and adds no dependency.
- **Output at the largest picture's size.** Moves and rescales the item when the folder changes.

## Review

- Round 1, independent AI session, 2026-11-22: changes requested. Twelve findings, all answered in this text:
  1. Upload in `process` was unbounded and freed memory on the render thread. Decision 7 now uses the text node's `ReservedUpload` and upload budget, and decision 6 adds shared `Arc` pixels.
  2. When the blend pass is created was not said. Decision 7: it is created in `warm` only when not `cut`, a live change reserves it off the render thread, and the costs are stated.
  3. With several canvases the output size was unclear. Decision 5: it is the main canvas's size, one node per source, and a size change replaces the node.
  4. What shows before the first decode, and when the thread stops, were not said. Decision 6 answers both.
  5. The watch check was undefined. Decision 6 states what is compared and what happens when the current or next file is removed.
  6. The fit filter and colour space were missing, and so was the EXIF parser. Decisions 5 and 3 add them.
  7. The meaning of `media.state` and `seek`, `next`/`previous` for media files, and the `SourceKind` addition were open. Decision 9 defines them.
  8. `io-nodes.md` and the MCP list were missing from Consequences. Both are added.
  9. A manual slide show is still a video track. Decision 10 says so.
  10. An OBS list that is only part of the folder was not handled. Decision 11 now warns.
  11. Hidden files, symbolic links, ties and when the cap applies were not said. Decision 2 answers all four.
  12. An interval could be shorter than the transition. Decision 4 shortens the transition.
- Round 2, independent AI session, 2026-11-22: changes requested. Seven findings, all answered:
  1. There was no route from the host to the node. Decision 8 adds the `SlideshowControl` handle and says who owns the timer, the order and the index, and how the decode thread is woken. A cut-to-fade change now replaces the node.
  2. Next during a transition could overwrite a texture being drawn. Decision 7 now holds a change until the transition ends.
  3. The canvas field had the wrong name and no `None` case, and the replace path was missing. Decision 5 fixes the name, gives the `None` size and states the new binder rule.
  4. Live `fit` and `order`, and the cap with `manual`, were not covered. Decision 4 now covers them.
  5. A free on the render thread depended on eviction order. Decision 6 adds a return queue and a no-free test.
  6. Where the activity default is declared was not said. Decision 10 names `NodeTypes::activity`.
  7. The `MediaPlayState` fields and the manual state were missing. Decision 9 adds both.
- Round 3, independent AI session, 2026-11-22: changes requested. Six findings, all answered:
  1. The binder cannot replace a node because of a key's value. `transition` is no longer a live key (decision 4).
  2. The binder never sees the canvas size. The size is now stored in the `width` and `height` settings (decision 5), so the binder needs no new rule.
  3. A single transition texture breaks the lease rule. Decision 7 now uses two in turn and skips a tick when both are leased.
  4. Where the activity default and "not still" are declared was not named. Decision 10 names `SLIDESHOW_ACTIVITY` and `is_still`.
  5. How the control side finds the current decode thread was not said. Decision 8 adds the registry and the waker.
  6. What the mailbox item is was not said. Decision 6 makes it a `SystemFrame` around the `Arc`, and that item goes back on the return queue.
- Owner: Salih approved the design and the proposed choices in the thread on 2026-11-22 (14:41 UTC): folder only first, JPEG for the image source too, measure the transition pass on the Mac before sign-off.
- Round 4, independent AI session, 2026-11-22: changes requested with one blocking finding. `ReservedUpload` textures cannot be drawn into. Decision 7 now names the new `rbs-gpu` `ReservedTargets` pair. The reviewer said round 4 approves once this is fixed, and found the other round-3 answers correct against the code.
- Round 5, independent AI session, 2026-11-22: approve. One wording fix (name the transition textures' format) applied in decision 7.
- Accepted on 2026-11-22: the reviews are recorded above, and the owner approved the design in the thread at 14:41 UTC. Every change after that approval answers a review finding. None of them changes what the user sees or the choices the owner made.
