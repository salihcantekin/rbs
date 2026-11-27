# Architecture Decision Records

An ADR records one decision that is hard to reverse or affects more than one crate: the context, the decision, and its consequences. Specs in [../specs](../specs/) describe the current contract; ADRs explain why it looks that way.

## Process

1. Copy [template.md](template.md) to `NNNN-short-title.md` with the next free number.
2. Open a pull request with status `Proposed`.
3. Get an independent review ([ADR-0014](0014-review-policy.md), `architecture-review` skill) and record it in the ADR's `## Review` section.
4. The status becomes `Accepted` only when the review is recorded and the owner approves; a PR may be merged while the ADR is still `Proposed`. To change a decision, write a new ADR that supersedes the old one and update the old one's status to `Superseded by ADR-NNNN`. Do not rewrite accepted ADRs.

## Index

| ADR | Title | Status |
| --- | --- | --- |
| [0001](0001-record-architecture-decisions.md) | Record architecture decisions | Accepted |
| [0002](0002-headless-engine-and-media-graph.md) | Headless engine built on a media graph | Accepted |
| [0003](0003-ports-and-adapters.md) | Every external technology behind a port with swappable adapters | Accepted |
| [0004](0004-single-master-clock.md) | Single master clock for all media | Accepted |
| [0005](0005-gpu-compositing-with-wgpu.md) | GPU compositing with wgpu and zero-copy frames | Accepted |
| [0006](0006-three-tier-plugin-model.md) | Three-tier plugin model | Accepted |
| [0007](0007-ffmpeg-as-optional-adapter.md) | FFmpeg only as an optional codec adapter | Accepted |
| [0008](0008-ui-as-api-client.md) | User interfaces are clients of the control API | Accepted |
| [0009](0009-performance-budgets-in-ci.md) | Performance budgets enforced in CI | Accepted |
| [0010](0010-project-license.md) | Project license | Accepted |
| [0011](0011-english-only-repository.md) | English for all repository content | Accepted |
| [0012](0012-ai-assisted-development.md) | Spec-first, AI-assisted development workflow | Accepted |
| [0013](0013-zero-cost-when-unused.md) | Zero cost when unused | Accepted |
| [0014](0014-review-policy.md) | Independent review for architecture, extra review for hot paths | Accepted |
| [0015](0015-ui-design-rules.md) | Binding UI design rules and a single design system | Accepted |
| [0016](0016-native-desktop-ui.md) | Native desktop UI, no web technology | Accepted |
| [0017](0017-flexible-panels-and-workspaces.md) | Flexible panels and saved workspaces | Accepted |
| [0018](0018-media-frame-representation.md) | Media frames as small values with leased memory | Accepted |
| [0019](0019-encode-mux-transport-ports.md) | Separate encode, mux and transport ports for outputs | Accepted |
| [0020](0020-node-contract-and-schedule-swap.md) | Node contract and lock-free schedule swap | Accepted |
| [0021](0021-scene-collection-model.md) | Scene collection as a validated model with scoped scenes and inverse-edit undo | Accepted (decisions 2, 3 and 7 superseded in part by ADR-0188) |
| [0022](0022-native-ui-toolkit.md) | Slint as the desktop UI toolkit | Accepted |
| [0023](0023-node-parameter-channels.md) | Latest-value parameter channels for running nodes | Accepted |
| [0024](0024-node-restart-with-back-off.md) | Restart failed nodes with back-off, contained to the failed node | Accepted |
| [0025](0025-encoder-surface-export.md) | Hand encoders exported GPU surfaces as native frames | Accepted |
| [0026](0026-partitions-and-queues.md) | Partitions joined by queue endpoint nodes | Accepted |
| [0027](0027-engine-loop.md) | Engine loop: a worker thread per partition in a headless rbs-engine crate | Accepted (decision 6 superseded in part by ADR-0154) |
| [0028](0028-cross-process-preview-surfaces.md) | Share preview surfaces with the UI process as native handles | Accepted |
| [0029](0029-final-ticks-on-drain.md) | Final ticks on drain | Accepted |
| [0030](0030-worker-thread-setup.md) | Worker thread setup: a thread policy port and a runtime attach hook | Accepted |
| [0031](0031-adapter-support-crates.md) | Adapter support crates | Accepted |
| [0032](0032-fast-deterministic-tests.md) | Fast, deterministic tests: no flaky or hanging test, fake time, tiers and per-test timeouts | Accepted |
| [0033](0033-audio-input-port.md) | Audio input port: pull planar `f32` from a lock-free sample ring, no resampling in adapters | Accepted |
| [0034](0034-preview-as-native-layer.md) | Show the engine preview as a native compositor layer, not through the toolkit's redraw | Accepted |
| [0035](0035-meters-as-native-layers.md) | Draw the moving parts of audio meters as native compositor layers | Accepted |
| [0036](0036-windows-preview-layer.md) | Cross-process preview on Windows: shared D3D12 textures, a named pipe and a composition swapchain layer | Accepted |
| [0037](0037-audio-graph.md) | Audio graph: block-cadence partitions on the shared executor, drift correction in input nodes, one graph format | Accepted |
| [0038](0038-io-graph-nodes.md) | Graph nodes for capture, encoding and outputs in an engine crate | Accepted |
| [0039](0039-scene-source-types.md) | Scene source types map to port nodes through a registry; the binder routes audio to a mixer per canvas | Accepted |
| [0040](0040-recording-composition.md) | A recording of a canvas is assembled by an engine crate; `rbs-cli record` is the first composition root that runs it | Accepted |
| [0041](0041-studio-recording-on-a-running-engine.md) | A studio keeps a canvas running for preview and meters; recordings start and stop on the running engine | Accepted |
| [0042](0042-loopback-presentation-latency.md) | Loopback audio is delayed by the render endpoint's measured output latency | Rejected |
| [0043](0043-end-of-stream-on-final-ticks.md) | Final ticks are marked, and the video encoder flushes on them | Accepted |
| [0044](0044-control-api-protocol-and-security.md) | Control API: one typed request layer, JSON-RPC over swappable transports, scoped token authentication | Accepted |
| [0045](0045-recording-tail-on-stop.md) | A stopped recording keeps its tail: the export ends first, the audio encoder flushes when its input ends | Accepted |
| [0046](0046-network-access-over-tls.md) | Network access to the control API: TLS on the remote control port, a generated key the client pins | Accepted |
| [0047](0047-several-outputs-per-canvas.md) | Several outputs per canvas: record and stream at once, encoders shared when their settings match | Accepted |
| [0048](0048-selection-overlay-on-the-preview-layer.md) | Draw the preview's selection outline and handles as native layers of the preview | Accepted |
| [0049](0049-main-window-areas-panel-states-and-sizes.md) | Main window areas, panel states (collapse, auto-hide, maximize) and size limits (workspace format 2) | Accepted |
| [0050](0050-command-registry-menus-and-toolbar.md) | One command registry for menus, context menus, the toolbar and the palette | Accepted |
| [0051](0051-user-data-profiles-and-scene-collections.md) | User data as versioned documents; output profiles and scene collections as libraries of named files | Accepted (decision 6 superseded in part by ADR-0188) |
| [0052](0052-ai-agent-control-through-mcp.md) | AI agents control RBS through an MCP server that is a plain control API client | Accepted |
| [0053](0053-item-style-and-video-filter-chains.md) | Item shape, border and shadow drawn by the compositor; video filters as a per-source chain of GPU nodes | Accepted (partly superseded by ADR-0110) |
| [0054](0054-preview-never-disappears.md) | The preview picture never disappears behind popups: split it around covers | Accepted |
| [0055](0055-crash-resilience.md) | Crash resilience: a fault in the app must not end a recording or a stream | Accepted |
| [0056](0056-keep-running-after-the-main-window-closes.md) | Keep running after the main window closes: Close and Quit are separate; Dock on macOS, tray icon on Windows | Accepted (decision 3 superseded in part by ADR-0156) |
| [0057](0057-user-themes-and-appearance-settings.md) | User themes as checked token overrides in a theme library; Settings › Appearance picks and edits them | Accepted; decisions 1 and 3 superseded in part by ADR-0123 |
| [0058](0058-crop-editing-of-sources.md) | Crop editing of sources: keep the scale, read the picture size from the capture node | Accepted |
| [0059](0059-scenes-lock-and-source-audio-in-the-studio.md) | Scenes, item lock and source volume in the studio | Accepted |
| [0060](0060-shortcuts-in-app-and-global.md) | Shortcuts bind registry commands to key chords, in the app or globally; Settings › Shortcuts edits them | Accepted |
| [0061](0061-studio-mode-in-the-desktop-app.md) | Studio mode in the desktop app: preview scene picture, cut transition, control API 1.9 | Accepted |
| [0062](0062-built-in-video-filters-and-properties-tabs.md) | Built-in video filters: one filter node behind a filter type trait, settings through parameter channels, and the Properties tabs | Accepted |
| [0063](0063-hold-the-program-while-its-scene-is-edited-in-preview.md) | Hold the program while its scene is edited in preview (studio mode) | Accepted (partly superseded by ADR-0110) |
| [0064](0064-transition-rail-with-the-pictures.md) | The Transition rail sits with the pictures: between or below Preview and Program, only in studio mode | Accepted |
| [0073](0073-pictures-area-and-studio-mode-switch.md) | The Pictures area, a resizable rail and Studio mode as one switch | Accepted |
| [0065](0065-global-sources-and-safe-source-removal.md) | Global sources as a global scene per canvas, and safe source removal | Accepted |
| [0066](0066-virtual-camera.md) | Virtual camera on Windows: the program as a camera for meeting apps, off by default | Accepted |
| [0069](0069-rbs-menu-and-command-search.md) | The RBS menu: an app menu with command search, and the menu bar only on macOS | Accepted |
| [0070](0070-virtual-camera-per-user-source-class.md) | Virtual camera on Windows: a per-user source class instead of camera properties | Accepted |
| [0071](0071-merged-title-bar-on-windows.md) | The toolbar row is the title bar on Windows, and the row leaves no gaps | Accepted; one sentence of decision 2 superseded by ADR-0123 |
| [0072](0072-theme-colour-rules-warn.md) | Theme colour rules warn instead of refusing a user theme; every editor change shows at once | Accepted; decision 1 superseded in part by ADR-0123 |
| [0074](0074-still-image-and-colour-sources.md) | Still sources: an image file and a solid colour or gradient, drawn once and costing nothing per frame | Accepted |
| [0075](0075-looks-and-library-panel.md) | Looks and the Library panel: presets of style and filters, and generated sample pictures | Accepted |
| [0077](0077-source-health-retarget-and-settings-window.md) | Source health, re-picking a source's target, and the Source settings window | Accepted |
| [0078](0078-saved-section-state-and-shared-section-components.md) | Saved section state and shared section components in every panel | Accepted |
| [0080](0080-hide-sources-in-the-mixer.md) | Hide sources in the Mixer | Accepted |
| [0081](0081-master-strip-in-the-mixer.md) | Master strip in the Mixer: a per-canvas master volume and mute | Accepted |
| [0082](0082-audio-filter-chains-on-sources.md) | Audio filter chains on sources: shared parameter schema (rbs-params), audio filter nodes, gain and noise gate | Accepted |
| [0083](0083-mixer-orientation-chosen-by-the-user.md) | The Mixer orientation (rows or columns) is chosen by the user, saved per workspace | Accepted |
| [0084](0084-source-transform-commands-and-source-context-menu.md) | Source transform commands (flip, rotate, fit, align) and a categorized source context menu | Accepted |
| [0085](0085-more-containers-codecs-and-advanced-encoder-settings.md) | More containers and codecs, and advanced video and audio encoder settings | Accepted |
| [0086](0086-recording-sources-separately-iso.md) | Record sources separately (ISO recording) next to the master recording | Accepted |
| [0087](0087-output-cost-estimate-and-hardware-limits.md) | Show the cost of outputs before they start, and the limits of this hardware | Accepted |
| [0088](0088-replay-buffer-and-retroactive-recording.md) | Replay buffer: keep the last minutes in memory, save a clip, or turn the buffer into a recording | Accepted |
| [0089](0089-recording-markers-and-editor-project-export.md) | Recording markers and an editor project (FCPXML) written from the recording manifest | Accepted |
| [0090](0090-recording-protection-disk-lag-and-adaptive-bitrate.md) | Recording protection: early warnings for disk and encoder trouble, ordered load shedding, and adaptive bitrate | Accepted |
| [0091](0091-per-scene-audio-levels.md) | Per-scene audio levels: a scene can remember its own volumes and mutes | Accepted |
| [0092](0092-loudness-metering-and-leveling.md) | Loudness: BS.1770 metering first, then a slow leveler, and gain advice for editing | Accepted |
| [0093](0093-vertical-second-canvas-from-the-same-sources.md) | A vertical (9:16) second canvas running at the same time as the main one | Accepted |
| [0094](0094-ui-interaction-patterns-and-tabler-icon-set.md) | UI interaction patterns (info hints, feature sections, one menu system) and the Tabler icon set | Accepted |
| [0095](0095-audio-ducker-and-key-tap.md) | Audio ducker as a filter with a pre-fader key tap from another source | Accepted |
| [0096](0096-capture-audio-companions-and-camera-microphone.md) | "Capture audio too" companions and the camera's own microphone | Accepted (decision 2 superseded in part by ADR-0186) |
| [0097](0097-audio-monitoring-and-buses.md) | Audio monitoring (output port, per-strip monitor modes) and buses with per-source send levels | Accepted |
| [0098](0098-transition-keeps-the-preview-scene.md) | Transition keeps the preview scene; swapping is a setting, off by default | Accepted |
| [0099](0099-studio-in-its-own-process.md) | Run the studio in its own process | Accepted |
| [0100](0100-configurable-glide-duration-and-easing.md) | Configurable glide duration and easing (collection defaults + multi-scene source override) | Accepted |
| [0101](0101-mixer-columns-panes-and-pinned-master.md) | Mixer columns use fixed strips, side-by-side clusters and horizontal scroll | Accepted |
| [0102](0102-output-size-distinct-from-canvas.md) | Output size distinct from the canvas | Accepted (macOS done; Windows remains) |
| [0103](0103-source-region.md) | A source may store a capture region distinct from the item crop | Accepted |
| [0104](0104-display-chrome-insets.md) | Display chrome insets hide the taskbar or Dock via the source region | Accepted |
| [0105](0105-timed-show.md) | Timed show draws a hidden item for a deadline without undo | Accepted |
| [0106](0106-picture-and-sound-on-one-source.md) | A source that has both picture and sound is a picture node and a sound node fed by one producer | Accepted |
| [0107](0107-temporal-sources-sleep-when-hidden.md) | Temporal sources sleep unless a shown scene draws them | Accepted |
| [0108](0108-media-file-source.md) | A local media-file source for video and animated pictures | Accepted |
| [0109](0109-browser-source.md) | A browser source is CEF in a helper process, one process per Active source | Accepted |
| [0110](0110-canvas-layout-space.md) | Item boxes stay in a canvas layout space and are fitted into the current size | Accepted |
| [0111](0111-vertical-outputs-start-with-the-main-ones.md) | Vertical outputs start and stop with the main ones, and each output shows its own state | Accepted |
| [0112](0112-calm-look-and-roomy-density.md) | The Calm look for the built-in themes and a roomy density | Accepted; decision 6 superseded in part by ADR-0120; high-contrast lines of decisions 2 and 3 by ADR-0123 |
| [0113](0113-experience-level-and-first-run.md) | An experience level and the first-run choice | Accepted |
| [0114](0114-screen-grammar.md) | One screen grammar: row, list row, section marks, held edits, one home | Accepted |
| [0115](0115-ui-stop-rule-for-agents.md) | A stop rule for UI changes by AI agents, with counted checks and an exceptions register | Accepted; decision 8 superseded by ADR-0123 |
| [0116](0116-audio-only-recording.md) | Audio recording, an output that writes the mix without a picture | Closed: shipped as WAV/FLAC recording formats instead |
| [0117](0117-still-sources-as-images.md) | Still sources are recorded separately as PNG images, not video | Accepted |
| [0118](0118-tint-buttons.md) | Tint buttons: soft tinted fills, round icon-only buttons, an outline only where the fill does not separate | Accepted; decision 3 superseded in part by ADR-0122 and ADR-0123 |
| [0119](0119-where-separate-files-are-encoded.md) | Separate files take the cheapest path per source, and the user picks where the rest is encoded | Accepted |
| [0120](0120-theme-effects.md) | Theme effects: Calm, Vivid or Tactile chosen with the theme, drawn only by library buttons | Accepted; superseded in part by ADR-0122 and ADR-0123 (decision 3) |
| [0122](0122-bold-theme-effects.md) | Bold theme effects: Glow, Glass and a deeper Tactile that read at a glance | Accepted; its high-contrast lines superseded by ADR-0123; decision 8 (frame bar) superseded in part by ADR-0136 |
| [0123](0123-remove-the-high-contrast-theme.md) | Remove the built-in high-contrast theme; Dark and Light are the built-in themes | Accepted |
| [0124](0124-pause-recording-and-hold-stream.md) | Pause a recording (one continuous file, cut before the encoder when owned, at the output when shared); hold a stream with a hold picture instead of pausing it | Accepted |
| [0125](0125-projector-windows.md) | Projector windows: the program of a canvas in a chromeless window for sharing apps, its own canvas-size picture only while open, Exact pixels size, cannot be minimised | Accepted |
| [0126](0126-measured-sync-offset-and-device-memory.md) | Measure a source's sync offset with a clap, and remember it with the device | Accepted |
| [0127](0127-multistreaming-to-several-destinations.md) | Multistreaming: one stream to several destinations, one shared encoder, each destination isolated; RTMPS; service presets | Accepted |
| [0128](0128-scene-transitions.md) | Scene transitions: Cut, Fade, Move, Swipe, Slide (Stinger later); the incoming scene drawn by its own compositor only while a transition runs | Accepted |
| [0129](0129-text-source.md) | A text source: a fixed box drawn on the CPU once per change through a text rendering port, updated in place; live kinds (clock, countdown, stopwatch, elapsed time) and crisp resizing | Accepted |
| [0130](0130-voice-filters-noise-suppression-compressor-limiter-eq.md) | Voice filters: noise suppression, compressor, limiter and equaliser, with filter latency | Accepted (decision 5 superseded in part by ADR-0155) |
| [0131](0131-import-obs-scene-collections-and-profiles.md) | Import OBS scene collections, profiles and hotkeys as a new collection and output profile, through a mapper registry, with a report of what did not come across | Accepted (decisions 4, 6 and 7 superseded in part by ADR-0188) |
| [0132](0132-browser-engine-css-interaction.md) | The browser source runs CEF fetched on first use, with custom CSS, reload, interaction and an opt-in sign-in | Accepted |
| [0133](0133-duplicate-reset-and-paste-source-attributes.md) | Duplicate a source (and a scene with new sources), reset a source, and paste chosen attribute groups onto other sources in one undo step; sound stays live | Accepted |
| [0134](0134-ai-tools-settings-page.md) | Start AI tools (`rbs-mcp`) from a Settings page, with a start-on-open option, an RBS-made token file and a generated connection line per AI tool | Accepted |
| [0135](0135-transitions-t-bar-scene-override-rehearse.md) | Transitions phase 3: a T-bar driving the transition by hand, a transition into a scene, and Rehearse on the Preview picture | Accepted |
| [0136](0136-no-halo-in-the-frame-bar.md) | No halo in the frame bar, so the Glow look keeps the bar's height | Accepted |
| [0137](0137-rbs-drawn-context-menus-and-lists.md) | Context menus and drop-down lists are drawn by RBS in the window; the macOS menu bar stays native; the projector menu stays native | Accepted |
| [0138](0138-split-the-generated-ui-crate.md) | Split the generated UI code into a base crate (shared globals and structs) and one crate per window group, for parallel and partial UI builds | Accepted |
| [0139](0139-remote-guests-rbs-to-rbs-over-webrtc.md) | Remote guests: one RBS shares its program to another over peer-to-peer WebRTC, paired by an invite code and an answer code, with no server of ours | Accepted |
| [0140](0140-stinger-transition.md) | Stinger transition: a media file played over the program change, its cut point found from the alpha | Accepted |
| [0141](0141-os-permissions-page-and-just-in-time-requests.md) | OS permissions: ask at the moment of use, a Settings › Permissions page, and a fix action on a blocked source | Accepted |
| [0142](0142-companion-sound-leaves-the-scene-with-its-picture.md) | A companion sound leaves a scene with its picture: Remove from scene takes the companions' items in that scene, other scenes stay (supersedes ADR-0065 decision 7 in part) | Accepted |
| [0143](0143-performance-rules-for-every-change.md) | Performance rules for every change: a cost statement per feature, no work without a consumer, shared work, the right size and rate, events instead of polling, edits that cost what they change | Accepted |
| [0144](0144-code-quality-rules-and-their-enforcement.md) | Code quality rules and how they are enforced: size ratchet, logic outside the toolkit crate, one command path, typed errors, shared test support, pinned toolchain, one verify gate, checked docs | Accepted |
| [0145](0145-add-ons-one-place-to-download-and-install.md) | Add-ons: one place to download, verify and install what RBS does not ship (origin, transfer and kind; the browser engine first) | Accepted |
| [0146](0146-picture-identity-lets-encoders-reuse-an-unchanged-input.md) | Picture identity on video frames, so encoders reuse an unchanged input instead of copying it | Accepted |
| [0147](0147-native-encoder-input.md) | Native encoder input chosen by capability: exporters per graphics API, native frames cross only to an encoder, readback stays the fallback (exits ADR-0038 decision 3 per encoder) | Accepted |
| [0148](0148-streaming-follows-its-destinations.md) | Streaming follows its destinations: every destination can be removed, Go live shows only while one is on, End stream never hidden while live | Accepted |
| [0149](0149-browser-page-automation-zoom-and-background-pages.md) | Browser page automation, zoom, background pages and load-error retry: periodic reload, the user's own script, page zoom, opt-in keep running when hidden, no error page on air | Accepted |
| [0150](0150-scrolling-layers-in-the-compositor.md) | Scrolling is a layer motion evaluated by the compositor node, for every source (ticker, crawl, credits; answers ADR-0129 decision 9) | Accepted |
| [0151](0151-leaving-studio-mode-keeps-the-program-as-it-was.md) | Leaving studio mode keeps the program as it was on air: held edits are put back as one undo step (supersedes part of ADR-0063 decision 4 and ADR-0098 decision 2) | Accepted |
| [0152](0152-source-activity-when-a-source-runs-and-what-it-does-on-return.md) | Source activity: one policy for every source (run while placed, shown or on air; continue, resume or restart on return; linger), defaults declared by the type | Accepted |
| [0153](0153-a-transition-of-its-own-outside-studio-mode.md) | A transition kind of its own outside studio mode (Move by default); the rail's kind is for studio mode | Accepted |
| [0154](0154-wake-the-control-thread-on-bypass.md) | Wake the control thread when a node is bypassed, and stop capping the control wait merely because a worker runs (supersedes part of ADR-0027 decision 6) | Accepted |
| [0155](0155-one-noise-state-until-each-channel-is-selected.md) | One noise-suppression state until each channel is selected (supersedes part of ADR-0130 decision 5) | Accepted |
| [0156](0156-ask-before-quitting-while-recording-and-stop-off-the-ui-thread.md) | Ask before quitting while recording too, and stop the studio on a shutdown thread with per-step budgets (supersedes part of ADR-0056 decision 3) | Accepted |
| [0157](0157-browser-gpu-pages-sync-on-a-shared-fence.md) | Browser pages on the GPU sync on a shared fence; the helper never waits on the CPU (supersedes the Windows sync and ring-size clauses of ADR-0109 decision 8 and ADR-0132 decision 8) | Accepted |
| [0158](0158-diagnostics-log.md) | A diagnostics log with levels: a facade in `rbs-core`, a rotating file written off the calling thread by `rbs-log`, nothing on hot paths | Accepted |
| [0159](0159-studio-host-in-its-own-crate.md) | The studio host in its own toolkit-free crate `rbs-studio-host`, adapters only through `HostConfig`, then split by concern | Accepted |
| [0160](0160-one-platform-adapter-list.md) | One platform adapter list in `rbs-platform`, shared by `rbs-app` and `rbs-cli`; only composition roots may depend on it | Accepted |
| [0161](0161-preview-layer-keeps-a-frame-whose-present-failed.md) | The Windows preview layer keeps a frame whose present failed and presents it again (supersedes the failed-present clause of ADR-0036 decision 7) | Accepted |
| [0162](0162-extension-ready-registries-and-plugin-order.md) | Extension-ready registries for every type a plugin could add, features built that way from now on, and the plugin tiers in the order sandbox, process, native (supersedes in part a consequence of ADR-0006) | Accepted |
| [0163](0163-one-command-path-for-the-window-and-the-api.md) | One command path: the window sends control API commands (`Command::Api`); every studio host `Command` variant classified at compile time, the rest window-only with a reason; drags share the API method (G-02) | Accepted |
| [0164](0164-protocol-minor-for-the-window-commands-left.md) | One protocol minor (1.100) for the window commands ADR-0163 could not move: gestures, settings fields, new methods, local files as a caller check | Accepted |
| [0165](0165-show-mouse-and-keyboard-input.md) | Show mouse and keyboard input (mouse first, keys later on the same port): one shared input port (Raw Input, listen-only event tap, XInput2), pointer effects as a capture filter, keys as a source, privacy defaults | Accepted |
| [0166](0166-item-set-rotation.md) | `item.set_rotation`: a free angle for an item in protocol 1.100, so the window's rotation and other transform ops go through the control API | Accepted |
| [0167](0167-browser-gpu-pages-on-macos.md) | Browser pages on macOS paint into studio-owned IOSurfaces handed over on a Mach service; the helper publishes from Metal's completion handler, so no fence (extends ADR-0157) | Accepted |
| [0168](0168-the-last-pending-window-commands.md) | The last 15 Pending window commands: window-only answers behind the window flag, the window picking the method, the OS picker and Interact input as `LocalOnly`, and protocol 1.101 (`source.targets` fields, `browser.forget_sign_in` `restart`) | Accepted |
| [0169](0169-audio-branch-rests-while-nothing-reads-it.md) | The meter sink is in the graph only while meters are wanted, so the audio branch (worker, device streams, filters, mixer) rests while nothing reads it (supersedes ADR-0041 decision 2 in part) | Accepted |
| [0170](0170-multi-track-recording-and-six-buses.md) | Multi-track recording: a recording writes 1 to 6 buses as audio tracks in one never-shared encoder chain; buses capped at 6 (amends ADR-0097) | Accepted |
| [0171](0171-image-slide-show-source.md) | Image slide show source: one folder of PNG and JPEG pictures, at most three decoded at a time, a fixed output size, a transition pass only during a change, controlled through the media methods | Accepted |
| [0172](0172-panels-menu-toggles-and-panels-reopen-where-they-were.md) | The Panels menu toggles its panel, and a panel closed in this session reopens where it was: its stack, beside its neighbour, its area, edge group or floating window (supersedes ADR-0073 decision 4 in part) | Accepted |
| [0173](0173-companion-sound-nested-under-its-picture.md) | A "Capture audio too" sound is shown under its picture in the Sources list: a disclosure arrow on the picture row, closed by default, the sound row with its level and mute; the Mixer is unchanged | Accepted |
| [0174](0174-sdr-video-is-display-referred.md) | SDR video is display-referred: BT.709-tagged Y'CbCr is decoded with the sRGB EOTF and "Video" output is encoded with the sRGB OETF, so pictures pass through unchanged as in players and OBS | Accepted |
| [0175](0175-camera-colour-space-and-range-choice.md) | A camera's colour space (Rec. 709, Rec. 601) and range (Limited, Full) can be chosen instead of what the device reports: `CaptureRequest` fields applied at open by adapters declaring `capture.color.override`, two camera keys, two rows in the Source settings window | Accepted |
| [0176](0176-camera-region-preview-from-the-running-source.md) | A camera's Region preview is one frame of the running source, read back once by the studio, when the adapter has no `region_grab`; a standby camera is not opened for it (supersedes ADR-0103 decision 4 for those sources) | Accepted |
| [0177](0177-a-scrolling-fill-moves-its-whole-shape.md) | A scrolling fill (a picture of at most 2 x 2 texels: Library shapes, plain frames) is as long as its box and its shape and border move with it (supersedes part of ADR-0150 decision 2 for fills) | Accepted |
| [0178](0178-a-source-region-keeps-the-item-in-place.md) | Setting a source region keeps the item's box centre and fits the region's aspect inside it; filling the canvas is the Fit menu (amends ADR-0103) | Accepted |
| [0179](0179-camera-background-removal.md) | Camera background removal: a video filter on a person matte: model spike first, then a `PersonMask` port with a GPU-native backend (OS backends where they exist); GPU-only default path, worker off the render thread | Accepted |
| [0180](0180-the-toolbar-row-is-the-title-bar-on-macos.md) | The toolbar row is the title bar on macOS too, with the window buttons on the right as on Windows, behind one `WindowFrame` interface (amends ADR-0071 decision 3) | Accepted |
| [0181](0181-one-rbs-at-a-time.md) | One RBS per settings folder on every system and with either close setting: an `instance.lock`, a second launch brings the running RBS forward and ends; several copies parked (amends ADR-0056, ADR-0051, ADR-0156) | Accepted |
| [0182](0182-source-groups.md) | Sources can be grouped, and a group moves, resizes and hides as one | Accepted (decisions 5 and 10 superseded in part by ADR-0186; decisions 7 and 8 superseded in part by ADR-0184; decision 9 superseded in part by ADR-0194) |
| [0183](0183-background-removal-picture-and-mask-in-step.md) | Background removal draws each picture with its own mask (Exact); the newest picture stays an option (Fast) | Accepted |
| [0184](0184-group-properties-batch-to-members.md) | Group Properties target the group and batch to its members | Accepted (decisions 3, 4 and 5 superseded in part by ADR-0185) |
| [0185](0185-scroll-a-group-as-one-picture.md) | Scroll a group as one picture | Accepted (decision 1 superseded in part by ADR-0188; decision 3 superseded in part by ADR-0187; decision 5 superseded in part by ADR-0193) |
| [0186](0186-do-not-regroup-grouped-items.md) | Do not regroup items already in a group | Accepted |
| [0187](0187-version-group-scroll-settings.md) | Version group Scroll settings for run identity | Accepted |
| [0188](0188-defer-new-nested-scenes.md) | Scenes cannot contain scenes | Accepted |
| [0189](0189-address-source-placements.md) | Address source placements independently | Proposed |
| [0190](0190-prepare-group-resources-on-executors.md) | Prepare group resources on their owning executor | Superseded by ADR-0193 |
| [0191](0191-incremental-node-release-for-quiesce.md) | Bounded control-thread service and quiesce | Superseded by ADR-0193 |
| [0192](0192-transactional-group-show.md) | Transactional group Show | Superseded by ADR-0193 |
| [0193](0193-simple-group-show-and-scroll-resources.md) | Simple group Show and group Scroll resources (supersedes ADR-0190, ADR-0191, ADR-0192; ADR-0185 decision 5 in part) | Accepted |
| [0194](0194-member-row-selects-the-member.md) | A member row in the Sources list selects that member (supersedes ADR-0182 decision 9 in part) | Accepted |
| [0195](0195-inactive-scenes-and-list-filters.md) | Inactive scenes and filters for the Scenes and Sources lists | Proposed |
| [0196](0196-load-control-api-schemas-in-parts.md) | Control API schemas load in parts: `api.describe` is an index with schemas on request, one shard per namespace parsed on first use, `rbs-mcp` lists a small fixed tool set by default | Accepted |
