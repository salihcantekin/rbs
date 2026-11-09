# ADR-0089: Recording markers and an editor project (FCPXML) written from the recording manifest

- Status: Proposed
- Date: 2026-11-09
- Deciders: Salih (owner)
- Builds on: [ADR-0004](0004-single-master-clock.md), [ADR-0021](0021-scene-collection-model.md), [ADR-0050](0050-command-registry-menus-and-toolbar.md), [ADR-0051](0051-user-data-profiles-and-scene-collections.md), [ADR-0060](0060-shortcuts-in-app-and-global.md), [ADR-0086](0086-recording-sources-separately-iso.md)
- Spec: [recording](../specs/recording.md), [control-api](../specs/control-api.md), [user-data](../specs/user-data.md), [shortcuts](../specs/shortcuts.md)

## Context

After a long take the first job in an editor is finding the moments that mattered and putting the files on a timeline. With separate files (ADR-0086) the second job gets bigger: five files that must be imported, placed and aligned. RBS knows both things at record time: when the user pressed "this is good" and when the scene changed, and exactly how the files relate (same start, same clock, sizes, frame rates). The owner approved two ideas: markers placed during recording with a shortcut, and an editor-friendly project written together with separate recordings.

Constraints: the files are produced by hot-path code that must not gain work for this feature (P15); the project writer is a pure conversion from data we already have; the editor formats are external contracts that must be tested against real editors, not assumed; Windows and macOS only.

## Decision

1. **Markers.** While a recording runs, `marker.add` (a registry command with a shortcut, global shortcuts allowed by ADR-0060, a Live controls button, control API `marker.add {label?, color?}`) appends a marker: master-clock time relative to the recording's `t0`, a kind (`manual`), an optional label (default "Marker N"), a colour from a small fixed set (tokens, not free colours). Outside a recording the command is disabled with the reason. A marker is a plain event on the control thread: no node, no per-frame work, no effect on the encoders (P13).
2. **Automatic markers (settings, each off or on, saved).** *Scene changes* (a chapter marker named after the scene the program switched to, from the live program-switch event, ADR-0021), *Recording protection events* (ADR-0090: encoder behind, disk low, as markers with their text, so the editor sees where a glitch is), *Stream start and end* when a stream runs during the recording. Default: scene changes on, the others off.
3. **Reaction-time offset (a user setting).** `marker_lead` moves manual markers earlier by 0 to 30 seconds (default 0), because a person presses the key after the moment. The marker keeps its real press time too, in the manifest.
4. **Storage: the recording manifest.** Markers are written to the recording manifest next to the media (`recording.rbs.toml`, a TOML document with `format` and `kind` like every user-data document, ADR-0051; specified once in `recording.md`, with ADR-0086 adding file entries; it is written for a plain recording too whenever markers exist or project export is on). Markers are appended to a pending list in memory and flushed to the manifest on a timer of seconds and at stop, off the real-time paths, so a crash loses at most the last few seconds of markers and never the media. The manifest has a `format` and `kind` like other documents (ADR-0051), no secret and no absolute path.
5. **Markers in the media files (phase 2).** Matroska chapters and a QuickTime/MP4 chapter text track are written at clean stop when the container allows (they need the final index), skipped for fragmented files that end uncleanly. The manifest stays the source of truth; embedding is a convenience for players and quick review. It needs muxer work (hot-path rules apply, P15) and is therefore its own step.
6. **Editor project from the manifest.** A pure function `project::build(manifest, format) -> bytes` in a new crate `rbs-project` (no dependency on adapters, GPU, FFmpeg or UI; depends on `rbs-core` for rational frame rates and times) turns a recording manifest into a project file. It reads only the manifest (never probes media; durations, sizes and rates come from RBS's own record), and checks that the files it names exist. The timeline has the master video and its mixed audio on the first lane, each separate video file on its own lane above with its sound muted, each separate audio file on its own audio lane, each clip placed at its `start_offset_ns` from the manifest (ADR-0086; markers are relative to `t0`), the length of the recording, the canvas frame rate as an exact rational (for example 1001/60000 s for 59.94), and the markers as timeline markers (scene changes as chapter markers, manual ones as standard markers with their label and colour mapped to the closest the format has). The project refers to media by file URL computed when it is written; moving the folder needs a re-export (one command, same data).
7. **Formats, in order.** (1) **FCPXML** at the lowest version that carries lanes, markers and rational timing, chosen in the first PR and recorded in the spec; opens in Final Cut Pro and DaVinci Resolve. (2) **FCP7 XML (xmeml)**, which Premiere Pro and Resolve import. (3) Later, per demand: OpenTimelineIO, and a multicam clip for FCPXML so the separate video tracks become angles. The spec lists, for each format, the editors and versions it was imported into by hand before the format is called supported; **a format is not announced as supported until it has a golden-file test in CI and has been imported in the real editor by hand** (the repository records which version was tried and what was checked: clips placed, aligned, markers visible). Not promised: effects, transitions, the layout the compositor used, or rebuilding the on-air cut.
8. **When it runs.** Setting *Write editor project when recording stops* (off by default; a choice of format) and an on-demand command `project.export {recording, format}` (control API, `control` class; `rbs-cli project --manifest <path> --format fcpxml` for scripts and for re-exporting after a move; UI: *Export editor project...* in the Recordings context). Both call the same function. Writing takes milliseconds and runs on the control thread after the media is closed; if it fails the recording and its manifest are untouched and the failure is reported.
9. **Control API** (next free minor version): `marker.add`, `marker.list {recording?}` (the running recording or a manifest path inside the recordings folder), `project.export`, events `marker.added`. `marker.add` is rate-limited per client so a held key cannot flood the manifest.
10. **UI** (visual: proposal, then screenshots in three themes, rules 15 and 16): a Marker button in Live controls with a counter, a toast on each marker ("Marker 3 at 12:31"), and the settings in Settings › Recording › Markers and project, with plain-language help for each option.

## Consequences

- The editor starts with the moments marked and, with separate files, a timeline with every track already aligned. No other tool in this class does this at record time.
- The feature adds no work to the encoders, muxers or any hot path in its first phases; embedding markers in containers (decision 5) is the only part that touches muxers and carries the P15 obligations.
- External formats drift; keeping the supported editor and version list in the spec, with a manual import checklist, is a standing cost.
- A new crate `rbs-project` and a manifest document change `recording.md` and `user-data.md`; `control-api.md`, `shortcuts.md` and the glossary change in the implementing PRs. `rbs-project` is a leaf crate above `rbs-core`, depended on only by composition roots (`rbs-app`, `rbs-cli`). The new crate is a layering addition and needs the independent review in this ADR's Review section.

## Alternatives considered

- **Write the project by probing the media files (FFmpeg/OS).** Needs a probing dependency and can disagree with what RBS knows; the manifest is exact and free.
- **Only markers, no project.** Simple, but the main benefit of separate files (instant aligned timeline) would be left for the user to do by hand.
- **Embed everything in the media files only.** No sidecar to lose, but chapters need a clean close and cannot carry a multi-file project.
- **A platform stream marker (Twitch/YouTube) on the same key.** Platform-specific and network-dependent; a plugin concern (plugin SDK), not core.
- **Free-form colours and rich text for markers.** More to design, store and map to each editor; the small fixed set maps cleanly.

## Review

Pending: independent review per ADR-0014.
