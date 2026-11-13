# Glossary of user-facing terms

UI text uses these terms and no synonyms (rule U13). Code may use different internal names; the mapping is listed.

| Term in UI | Meaning | Not | Internal name |
| --- | --- | --- | --- |
| Scene | A layout of sources shown on a canvas | Layout, page | `Scene` |
| Source | Something that produces video or audio (screen capture, window capture, camera, image, text, media, browser, audio device), as placed in a scene with its position, size and crop. The panel listing them is "Sources" | Input, layer, element, item | Source node; placement is a scene item |
| Look | A preset of a source's shape, border and filters, copied onto the selected source in one undoable step. Later edits to the source do not change the look | Preset, style, theme | `Look` |
| Library | The panel that applies looks to the selected source and adds backgrounds, frames, a PNG file and shapes. It is not the folder of saved themes, workspaces and scene collections (ADR-0051), which the UI never calls a library | Assets, templates | `rbs.library` |
| Filter | An effect applied to a source or audio | Effect, plugin | Filter node |
| Transition | How the view changes from one scene to another | Effect | Transition node |
| Canvas | The pixel grid and frame rate a scene is drawn on, for example horizontal 1920x1080 or vertical 1080x1920. Scene items are placed in these pixels | Resolution, Output size | Canvas |
| Output size | The encoded picture size of one recording or one stream. "Same as canvas", when nothing is stored, encodes at that output's canvas. It is not the size of the preview panel | Resolution, Canvas | `output_size` on the output profile; absent means that output's canvas (ADR-0102) |
| Output | A destination: a streaming service, a recording, a virtual camera | Target | Output node |
| Go live / End stream | Start / stop streaming to the selected outputs | Start stream, Stop live | - |
| Record | Save the program to a file. "Capture" is used only in source type names (Screen capture, Window capture) | Save, Capture (as a verb for recording) | Recording output |
| Audio recording | A recording of the mix with no picture, an output of its own beside the recording; the verb is "Record audio" (ADR-0116) | Audio-only mode, sound recording, podcast mode | Audio-only output |
| Preview / Program | What you are preparing / what viewers see (studio mode) | - | - |
| Mixer | The audio panel with levels and controls | Audio panel | Audio graph UI |
| Automatic, Rows, Columns | Mixer layouts: follow the panel's shape, sources stacked top to bottom, or side by side | Horizontal, Vertical, Auto | Orientation (in UI text) |
| Workspace | A saved arrangement of panels, floating windows and projectors. It never holds scenes, sources or output settings | Profile, layout, scene collection | UI workspace file |
| Scene collection | A set of scenes and sources saved together. It holds no panel layout and no output settings | Project, workspace | Scene document |
| Global source | A source shown and heard in every scene of a canvas, kept in Settings › Global sources | Global audio device, shared source | Item of the canvas's global scene (ADR-0065) |
| Remove from scene / Delete source | Remove the source from this scene only / remove it from every scene | Delete (for removing from one scene) | Scene item removal / source removal |
| Studio mode | Separate preview and program, with transitions between them | Two-pane mode | - |
| Projector | A full-screen or window view of a canvas, scene or source on any display | External view | - |
| Track | One of the audio mixes written to a recording or sent to an output | Channel | Audio bus output |
| Monitor | Listening to a source locally without sending it to outputs | Cue | Monitoring send |
| Monitor off / Monitor and output / Monitor only | A source's monitor modes: not on the monitor; on the monitor and the outputs; on the monitor only, off the outputs while the monitor plays | Cue, Listen | `SourceMonitor::Off` / `Also` / `Only` |
| Solo in monitor | While any source is soloed, the monitor plays only soloed sources; the outputs are unchanged | Cue, PFL | Monitor solo (session state) |
| Shortcut | A key combination for a command; "global shortcut" when it works outside RBS | Hotkey | - |
| Stinger | A transition that plays a video over the change | Video transition | Stinger transition node |
| Output profile | A saved set of output settings (service, encoder, bitrate, recording format, output size). It holds no scenes and no panel layout | Profile, workspace | Output configuration |
| Remote control | Letting other apps (Stream Deck, panels, scripts) control and watch RBS through the control API. Off by default | API server, WebSocket server | Control API server (`rbs-api`, `rbs-api-ws`) |
| Access token | A secret one app uses for remote control, with a name and an access level (View only, Control, Full control). Shown once; can be revoked | Password, key, API key | `rbs_api::Token` with its scopes |
| Access from other computers | Remote control by apps on other computers on the network, over an encrypted connection. Off by default | Network mode, LAN access, remote access | Network access, TLS (`WsConfig::network`, ADR-0046) |
| Key pin | What apps on other computers check before they send an access token: `sha256//` and the hash of RBS's public key. Stays the same when the certificate is renewed | Fingerprint, thumbprint | SPKI SHA-256 pin (`TlsIdentity::key_pin`) |
| Keep running | RBS stays running with no window after the main window is closed: outputs and remote control go on. Ended by Quit | Minimize to tray, background mode | `on_close = "keep_running"` (ADR-0056) |
| Notification area | The icons at the end of the Windows taskbar, where RBS shows its icon while it keeps running | System tray, tray | Windows tray icon (ADR-0056) |
| Area | One of the three regions around the centre of the main window (left, right, bottom) that can be shown or hidden with its button. Hiding an area keeps its panels | Dock, sidebar, region | Main window area (`left`, `right`, `bottom`, ADR-0049) |
| Edge tab | A tab at the window edge for a panel that is set to auto-hide; pointing at it or pressing it opens the panel | Side tab, pin tab | Auto-hide group tab |
| Collapse / Expand | Shrink a panel to its title row (or a narrow strip) and back, without closing it | Minimize, fold | Stack state `collapsed` |
| Auto-hide / Pin | Move a panel to an edge tab so it opens only when needed / put it back in place | Unpin, slide-out | Stack state `auto-hide` / `docked` |
| Maximize / Restore | Let one panel fill the main window for a while / bring the other panels back. Not saved in the workspace | Full screen, zoom | Transient maximize |
| Toolbar | The row of command buttons under the menu bar, which the user chooses in Customize toolbar | Ribbon, action bar | Workspace `toolbar` (ADR-0050) |
| Menu bar | The menus at the top of the window (or of the screen on macOS) that hold every command | App menu, main menu | Slint `MenuBar` built from the command registry |
| Capture region | Pixels cut from the full capture of a screen, window or camera source, stored on the source (not the scene crop) | Crop | `Source::region` (ADR-0103) |
| Display chrome | Insets that hide taskbar or Dock on a display capture, stored on the source | Work area crop | `Source::chrome` (ADR-0104) |
| Timed show / Show | A hidden source may store how long Show draws it; Show is a live cue until that time ends | Flash, stinger | `show_for_ms`, `item.show` (ADR-0105) |
