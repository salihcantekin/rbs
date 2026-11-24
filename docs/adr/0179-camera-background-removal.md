# ADR-0179: Camera background removal: key and reference-frame options first, a person-segmentation port later

- Status: Accepted (2026-11-24)
- Date: 2026-11-24
- Deciders: Salih (owner)
- Builds on: [ADR-0053](0053-item-style-and-video-filter-chains.md) decisions 2, 6 and 7 (filters are GPU nodes of a per-source chain; background removal gets its own port), [ADR-0006](0006-three-tier-plugin-model.md) (AI models belong to the Process tier), [ADR-0013](0013-zero-cost-when-unused.md), [ADR-0162](0162-extension-ready-registries-and-plugin-order.md)

## Context

The owner asked (2026-11-24) whether background removal for cameras needs AI at all, and wants the OBS plugin looked at. Nothing is built: `org.rbs.filter.chroma-key` and `org.rbs.filter.blur` exist ([video filters](../specs/video-filters.md)); ADR-0053 only promises "the segmentation port ADR". Neither feature starts now; this ADR fixes the direction so the work can start later without a new round of questions.

**How the OBS plugin works** (royshil/obs-backgroundremoval, now occ-ai; from its README and release notes, the per-frame code not read line by line). It is an OBS video filter that runs a small portrait-segmentation network through ONNX Runtime on the CPU, with optional CUDA, ROCm, MIGraphX or CoreML execution providers. Models include MediaPipe selfie segmentation, SINet and PP-HumanSeg. Each frame is scaled down to the model size (about 256 px), run through the network, and the mask is scaled back up, then post-processed (threshold, feather or blur, temporal smoothing, an expand slider) and used as the alpha channel. The frame goes through system memory, which is why it costs CPU and a thread even on a good machine, and why OBS users report high load.

**Is AI required?** Not for every case:

| Method | Needs | Works when | Fails when |
| --- | --- | --- | --- |
| Chroma key (exists) | A green or blue backdrop | A real screen and even light | No backdrop |
| Reference-frame difference | A frame of the empty background, a fixed camera | Fixed camera and light, a plain room | Moving camera, auto exposure or auto white balance, shadows |
| Depth camera mask | A depth camera (RealSense, Kinect, Apple LiDAR cameras) | The device exists | Ordinary webcams |
| OS person segmentation | Windows Studio Effects (NPU PCs), Apple Vision `VNGeneratePersonSegmentationRequest` | The OS offers it | Older PCs, Linux |
| Own neural segmentation | A model of about 1 MB to 10 MB, a GPU or CPU run each frame | Any ordinary webcam, any room | A weak GPU (cost), hair and hands (edge quality) |

Only the last two give "remove my room" with an ordinary camera and no backdrop, which is what viewers expect from the feature. The first three are cheap and need no model.

## Decision

