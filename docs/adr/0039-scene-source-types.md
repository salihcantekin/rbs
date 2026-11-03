# ADR-0039: Scene source types map to port nodes through a registry; the binder routes audio to a mixer per canvas

- Status: Accepted
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
5. **The binder binds audio when asked** (`SceneBinder::with_audio(partition)`, an audio partition): **one mixer node (`org.rbs.audio.mixer`) per canvas** in the audio partition, kept while the canvas exists, so each canvas's outputs carry only that canvas's audio (a vertical canvas streamed elsewhere does not get the main canvas's microphone). Audio-only source nodes go into the audio partition. A mixer's inputs are the audio-only sources **visible in its canvas's program scene**, so a microphone in the preview scene only is not on air. `BindReport::mixers` lists the inputs per canvas; the producer is the source node, in the mixer's partition, so it is the strip's producer, and the application keeps strip settings per source and rebuilds strips after every sync (an input without a strip is not heard, ADR-0037). A source with video and audio outputs binds only its video (`AudioOnVideoSource`): its audio would cross a partition queue whose reader the mixer sees as the producer, which the binder cannot name yet; no built-in type has both. Audio filters are not bound yet (`FilterOnAudioSource`). Without `with_audio`, audio-only sources are `NoVideoOutput` as before.
6. **Changed settings make a new source node, break-before-make.** The binder remembers each source's settings; when they change, the source's node is retired and a new one of the same type and partition created; its filter nodes stay and the chain is linked to the new node. `BindReport` lists created, retired and replaced source nodes and created and retired filter nodes (including nodes someone else removed), so the application adopts new instances and takes old ones back after the runtime released them, without diffing the graph. Because the runtime prepares a new node before it releases the old one, and an exclusive device (most cameras) refuses a second session, the composition root applies a replacement in two steps: first with the new node in Standby (the old instance is released), then with it Active. Parameters that must change without a restart (sync offset, gain) go through parameter channels, not settings.
7. **Compatibility.** The binder changes are additive: `BindReport`, `BindProblem` and `BindError` are `#[non_exhaustive]`; a binder without `with_audio` binds what it did before, except that a settings change now gives the source a new node id.

## Consequences

- The headless record command and `rbs-app` only register platform adapters, bind, instantiate what `created_sources` lists and attach encoders to the compositor and the mixer.
- macOS and Linux need no engine change: registering ScreenCaptureKit, AVFoundation, PipeWire and Core Audio adapters installs the same types.
- Pickers can list targets per type (`SourceTypes::targets`) without knowing adapters.
- Settings edits restart the source through a break-before-make swap (filters keep their state); live tweaks need parameter channels and, later, a settings schema per type that marks which keys apply live.
- Not decided here: plugin source types (they bring their own descriptor and factory through the plugin SDK), audio of sources that also have video, audio that does not depend on the scene (a global microphone or system audio; today it must be placed in every program scene), audio filters, per-source monitoring, mixer buses, keeping preview audio warm, and which settings keys become a schema.

## Alternatives considered

- **One type id per adapter** (`org.rbs.source.wgc-display`): ties documents to a platform and to vendors (P3); a scene saved on Windows would not load on macOS.
- **One generic capture type with a `kind` setting:** hides what a source is from the binder and the UI, and lets an edit change a display into a camera under the same node.
- **Instances built inside the binder:** `rbs-scene` would depend on the GPU, the node crates and the ports, and could no longer be used by tools that only edit scenes.
- **Rebuild instances in place on a settings change (take, rebuild, adopt under the same id):** `Controller::take` returns idle instances only, so the node must leave the schedule first anyway, which is the same two applies as break-before-make; a new id makes the restart visible in the report and the graph and keeps ADR-0020's rules unchanged.
- **Recreate the filters with the source:** simpler, but loses filter state and GPU resources for a change that does not concern them.
- **One mixer for all canvases:** mixes every canvas's program audio into every output; a single-valued API would be costly to change later.
- **Mixer inputs from every wanted source (program and preview):** puts the preview scene's microphone on air.
- **Link a video source's audio through the partition queue now:** the strip producer would be the queue reader's reserved id, known only after lowering; deferred until the binder or the engine can report it.

## Review

- Reviewer: independent AI session (fresh context: proposal, specs and code only), 2026-11-03. Verdict: changes requested, 13 findings. Resolution:
  1. Video+audio source got a strip that matches nothing (its audio crosses a queue): such sources now bind video only and report `AudioOnVideoSource`; mixers take audio-only sources (decision 5).
  2. New node prepared while the old exclusive device still runs: break-before-make written into decision 6 and the scene sources spec (composition root steps); `BindReport::replaced_sources` added.
  3. One global mixer mixed all canvases: one mixer per canvas (`mixer(canvas)`, `BindReport::mixers`, `retired_mixers`).
  4. Nodes removed by someone else were not reported retired: `heal` reports them; the random test removes nodes externally.
  5. Settings change recreated filters: filters stay, the chain is relinked.
  6. Spec took retired instances back before apply: steps reordered (take after collect).
  7. `target` without `adapter` went to the best adapter: now the adapter that lists it, else the best (picker ids); test added.
  8. Strips keyed by node id: spec and example keep strips per source and rebuild them from the mixer inputs.
  9. Scene-independent audio: listed under "Not decided here".
  10. Test comment fixed. 11. Decision 5 wording fixed. 12. Type id equality test added; the audio partition must not be the render partition (documented). 13. Graph type ids documented in the spec.
- Owner approval: Salih, 2026-11-03 (early-phase merge policy: ADRs may be marked Accepted after the independent review).
