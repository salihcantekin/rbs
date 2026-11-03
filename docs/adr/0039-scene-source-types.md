# ADR-0039: Scene source types map to port nodes through a registry; the binder routes audio to one mixer

- Status: Proposed
- Date: 2026-11-03
- Deciders: Salih (owner)

## Context

The scene model (ADR-0021) stores sources as a node type id plus settings, and the scene binder turns a collection into a graph description: one node per source, filter chains, one compositor per canvas and role. It only needs node descriptors (`NodeTypes`). Nothing yet says which node type a display, a window, a camera or a microphone is, how a source's settings choose an adapter and a device, or who builds the node instance the runtime runs. The capture node (ADR-0038) and the audio input node and mixer (ADR-0037) exist, but are only wired by hand in examples. The binder reports audio sources as `NoVideoOutput`.

Forces:

- **P1:** engine code never depends on an adapter; the composition root (`rbs-app`, `rbs-cli`) registers adapters.
- **P3:** no vendor names in engine code; choose adapters by capability.
- **P13:** a source that is installed or in Standby holds no device; a type with no adapter costs nothing.
- **ADR-0020:** node ids stay stable so the runtime keeps running instances across swaps; an instance's descriptor must equal the node's descriptor in the graph.
- **ADR-0037:** a mixer input without a strip is not heard; audio nodes run in an audio partition at block cadence.
- Windows first (owner, 2026-11-03): display, window, camera, microphone and system audio, then the headless record command, which needs exactly this mapping.

## Decision

1. **Built-in source types** are reverse-DNS type ids, one per kind of thing captured: `org.rbs.source.display`, `.window`, `.camera` (capture port, `TargetKind`) and `org.rbs.source.audio-input`, `.audio-output` (system audio, loopback), `.application-audio` (audio input port, `EndpointKind`). The type says what is captured, never which adapter or vendor; the adapter is a setting.
2. **A registry, `rbs_sources::SourceTypes`,** in a new engine-layer crate `rbs-sources` (depends on `rbs-scene`, `rbs-nodes`, `rbs-audio`, `rbs-gpu`, the capture and audio input ports; never on an adapter). The composition root registers capture factories and audio backends (`rbs_core::registry::Registry` per port) and probes them. A source type is **installed** when a probed adapter declares its kind (`capture.target.kinds` or `audio.input.kinds`); `node_types()` gives the binder the descriptors of installed types plus the compositor and the mixer, so a type without an adapter is reported by the binder as unknown and costs nothing.
3. **Settings choose the adapter and the target:** `adapter` (adapter id) and `target` (capture target id or audio endpoint id, both adapter-defined, so they are stored together), plus `cursor`, `width`/`height`, `fps` for video. Missing `target` means a default: the first listed display or camera, the OS default audio endpoint; windows have no default. Unknown keys are ignored so newer documents load.
4. **`instantiate(source, ctx)` builds the instance** for a node the binder created: a `CaptureSourceNode` (render partition; opens its session in `prepare`) or an `InputNode` (audio partition; the stream is opened here in Standby, which the audio input port guarantees holds no device activity). Every capture type shares the capture node's descriptor and every audio type the input node's, so instances always match the graph.
5. **The binder binds audio when asked** (`SceneBinder::with_audio(partition)`): one mixer node (`org.rbs.audio.mixer`) in the audio partition, created with the binder's first sync and kept; audio-only source nodes in the audio partition; sources with video stay in the render partition (their audio crosses through an audio queue, ADR-0026). The mixer's inputs are the sources with an audio output **visible in a program scene** of any canvas, so a microphone in the preview scene only is not on air. `BindReport::mixer` lists the inputs, so the application gives each a strip (an input without a strip is not heard, ADR-0037). Audio filters are not bound yet (`FilterOnAudioSource`). Without `with_audio` the binder behaves as before.
6. **Changed settings make a new node.** The binder remembers each source's settings; when they change, the source's node and its filter nodes are retired and new ones created. `BindReport` lists created and retired source and filter nodes, so the application adopts new instances and takes old ones back without diffing the graph. A device change therefore restarts the source through an ordinary schedule swap; parameters that must change without a restart (sync offset, gain) go through parameter channels instead, not settings.

## Consequences

- The headless record command and `rbs-app` only register platform adapters, bind, instantiate what `created_sources` lists and attach encoders to the compositor and the mixer.
- macOS and Linux need no engine change: registering ScreenCaptureKit, AVFoundation, PipeWire and Core Audio adapters installs the same types.
- Pickers can list targets per type (`SourceTypes::targets`) without knowing adapters.
- Settings edits restart the source (and its filters lose state); live tweaks need parameter channels and, later, a settings schema per type that marks which keys apply live.
- Not decided here: plugin source types (they bring their own descriptor and factory through the plugin SDK), audio filters, per-source monitoring, mixer buses, keeping preview audio warm, and which settings keys become a schema.

## Alternatives considered

- **One type id per adapter** (`org.rbs.source.wgc-display`): ties documents to a platform and to vendors (P3); a scene saved on Windows would not load on macOS.
- **One generic capture type with a `kind` setting:** hides what a source is from the binder and the UI, and lets an edit change a display into a camera under the same node.
- **Instances built inside the binder:** `rbs-scene` would depend on the GPU, the node crates and the ports, and could no longer be used by tools that only edit scenes.
- **Rebuild instances in place on a settings change (take, rebuild, adopt under the same id):** the runtime only swaps idle instances, so it would still stop the node; a new id makes the restart explicit and keeps ADR-0020's rules unchanged.
- **Mixer inputs from every wanted source (program and preview):** puts the preview scene's microphone on air.

## Review

Pending: independent AI session (ADR-0014), before the status changes to Accepted.