1. **Background removal is a video filter type, not a separate source feature.** It sits in a camera's filter chain (ADR-0053 decision 6) as registered types with type ids (`org.rbs.filter.<name>`, ADR-0162 and rule 19), declares its settings as an `rbs-params` schema, and is Standby (no node, no model loaded, no thread) while disabled (P13). *Reference background* is its own filter type, `org.rbs.filter.reference-background`; the segmentation filter is `org.rbs.filter.background-removal`. Segmentation backends are entries of a backend registry with type ids, never a closed list or a `match` in engine code.
2. **Staged, cheapest first, each stage a separate step.**
   1. *Key tools without a model*: improve chroma key if needed (a colour key and a luma key are small additions) and add **Reference background** (capture the empty room once; a per-pixel difference with a threshold and feather, all on the GPU). The user is told plainly that it needs a fixed camera and fixed exposure.
   2. *Segmentation port*: a `PersonMask` port, `rbs-segment` (no vendor or runtime dependency, P1), takes a lent frame and returns a coverage mask. A capability says which memory kind the backend takes and gives (GPU texture, CPU buffer) and whether it is hardware-assisted (P3). Engine code never branches on the backend.
   3. *Backends*, each its own adapter crate behind the port, chosen by capability and offered in a settings list: OS backends first (Apple Vision; on Windows a Windows ML model run), then a bundled small model through a runtime that has a GPU provider (ONNX Runtime or a Rust inference crate; the choice and its licence, size and `cargo deny` result are decided in the backend's own PR).
3. **No readback on the default path (P4, rule 6).** A backend that takes GPU textures is preferred. A backend that needs CPU pixels must say so in its capabilities, and the filter then downscales on the GPU first and reads that small image back (about 256 px, under 0.3 MB). This is an **explicit, named exception** to ADR-0053 decision 6 ("no readback") and to the no-extra-CPU-copy rule, limited to the downscaled mask input: the map is asynchronous and never waited on during render (P6), and the change needs a `hot-path-review`, measurements and a human maintainer's approval (P15) before it merges. The full-resolution frame never leaves the GPU.
4. **Off the render thread.** Inference runs on its own worker. OS API backends run in-process on that worker; the bundled model runs in-process too, as a Rust or system runtime on that worker, until the Process tier exists (ADR-0162 order: sandbox, process, native). A model that needs a separate process waits for that tier and a texture-sharing design there; this ADR does not pre-decide it. The render thread uses the latest finished mask and never waits (P6); a stale mask is reused and blended by a temporal filter. Mask latency is documented and the filter shows no picture it cannot cover: until the first mask exists the filter publishes nothing, so the compositor skips the layer (P7) and the unfiltered input is never shown (ADR-0053 decision 6).
5. **Crates.** The port `rbs-segment` is a port crate with no vendor dependency. The nodes live in a new engine crate `rbs-filters-segment` (depends on `rbs-filters`' filter API, `rbs-gpu`, `rbs-graph`, `rbs-core` and the `rbs-segment` port), because `rbs-filters` must keep its ADR-0053 dependency list; this is a recorded layering addition, and `check-layers` is updated in the same PR. Backends are adapter crates wired by `rbs-app`, `rbs-cli` and `rbs-mcp` only.
6. **Mask refinement on the GPU:** threshold, feather, expand and temporal smoothing are cheap shaders in the filter, shared by every backend and by Reference background.
7. **Everything runs on the user's computer.** No frame, mask or model query goes to a server (ADR-0139 decision 7 spirit; ADR-0046). The method list in the filter shows what this computer supports: *Automatic*, *System* (OS segmentation: Apple Vision, Windows Studio Effects or Windows ML), *Local model* (the bundled small model through a runtime) and *Reference background*. *Automatic* prefers System, then Local model. Windows Studio Effects is not a `PersonMask` backend: it is a camera-driver effect set through Media Foundation extended camera controls, so it is a *capture-side* capability of a camera (a later, separate decision). A method the computer lacks is not listed (P3: by capability, never by OS name in engine code).
8. **Models are data, not code.** Model files ship as an add-on or download chosen by the user (ADR-0006 tiers), with their licence recorded in `THIRD-PARTY`; the app installer does not bundle a model by default (size, zero cost when unused). No model is downloaded without the user asking.
9. **Measure before accepting a backend.** Each backend states CPU, GPU and memory cost at 1080p30 on the reference machines (M4 Pro, RTX 4090 + AMD iGPU) with the `measure-performance` skill. The filter, when off, adds no cost (verified by a test of the schedule).
10. **Reference frame data.** The captured reference frame is stored in user data (not in the scene document) and sent to the filter as a resource handle through the parameter channel (ADR-0053 decision 7); the node never reads files.
11. **UI.** A visual change needs the owner's proposal and dark and light screenshots first (rules 15, 16, 18): the filter's panel uses `SettingRow`s, and its wording uses the glossary term "Background removal" (added to `docs/ui/glossary.md` in the same change), with the method list shown from the Standard experience level up.

## Consequences

- Users without a green screen get something useful early (Reference background) at near-zero cost, and the AI step is optional and swappable.
- A new port crate, its conformance tests, and a spec `docs/specs/person-mask-port.md` (written before code) follow; `video-filters.md` gains the new types and the transparent-until-ready rule.
- OS backends add platform-specific adapters (Windows, macOS); Linux has only the bundled model, so its quality and cost need measuring before it is promised.
- Edge quality (hair, hands) will not match a cloud-grade matting model; the ADR promises "good enough for a streamer's camera", not studio matting.

## Alternatives considered

- **Embed ONNX Runtime and a model like the OBS plugin, as the only method.** Simplest, but CPU readback of each frame, a large dependency in the app, and no option for users with a backdrop or a fixed camera. Rejected as the only method; kept as one backend.
- **Chroma key only.** Zero cost, but needs a backdrop; does not answer the request.
- **Ship it as a third-party plugin only.** Plugins are not built yet (ADR-0006), and a good built-in filter chain is part of the product vision (beyond OBS parity, rule 17).
- **Cloud segmentation.** Latency, privacy and a server we do not run (ADR-0139 reasoning). Rejected.

## Review

- 2026-11-24, independent AI session (fresh, ADR + specs + code only): **changes requested**. Findings: (1) the readback exception was unnamed, (2) Windows Studio Effects is a camera-driver effect, not a frame backend, (3) Process-tier contradiction, (4) crate layering unnamed, (5) rule 19 registries, (6) before-first-mask rule, (7) glossary and experience wording, (8) reference-frame storage. All eight were applied in the decisions above (decisions 1, 2, 3, 4, 5, 10, 11).
- Owner approved on 2026-11-24 ("accept and start"). The edits were not re-reviewed by the reviewer, at the owner's request for a short review; the readback exception still needs its own hot-path review before code merges.
