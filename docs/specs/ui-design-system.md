# Spec: UI design system

- Status: Draft
- Owner crate(s): `rbs-ui` (toolkit-neutral model: panels, layout, workspaces, layout undo, tokens, live indicator), `rbs-app` (Slint desktop shell; `ui/tokens.slint` is generated from the token file at build time, `ui/components.slint` is the component library)
- Related ADRs: [0008](../adr/0008-ui-as-api-client.md), [0013](../adr/0013-zero-cost-when-unused.md), [0015](../adr/0015-ui-design-rules.md), [0016](../adr/0016-native-desktop-ui.md), [0017](../adr/0017-flexible-panels-and-workspaces.md), [0022](../adr/0022-native-ui-toolkit.md)
- Rules: [UI principles](../ui/principles.md)

## Purpose

The single source of visual and interaction decisions for every RBS interface.

Binding now (once ADR-0015 is accepted): token names, the component list and contracts, the main window structure, the core flow interaction limits, and the list of non-undoable actions. Tunable: token values, which are initial and will be adjusted in Phase 2 usability testing.

## Tokens

Tokens are defined once in a data file ([`crates/rbs-ui/tokens.toml`](../../crates/rbs-ui/tokens.toml)) and generated into the native toolkit's format (ADR-0016), and exported as CSS variables only for remote clients outside the desktop app. UI code references tokens by name only (rule U6).

File layout (format 1): `[fixed]` holds the colours no theme may change (`color.state.live`, `color.text.on-live`); `[themes.dark]`, `[themes.light]` and `[themes.high-contrast]` each define exactly the theme colour names below, as opaque `#rrggbb`; `[length]` (logical pixels: the spacing, size, radius, line, menu, window and keyboard step tokens below), `[duration]` (milliseconds: `motion.fast`, `motion.normal`) and `[type]` (`{ size, line, weight }` for the five text styles) hold the rest. Token names are lowercase words joined by `.` or `-`. A custom theme is a complete token file; its `[fixed]` values must equal the built-in ones (`#c4262e` and `#ffffff`), otherwise it is refused. Elevation and easing tokens are added to the file together with the components that use them. `rbs_ui::tokens` parses the file and fails on a missing, unknown or invalid token, a changed fixed colour, and any failed contrast rule:

- text: `color.text.primary` and `.secondary` on every surface, `color.text.inverse` on `color.accent`, and `color.text.primary` on `color.accent.subtle`: at least 4.5:1 (7:1 in `high-contrast`); `color.text.on-live` on `color.state.live`: at least 4.5:1 in every theme, because both are fixed;
- non-text: `color.border.focus` on `surface.base` and `.raised`, `color.border.strong`, `color.state.*` and `color.action.destructive` on `surface.raised`, `color.meter.*` on `surface.sunken`, and `color.state.live` on `surface.base`: at least 3:1;
- `color.state.error`, `color.action.destructive` and `color.meter.clip` differ in hue from `color.state.live` by at least 45 degrees (rule U12).

### Colour (semantic names, per theme)

| Token | Use |
| --- | --- |
| `color.surface.base` / `.raised` / `.overlay` / `.sunken` | Window background, panels, popovers, wells |
| `color.text.primary` / `.secondary` / `.disabled` / `.inverse` | Text |
| `color.border.subtle` / `.strong` / `.focus` | Dividers, inputs, focus ring |
| `color.accent` / `.accent.hover` / `.accent.subtle` | Primary actions, selection |
| `color.state.live` / `color.text.on-live` | Live and recording only (rule U12), and the text on it; fixed, themes cannot change them |
| `color.state.warning` / `.healthy` | Status |
| `color.state.error` / `color.action.destructive` | Errors and destructive actions; hue clearly distinct from `color.state.live`, always with icon or text |
| `color.meter.low` / `.mid` / `.high` / `.clip` | Audio meters (paired with markers, never colour alone) |

Themes: `dark` (default when the OS is dark), `light`, `high-contrast`. Every pair of text and surface tokens must pass the U7 contrast check; CI verifies it from the token file.

### Typography

One family (initial choice: Inter, with the OS UI font as fallback). Numbers in meters and stats use its tabular figures.

| Token | Size / line height (px at 100 %) | Weight |
| --- | --- | --- |
| `type.caption` | 12 / 16 | 400 |
| `type.body` | 13 / 20 | 400 |
| `type.label` | 13 / 20 | 500 |
| `type.title` | 15 / 22 | 600 |
| `type.heading` | 20 / 28 | 600 |

### Space, size, shape, motion

