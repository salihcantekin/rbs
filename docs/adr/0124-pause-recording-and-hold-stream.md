# ADR-0124: Pause a recording, hold a stream

- Status: Proposed
- Date: 2026-11-15
- Deciders: Salih (owner)
- Builds on: [ADR-0013](0013-zero-cost-when-unused.md), [ADR-0038](0038-io-graph-nodes.md), [ADR-0043](0043-end-of-stream-on-final-ticks.md), [ADR-0045](0045-recording-tail-on-stop.md), [ADR-0047](0047-several-outputs-per-canvas.md), [ADR-0060](0060-shortcuts-in-app-and-global.md), [ADR-0086](0086-recording-sources-separately-iso.md), [ADR-0088](0088-replay-buffer-and-retroactive-recording.md), [ADR-0089](0089-recording-markers-and-editor-project-export.md), [ADR-0090](0090-recording-protection-disk-lag-and-adaptive-bitrate.md), [ADR-0093](0093-vertical-second-canvas-from-the-same-sources.md), [ADR-0111](0111-vertical-outputs-start-with-the-main-ones.md), [ADR-0119](0119-where-separate-files-are-encoded.md)
- Spec: [recording](../specs/recording.md), [io-nodes](../specs/io-nodes.md), [audio-graph](../specs/audio-graph.md), [control-api](../specs/control-api.md), [ui glossary](../ui/glossary.md)
- Owner decisions: 2026-11-15, the five choices of the analysis (`pause-resume/analysis.md` in the project files), all as recommended

## Context

RBS starts and stops recordings and streams but cannot pause them. OBS has Pause Recording, and users expect it. OBS has no way to pause a stream; users switch the program to a "Be right back" scene, which also changes the recording and takes the program away from the operator.

What the engine has today:

- Outputs are started and stopped one at a time (`Studio::start_output`, `stop_output`). A video encoder or audio encoder is shared between outputs of one canvas when their settings are equal (ADR-0047). The default recording (12 Mbit/s) and stream (6 Mbit/s) do not share.
- The video export already loads one atomic (`ExportEnd::at`) and compares each canvas time with it per tick, to end at a chosen time (ADR-0045). `u64::MAX` means "run".
- `EncoderChange::RequestKeyframe` exists in the codec port, gated by the capability `codec.reconfigure: keyframe`. NVENC, AMF and QSV (`rbs-codec-hw`), VideoToolbox and Media Foundation declare it. The video encoder node sends it only on its own carry-over overflow; nothing outside the node can ask for one (RTMP's `KeyframeRequested` is only counted, and ADR-0090's bitrate change needs the same path).
- Separate files (ADR-0086, ADR-0119) have their own exports and encoders; the replay buffer (ADR-0088) has its own export and encoder.
- The recording manifest and markers (ADR-0089) map master time from `t0` to positions in the files; the stop rewrite (stage 2, in progress) records each file's first sample and length from the output node.
- Rule 10 (P13): no per-iteration check of whether a feature is on in a hot loop; P15: hot-path changes need numbers and the maintainer's sign-off.

Stopping the data of a live stream does not work as a pause: services treat a silent connection as a lost or ended broadcast.

## Decision

### Recording

