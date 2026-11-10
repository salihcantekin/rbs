# ADR-0096: "Capture audio too" companions and the camera's own microphone

- Status: Proposed
- Date: 2026-11-10
- Deciders: Salih (owner)
- Spec: [capture-port](../specs/capture-port.md), [scene-model](../specs/scene-model.md), [control-api](../specs/control-api.md), [audio-graph](../specs/audio-graph.md)
- Builds on: [ADR-0077](0077-source-health-retarget-and-settings-window.md) (Unavailable state), [ADR-0082](0082-audio-filter-chains-on-sources.md), [ADR-0095](0095-audio-ducker-and-key-tap.md), [ADR-0013](0013-zero-cost-when-unused.md)

## Context

Window, Display and Camera sources show a picture only. Users who capture a game window or a camera also want its sound, and today they must add and name a second audio source by hand and keep it in sync with the picture. The owner approved the product shape on 2026-11-10 (proposal D and E, decisions 1 to 3): a checkbox "Capture audio too", off by default; the display never captures RBS's own sound; the camera's microphone is suggested in the Add source dialog only.

Facts (analysis-and-roadmap section 3, hardware-verified): Windows 10 2004+ has process loopback with include and exclude modes; macOS 14.4+ has CoreAudio process taps by pid; a camera's microphone is an ordinary audio input device that the OS may associate with the same USB device.

## Decision

1. **A companion is a normal audio source.** Checking "Capture audio too" on a picture source adds an audio-only source named after it ("Chrome sound", "System sound", "Cam mic") in the same scene and group, with its own Mixer strip, fader, mute, sync offset and filters. Nothing new in the mixer or graph. Unchecking removes it (one undo step each way).
2. **Scene model hint.** The audio source carries a `companion_of` hint (the picture source id) in its source record. It is a hint: no behaviour depends on the graph, and a scene without it loads unchanged (scene format unchanged, optional field ignored by older readers). Deleting the picture deletes the companion; a hidden or removed picture hides the companion from the program mix, so the sound is never on air without its picture. A user who edits the companion's own settings keeps them when the checkbox is toggled off and on again only within one undo history.
3. **Capture port addition (minor).** `CaptureTarget` of a window gains `owner_pid: Option<u32>`; camera and audio targets gain `related: Option<TargetId>`, a generic id of a paired device. Adapters fill them best effort; engine code never looks at vendor or model names (P3).
4. **Audio adapters.** The system loopback source gets an option `exclude_process: Option<u32>`: WASAPI process loopback in exclude mode on Windows, a CoreAudio tap with the process excluded on macOS. Per-app capture uses include mode (Windows) or a tap by pid (macOS). Display companions always set `exclude_process` to RBS's own pid; it is not a user setting, because capturing RBS's own sound would double everything. Window companions follow the window's application (`owner_pid`) and are re-pickable through the ADR-0077 Source tab.
5. **Availability.** Per-app sound needs Windows 10 2004+ or macOS 14.4+; on older systems the checkbox for Window is hidden with a reason. A closed window or stopped application puts the companion in the existing Unavailable state with a reason and "Choose another..." (ADR-0077); the companion keeps its settings.
6. **Camera microphone (E).** When adding a Camera whose adapter reports `related`, the Add source dialog preselects the checkbox on that device and says "Cam mic found: <name>. Add it too?". With no pairing the checkbox is hidden and the user adds a microphone by hand as today.
7. **Echo.** An info button next to the checkbox explains that capturing system sound or another app's sound can play the same audio twice. The app warns once when a new companion overlaps another active strip (for example Display system sound plus a Window companion). The warning is advice, not a block.
8. **Control API 1.20 additions** (version number to be taken when the work lands, the typed-`unavailable` change in #257 may take 1.20 first): `source.add` accepts `with_audio: bool` (default false); `source.set_companion_audio {source, enabled}`. Errors name an unsupported OS version.
9. **Zero cost (P13).** Off by default: no companion means no audio source, device handle or thread. The `companion_of` hint adds nothing to the graph.
10. **Hot path.** No hot path is edited: the companion is an ordinary audio source node. Adapter threads follow their own contracts and conformance tests.

## Consequences

- One checkbox replaces a manual two-step setup, and filters (gate, ducker) work on companions unchanged.
- The capture port grows two optional fields: a minor contract change reviewed with this ADR.
- Process loopback and taps cannot be tested in the cloud: device tests on the Windows machine and the Mac use one device slot, with numbered steps.
- Echo is possible and only advised against.

## Alternatives considered

- **Mixed audio inside the picture source.** Breaks the one-source-one-strip rule and the filters, fader and sync offset per sound.
- **Always on.** Costs a device handle for users who do not want sound and risks surprise echo.
- **User-selectable self-exclusion.** A setting that is only ever harmful when off.
- **Vendor-specific camera microphone lookup.** Violates P3; the generic `related` id covers it.

## Review

Pending: independent review per ADR-0014 (port change and control API change). Owner product approval: Salih, 2026-11-10 (proposal D and E, recommended decisions).