| Group | Tokens |
| --- | --- |
| Spacing (4 px grid) | `space.1`=4, `space.2`=8, `space.3`=12, `space.4`=16, `space.6`=24, `space.8`=32 |
| Control height | `size.control.sm`=24 (dense Advanced/Expert views only), `.md`=28 (default), `.lg`=32 |
| Radius | `radius.sm`=4, `radius.md`=6, `radius.lg`=10 |
| Lines | `size.border`=1, `size.focus-ring`=2 |
| Icons | `size.icon.box`=16 (icon box), `size.icon.stroke`=1.5 (line width) |
| Menus | `size.menu.sm`=220, `size.menu.md`=300 (menu widths) |
| Windows | `size.window.w`=1440, `size.window.h`=900 (first-start main window), `size.window.min-w`=1280, `size.window.min-h`=720 (U11 minimum of the main window), `size.floating.w`=640, `size.floating.h`=420 (first size of a floating window) |
| Keyboard steps | `step.nudge`=8, `step.nudge-large`=128 (splitter move per arrow key, with Shift) |
| Elevation | `elevation.0` (flat), `elevation.1` (popovers), `elevation.2` (dialogs); subtle, no heavy shadows |
| Motion | `motion.fast`=120 ms, `motion.normal`=180 ms, `motion.easing.standard`; all motion disabled under "reduce motion" |

A token file cannot set `size.focus-ring` below 2 or `size.icon.box` / `size.icon.stroke` below 12 / 1 (U7) or `size.window.min-w` / `.min-h` below 1280 / 720 (U11); the loader rejects such a file.

## Components

The library is the only way to build UI (rule U6). Each component has: states (default, hover, pressed, focused, disabled, error), keyboard behaviour, accessible name and role, light/dark/high-contrast screenshots, and usage notes.

Initial set: Button (primary, secondary, ghost, danger, live: only to stop a live or recording output, in the fixed live colour), IconButton, Toggle, Checkbox, Radio, Select, Combobox, Slider (with numeric entry), NumberField (drag to change), TextField (editable, disabled, read-only for text the user copies, such as a token shown once: `copy-all()` puts its text on the clipboard, or password for a secret: what is typed is never shown and leaving the field does not confirm it), ColorPicker, Tabs, SegmentedControl, List and TreeList (drag reorder, inline rename), ContextMenu, Popover, Dialog, Toast, Tooltip, PropertyGrid (generated from a settings schema), EmptyState, StatusBadge (live, recording, warning), AudioMeter, AudioStrip (meter, fader, mute, monitor, filters), PreviewCanvas (handles, snapping, guides), SceneCard (thumbnail, name, live badge), TransitionPicker (preview thumbnail), CommandPalette, KeyboardShortcutHint.

### Icons

One icon set, drawn by the library's `Icon` component as vector paths in a 16 x 16 view box, sized by `size.icon.box` and stroked with `size.icon.stroke`. Icons are never font glyphs: a glyph missing from the OS font shows as an empty box (seen on Windows with U+22EF and U+25BE). Initial set: `more`, `chevron-down`, `chevron-up`, `check`, `close`, `dot` (filled, live state), `ring` (outline, not live). An icon is decorative (no accessible role): the control that shows it carries the name, and state is never shown by the icon alone (U7).

- `Button` takes an optional trailing `icon`; its ink follows the button's text colour, including disabled.
- `NumberField` takes a `label` (its accessible name; role spinbox with the value, increment and decrement actions), a `value`, a `step` and an `edited(float)` callback. Return or leaving the field commits text that differs from what it showed; the field then shows `value` again, so a refused edit never stays in it. Text that is not a number goes back to `value`. Up and Down step from what is typed (Shift: ten steps) and commit at once. States: normal, focused (focus border), disabled (no background, disabled text, no stepping). It shows whole numbers. Drag to change is not there yet.
- `IconButton` is the icon-only button (ghost, dense, square). Its `name` must be set and becomes the accessible name (Slint cannot enforce required properties; an accessibility-tree check is a follow-up).

### AudioMeter

The meter of rule U16, used alone and inside AudioStrip. Logic in `rbs_ui::meter` (toolkit-neutral, tested without a window).