1. **Pause and resume.** A running recording can be paused and resumed any number of times. Paused, nothing reaches its files; resumed, it continues the same files with no gap in their timeline (media time after resume = master time minus the total paused time). Pause acts on every file of the recording at one instant: the main file, every separate file and picture file (ADR-0086, ADR-0117). Stop while paused finalises at once; there is no tail to keep. The recording's encoder sessions stay open while paused (resume must be immediate and must not fail on a hardware session limit); the estimate (ADR-0087) still counts them.
2. **The cut instant.** The control thread picks `pause_at` = the next render tick's time and `resume_at` = the next render tick's time at resume. Both lie on the frame grid, so the offset is a whole number of frames. Audio uses the same instants; the offset in samples carries the remainder (29.97 fps at 48 kHz: 1601.6 samples per frame), so audio and video never drift apart by more than one sample.
3. **Where the cut is made: before the encoder when the recording owns it.** When the recording's video chain (export and encoder) is its alone, the export takes no canvas with a time in `[pause_at, resume_at)` and stamps later ones `t - paused_total`. When its audio encoder is its alone, the audio encoder node drops the samples in that window (a block straddling an instant is split), fades 5 ms out before the cut and 5 ms in after it, and shifts later blocks the same way. While paused these chains do no readback and no encode. At resume the encoder is asked for a keyframe when it declares `codec.reconfigure: keyframe`, so every segment starts at a keyframe (editors cut there cleanly); without the capability nothing is asked, the stream stays valid.
4. **Where the cut is made: at the output when an encoder is shared.** When an encoder of the recording is shared with another output (a stream "with the same settings"), that stream must keep it, so the recording's output node makes the cut on packets: in decode order, it drops packets from the first one whose pts is at or after `pause_at`; with B-frames, frames shown before `pause_at` that follow a dropped packet in decode order are lost (at most the B-frame count). At resume it asks the shared encoder for a keyframe and passes packets again from the first keyframe whose pts is at or after `resume_at`, shifted by the paused total; audio packets pass from the first one at or after that keyframe's pts. An encoder without the capability resumes at its next natural keyframe (at most the keyframe interval); the state says "resuming" until then. The extra keyframe also goes to the stream; that is allowed.
5. **The host chooses at each pause.** Sharing can change while a recording runs (a stream may join its encoder later), so the studio host decides decision 3 or 4 per chain at every pause from the sharing it already tracks. A chain whose output is paused is not shared with an output started during the pause (as an ended export is not, ADR-0045).
6. **Zero cost when unused.** The pause gate is chosen when an output starts, as a type parameter like ADR-0089's span policy: recordings get `PauseGate`, streams, the replay buffer and the virtual camera get `NoGate`, which compiles to nothing. For a recording that is never paused: the export's window check reuses the existing `ExportEnd::at` load and compare (no new per-tick check); the audio encoder node adds one relaxed atomic load per block; the recording's output node one relaxed load per tick; the timestamp shift is a subtraction of a node-local value. These are hot-path changes (P15) measured against main before merge.
7. **External keyframe requests.** The video encoder node gets a lock-free request (an atomic flag set on the control thread, swapped once per tick in `process`), turned into `EncoderChange::RequestKeyframe` only when the session declares the capability. It is built once and later serves RTMP `KeyframeRequested` and ADR-0090's bitrate changes (their own decisions).
8. **Vertical and several outputs.** Pause acts on the recording the caller names: the main one, the vertical one, or `all`. Pause in the frame's group acts on the recordings that Record started together (ADR-0111 flags).
9. **Manifest and editor project.** The manifest (format 1, added keys) gets `paused_ns` and one `[[pauses]]` table per pause: `at_file_ns` (the join in the files), `from_ns` and `to_ns` (master time from `t0`). Markers are mapped from master time to file time through the pauses. A marker added while paused is placed on the join of that pause and gets `while_paused = true`. The editor project writes one marker per pause on its join ("Paused 2:10"), switched by the recording setting "Pauses become markers in the editor project" (on by default). The span tracking of ADR-0089 stage 2 reads the timestamps the output node writes, after the gate, so first sample and length stay right.
10. **Other features.** The replay buffer keeps running during a pause. Recording protection (ADR-0090) does not watch disk speed or bitrate while paused and computes "time left" from the recorded rate. "Record when the stream starts" is unchanged.

### Stream

11. **No pause of the data; a hold instead.** A live stream is never stopped by a pause. *Hold stream* keeps the connection and sends a hold picture with silence, while the program, the preview and the scenes stay free to edit. Resume stream returns to the program at the next frame. The stream's timestamps keep running.
12. **Hold picture, phase 2a.** `freeze` (default): the stream's export stops reading back and hands the encoder its last converted frame again on each tick; `black`: a black frame converted once. The audio encoder node gets silence in place of the mix (the gate of decision 3 in its "silence" mode). Phase 2b adds `scene`: a scene of the collection chosen in Settings, Stream, rendered by an extra compositor added to `RENDER` and linked to the stream's export only while holding, with that scene's sound when the user picks it; its sources open only then (P13).
13. **Hold with a shared encoder.** When the stream shares an encoder with a recording, holding the stream also pauses that recording (decision 4 on its side), so the hold picture never goes into the file. Keeping the recording of the program while a shared stream holds is not offered.
14. **Break.** One command holds every live stream and pauses every recording; the same command resumes both.

