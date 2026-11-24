# Glossary of user-facing terms

UI text uses these terms and no synonyms (rule U13). Code may use different internal names; the mapping is listed.

| Term in UI | Meaning | Not | Internal name |
| --- | --- | --- | --- |
| Scene | A layout of sources shown on a canvas | Layout, page | `Scene` |
| Source | Something that produces video or audio (screen capture, window capture, camera, image, text, media, browser, audio device), as placed in a scene with its position, size and crop. The panel listing them is "Sources" | Input, layer, element, item | Source node; placement is a scene item |
| Slide show | A source that shows the PNG and JPEG pictures of one folder one after another, on a timer or with Next and Previous, with a transition between them (ADR-0171). Its buttons say "Next picture" and "Previous picture" | Slideshow, image slideshow, carousel, gallery | `org.rbs.source.slideshow`, `SourceKind::Slideshow` |
| Look | A preset of a source's shape, border and filters, copied onto the selected source in one undoable step. Later edits to the source do not change the look | Preset, style, theme | `Look` |
| Duplicate source | Make an independent copy of a source, with its own settings, placed above it (ADR-0133). A camera copy starts in standby | Clone, copy source | `source.duplicate` |
| Duplicate scene with new sources | Copy a scene whose sources are copies too, so changing one scene does not change the other ("Duplicate scene" shares the sources) | Deep copy, clone scene | `scene.duplicate {sources: copied}` |
| Group | Two or more sources of one scene that move, resize, hide and lock together; the Sources list shows them under one row with an arrow (ADR-0182) | Folder, set, nested scene | `Group`, `group.create` |
| Ungroup | Dissolve a group; its sources stay where they are | Break, release | `item.ungroup`, `group.remove` |
| Align | Line the selected sources up on one edge or centre line (Arrange menu) | Snap, justify | `item.align_items` |
| Distribute | Spread three or more selected sources so the gaps between them are equal | Space evenly, tidy | `item.distribute` |
| Attributes | The parts of a source and its placement that can be copied or reset on their own: Position and size, Crop, Opacity, Shape and border, Scroll, Picture filters, Sound filters, Source options, Capture region, Glide, Record separately, Volume and monitoring, Sync offset | Properties, settings (for these groups) | `AttributeGroup` |
| Copy attributes | Remember a source's attributes to paste chosen ones onto other sources | Copy style, copy settings | `source.copy_attributes` |
| Paste attributes | Write chosen copied attributes onto one or more sources, in one undo step | Paste style, apply settings | `source.paste_attributes` |
| Reset source | Put chosen attributes of a source back to their defaults. A filter's "Reset to defaults" button is a different thing | Reset to defaults, factory reset | `source.reset` |
| Library | The panel that applies looks to the selected source and adds backgrounds, frames, a PNG file and shapes. It is not the folder of saved themes, workspaces and scene collections (ADR-0051), which the UI never calls a library | Assets, templates | `rbs.library` |
| Effects (theme) | How lively a theme's controls look: Calm, Glow, Glass or Tactile. Only the theme editor's label; a filter or transition is never an effect | Style | `effects` key of a theme |
| Experience | How many settings the app shows: Simple, Standard or Pro. Shown as "Settings shown"; a place that leaves options out says "More options are hidden" | Mode, user level, expert mode | `experience` in `app.toml` (ADR-0113) |
| Log detail | How much the log file holds: Errors only, Warnings, Normal, Detailed or Everything, in Settings › Advanced (ADR-0158). "Save logs" puts the latest log files and crash reports in one zip for a bug report | Log level, verbosity, debug mode, diagnostics | `log_detail`, `rbs_core::log::Filter` |
| Add-on | Something RBS does not ship but downloads, checks and installs on request in Settings › Add-ons, today the browser engine that browser sources need (ADR-0145). It is not a plugin: an add-on is a fixed part RBS knows; a plugin adds new features | Plugin, extension, download, package | `rbs-add-ons` catalog entry |
| Browser engine | The add-on that shows web pages in browser sources. A browser source without it says so and leads to Settings › Add-ons | CEF, Chromium, runtime, web engine | `org.rbs.browser-engine` |
| Filter | An effect applied to a source or audio | Effect, plugin | Filter node |
| Transition | How the view changes from one scene to another | Effect | Transition node |
| Canvas | The pixel grid and frame rate a scene is drawn on, for example horizontal 1920x1080 or vertical 1080x1920. Scene items are placed in these pixels | Resolution, Output size | Canvas |
| Vertical output | The recording or stream of the vertical canvas. With "Main + vertical" (the default), Record and Go live start and stop it with the main one; "Main only" leaves it to its own menu entry (ADR-0111). UI text says "vertical", not an aspect ratio, because a custom vertical canvas need not be 9:16 | 9:16 output, Shorts, second output | `OutputCanvas::Vertical` |
| Output size | The encoded picture size of one recording or one stream. "Same as canvas", when nothing is stored, encodes at that output's canvas. It is not the size of the preview panel | Resolution, Canvas | `output_size` on the output profile; absent means that output's canvas (ADR-0102) |
| Output | A destination: a streaming service, a recording, a virtual camera | Target | Output node |
| Go live / End stream | Start / stop streaming to the selected outputs | Start stream, Stop live | - |
| Record | Save the program to a file. "Capture" is used only in source type names (Screen capture, Window capture) | Save, Capture (as a verb for recording) | Recording output |
| Pause recording / Resume recording | Stop writing to the recording's files for a while and continue the same files later; the file has no gap. Planned (ADR-0124) | Suspend, Freeze recording | `Studio::pause_output` |
| Hold stream / Resume stream | Keep a live stream connected while viewers see a hold picture with silence; the program stays free to edit (ADR-0124) | Pause stream, Stop stream (it does not stop) | `hold_output` |
| Break | Hold every live stream and pause every recording at once, and resume them together (ADR-0124) | BRB, Intermission | `output.break_toggle` |
| Preview / Program | What you are preparing / what viewers see (studio mode) | - | - |
| Sync offset | How much later (positive) or earlier (negative) a source's sound plays, in milliseconds, to match the picture. A microphone remembers its value on this computer (ADR-0126) | Audio delay, latency, A/V sync | `sync_offset_ms` |
| Measure sync offset | Find a source's sync offset from claps a picture source sees: the window lists the picture sources under "Clap in view of", and nothing changes until Apply (ADR-0126) | Calibrate, auto-sync, test | `sync.measure_start` |
| Mixer | The audio panel with levels and controls | Audio panel | Audio graph UI |
| Automatic, Rows, Columns | Mixer layouts: follow the panel's shape, sources stacked top to bottom, or side by side | Horizontal, Vertical, Auto | Orientation (in UI text) |
| Workspace | A saved arrangement of panels, floating windows and projectors. It never holds scenes, sources or output settings | Profile, layout, scene collection | UI workspace file |
| Scene levels | A scene's own volume and mute for the sources it chooses (Own levels), heard while it is on air; Shared levels uses the sources' own (ADR-0091) | Scene mix, snapshot | `SceneAudio` |
| Scene collection | A set of scenes and sources saved together. It holds no panel layout and no output settings | Project, workspace | Scene document |
| Global source | A source shown and heard in every scene of a canvas, kept in Settings › Global sources | Global audio device, shared source | Item of the canvas's global scene (ADR-0065) |
| Remove from scene / Delete source | Remove the source from this scene only / remove it from every scene | Delete (for removing from one scene) | Scene item removal / source removal |
| Studio mode | Separate preview and program, with transitions between them | Two-pane mode | - |
| Interact window | A window of its own that shows one browser page and sends it clicks, scrolling and typing while it has focus, to log in or press a button on the page; it keeps the page running while open (ADR-0132 decision 6) | Interact mode, browser window | `ui.interact.*` |
| Projector | A window of its own that shows the program of the main or the vertical canvas and nothing else, to share in a video call or put on another display; sized as Window, Exact pixels or Full screen (ADR-0125). Scene and source projectors are not built | External view, output window | `ui.projector.*` |
| Track | One of the audio mixes written to a recording or sent to an output | Channel | Audio bus output |
| Separate files / Record separately | Files a recording writes beside its main file, one per marked source, in a folder named like the recording; a source is marked with Record separately | ISO (in UI text), iso track | ISO recording (ADR-0086) |
| Test this computer | The Settings action that measures how many encoder sessions this computer keeps up with and how fast the recording folder writes, so the estimate and the separate files use measured numbers; it takes up to about a minute and nothing can start while it runs | Benchmark, calibrate, speed test | `calibration.run` (ADR-0087 decision 4) |
| Monitor | Listening to a source locally without sending it to outputs | Cue | Monitoring send |
| Loudness | How loud the program sounds as streaming sites and broadcasters measure it, in LUFS (ITU-R BS.1770); shown on the master with Show loudness. Short-term, Momentary, Integrated, Range and True peak are its values (ADR-0092) | Volume, level (those are the fader and the dBFS meter) | `rbs_audio::loudness` |
| Target (loudness) | The loudness the user aims at, a mark on the master's loudness bar (Settings › Audio › Loudness). Only with "loudness" nearby; an output is never a target | Goal, normalization level | `loudness.toml` `target` |
| True-peak limit | The highest true peak (dBTP) the program should reach; the master's dot lights once it was passed | Ceiling (the leveler's word, ADR-0092 stage 3) | `true_peak_limit` |
| Monitor off / Monitor and output / Monitor only | A source's monitor modes: not on the monitor; on the monitor and the outputs; on the monitor only, off the outputs while the monitor plays | Cue, Listen | `SourceMonitor::Off` / `Also` / `Only` |
| Solo in monitor | While any source is soloed, the monitor plays only soloed sources; the outputs are unchanged | Cue, PFL | Monitor solo (session state) |
| Shortcut | A key combination for a command; "global shortcut" when it works outside RBS | Hotkey | - |
| Stinger | A transition that plays a video over the change | Video transition | Stinger transition node |
| One line | A text source drawn on a single line as wide as its text, for Scroll (ADR-0150) | Single line, no wrap (in UI text) | `single_line` |
| Scroll | A source's picture moving through its box, like a news ticker or end credits: direction, speed, gap between copies, repeat or once, edge fade (ADR-0150) | Marquee, crawl, ticker (in UI text) | `Layout::scroll` |
| Output profile | A saved set of output settings (service, encoder, bitrate, recording format, output size). It holds no scenes and no panel layout | Profile, workspace | Output configuration |
| Remote control | Letting other apps (Stream Deck, panels, scripts) control and watch RBS through the control API. Off by default | API server, WebSocket server | Control API server (`rbs-api`, `rbs-api-ws`) |
| Access token | A secret one app uses for remote control, with a name and an access level (View only, Control, Full control). Shown once; can be revoked | Password, key, API key | `rbs_api::Token` with its scopes |
| Access from other computers | Remote control by apps on other computers on the network, over an encrypted connection. Off by default | Network mode, LAN access, remote access | Network access, TLS (`WsConfig::network`, ADR-0046) |
| AI tools | Letting AI apps on this computer (Claude, Cursor and others) use RBS through `rbs-mcp`, within the access and permission the user sets. Off until started | MCP, MCP server, agents, AI integration | The AI tools token (`kind = "ai_tools"`) and `rbs-mcp` over local IPC (ADR-0134, ADR-0052) |
| Allow actions that cannot be undone | A token's permission to record, stream, delete, change what is on air and reach other computers. Off for AI tools by default | Dangerous mode, unsafe actions | `TokenEntry::irreversible`, `not_permitted` (ADR-0052 decision 4) |
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
| Permissions | What the operating system lets RBS use: Camera, Microphone, Screen and windows, System and app sound, Input monitoring. The Settings section that shows them | Privacy, access rights, TCC | `rbs_sources::permissions::PermissionKind` (ADR-0141) |
| Input monitoring | The permission to see the mouse (later the keyboard) anywhere on the desktop, which Click rings and Pointer halo need on macOS. "No permission needed" on Windows | Accessibility, input access, keylogging | `PermissionKind::InputMonitoring` (ADR-0165 decision 7) |
| Allowed / Not asked yet / Blocked / Managed / Asked each time / Asked on first use / Not needed | One permission's status. Blocked: refused, only the system's privacy settings change it. Managed: set by an organization. Asked each time: the system's dialog asks on every pick | Denied, granted, restricted, not determined | `PermissionStatus` (ADR-0141 decision 1) |
| Allow / Fix permission / Open system settings | Allow shows the system's question once; Fix permission opens Settings › Permissions from Properties; Open system settings opens the system's privacy pane | Grant, request, authorize | `Command::RequestPermission`, `privacy::open` (ADR-0141 decisions 7 and 8) |