- **Scale:** dBFS from -60 (floor; lower levels show an empty bar) to 0, linear in dB. Labelled marks at 0, -6, -12, -18, -24, -36 and -48 dBFS, drawn as static text and ticks.
- **Zones:** the lit bar is `color.meter.low` below -18 dBFS, `color.meter.mid` from -18 to -6 and `color.meter.high` above -6. The zone marks are on the labelled scale, so a zone is never shown by colour alone (U7).
- **Peak hold:** a 2 px line at the highest level of the last 1.5 s, then falling at 20 dB/s; never below the bar. It takes the colour of its zone.
- **Clip indicator:** a box at the end of the bar, outlined while not clipped and filled with `color.meter.clip` once a level reaches 0 dBFS. It stays lit until the operator resets it (click, or the strip's reset command); filled versus outlined is the non-colour cue.
- **Orientation:** vertical (grows upward) or horizontal (grows rightward).
- **Rates:** levels update at 30 Hz while the meter is visible and its audio is active, not at all otherwise (P13, `UpdateRate` of the panel).
- **Accessibility:** one element per meter with role progress indicator, the channel's name as its label and as its value the highest level since the last update in whole dBFS, updated at most once per second (screen readers do not need 30 updates per second). The level is rounded down, so it never reads higher than it was, and capped at +24 dBFS. Text: one Fluent message `meter-value` with variables `$db` (whole dBFS; absent below -60 dBFS) and `$clipped` (`yes` or `no`) and selectors on both, so each language orders the parts itself (English: "-12 dBFS", "silent", "-12 dBFS, clipped"). Messages are never concatenated. `rbs_ui::meter::Meter::take_report` gives the value, `AccessibleLevel::english` the English text until Fluent strings exist.
- **Rendering:** the toolkit draws the static parts (bar background, scale, labels, the clip indicator's outline) and owns the accessible element. The moving parts (lit zones, peak line, filled clip indicator) are drawn by a native compositor layer where one exists (macOS: `rbs_app::native::MeterBridgeLayer`, [ADR-0035](../adr/0035-meters-as-native-layers.md)), so meter updates do not make the toolkit redraw the window; elsewhere the toolkit draws them too. Both paths take their geometry from `rbs_ui::meter::MeterView::frames` and their colours from the theme's tokens. Toolkit content drawn over a meter bridge (menus, popovers, dialogs) hides the layer while it overlaps, as for the preview layer.

## Main window structure

```
+--------------------------------------------------------------------------+
| Title bar: scene collection, workspace, command palette, settings        |
+-----------+------------------------------------------+-------------------+
| Scenes    |  Preview  (| Program in studio mode)     | Properties of the |
| (cards)   |                                          | selected scene,   |
|-----------|                                          | source, filter or |
| Sources   |                                          | output            |
| in scene  |                                          |                   |
+-----------+------------------------------------------+-------------------+
| Mixer (audio strips)                     | Transition | Go live | Record  |
+--------------------------------------------------------------------------+
| Status bar: live/rec state + time, bitrate, dropped frames, CPU, GPU     |
+--------------------------------------------------------------------------+
```

## Studio panels (first version, ADR-0041)

The app runs the studio in its own process on a control thread (`rbs_app::studio_host`, ADR-0041 decision 7). The UI sends commands and receives updates; it never calls the studio. The studio starts with one canvas (1920 x 1080, 30 fps) and one scene on air; scene documents and the Scenes panel come later.

- **Program and Preview.** While studio mode does not exist yet, both panels show the scene on air. The picture goes to Program when that panel is open, otherwise to Preview, and the other panel says where the picture is. The picture is the studio's preview surfaces (1280 x 720, a ring of 3) shown as a native layer over the panel's content: a Core Animation layer on macOS (ADR-0034) and a DirectComposition visual on Windows (ADR-0036). It is letterboxed, and a new frame changes only the layer. The layer is hidden while an in-window popup is open (each popup counts itself in `Studio.overlays`), while a toast or the error bar shows, and while the panel is not in the main window. A panel that cannot show the picture says why in words: the studio is starting, the platform has no preview yet (Linux), the session cannot show the layer (for example remote desktop), the picture is in the main window, or the studio stopped. Accessible role: image, named after the panel. **Editing on the picture (ADR-0048):** a press on a source selects the topmost one under the pointer (outside every source clears the selection) and dragging moves it; the selected source shows a 1 px `color.accent` outline and eight 8 px handles (`color.accent` border, `surface.raised` inside; outline only when the box is smaller than three handles on screen). Corner handles resize keeping the aspect ratio, Shift (held at the press) resizes freely; edge handles change one side; a drag never makes a box smaller than 8 canvas pixels or turns it inside out. Moved edges and the box's centre snap to the canvas's edges and centre within 8 logical pixels unless Ctrl (Cmd on macOS) is held at the press. With the panel focused, arrow keys move the selected source by 1 canvas pixel (Shift: 10) and Escape clears the selection. The marks move with the pointer at once; the studio gets at most one new box at a time (the newest per source waits, each box is numbered and the studio answers every one, even an unchanged or refused box), so the picture follows within a round trip and a refused box puts the marks back where the studio has the source. With a preview layer the layer draws the marks (they show over the live picture); where no layer shows the picture the toolkit draws them over the panel.
- **Sources.** The panel lists the scene's sources, top first. Each row has the name, the type ("Screen capture", "Window capture", "Camera", "Audio input", "System audio", "Application audio") and the state in words: "Running"; "Not shown, closed" for a video source with no picture or recording to feed, which is not opened (P13); or "Not running: <reason>". Each row has a Delete source button. The list order is the drawing order: a source is drawn over every source below it. The selected row of a video source also has "Move <name> up" and "Move <name> down" (IconButtons with `chevron-up` and `chevron-down`; only on the selected row, so names and states keep their room). A move goes over or under the next source with a picture, skipping sound-only sources, which draw nothing; the button is disabled when there is none that way. Sound-only sources have no move buttons. Clicking a row, or Return or Space on a focused row, selects it: accent-subtle background and an accent bar at its start (not colour alone, U7), and the accessible selected state. One selection is shared by every window; the source an Add source adds is selected, and a deleted one is no longer. The empty state offers Add source.
- **Add source (core flow, 3 interactions).** Add source, then a type (only types this system's adapters provide), then what it captures (the OS's displays, windows, cameras or endpoints, with the default marked). The source is added on top: a screen or window is as large as fits on the canvas with its own aspect ratio, centred (a 21:9 display gets bars above and below on a 16:9 canvas), at the canvas rate; a camera asks for 1280 x 720 at 30 fps and sits 4/15 of the canvas wide in the bottom-right corner; audio is placed in the scene only so that it plays. A name the collection already uses gets " 2", " 3" and so on. The picker has no live preview yet; the new source is selected, with handles on the picture.
- **Properties.** Shows the selected source: its name and, for a video source, X and Y (the box's top-left corner) and Width and Height, as NumberFields in canvas pixels, with a hint saying so. A value applies when confirmed with Return or by leaving the field; Up and Down arrows change it by 1 (Shift: 10) at once. Text that is not a number goes back to the current value; a size of 0 is refused with an error. The fields change the layout the canvas shows: the item's own layout for this canvas if it has one, else its default layout. Rotation and crop stay as they are and get fields later. A refused change is said in plain words in the error bar. A sound-only source says it has no place on the canvas. Nothing selected: an empty state that points to the Sources panel. The same box can be moved and resized with handles on the picture (Program and Preview).
- **Delete source** removes the source from every scene. A toast "<name> deleted." with Undo shows for the toast time; Undo puts it back (it undoes the delete only while it is the newest edit; any other edit hides the toast). The focus moves to the list, so Ctrl+Z (Cmd+Z) right after a delete works too.
- **Undo and Redo of scene edits (U4).** Adding a source, deleting one, moving it a layer up or down and placing its box (picture panel or Properties) are undoable, in one history shared with control API clients (`edit.undo`, `edit.redo`). One user action is one step: all the boxes of one drag on the picture panel are one step, so Undo puts the source back where the drag started; each arrow-key nudge and each confirmed Properties value is a step of its own. A new edit clears what is left to redo. At least 100 steps are kept; a canvas change clears the history (old boxes would come back at the old size). The Sources panel has Undo and Redo (ghost Buttons beside Add source, disabled when there is nothing to apply); their accessible names say what they apply to ("Undo moving or resizing Camera", "Redo deleting Display", "Undo the layer change of …", "Undo adding …"). In the main window and every floating window, Ctrl+Z (Cmd+Z) undoes and Ctrl+Shift+Z (Cmd+Shift+Z) or Ctrl+Y redoes, from any control that does not take the keys itself (text fields keep their own undo); a click where no control takes the focus gives it to the window, so the keys work there too. With nothing to apply a key does nothing. This history is separate from the layout history (U21: "Undo layout change" in the workspace menu).
- **Mixer.** One horizontal AudioMeter per playing audio source, in list order, with the labelled scale under the bars and a clip box that resets on click or activation. Meters update at 30 Hz only while the Mixer is the active tab of a shown stack and has meters; otherwise the timer is stopped (U5, U21). Mute, volume and monitoring (U16) come with the strip controls. Windows and Linux draw the meters in Slint; the macOS meter bridge is not used by the panel yet.
- **Live controls (Record, Go live).** Two buttons: Record (Stop recording while a recording runs) and Go live (End stream while a stream runs), each in the fixed live colour while its output runs (U12). The text under them says the state in words: starting, "Recording to <file>", stopping, the last outcome ("Saved <path>", with dropped frames if any) or why recording is not possible (no video encoder on this system, the studio stopped); for the stream: "Connecting to <server>…", "Live on <server>", "Not connected to <server>, trying again (attempt n)", "Ending the stream…", the last outcome ("The stream ended after <duration>", with dropped frames, or why it stopped) or why it cannot start (no server or stream key yet, with a "Stream settings" button). Until several outputs run per canvas, one output runs at a time: Go live is disabled with "Stop the recording to go live." while recording, and Record with "End the stream to record." while live. Stopping a recording needs no confirmation while nothing is live (U4: confirmations are for actions while live). End stream always asks first, in place under the button ("End the stream? Viewers see it end." with "Yes, end the stream" in the live colour and Keep streaming; End stream itself is disabled while it asks); it cannot be undone. The confirmation box is a group named "End the stream?" and Keep streaming has the focus. While recording, the live indicator shows "Recording", the duration (1 Hz timer only while recording or live) and the file name as the output; while live, "Live", the time since going live and the server's host name, followed by "not connected" while the connection is down. Closing the app stops the recording and closes the file first. While a stream runs, closing the main window asks first in the window frame ("End the stream and quit?" with End stream and quit in the live colour, and Keep streaming, which has the focus); the window stays open until it is answered. The Outputs line says "live now" only while the stream reaches the server ("connecting" before); the live indicator shows Live from Go live on, with "not connected" until then.
- **Outputs.** One line per output saying what it will do: for the recording, the format, encoder, bitrates and folder ("MKV · best encoder · video 12000 kbit/s · audio 160 kbit/s · C:\Users\me\Videos") (the video part names the rate control: "video 12000 kbit/s padded", "video VBR 12000 kbit/s, up to 18000", "video quality 23"); for the stream, the protocol, server, encoder and bitrates ("RTMP · rtmp://live.example.com/app · best encoder · video 6000 kbit/s · audio 160 kbit/s", or "No server yet"), with "live now" in its title while it runs; and a "Recording settings" and a "Stream settings" button that open the Settings window there. Settings are not edited in the panel (U17): a docked panel is often too small for a form.
- **Settings window, Video section (U17).** The canvas the studio runs. Canvas size: 1280 x 720, 1920 x 1080, 2560 x 1440, 3840 x 2160, 1080 x 1920 (vertical) and the main display's own size, marked "(main display)" (even sides, at most 7680); a saved size that is none of these is listed too. Frame rate: 24, 25, 30, 50 or 60 (a segmented control). A change saves at once and restarts the studio with the new canvas (ADR-0041: a running studio's canvas does not change); the program scene's items are placed again: a picture that filled the canvas's width or height, centred, is fitted to the new canvas with its own aspect ratio, anything else keeps its size relative to the canvas and its margins to the nearest edges. The section says that the picture stops for a moment, and while the studio restarts the controls are disabled and say so. Not while recording: the controls are disabled with "Stop the recording to change the canvas." The preview keeps the canvas's aspect ratio (its longer side 1280 pixels). Kept in `video.toml` in the config directory (`format_version = 1`, `width`, `height`, `fps`); an unreadable or invalid file gives 1920 x 1080 at 30 and a note in the section. Screenshot: `docs/ui/screenshots/settings-video-dark.png`.
- **Settings window, Recording section (U17).** A separate window with the list of sections on the left (Video, Recording, Stream and Remote control so far; the search field comes when there are more) and the section's settings on the right. Recording: Folder (a full path, confirmed with Return or by leaving the field; default: the user's Videos folder, Movies on macOS), Format (MKV or MP4, a segmented control), Video encoder ("Automatic (best available)" or one of the encoders this system has: only the picked one is tried, and the recording does not start if it cannot open), the video's size setting for the rate control (Video bitrate for CBR, Target bitrate for VBR: presets 2500 to 80000 kbit/s; Quality level for constant quality: 14 to 36, default 23, lower is better) and Audio bitrate (AAC 96, 128, 160 or 192 kbit/s, the rates Media Foundation AAC accepts). A "Show advanced settings" button opens, in the same place (U2), every other setting that changes the recording's quality, each with its default and a one-line explanation: Rate control (CBR, the default; VBR; Constant quality), Maximum bitrate (VBR only; "Automatic", 1.5 times the target, or a preset), "Pad to the full bitrate" (a Checkbox, CBR only, off by default; disabled with a note when the encoder does not declare `cbr_padded`), Keyframe interval (1, 2, 3, 4, 5 or 10 s; default 2 s), Encoder preset (Quality, Balanced, Speed; default Balanced), Profile ("Automatic" or one the encoder declares) and B-frames (0 to the encoder's maximum; disabled with a note when it makes none). The choices come from the picked encoder's capabilities, or from all encoders' for "Automatic"; a saved value the encoder lacks is still shown so it is never changed silently. Changes apply to the next recording and are saved at once; while a recording runs they are disabled and say so. Each recording gets a new file "RBS <local date> <time>.<ext>"; an existing file is never replaced. The settings are kept in `recording.toml` in the config directory (`format_version = 1`, `folder`, `format` = `"mkv"` or `"mp4"`, optional `encoder` adapter id, `rate_control` = `"cbr"`, `"vbr"` or `"constant_quality"`, `video_kbps`, optional `max_video_kbps`, `quality`, `keyframe_seconds`, optional `profile`, `preset` = `"quality"`, `"balanced"` or `"speed"`, `b_frames`, `cbr_padding`, `audio_kbps`; fields a file from an earlier build lacks take their defaults); an unreadable or invalid file gives the defaults and a note in the section. This is the first, local form of an output profile; profiles with streaming outputs replace it later.
- **Settings window, Stream section (U17).** Where Go live sends the program, and how it is encoded. Server (a TextField with the full URL, `rtmp://host[:port]/app` or `srt://host:port`, confirmed with Return, by leaving the field, by closing Settings or by pressing Go live, so a server typed and not confirmed is never lost; other schemes, `rtmps` included, are refused with a note until a transport has them, and so is anything after `?`, such as `?streamid=` or `?passphrase=`, since a secret would then be in the URL: it goes in the Stream key field; a hint says to copy it from the streaming service). Stream key (a password TextField: what is typed is never shown, confirmed with Return or "Save key"; with SRT it is the optional stream id). The key is never written to a settings file, log or URL: it is kept in the operating system's credential store (the macOS Keychain, the Windows Credential Manager) under the service `RBS` and the account `stream key`, and the hint says so; where RBS has no credential store yet (Linux today) it is kept in memory until RBS closes and the hint says that instead. Under the field: "A stream key is saved." or "No stream key yet.", and "Remove key" (asks nothing; it can be typed again). The store is read only when a stream starts and written only when a key is saved or removed, on the studio's control thread (an OS prompt to allow access never blocks the window); nothing reads it at start-up. "Reconnect when the connection drops" (a Checkbox, on by default: up to 10 tries, 1 s doubling to 30 s apart, as `rbs-cli stream`; a server that refuses the key is not retried). SRT latency (shown only for an `srt://` server: Default (120 ms), 200, 500, 1000 or 2000 ms). Then the same encoder fields as the Recording section, with their own values and the stream's defaults (CBR 6000 kbit/s, keyframe every 2 s, which most services require, balanced preset, audio 160 kbit/s); constant quality is not offered, since services need a bitrate. Changes apply to the next stream and are saved at once; while a stream runs they are disabled and say so. Kept in `stream.toml` in the config directory (`format_version = 1`, `server`, `key_saved`, `reconnect`, optional `srt_latency_ms`, and an `[encoding]` table with the Recording section's encoder fields and names); an unreadable or invalid file gives the defaults and a note in the section. Starting a stream without a server or saved key does not open anything and says what is missing; the note then follows the settings (it names what is still missing and goes once nothing is). Screenshots: `docs/ui/screenshots/settings-stream-{dark,light,high-contrast}.png`.
- **Settings window, Remote control section (U17, ADR-0044).** Lets other apps on this computer (Stream Deck, panels, scripts) control RBS through the control API. "Allow remote control" (a Checkbox, off by default; while off nothing of the API exists). A status line under it: where apps connect (`ws://localhost:<port>`, "Other computers cannot connect"), or in the error colour why it is not running ("another app is using port 4456" with what to do, or that the studio did not start). Port (a TextField, 1024 to 65535, default 4456; confirmed with Return or by leaving the field; a refused value is put back with a note; a change disconnects connected apps, said in the hint). Access tokens: Name (TextField) and Access (SegmentedControl: View only, Control (the default, for panels), Full control, each explained in one line), and "Create access token". A new token is shown once in a bordered group with a read-only TextField, Copy (primary) and Done; it is gone after Done or when the Settings window closes. The list shows each token's name and access with "Revoke"; revoking disconnects the apps using it and shows "<name> was revoked" with Undo, which puts the same token back (until another token is made or the window closes). Name problems (empty, longer than 64 characters, taken) are shown under the button. Kept in `remote-control.toml` in the config directory (`format_version = 1`, `enabled`, `port`, `[[tokens]]` with `name`, `scopes`, `hash`: only SHA-256 hashes); on Unix the file is mode 0600, and a file other users can write is refused. An unreadable, invalid or refused file leaves remote control off, is moved to `remote-control.toml.refused`, and the section says why. Screenshots: `docs/ui/screenshots/settings-remote-control-{dark,light,high-contrast}.png`. **Network access (ADR-0046)**, under the port: "Allow access from other computers" (a Checkbox, off by default, disabled while remote control is off), with a hint to turn it on only on trusted networks and that a refused firewall prompt blocks other computers without notice. While on: a status line (where other computers connect, `wss://<name>.local:<port>` or the computer's address, or in the error colour why they cannot, such as a missing key, with "New key" as the fix); "Key pin" (`sha256//...`) and "Certificate fingerprint (SHA-256)", each a read-only TextField with Copy, the fingerprint's hint saying where the certificate came from and when it was just renewed; "Allow remote admin" (a Checkbox, off by default; a change restarts the server, said in the hint); "Certificate" (a SegmentedControl: Made by RBS, Your own; the hint says browser panels need the user's own certificate); for Your own, "Certificate file" and "Key file" (TextFields, full paths) and "Use these files", refused with a note when they cannot be read or do not match; for Made by RBS, "New key…", which asks first in a bordered group ("Make new key" primary, "Cancel"), since it cannot be undone. While other computers can connect, the frame bar shows "Reachable from other computers" in every workspace. Screenshots: `docs/ui/screenshots/settings-remote-control-network-{dark,light,high-contrast}.png`.
- **Errors.** The studio not starting or stopping (no usable GPU, a studio node failing for good) shows in the error bar and in the panels. A hardware GPU is preferred; without one the app uses a software adapter.
- **Checks without devices.** A build with the `demo-sources` feature and `RBS_DEMO_SOURCES=1` adds the reference adapters (synthetic display, camera, microphones, loopback) for screenshots and UI checks. `crates/rbs-app/tests/studio_host.rs` covers the host on those adapters: add, unique names, delete and undo, moving a source up and down (past a sound-only source), setting its box (and refusing a zero size), undo and redo of every edit kind with a drag as one step, and shutdown. `crates/rbs-app/tests/studio_record.rs` covers recording through the host with fake encoders: the picked encoder only, automatic fallback, refusals (a running recording, an existing file, an unknown encoder) and a closed file after stop. `crates/rbs-app/tests/studio_stream.rs` covers streaming through the host with a fake RTMP transport: the key read from the store reaches the transport only, a missing key refuses before anything opens (a missing or invalid server is refused by the UI, `stream_profile` unit tests), connection changes reach the UI, one output at a time, and a clean end.

