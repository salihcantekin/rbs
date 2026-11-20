# Architecture overview

RBS is a headless engine built around a **media graph**: a set of nodes (sources, filters, compositors, encoders, muxers, outputs) connected by typed links and driven by one master clock. Every user interface is a client of the engine's control API. Every external technology is reached through a **port** with swappable **adapters**.

```
 Desktop UI      Web panel / Stream Deck      CLI / scripts
      \                   |                       /
       +------- Control API (IPC / WebSocket) ---+
                          |
 +------------------------ Engine (headless) ------------------------+
 |  Scene document + command log          Master clock               |
 |                                                                   |
 |  Video:  Sources -> Filters -> Compositor (per canvas) -> Encoders |
 |                                                     \             |
 |  Audio:  Devices -> Audio graph (buses, mixes) -> Audio encoders  |
 |                                                       \           |
 |                                         Muxers / Transports -> Outputs
 +-------------------------------------------------------------------+
                          ^
            Plugin host: native ABI | WASM sandbox | out-of-process
```

## Layers

| Layer | Crates | May depend on |
| --- | --- | --- |
| Foundation | `rbs-core` (time, clock, timing, media types, capabilities, registry, the thread policy port, errors, metrics) | std, small utility crates (`bytes`) |
| Audio filters | `rbs-audio-filters` (Gain, Noise gate, Ducker, Noise suppression, the audio filter node; ADR-0082, ADR-0130) | `rbs-core`, `rbs-graph`, `rbs-params`, `rbs-denoise` |
| Speech denoiser | `rbs-denoise` (RNNoise, a vendored and patched copy of `nnnoiseless`; ADR-0130) | `realfft` |
| Settings schema | `rbs-params` (the field, value and validation types every filter kind shares; ADR-0082) | none (std only) |
| Format helpers | `rbs-media-format` (H.264/HEVC Annex B and length-prefixed conversion, parameter-set parsing, FLV tag bodies, ADTS; pure functions shared by muxer and transport adapters, ADR-0019), `rbs-browser-ipc` (browser helper messages and page rules; ADR-0109. Not CEF), `rbs-vcam-ring` (the virtual camera's shared-memory frame ring between an adapter and its consumer process; `std` only; ADR-0066) | Foundation only |
| Engine | `rbs-graph`, `rbs-scene`, `rbs-gpu`, `rbs-audio`, `rbs-engine` (engine loop: a worker thread per partition, control loop, shutdown; ADR-0027), `rbs-nodes` (built-in nodes wrapping the ports: capture sources, video export, encoders, outputs; ADR-0038), `rbs-sources` (scene source types to nodes through registered adapters; ADR-0039), `rbs-record` (the recording of a canvas: scene, encoders, muxed output on the engine; ADR-0040) | Foundation, ports, `wgpu`, the recording model (`rbs-manifest`), the speech denoiser (`rbs-denoise`); `rbs-engine` only `rbs-core` and `rbs-graph` |
| Ports | `rbs-capture`, `rbs-audio-io`, `rbs-codec`, `rbs-mux`, `rbs-transport`, `rbs-vcam` (virtual camera; ADR-0066), `rbs-media-decode` (local media-file decode; ADR-0108. FFmpeg adapter not linked yet), `rbs-text` (text layout and drawing; ADR-0129) | Foundation only |
| Adapter support | `rbs-codec-hw` (vendor-neutral session logic shared by the hardware video encoder adapters: slot ring, port contract, capability set, configuration checks; ADR-0031), `rbs-browser-shm` (the browser helper's studio-allocated frame section; `libc` and `windows` for OS shared memory only; ADR-0109), `rbs-system-probe` (OS facts shared by adapters and composition roots: free disk space and memory for recording protection, ADR-0090, and the app identity that holds the OS privacy grants, ADR-0141; plain functions, no RBS crate; `libc` and `windows-sys` only) | Its port, Foundation, format helpers; never a vendor library or an adapter, except the OS bindings named in xtask (`rbs-vcam-mf-source`, `rbs-browser-shm`, `rbs-system-probe`); engine crates may not depend on those three |
| Adapters | `rbs-capture-wgc`, `rbs-capture-sck`, `rbs-capture-avf`, `rbs-capture-mf`, `rbs-capture-pipewire`, `rbs-codec-ffmpeg`, `rbs-codec-amf`, `rbs-codec-nvenc`, `rbs-codec-qsv`, `rbs-codec-vaapi`, `rbs-codec-videotoolbox`, `rbs-codec-audiotoolbox`, `rbs-codec-mf`, `rbs-mux-mpegts`, `rbs-mux-flv`, `rbs-mux-fmp4`, `rbs-mux-matroska`, `rbs-transport-file`, `rbs-transport-rtmp`, `rbs-transport-srt`, `rbs-transport-whip`, `rbs-thread-os` (thread policy port in `rbs-core`, ADR-0030), `rbs-text-parley` and `rbs-text-test` (text renderers, ADR-0129), `rbs-media-anim`, `rbs-media-avf` and `rbs-media-mf` (media-file decoders, ADR-0108), `rbs-browser-runner` (test-double browser helper) and `rbs-browser-cef` (CEF browser helper executable, outside the workspace with its own lock file; ADR-0109, ADR-0132), `rbs-add-ons-sys` (ADR-0145, implements the `rbs-add-ons` traits), ... | Their port, Foundation, format helpers, adapter support crates of their port, vendor crates |
| Plugins | `rbs-plugin-sdk` (manifest), `rbs-plugin-host` (reads manifests; loads nothing yet) | The SDK crate only, until a loader exists |
| Control | `rbs-api` (typed requests, events, session, scopes; `ControlTarget` and `ControlTransport` traits; ADR-0044) | Foundation, `serde`, `serde_json`, `sha2`, `subtle`, `getrandom` (`schemars` behind a feature); no engine, adapter, network or UI crate |
| Control transports | `rbs-api-ws` (WebSocket), `rbs-api-ipc` (named pipe, Unix socket) | `rbs-api`, network and IPC crates |
| UI model | `rbs-ui` (toolkit-neutral panels, layout, workspaces, layout undo, design tokens, live indicator; ADR-0022 decision 4) | std, `serde`, `toml`; no engine, port, adapter or UI toolkit crate |
| Application settings | `rbs-remote-settings` (remote control settings file and config directory, shared by `rbs-app` and `rbs-cli`; ADR-0044) | `rbs-api`, `serde`, `toml`; no toolkit, engine or adapter crate |
| Recording model | `rbs-manifest` (the recording manifest: files, their timing, markers, TOML text; ADR-0089) | `rbs-core`, `toml`; no engine, port, adapter or UI crate |
| Editor projects | `rbs-project` (FCPXML from a recording manifest; ADR-0089). Used only by composition roots | `rbs-core`, `rbs-manifest` |
| User data | `rbs-user-data` (versioned documents, output profiles, scene collection documents, import and export; ADR-0051) | `rbs-scene`, `rbs-core`, `rbs-graph`, `serde`, `toml`; no toolkit, engine or adapter crate |
| Add-ons | `rbs-add-ons` (catalog, download and unpack checks, installed add-ons; ADR-0145) | `rbs-core`, `serde`, `toml`, `sha2`, `unicode-normalization`; no toolkit, engine or adapter crate (HTTP, decompression and file system operations come through `rbs-add-ons-sys`, an adapter) |
| Importers | `rbs-import-obs` (reads an OBS scene collection and profile into an in-memory import plan and report; writes nothing; ADR-0131). Used only by composition roots | `rbs-core`, `rbs-graph`, `rbs-scene`, `rbs-params`, `rbs-ui`, `serde`, `serde_json`; no engine, user-data, adapter or toolkit crate |
| Applications | `rbs-app` (desktop UI; the Rust code Slint generates from its UI files lives in separate crates so it is compiled once per build and in parallel (ADR-0138): `rbs-app-ui-base` (shared globals, structs, enums and tokens, a Slint library), `rbs-app-ui-main`, `rbs-app-ui-settings`, `rbs-app-ui-dialogs` and `rbs-app-ui-probes` (window groups), `rbs-app-ui-single` (the live preview's one-crate build) and the facade `rbs-app-ui`, the only one `rbs-app` names), `rbs-cli` (headless `record`, `token`; ADR-0040) | Everything; this is the composition root |
| Agent application | `rbs-mcp` (MCP on stdio, ADR-0052; the unreachable-studio slice only so far) | `rbs-api`, `rbs-api-ipc` (its client, not wired yet), `rbs-remote-settings`; a composition root that uses no engine, port, other adapter or toolkit crate |

Arrows only point down or sideways into ports. Adapters never depend on each other; code several adapters share lives in an adapter support crate, which holds no vendor code. The engine never depends on an adapter.

## Key concepts

- **Media graph**: nodes and typed links (GPU video frame, audio block, encoded packet, data/event). The graph can change while running; changes apply atomically on a frame boundary. Spec: [media-graph.md](../specs/media-graph.md).
- **Canvas**: the composition grid (pixel grid, frame rate and colour space) with its own compositor node. Output size is the encoded picture size. Many canvases share the same sources.
- **Scene**: a declarative tree given to a compositor: layers, groups, transforms, crops, masks, blend modes, filter chains. Scenes can nest and be used as sources.
- **Command log**: every state change is a command. Undo, redo, remote control, macros and replay share this mechanism.
- **Port and adapter**: a trait plus capability model for one kind of external technology, and the implementations of it. Spec: [backend-ports.md](../specs/backend-ports.md).
- **Registry**: adapters register at start-up (statically or from plugins). The engine asks the registry for the best adapter that satisfies a capability request, following a user-adjustable preference order.

## Threads

| Thread | Work | Priority |
| --- | --- | --- |
| Audio | Device callbacks, mixing, audio filters; lock-free queues, no allocation | Real-time (MMCSS, `THREAD_TIME_CONSTRAINT_POLICY`, rtkit) |
| Render (one per canvas) | Composition, GPU filters, hand-off to encoders | High |
| Encoder workers | One per encoder session | High |
| Network | Transports on a `tokio` runtime: connect, reconnect, adaptive bitrate | Normal |
| Control | Command handling, source warm-up, file I/O | Normal |

## Memory model

The shared frame, block and packet types are defined in the [media types spec](../specs/media-types.md) (ADR-0018).

- GPU textures come from reference-counted pools per format and size; nodes borrow handles.
- Audio uses a fixed block size for the whole graph (default 480 samples at 48 kHz = 10 ms), in pre-allocated ring buffers.
- Encoded packets use pooled, reference-counted buffers (`bytes::Bytes`) so one encode can feed many outputs without copies.

## Colour

Composition runs in linear light on at least 16-bit float textures. Colour-space conversion happens explicitly at inputs and outputs (sRGB, Rec.709, Rec.2020, PQ, HLG). Conversion to NV12 or P010 for encoders runs as a GPU compute pass.

## Related decisions

See the [ADR index](../adr/README.md).
