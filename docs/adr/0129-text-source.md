# ADR-0129: A text source drawn on the CPU once per change, through a text rendering port, updated in place

- Status: Proposed
- Date: 2026-11-16
- Deciders: Salih (owner)
- Builds on: [ADR-0013](0013-zero-cost-when-unused.md), [ADR-0039](0039-scene-source-types.md), [ADR-0053](0053-item-style-and-video-filter-chains.md), [ADR-0062](0062-built-in-video-filters-and-properties-tabs.md), [ADR-0074](0074-still-image-and-colour-sources.md), [ADR-0077](0077-source-health-retarget-and-settings-window.md), [ADR-0107](0107-temporal-sources-sleep-when-hidden.md)

## Context

There is no text source. Titles, names, lower thirds, a "be right back" line, a now-playing line written by another program and a ticker all need a picture made elsewhere today. OBS has a text source on every platform, so this is a parity gap the owner ranked fourth (OBS gap list, 2026-11-16).

Forces:

- **P1 and P2.** Laying out and drawing text needs a shaping library and the system's fonts. Enumerating system fonts calls OS APIs (CoreText, DirectWrite, fontconfig). The engine crates (`rbs-sources`, `rbs-nodes`, `rbs-scene`) must not call them directly, and a different renderer (for example DirectWrite, for exact Windows metrics) must be possible later without a scene change.
- **P6 and P13.** Text that does not change must cost nothing per frame, like a still (ADR-0074). Text that changes (typing in Properties, a file another program rewrites, a control API call) must not rebuild the node: a rebuild is break-before-make and shows a gap on air. A changed picture must not be drawn or allocated in `Node::process`.
- **The binder rebuilds a source on every settings edit** (`rbs-scene` binder, "Sources whose settings changed get a new node"). Filters already have the other path: `changed_filters` keeps the node and the host sends new values (ADR-0062 decision 3). Sources have no such path.
- **Owner rules.** The default font is the system font (owner, 2026-11-16). UI uses theme tokens only, stays compact, and a visual change is approved with dark and light screenshots before merge. Beyond-OBS ideas are welcome in every area (vision, rule 17).
- **Licences.** GPL-3.0-or-later. `parley`, `fontique`, `harfrust`, `swash` and `skrifa` are MIT or Apache-2.0 and already in `Cargo.lock` through Slint 1.18, at the versions this ADR uses.

## Decision

1. **Scene type `org.rbs.source.text`, node type `org.rbs.text`.** One local text block per source. The node is kind `Source`, one "video" output, render partition, like `org.rbs.still`. `SourceInstance` gains `Text { node, stats, updater }` and `SourceClass` gains `Text`. A text source is not temporal (ADR-0107 decision 7 is unchanged): it stays scheduled while placed, as a still does.

2. **Drawing text is a port.** A new port crate `rbs-text` owns the contract: `TextStyle` and `TextBlock` (the settings below, typed and validated), `TextPicture` (straight-alpha sRGB `Rgba8`, width, height, and the logical text bounds), `TextError`, and the trait

   ```rust
   pub trait TextRenderer: Send + Sync {
       fn id(&self) -> &str;
       /// Lays out and draws `block`. Control side only: may block, read fonts and
       /// allocate. Never called from `Node::process`.
       fn render(&self, block: &TextBlock) -> Result<TextPicture, TextError>;
       /// Font families installed on this computer, for pickers. May be slow.
       fn families(&self) -> Result<Vec<String>, TextError>;
   }
   ```

   `rbs-text` depends on `rbs-core` only. The first adapter, `rbs-text-parley`, uses `parley` (layout, line breaking, bidi, font fallback through `fontique` and the system font list) and `swash` (glyph outlines and colour glyphs). `rbs-text-test` is a deterministic fake that draws each glyph as a box from a fixed metric, for conformance and binder tests without fonts. `rbs-sources` and `rbs-nodes` may depend on `rbs-text`, never on an adapter. The composition roots register a renderer (`SourceTypes::register_text`). `org.rbs.source.text` is installed only when a renderer is registered, the same rule as a media decoder. The document never names the renderer.