## Panels and workspaces (contract for U21)

Workspaces are client-side UI state. The desktop UI crate owns the panel model and the workspace format; the engine and `rbs-core` know nothing about them (P1).

### Panel model

- A panel id is `<namespace>.<panel>` in lowercase ASCII letters, digits, `.`, `-` and `_`, at most 128 bytes, with optional parameters `?key=value&key=value` in the same characters. Each key appears once and parameters are stored sorted by key, so one panel has exactly one id.
- Every panel has a stable id (`rbs.scenes`, `rbs.sources`, `rbs.preview`, `rbs.program`, `rbs.properties`, `rbs.mixer`, `rbs.live-controls`, `rbs.transition`, `rbs.outputs`, `rbs.stats`, `rbs.status-bar`, plugin panels as `<plugin-id>.<panel>`), a title, an icon, a default dock position and an optional shortcut.
- Panels that can exist more than once take parameters in the id: `rbs.preview?canvas=vertical`, `rbs.program?canvas=horizontal`. Without a parameter they show the canvas being edited.
- A layout is a tree per window: splits (horizontal or vertical, with ratios) whose leaves are tab stacks of panels. Floating windows and projectors have their own tree or content plus their position.
- Each built-in panel declares how often it must redraw while visible: on change only (lists, properties, controls), at most N times per second (`rbs.mixer` 30 Hz, `rbs.stats` and `rbs.status-bar` 1 Hz), or at display rate (`rbs.preview`, `rbs.program`). The shell runs at most one redraw timer, at the fastest rate any visible panel needs, plus 1 Hz for the live indicator while live or recording, and no timer otherwise. Hidden tabs and closed panels never count.
- `rbs.program`, `rbs.live-controls` and `rbs.mixer` are guarded: closing them while live (directly or by closing their floating window) shows the undo toast of U21.
- The live indicator is not a panel and cannot be removed (U4, U21). The workspace menu and Panels menu are part of the window frame, not panels.

