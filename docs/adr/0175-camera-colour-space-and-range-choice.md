# ADR-0175: A camera's colour space and range can be chosen

- Status: Accepted
- Date: 2026-11-22
- Deciders: Salih (owner)

## Context

A camera's Y'CbCr frames are decoded with the matrix (BT.601, BT.709) and code range (limited, full) its adapter reports. Media Foundation takes them from `MF_MT_YUV_MATRIX` and `MF_MT_VIDEO_NOMINAL_RANGE`, with Media Foundation's defaults when absent: BT.601 below 720 lines, BT.709 from 720, limited range (`rbs-capture-mf` `color_of`). Some devices report them wrongly or not at all; HDMI capture cards pass through whatever the camera sends. A wrong range makes blacks grey or crushed; a wrong matrix shifts skin and greens.

OBS lets the user pick the colour space and range of a video capture device. The owner asked for the same on 2026-11-22, while comparing a Cam Link 4K in OBS and RBS ([ADR-0174](0174-sdr-video-is-display-referred.md) fixed the tone curve, the main cause of that difference). Tone (brightness, contrast, saturation) is already the Colour correction filter's job.

## Decision

1. **Port.** `CaptureRequest` gains `color_matrix: Option<MatrixCoefficients>` and `color_range: Option<ColorRange>` (`None` is Automatic), set with `with_color(matrix, range)`. `CaptureRequest::color_for(device)` returns the device's colour with the user's matrix and range applied, and an RGB colour (an `Rgb` matrix) unchanged. The struct is `#[non_exhaustive]`, so the fields are a minor change.
2. **Capability.** An adapter that honours them declares the flag `capture.color.override`. It calls `color_for` once when it builds the session's `VideoFormat`, never per frame, so `format()` and every frame carry the choice (capture port invariant 4). Adapters without the flag ignore the fields. Media Foundation and the test adapter declare it. AVFoundation does not yet: its `'420v'` frames come from an OS conversion that is always limited range, so tagging them Full would mis-decode them; requesting `'420f'` for Full is a follow-up. PipeWire delivers RGB only.
3. **Source keys.** A camera source stores `color_matrix` (`"bt709"`, `"bt601"`) and `color_range` (`"limited"`, `"full"`); absent is Automatic; other values are refused. They are option keys (Reset source removes them, Paste attributes copies them). `source.set_settings` sets and clears them (`null`); no new API method. They are plain keys until the camera type has an `rbs-params` schema (rule 19), and move into it then.
4. **Effect.** A change replaces the camera's node like a mode change (the binder's settings replace, break-before-make for exclusive devices). No per-frame cost anywhere (P13).
5. **UI.** The Source settings window's Video card adds two rows after Frame rate for a camera whose adapter declares the flag: Colour space (Automatic, Rec. 709, Rec. 601) and Colour range (Automatic, Limited, Full), each a `Select` with a reset to Automatic. They are part of the window's draft (Apply, OK).

## Consequences

- Users can fix a camera that reports its colours wrongly, as in OBS, on Windows first.
- Specs: `capture-port.md` (request fields, capability), `scene-sources.md` (keys), `ui-design-system.md` (window rows).
- Tests: `rbs-capture` `color_for` unit tests; the test adapter's camera delivers the chosen colour in `format()` and frames; `rbs-sources` validates the keys and keeps them option keys; `rbs-app` sends the choice and `null` for Automatic.
- Follow-ups: AVFoundation (`'420f'` request); the keys moving into the camera's parameter schema; a conformance-suite check for adapters declaring `capture.color.override`.
- An unknown stored value (from a newer file) opens the camera as Automatic; `source.set_settings` refuses it.

## Alternatives considered

- **Override per frame in the capture node.** Works for every adapter without a port change, but adds a per-frame test (P13) and would tag frames whose real range an OS conversion fixed (AVFoundation). Rejected.
- **Only a Colour correction filter.** It changes tone after decoding; it cannot undo a wrong matrix or range exactly. Rejected.
- **BT.2020 and HDR choices.** No SDR camera path needs them; HDR is out of scope (ADR-0174). Left out.

## Review

- 2026-11-22, a second independent AI session verified this ADR against the code diff: decisions met; fixed after it: an unknown stored value falls back to Automatic at open, import order, spec sentence placement, conformance wording.
- 2026-11-22, independent AI session (fresh context; reviewed this decision as decision 6 of the ADR-0174 draft, with specs and code): **changes requested**, all addressed here: a capability so the UI shows the rows only where honoured (decision 2); AVFoundation's range is an OS conversion, not a tag (decision 2); the port contract written with types and semver note (decision 1, `capture-port.md`); the requested conformance check is an adapter test for now (`rbs-capture-test` `camera_frames_carry_the_users_colour_choice`: `format()` and frames), and a check in `rbs_capture::conformance` for adapters declaring the flag is a follow-up; the source keys listed in known keys, validation, option keys and the spec tables, with Reset meaning Automatic and no new API method (decision 3); the rule-19 note (decision 3). It confirmed that a settings change replaces the node, with no per-frame cost.
- Owner, 2026-11-22: approved this work's ADRs and visual changes in advance ("Görsel onayları, adr'ların hepsini onaylanmış kabul ediyorum").
