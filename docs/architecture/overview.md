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

| Layer | Crates (planned) | May depend on |
| --- | --- | --- |
| Foundation | `rbs-core` (time, clock, timing, media types, capabilities, registry, the thread policy port, errors, metrics) | std, small utility crates (`bytes`) |
| Audio filters | `rbs-audio-filters` (Gain, Noise gate, the audio filter node; ADR-0082) | `rbs-core`, `rbs-graph`, `rbs-params` |
| Settings schema | `rbs-params` (the field, value and validation types every filter kind shares; ADR-0082) | none (std only) |
| Format helpers | `rbs-media-format` (H.264/HEVC Annex B and length-prefixed conversion, parameter-set parsing, FLV tag bodies, ADTS; pure functions shared by muxer and transport adapters, ADR-0019) | Foundation only |
| Engine | `rbs-graph`, `rbs-scene`, `rbs-gpu`, `rbs-audio`, `rbs-engine` (engine loop: a worker thread per partition, control loop, shutdown; ADR-0027), `rbs-nodes` (built-in nodes wrapping the ports: capture sources, video export, encoders, outputs; ADR-0038), `rbs-sources` (scene source types to nodes through registered adapters; ADR-0039), `rbs-record` (the recording of a canvas: scene, encoders, muxed output on the engine; ADR-0040) | Foundation, ports, `wgpu`; `rbs-engine` only `rbs-core` and `rbs-graph` |
| Ports | `rbs-capture`, `rbs-audio-io`, `rbs-codec`, `rbs-mux`, `rbs-transport`, `rbs-vcam` (virtual camera; ADR-0066) | Foundation only |
| Adapter support | `rbs-codec-hw` (vendor-neutral session logic shared by the hardware video encoder adapters: slot ring, port contract, capability set, configuration checks; ADR-0031), `rbs-vcam-ring` (the virtual camera's shared-memory frame ring between an adapter and its consumer process; `std` only; ADR-0066) | Its port, Foundation, format helpers; never a vendor library or an adapter |
| Adapters | `rbs-capture-wgc`, `rbs-capture-sck`, `rbs-capture-avf`, `rbs-capture-mf`, `rbs-capture-pipewire`, `rbs-codec-ffmpeg`, `rbs-codec-amf`, `rbs-codec-nvenc`, `rbs-codec-qsv`, `rbs-codec-vaapi`, `rbs-codec-videotoolbox`, `rbs-codec-audiotoolbox`, `rbs-codec-mf`, `rbs-mux-mpegts`, `rbs-mux-flv`, `rbs-mux-fmp4`, `rbs-mux-matroska`, `rbs-transport-file`, `rbs-transport-rtmp`, `rbs-transport-srt`, `rbs-transport-whip`, `rbs-thread-os` (thread policy port in `rbs-core`, ADR-0030), ... | Their port, Foundation, format helpers, adapter support crates of their port, vendor crates |
| Plugins | `rbs-plugin-sdk`, `rbs-plugin-host` | Foundation, ports |
| Control | `rbs-api` (typed requests, events, session, scopes; `ControlTarget` and `ControlTransport` traits; ADR-0044) | Foundation, `serde`, `serde_json`, `sha2`, `subtle`, `getrandom` (`schemars` behind a feature); no engine, adapter, network or UI crate |
| Control transports | `rbs-api-ws` (WebSocket), `rbs-api-ipc` (named pipe, Unix socket) | `rbs-api`, network and IPC crates |
| UI model | `rbs-ui` (toolkit-neutral panels, layout, workspaces, layout undo, design tokens, live indicator; ADR-0022 decision 4) | std, `serde`, `toml`; no engine, port, adapter or UI toolkit crate |
| Application settings | `rbs-remote-settings` (remote control settings file and config directory, shared by `rbs-app` and `rbs-cli`; ADR-0044) | `rbs-api`, `serde`, `toml`; no toolkit, engine or adapter crate |
| User data | `rbs-user-data` (versioned documents, output profiles, scene collection documents, import and export; ADR-0051) | `rbs-scene`, `rbs-core`, `rbs-graph`, `serde`, `toml`; no toolkit, engine or adapter crate |
| Applications | `rbs-app` (desktop UI; `rbs-app-ui` holds the Rust code Slint generates from its UI files, a separate crate only so it is compiled once per build), `rbs-cli` (headless `record`, `token`; ADR-0040) | Everything; this is the composition root |
| Agent application | `rbs-mcp` (planned: MCP server for AI agents, a plain control API client with its own token; ADR-0052) | `rbs-api`, `rbs-api-ipc` (its client), `rbs-remote-settings`, an MCP library; a composition root that uses no engine, port, other adapter or toolkit crate |

Arrows only point down or sideways into ports. Adapters never depend on each other; code several adapters share lives in an adapter support crate, which holds no vendor code. The engine never depends on an adapter.

## Key concepts

- **Media graph**: nodes and typed links (GPU video frame, audio block, encoded packet, data/event). The graph can change while running; changes apply atomically on a frame boundary. Spec: [media-graph.md](../specs/media-graph.md).
- **Canvas**: an output resolution, frame rate and colour space with its own compositor node. Many canvases share the same sources.
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