### Workspace file

Workspaces are stored as versioned TOML in the user's config directory, one file per workspace, so they can be shared and kept in version control. A JSON Schema for the format is published with the UI crate and checked in CI.

```toml
format = 1
name = "Minimal live"
locked = true

[[windows]]
kind = "main"
layout = { split = "vertical", ratios = [0.78, 0.22], children = [
  { tabs = ["rbs.program"] },
  { tabs = ["rbs.live-controls", "rbs.mixer"] },
] }

[[windows]]
kind = "floating"
layout = { tabs = ["rbs.stats"] }
placement = { display = "edid:DEL-41A3-7H2K9", connector = "DP-2", size = [2560, 1440], scale = 1.5, rect = [0, 0, 1280, 720] }

[[windows]]
kind = "projector"
content = "program?canvas=vertical"
placement = { display = "edid:SAM-7012-HX55", connector = "HDMI-1", size = [1920, 1080], scale = 1.0, fullscreen = true }
```

- `placement.display` is a stable fingerprint derived from the display's EDID (manufacturer, product code, serial). `connector`, `size` (physical pixels) and `scale` help when the fingerprint is ambiguous. `rect` is in logical pixels relative to that display.
- Matching order when a workspace is applied: (1) same fingerprint, (2) same connector with the same size, (3) any display with the same size and scale, (4) no match: the window docks into the main window and a projector is not opened. A rect that would fall outside the matched display is clamped to it.
- Export removes `placement` display details (fingerprint, connector) and keeps only size, scale and relative rect, so exported workspaces are portable and contain no hardware identifiers. Import then matches by step (3) or docks.
- `locked` blocks pointer moves, resizes and close buttons. Commands from the palette, shortcuts and the control API still apply.
- Unknown panel ids (for example from an uninstalled plugin) are kept in the file and shown as a placeholder that offers to install or remove them.
- The format is versioned; RBS reads older versions and migrates them. A file with a newer `format` is refused with a message, never rewritten.
- Validation: unknown keys are errors; there is exactly one `main` window and it is first; a panel id appears at most once in the whole workspace; a split has at least two children, one positive ratio per child, ratios summing to 1 (within 0.01, then normalised) and at most 16 levels of nesting; a tab stack may carry `active` (index of the visible tab, default 0); only the main window's root may be an empty stack (`{ tabs = [] }`, every panel closed); a floating window holds at least one panel; a projector has `content` and no `layout`; names have 1 to 64 characters, no control characters, and are unique ignoring case (Unicode lowercase, not locale-specific).
- Files are named after a slug of the workspace name and written atomically (temporary file, then rename). Files that fail to parse are listed as problems and left untouched. Built-in workspaces need no file; saving a changed built-in writes an override file, and restoring its default removes it.

