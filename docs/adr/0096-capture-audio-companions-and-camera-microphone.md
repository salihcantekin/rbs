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

1. **A companion is a normal audio source.** Checking "Capture audio too" on a picture source adds an audio-only source named after it ("Chrome sound", "System sound", "Cam mic") in the same scene and group, with its own Mixer strip, fader, mute, sync offset and filters. Nothing new in the mixer or graph. Unchecking is one undo step that removes the source with its filters, fader and offset; undo and redo restore them exactly. A later check creates a fresh source with defaults.
2. **Scene model.** The audio source record gets an optional `companion_of` (the picture source id). Whether older readers keep it is decided in the implementation PR by a round-trip test against the collection loader; if an older reader would drop it on save, the scene format number is bumped (reviewed with the PR) instead of claiming "unchanged". Rules: deleting the picture deletes the companion in one undo record; deleting only the companion clears the link; a picture placed in several items or scenes has one companion, and the companion's sound is audible while at least one item of the picture is visible. The binder gains this cross-source dependency (like the ducker key; hidden items already go Standby), not a flag the mixer tests per block (rule 10).
3. **No pid in the capture port.** `CaptureTarget` does not gain `owner_pid` (capture-port.md keeps `app` as a UI-only label, not a process id). A window companion is matched to an `Application` endpoint of audio-io-port (`EndpointKind::Application`) by `CaptureTarget::app` equality (the ADR-0077 rule) plus an adapter-side hint. The stored setting is the endpoint id and app label, never a pid, and it is re-resolved at every open. Per-app capture takes the application's whole process tree on Windows; macOS taps the resolved pid inside the adapter, a transient adapter-internal value. Several windows of one application share one companion sound.
4. **Excluding RBS's own sound.** audio-io-port gains an `OutputLoopback` open option `exclude_own_process: bool` and the capability `audio.loopback.exclude_self` (WASAPI process-loopback exclude mode on Windows 10 2004+; a CoreAudio global tap with an exclusion list on macOS 14.4+). Display companions always set it; it is not a user setting. Without the capability (older OS, other platforms) the Display companion checkbox is hidden with a reason, instead of risking echo. `audio-io-port.md` is updated in the same change.
5. **Related device hint.** Camera targets gain `related: Option<RelatedDevice>` where `RelatedDevice { kind: Audio, id: String }` is an opaque id the audio adapter can resolve; if it does not resolve, the suggestion is not shown. The field is `#[non_exhaustive]`, additive (minor). Matching is adapter-side with a conformance test. Engine code never looks at vendor names (P3).
6. **Availability.** A closed window or stopped application puts the companion in the existing Unavailable state with a reason and "Choose another..." (ADR-0077); it keeps its settings. Per-app sound on an unsupported OS: the Window checkbox is hidden with a reason. If macOS System Audio Recording permission is denied the tap delivers silence; the UI says so on the strip hint.
7. **Camera microphone (E).** In the Add source dialog for a Camera with a resolvable `related` device: "Cam mic found: <name>. Add it too?", checkbox preselected. If that microphone already exists as a source, the suggestion is not shown. With no pairing the checkbox is hidden and users add a microphone by hand.
8. **Echo.** An info button explains that capturing system sound plus an app's sound can play it twice. The app warns once when a new companion overlaps another active strip (for example Display system sound plus a Window companion). Advice, not a block.
9. **Control API.** The next free minor at merge (1.19 is the ducker; a typed-`unavailable` change may take the next): `source.add` accepts `with_audio: bool` (default false); `source.set_companion_audio {source, enabled}` (scope `control`, one undo step); source rows report `companion_of`. An unsupported OS or missing capability returns the existing `unavailable` error kind. The UI uses the same calls (U20 parity).
10. **Zero cost (P13).** Off by default: no companion means no source, device handle or thread. Pairing lookup (`related`) and endpoint enumeration run only while the Add source dialog is open, never at startup.
11. **Hot path.** None edited: a companion is an ordinary audio source node.

## Consequences

- One checkbox replaces a manual two-step setup, and filters (gate, ducker) work on companions unchanged.
- The capture port grows one optional field and audio-io-port one open option and capability: two minor contract changes (capture port, audio-io port) reviewed with this ADR.
- Process loopback and taps cannot be tested in the cloud: device tests on the Windows machine and the Mac use one device slot, with numbered steps.
- Echo is possible and only advised against.

## Alternatives considered

- **Mixed audio inside the picture source.** Breaks the one-source-one-strip rule and the filters, fader and sync offset per sound.
- **Always on.** Costs a device handle for users who do not want sound and risks surprise echo.
- **User-selectable self-exclusion.** A setting that is only ever harmful when off.
- **Vendor-specific camera microphone lookup.** Violates P3; the generic `related` id covers it.

## Review

Round 1, independent AI session (2026-11-10): changes requested (9 concerns: pid in the wrong port, undefined self-exclusion option and capability, cross-port `related` type, unproven scene format claim, hide/delete/undo semantics, API version, missing cases, P13 for lookups). All resolved in this text. Round 2 (same day): approve. Notes for the implementation PR: the rule that a companion is audible while one of its picture's items is visible is new binder behaviour (a cross-source dependency, like the ducker key), so scene-model.md and audio-graph.md are updated with a binder test; the round-trip test result decides the scene format number and is recorded in the PR; the capture-port and audio-io-port changes land with conformance tests. Owner product approval: Salih, 2026-11-10 (proposal D and E, recommended decisions).