3. **Settings** (the scene sources spec holds the table). All are optional:
   - Content: `text` (Text, `\n` breaks a line), `file` (Text, a local UTF-8 or UTF-16 file with a BOM; wins over `text`), `file_lines` (Int ≥ 0, 0 = whole file, N = the last N lines, for chat logs).
   - Font: `font` (Text family name; absent is the system UI font), `size` (Float px, 1 to 1024, default 72), `weight` (Int 100 to 900, default 400), `italic` (Bool).
   - Fill: `color` (default `#ffffff`), optional `color2` and `angle` for a vertical-to-any-angle linear gradient, as the colour source.
   - Box and alignment: `width` and `height` (Int px; absent or 0 = fit the text), `align` (`left`, `center`, `right`; default `left`), `valign` (`top`, `middle`, `bottom`; used when `height` is set), `wrap` (Bool, default true when `width` is set), `overflow` (`clip`, `fit` (shrink the font until it fits the box, down to 25 %), `ellipsis`), `line_spacing` (Float multiplier, 0.5 to 3, default 1), `letter_spacing` (Float px).
   - Outline: `outline_width` (Float px, 0 = none, at most 64), `outline_color` (default `#000000`).
   - Shadow: `shadow_color` (absent = none), `shadow_x`, `shadow_y` (Float px), `shadow_blur` (Float px, 0 to 64).
   - Background: `background` (a colour; absent = transparent) and `padding` (Float px). Rounded corners and a border come from the item's Shape and border (ADR-0053), not from new settings.

   A setting outside its range is `InvalidSetting` from the app and the API, and is clamped with a `SourceSettingsAdjusted` note when read from a document (ADR-0062 decision 8). A family that is not installed falls back to the system UI font and the source reports `FontMissing { family }` as a note, never as a failure. Settings use the value kinds that documents already store, so the scene format does not change.

4. **One picture per change, drawn on the control side.** The renderer runs on the host's control thread for an edit, and on the text updater (decision 6) for a file change. The picture is capped like a still: a side over 8192 px or more than 128 MiB of pixels is refused with `InvalidSetting` on the setting that grew it (`size`, `width`, `height`, `file`), and an empty text is a 1 x 1 transparent picture so the item keeps its place. Outline and shadow are drawn into the same picture (stroke of the glyph outlines, then a blurred, offset copy of the alpha), so the compositor sees one plain picture and filters, crop and Shape and border apply to it unchanged.

5. **Settings edits update the node in place.** A node type may declare its settings live (`NodeTypeTable` entry flag `live_settings`, default false; only `org.rbs.text` sets it in this ADR). For such a source the binder keeps the node when settings change and reports it in a new `BindReport::changed_sources`, as `changed_filters` does. The host renders the new picture and hands it to the node through the instance's `TextUpdater`: a single-slot, lock-free mailbox. `process` takes the newest picture with one atomic swap, uploads it with the existing `SourceUpload` into a texture it already holds when the size is unchanged (one `write_texture`, no allocation), and publishes it with a new generation (ADR-0117 decision 4). A size change acquires one texture of the new size from the pool, the same one-time allocation a filter makes on a size change (ADR-0062 decision 5). The replaced pixel buffer is returned to the updater through a second slot and dropped there, never on the render thread. With no new picture, `process` does one relaxed load and returns. `warm` uploads the last picture, `cool` and `release` give the texture back; the node keeps the CPU pixels of its latest picture, as a still does (ADR-0074 decision 10). An edit is still one undo step and still saved; only the rebuild goes away.

6. **A file follows its changes only while the node is warm.** A text source with `file` set registers with one shared text updater in `rbs-sources` when its node warms and leaves it when it cools. The updater is a thread that exists only while at least one source is registered (P13). Every 500 ms it checks the registered files' size and modification time (no content read when both are unchanged), reads a changed file with a 1 MiB limit (larger is `InvalidSetting` on `file`, shown as the source's problem), renders, and posts the picture. A file that is replaced by rename, deleted, or briefly missing keeps the last picture and is read again when it returns; a missing file at first bind shows the source as Unavailable with "File not found", as an image does. Reading the file follows the local-file rule of ADR-0108 decision 9 (canonical local regular file only). A `file` path is the same accepted permission gap as an image `path` (ADR-0074 decision 9).

7. **Control API.** `SourceKind::Text`; `source.add` with kind `text` and a `text` or `file`; the existing source settings commands read and write the keys above with strict checks; `source.set_text {source, text}` replaces only `text` (one undo step, no rebuild), for a Stream Deck button or a script that changes a name or a score; `text.fonts` lists `families()`. The numbers go to the next free protocol minor at merge.