### Control and UI

15. **Control API 1.57** (number final at merge). `record.pause {canvas?}`, `record.resume {canvas?}`, `record.pause_toggle {canvas?}`; `stream.hold {picture?}` (`freeze`, `black`, `scene` once 2b exists), `stream.resume`, `stream.hold_toggle`; `output.break_toggle`. All `control` scope, live class; the stream methods change what is on air and take the on-air permission check. `conflict` when nothing runs to pause or hold, or it is already in that state. `record.state` and `record.canvas_state` add `paused`, `paused_ms`, `recorded_ms`; `state` stays `recording` while paused, so an older client never thinks the recording stopped. `stream.state` adds `held` and `held_ms`. Every method is a registry command (ADR-0050, ADR-0060), so shortcuts (global allowed), the RBS menu, Stream Deck and MCP get it. The obs-websocket compatibility layer, when built, maps `PauseRecord`, `ResumeRecord`, `ToggleRecordPause` and the paused output state.
16. **UI (visual approval before merge, rule 16).** A round Pause button beside Stop recording, only while a recording runs; paused, it becomes Resume and the indicator reads PAUSED with the stopped recorded time in the warning colour, never the live red. A round Hold button beside End stream while live; holding, the indicator reads ON HOLD. The status bar shows the paused or held time. Live controls get the same commands; vertical chips get Pause in their menu. Settings, Stream: what the hold sends (picture, sound). Settings, Recording: the pause-marker switch. Glossary: Pause recording, Resume recording, Hold stream, Resume stream, Break.

### Order of work

17. Phase 1: this ADR and specs; the encoder keyframe request (decision 7); the gates and `Studio::pause_output` / `resume_output` with `TestClock` tests; host, API, manifest and editor project (after ADR-0089 stage 2 merges); UI; hardware runs on Mac (VideoToolbox) and SC-PC (NVENC, AMF). Phase 2a: hold freeze and black, Break. Phase 2b: hold scene.

## Consequences

- Pausing matches OBS and goes past it: a recording that shares its encoder with the stream can pause, owned encoders do no work while paused, separate files pause together, and every join shows in the editor project.
- Streams can take a break without ending the broadcast or touching the program.
- New public contract: `rbs-record` (`pause_output`, `resume_output`, hold calls, `Progress` paused fields), io-node behaviour (export window, audio encoder gate, output gate, encoder keyframe request), manifest keys, control API 1.57. Each needs this ADR's independent review.
- Hot-path changes in the export, video and audio encoder nodes and the output node: numbers with `measure-performance` and the `hot-path-review` checklist, then the owner's sign-off.
- Tests: A/V offset after three pauses within one sample and one frame; pause and resume on frame and block boundaries and inside a block; B-frame cut on a shared chain; a capability-less encoder resuming at its natural keyframe; separate files sharing the join; stop while paused; a stream joining during a pause gets its own encoder; markers while paused; manifest and FCPXML pause markers; hold keeps timestamps and the connection; holding a shared stream pauses the recording. All on `TestClock`, no sleeps.
- Follow-ups not decided here: "Resume into a new file" as an option; a time limit or auto-resume for hold; replay buffer pause.

## Alternatives considered

- **Cut always at the output node.** One place for every case, but owned encoders would keep encoding paused content, and resume would always need a keyframe. Rejected: wasted GPU and encoder work (rule 17).
- **Cut always before the encoder.** Cheapest, but impossible while the encoder also feeds a stream. OBS takes this path and refuses to pause in that case. Rejected.
- **Insert a gate node into the graph at pause time.** No new check on the run path, but every pause would relink the graph mid-recording, with a glitch risk at the moment the user wants a clean cut. Rejected.
- **A file per segment.** Simple, but the user asked for OBS's behaviour (one file). Kept as a later option.
- **Stop sending on a stream pause.** Services end or degrade the broadcast. Rejected.
- **Switch the program to a "BRB" scene.** Possible today by hand; it changes the recording and the preview and takes the program away. Hold is the better default; the hold scene (2b) gives the same look without those costs.

## Review

Independent review per ADR-0014: pending.
