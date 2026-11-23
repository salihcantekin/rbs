# ADR-0176: A camera's Region preview is one frame of the running source

- Status: Accepted (2026-11-23)
- Date: 2026-11-23
- Deciders: Salih (owner)
- Supersedes: [ADR-0103](0103-source-region.md) decision 4, for sources whose adapter has no `region_grab`

## Context

ADR-0103 decision 4 gives display, window and camera sources a Region section in Source settings, with a preview taken by a new capture-port method, `region_grab`. The capture port spec says cameras answer `Unsupported`, because a camera cannot be grabbed without a light. Only the WGC, ScreenCaptureKit and test adapters implement it. Media Foundation and AVFoundation do not, so on Windows and macOS a camera's Region section shows "Waiting for a preview…" over an empty box for ever (owner report, Brio, Windows, 2026-11-23).

Opening the device a second time for one frame is not a fix. A camera shown in a scene is already open, and on Windows a second open of a device in use usually fails; where it works it may change the device's mode or light.

The studio already holds the camera's pictures on the GPU while the source runs, and already reads one source picture back as RGBA for image tracks (`RgbaDownload`, ADR-0117 decision 5) and draws one source's picture for Interact windows (`Studio::add_source_picture`, ADR-0132 decision 6).

## Decision

1. **Where the picture comes from.** The adapter's grab stays first, on its worker thread, so displays and windows are unchanged. Its message to the control thread says whether the answer was `Unsupported` (not a timeout, a permission error or a missing target). Only on `Unsupported`, and only while the request's generation is still current (the section has not closed or opened again), the control thread asks the studio for one frame of the running source instead. The studio grab shows exactly what the source already captures, so it adds no exposure beyond the running capture.
2. **Studio grab.** `Studio::add_source_grab(collection, sources, source, max_long)` adds, on the render partition, a normalisation pass from the source's own node (before its filters and before the region and item crop, so the whole capture is shown) at a size that fits a square of `min(max_long, REGION_GRAB_LONG)` and keeps the aspect, and a grab node behind it. The grab node creates its `RgbaDownload` in `prepare`. In `process` it submits the first picture (never waits), then makes one non-blocking `take` per tick until the readback completes, puts the frame (straight-alpha sRGB RGBA8) in a one-slot `GrabSlot` with `try_lock` (a frame it cannot store yet is kept for the next tick), and wakes the control thread (`ControlWake`). After that every tick is one compare. The host takes the frame and removes both nodes with `Studio::remove_picture`. They are an `AddedPicture` with a `holds: false` mark and an empty `PreviewNodeStats` (no endpoint), so a source node replaced meanwhile is relinked as for Interact pictures, faults show in `picture_failure`, and only pictures with `holds: true` (Interact) count for the Shown hold.
3. **No light.** The grab does not hold the source at Shown. A source in standby has no node (or no picture size yet), the call answers `NotBound`, and the section keeps its text and lets the user type edges, as today. The grab never opens a device.
4. **Lifecycle (P13).** Nothing exists until a Region section opens; at most one grab per host. While it waits the control thread looks at it every 100 ms (and when the node wakes it): a `NotBound` source is asked again, so a camera that is just opening still gets its frame. It ends, and the section is told, when the frame arrives (the picture) or when no frame can come (no picture): the grab node failed, the picture has a fault, the picture is gone (the source left), or 3 s passed without a frame (the source lost its node, a frozen device). It ends without telling the section when the section closes or opens again (the existing generation counter). A studio restart forgets the grab's picture without removing it (picture ids start again in the new studio) and asks again, with a new 3 s on the new clock. Its readback counts in `GpuMetrics::frames_cpu_copied` like the image track's.
5. **Privacy.** As ADR-0103 decision 4: the frame goes only to the Source settings window, is not logged and not stored.

## Consequences

- Every camera gets a Region preview on Windows, macOS and Linux with no adapter work, and any later source kind whose adapter has no `region_grab` gets one too (rule 19: no new list of kinds).
- The preview is one frame. It is taken again only when the section opens again, as for displays and windows.
- A standby camera shows no preview. The section's text already says edges can be typed.
- ADR-0103 decision 5 (the region step of Add source) will get no camera preview this way, because a camera being added is in no scene yet. Accepted for now; that step is not built yet and decides its own preview when it is.
- Specs: `capture-port.md` (cameras: the host's studio grab instead), `io-nodes.md` (grab node), `gpu-compositor.md` is unchanged (`RgbaDownload` reused as specified).
- Tests: the grab node delivers one frame and then does nothing (rbs-nodes, GPU test like the image track's); the studio grab of a running test camera delivers a frame and a standby one is `NotBound` (rbs-record); the host falls back to the studio grab on `Unsupported` (rbs-studio-host).
- Render partition: two nodes exist only for a few ticks while the section opens. No change to any existing node's `process`.

## Alternatives considered

- **`region_grab` in the Media Foundation and AVFoundation adapters** (open the camera for one frame). Fails or disturbs a camera already in use, needs device work on each platform, and turns on a standby camera's light. Rejected.
- **Hold a standby camera at Shown to grab it.** Turns the light on without the user starting the camera. Rejected; a later ADR can offer it behind an explicit button.
- **Only change the text** for cameras. Leaves the drag rectangle without a picture. Rejected by the owner (2026-11-23).

## Review

Reviewer: independent AI session
Date: 2026-11-23
Verdict: changes requested
Concerns and resolutions (author, 2026-11-23, written into the decisions):
1. `Unsupported` was lost before the fallback (`region_grab(...).ok()` on the worker). -> Decision 1: the message carries whether the answer was `Unsupported`; the control thread starts the studio grab only then and only for the current generation.
2. The source can lose its node while the grab waits (no Shown hold), leaving the section waiting. -> Decision 4: the host gives up after 3 s, and when the picture is gone or faulted, and tells the section.
3. The Shown-hold logic counted every source picture, so a grab would keep a source held after an Interact window closed; the grab has no `PreviewNodeStats`. -> Decision 2: `AddedPicture.holds`, only `true` pictures count; the grab carries empty stats.
4. A restart left the section waiting. -> Decision 4: a picture gone with a restart tells the section (no picture).
5. ADR-0103 decision 5 (Add source) gets no camera preview. -> Named in Consequences as accepted for now.
6. "One compare" needed a qualifier; where the download is created. -> Decision 2 reworded.
7. The fallback could reach screen adapters. -> Decision 1: it runs only on `Unsupported` and shows only what the source already captures.

Second round (independent AI session, 2026-11-23): concerns 1-7 resolved; one new item. A studio restart starts picture ids again, so the grab's old id could name a projector's or Interact picture in the new studio and remove it, and its 3 s ran on the old clock. -> Resolved in decision 4: the restart path forgets the grab's picture without removing it and asks again with a new deadline (`Host::restart_studio_grab`).

Owner approval: Salih, 2026-11-23: "merge et main'e" ("merge it to main"), after reading the summary of this ADR, its reviews and the render-partition node (rule 12). Device check on Windows and macOS follows on the next build.