8. **Scrolling is a generic, animated video filter, not part of the text node.** `org.rbs.filter.scroll` (`speed_x`, `speed_y` in px/s, may be negative; `gap` in px between repeats) moves the source picture and wraps it, so a ticker is a text with a fixed `width` and a scroll filter, and an image or a browser page can scroll too. `FilterType` gains `fn animated(&self) -> bool { false }` (a default method, as the spec allows). For an animated type the filter node writes the master-clock time since `warm`, in seconds, into `u[15].z` (unused today) and does not take the unchanged-input shortcut (step 2a), so it draws every tick while it is in the schedule and nothing when it is removed or disabled (P13). The speed is in source pixels per second of master time, so it does not depend on the frame rate. This change is in a hot path: it needs `hot-path-review`, a before and after measurement, and the owner's sign-off (P15) before it merges, separately from the text source.

9. **Phases.** Each phase is its own pull request: (a) the port, the fake and the parley adapter with conformance tests; (b) the node, the type, `live_settings` and `changed_sources` in the binder, and the file updater; (c) the control API; (d) live kinds (decision 11); (e) crisp resizing (decision 12); (f) the desktop UI (decision 10), after the owner approves the proposal and the screenshots; (g) the scroll filter, after its measurement and sign-off. Phases (a) to (e) change nothing a user can see until (f) shows them.

