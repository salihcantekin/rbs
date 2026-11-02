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
| Foundation | `rbs-core` (time, clock, timing, media types, capabilities, registry, errors, metrics) | std, small utility crates (`bytes`) |
| Format helpers | `rbs-media-format` (H.264/HEVC Annex B and length-prefixed conversion, parameter-set parsing, FLV tag bodies, ADTS; pure functions shared by muxer and transport adapters, ADR-0019) | Foundation only |
| Engine | `rbs-graph`, `rbs-scene`, `rbs-gpu`, `rbs-audio` | Foundation, ports, `wgpu` |
| Ports | `rbs-capture`, `rbs-audio-io`, `rbs-codec`, `rbs-mux`, `rbs-transport` | Foundation only |
| Adapters | `rbs-capture-wgc`, `rbs-capture-sck`, `rbs-capture-pipewire`, `rbs-codec-ffmpeg`, `rbs-codec-nvenc`, `rbs-codec-videotoolbox`, `rbs-mux-mpegts`, `rbs-mux-flv`, `rbs-mux-fmp4`, `rbs-transport-rtmp`, `rbs-transport-srt`, `rbs-transport-whip`, ... | Their port, Foundation, format helpers, vendor crates |
| Plugins | `rbs-plugin-sdk`, `rbs-plugin-host` | Foundation, ports |
| Control | `rbs-api` (schema + server) | Engine, Foundation |
| Applications | `rbs-app` (desktop UI), `rbs-cli` | Everything; this is the composition root |

Arrows only point down or sideways into ports. Adapters never depend on each other. The engine never depends on an adapter.

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