### Built-in workspaces

| Name | Main window |
| --- | --- |
| Edit | Scenes over Sources on the left, Preview in the centre, Properties on the right; below: Mixer (tabs with Stats and Outputs), Transition, Live controls |
| Live | Preview and Program side by side over the Mixer; on the right Live controls (tab with Transition) over Stats (tab with Scenes) |
| Minimal | Program over Live controls |
| Audio | Program and Live controls over a large Mixer |

### Switching

- Client-scoped control API commands `ui.workspace.list` and `ui.workspace.switch(name)` ([control API](control-api.md#client-scoped-ui-commands)), available in the command palette and the workspace menu. Shortcuts are user-assigned; none is bound by default (U21).
- Optional `on_go_live` and `on_stream_end` workspace settings, stored in the UI settings, not in the scene collection or output profile.
- Switching only changes UI state; it never reconfigures sources or outputs.
- Layout changes and switches go into the UI's own layout undo history, not the engine command log. A history step is a whole-workspace snapshot; the moves of one splitter drag form one step; changes that leave the layout as it was record nothing; at least 100 steps are kept.
- Every layout change is a command (open, close, show tab, move to stack, move to edge, float, dock, close window, resize, lock, switch). Pointer and keyboard both issue these commands. When `locked` is set, close, move to stack, move to edge, float, dock, close window and resize are refused if they come from the pointer; open, show tab, lock or unlock and switch are always allowed, and every command from the palette, a shortcut or the control API applies.
- The result of every command is validated against the rules above; a command whose result would break one (for example splits nested deeper than 16 levels) is refused, so a workspace can always be saved and loaded again.
- A switch that leaves a guarded panel closed while live shows the same undo toast as closing it. An automatic switch (on go live or stream end) shows one toast naming the previous workspace with a one-click way back; that toast replaces the undo toast for any guarded panel the switch closed, because going back reopens it.
- Deleting a workspace asks for confirmation; deleting a built-in workspace is not possible, only restoring its default.

## Core flows (contract for U1)

Interactions are counted as defined in the UI principles. "Max" is the limit for the whole flow with default choices.

| Flow | Steps | Max |
| --- | --- | --- |
| Add source | "+" on the Sources panel (or drop a file or window onto the preview: 1 drag, done) -> pick a type from large icons with search, recent first -> confirm in the picker, which shows a live preview -> the source is selected on the preview with handles | 3 |
| Add and adjust a filter | Select the source on the preview or list -> "+" in the Filters section of Properties -> pick from a searchable list with previews -> adjust in place; before/after toggle | 3 + adjustments |
| Choose a transition | Transition control next to Go live shows the current one -> pick another (duration inline) | 2 |
| Create a scene | "+" on Scenes -> type a name (optional template) and confirm | 3 |
| Switch workspace | Workspace menu in the window frame, or a user-assigned shortcut | 2 (1 with a shortcut) |
| Go live | Go live button (first time only: pick service, sign in or paste key, automatic checks for bandwidth and encoder) | 1 after setup |

## Actions that cannot be undone

End stream, stop recording, delete a scene collection, delete a recording file, sign out of a service. "Remove from scene" (undoable) and "Delete source" (removes it from every scene; undoable) are distinct commands. Everything else that edits scenes, sources, filters, transitions, audio or settings is undoable.

## Keyboard

Docking by keyboard (U21): Tab moves focus through the frame (workspace menu, Panels menu), then each tab stack's tabs and their buttons, then the splitters. On a focused tab, Enter or Space shows it, the Menu key or Shift+F10 opens its panel menu (move to an edge, move to the next tab stack, float or dock back, close), and Delete closes it. A focused splitter moves by `step.nudge` per arrow key, `step.nudge-large` with Shift. Menus open with the first item focused; Tab and Shift+Tab move between items, Enter or Space chooses one and closes the menu, Escape closes it. Delete or Backspace on a focused tab closes it.

Every command is in the command palette (Ctrl/Cmd+K) with its shortcut. Defaults include: switch to scene 1-9 (Ctrl/Cmd+1..9), transition preview to program (Ctrl/Cmd+Enter; Space also works while the preview or program has focus), undo/redo, nudge source (arrows, Shift for 10 px), duplicate, delete, toggle studio mode. All shortcuts are rebindable; global hotkeys are opt-in.

## Requirements on the UI toolkit

The native toolkit chosen under ADR-0016 (Slint, iced or Qt through `cxx-qt`; ADR-0022 proposes Slint) must satisfy:

1. Renders the engine's shared GPU surface for preview and program without CPU copies.
2. Supports the tokens and components above, including custom drawing for meters and the preview canvas at display refresh rate.
3. Accessibility APIs on Windows, macOS and Linux (screen readers, keyboard focus).
4. Fluent-based i18n and right-to-left layout.
5. Idle CPU near zero when nothing changes on screen.
6. A docking system with tabs, splits and floating windows across monitors (U21), or the ability to build one.
7. A licence compatible with ADR-0010.
8. Rich visualisations (meters, equalizer curves, spectrum, histogram, scopes): light graphics with the toolkit's vector primitives; dense or per-pixel graphics drawn on the GPU and shown as textures. Graphics that move at the meter rate or faster are native compositor layers where the platform has a layer backend (ADR-0034 for the preview, ADR-0035 for meters), because the toolkit redraws the whole window for any change.

The comparison prototype and its measurements are in [`prototypes/ui-toolkit`](../../prototypes/ui-toolkit/README.md); the choice is recorded in [ADR-0022](../adr/0022-native-ui-toolkit.md).

## Open questions

- Final font choice and licence.
- JSON Schema for the workspace file (promised by ADR-0017): not delivered yet; the Rust parser in `rbs-ui` is the reference until it is.
- Expressiveness of the declarative plugin panel format (forms, lists, charts, live previews).