10. **Desktop UI** (proposal; binding only after the owner's approval of screenshots). Add source lists Text, which adds "Text" in the system font, centred on the canvas, and opens Properties on a new first tab, Text. The tab holds a multi-line text field, then compact rows: Font (family picker using the picker overlay, with the system font first), Size, Bold and Italic, Colour, Alignment (`SegmentedControl` left, centre, right), and Read from file (a file row with a Lines number). Outline, Shadow and Background are collapsed groups whose header `Toggle` turns them on, so a plain text uses four rows. Scroll is one row with a speed and a direction that adds or edits the scroll filter, so the Filters tab shows the same filter. Every size and colour of the tab comes from tokens; help sits behind (i). The Source settings window (ADR-0077) of a text source starts with **Kind** (decision 11: Text, Clock, Countdown, Stopwatch, Recording time, Stream time) and the fields of that kind; Properties shows the kind's own fields under the text field (owner, 2026-11-16).

11. **Live kinds** (owner, 2026-11-16). The setting `kind` (Text, default `text`) chooses what the source shows: `text` (the `text` or `file` content), `clock` (local time and date), `countdown` (to a time of day, `target` `HH:mm[:ss]`, or for a duration, `duration_ms`, started by the user), `stopwatch` (counts up, started by the user), `recording_time` and `stream_time` (how long the main recording or stream has run; `end_text` or empty while stopped). `format` (Text) is a pattern with `HH`, `H`, `mm`, `ss`, `dd`, `MM`, `yyyy`, `ddd` (weekday name) and literal text in quotes; each kind has a default (`HH:mm:ss`, or `mm:ss` under an hour for timers). `countdown` has `end_text` (shown at zero, default `00:00`). Style settings (decision 3) apply to every kind. In `text`, `fields` (Bool, default false) turns on inline fields `{time}`, `{date}`, `{time:FORMAT}`, `{scene}` (the program scene of the main canvas), `{recording_time}` and `{stream_time}`; with `fields` off, braces are plain text, so no existing text changes meaning.
   - **Timing.** A live source registers with the text updater (decision 6) while its node is warm, and the updater wakes at the next second boundary of the soonest registered source, renders only the sources whose shown string changed, and sleeps when none is registered (P13). Wall-clock time (for `clock`) is read on the updater through an injected clock (`TextClock`, real or test), never in a node. Countdown, stopwatch and the elapsed times follow the master clock, so they do not drift from the recording. A font with proportional digits is drawn with tabular figures (`tnum`) when the font has them, so a clock does not shimmer.
   - **Running state is live state, not a document edit.** `countdown` and `stopwatch` are started, paused and reset with `text.timer {source, action: start | pause | reset}` (control API, global shortcuts, Stream Deck), and the run state is held by the host like volume (ADR-0059): not on the undo stack, saved with the collection only as "stopped at value" so a restart shows the last value, not a running timer. A countdown to a time of day needs no start.
   - A kind the build does not know is shown as its `text` with a `SourceSettingsAdjusted` note, so an older build keeps the document.

12. **Crisp at any size** (owner, 2026-11-16). When the user resizes a text item in the preview or with Transform, and the source is placed exactly once in the collection, the host scales the source's `size`, `width`, `height`, `outline_width`, `shadow_*` and `padding` by the item's scale instead and sets the item's scale back to 1, as one undo step, so the picture is drawn at the pixels it is shown at. A text source placed more than once (a global source, or one shared by two scenes) is scaled as a bitmap, as today, because its settings are shared; Properties says so in its (i) help. A drag that is still in progress is shown stretched and renders once on release, so a drag does not render on every pointer move. Control API transforms are unchanged (they scale the item); `source.set_text` and the settings commands are how a client changes the size.

## Beyond parity (rule 17)

OBS draws text with GDI+ on Windows and FreeType elsewhere, so features differ by platform (gradients, vertical text and the "chat log" mode only on Windows), complex scripts are not shaped on macOS and Linux, a scaled text item is a stretched bitmap, a countdown or a clock needs a script, and file reading polls the file every second even while the source is hidden in another scene.

This decision gives the same shaping, fallback fonts, colour emoji, gradient, outline, shadow and last-N-lines mode on every platform; edits and file changes update in place without a rebuild; a ticker is a filter that any source can use; a file is followed only while the source is in the schedule; and the control API can change a line in one call. A clock, a countdown, a stopwatch and the recording or stream time are built in (decision 11) where OBS needs a script, and they follow the master clock; text stays sharp when it is enlarged (decision 12) where OBS stretches a bitmap.

Left for a later decision: **lines to step through** (a list of lines with `source.text_next` and a hotkey, for lower-third names in an interview, with a fade between pictures).

## Consequences

- A plain title costs one render and one upload, then nothing per frame. Typing in Properties updates the picture on the next tick without a gap.
- New crates: `rbs-text` (port), `rbs-text-parley` (adapter) and `rbs-text-test` (fake). `cargo xtask check-layers` must list them in their layers. `rbs-text-parley` adds no crate that is not already in `Cargo.lock`; it compiles `parley` and `swash` a second time only if its features differ from Slint's.
- `rbs-scene` gains `live_settings` and `BindReport::changed_sources` (a contract change of the binder, reviewed with this ADR). `rbs-filters` gains `FilterType::animated` and the use of `u[15].z` (video-filters spec).
- Spec updates in the phase that implements each part: a new `text-port.md`; `scene-sources.md` (type, settings, `Text` instance, updater); `scene-model.md` (live settings); `video-filters.md` (animated filters, the scroll filter); `control-api.md`; `ui-design-system.md` and the glossary (Text tab) with phase (d); `hot-paths.md` for the text node's `process` and the animated filter.
- Tests: the port's conformance suite on the fake and the parley adapter (a known string gives a stable size, fallback for a missing family, refusals at the caps); the node on `TestClock`: no allocation in `process` at a steady size, one upload per change, the old buffer dropped off the render thread; the binder keeps a live-settings node and rebuilds others; the updater exists only while a file source is warm and keeps the last picture when the file is replaced.
- Negative: the CPU copy of the latest picture stays for the life of the node (as for a still). A large text at a large size is a large picture (a full-canvas text at 3840 x 2160 is about 33 MiB of CPU memory and a texture). Rendering on the control thread can take tens of milliseconds for a long text; moving it to a worker is a follow-up if measurements show a stall. Font fallback depends on what the computer has installed, so the same document can look different on another machine (the same as OBS).

## Alternatives considered

- **Render text with Slint's renderer.** It is a UI dependency (P1): the engine must not depend on a UI toolkit, and it would tie text in recordings to the UI's version.
- **Use `parley` directly in `rbs-sources`, as `png` is (ADR-0074).** Smaller, but it puts OS font enumeration into an engine crate and leaves no way to plug in a native renderer.
- **Draw glyphs on the GPU every frame (an atlas and a shader).** Sharper when scaled and cheap for animation, but it costs per frame for text that does not change, needs a new pipeline in the compositor, and outline and shadow become shader work. A picture per change wins for titles; the crisp-at-any-size step above gets most of the sharpness.
- **Rebuild the node on every edit, as other sources do.** Simple, but each keystroke and each file change would be a break-before-make gap on air.
- **Scroll inside the text node.** Works for text only; a generic animated filter serves images and pages with the same code.
- **Watch files with OS notifications (`notify`).** No polling, but a new dependency with platform back-ends and known misses on network drives; a 500 ms metadata check while warm is cheap and predictable.

## Review

Independent review per ADR-0014. Record: reviewer (human handle, or "independent AI session"), date, verdict (approve / changes requested), main concerns and how each was resolved.
