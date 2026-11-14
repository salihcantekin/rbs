# ADR-0119: Where and how separate files are encoded (smart mode and rehearsal)

- Status: Accepted (2026-11-14)
- Date: 2026-11-14
- Deciders: Salih (owner)
- Amends: [ADR-0086](0086-recording-sources-separately-iso.md) decisions 3 (default input), 4 (quality settings), 6 (manifest: the reference path) and 9 (isolation); builds on [ADR-0087](0087-output-cost-estimate-and-hardware-limits.md) (cost estimate and limits) and [ADR-0117](0117-still-sources-as-images.md) (still sources as images)
- Related: [ADR-0004](0004-single-master-clock.md), [ADR-0025](0025-encoder-surface-export.md), [ADR-0030](0030-worker-thread-setup.md), [ADR-0038](0038-io-graph-nodes.md), [ADR-0085](0085-more-containers-codecs-and-advanced-encoder-settings.md), [ADR-0089](0089-recording-markers-and-editor-project-export.md), [ADR-0108](0108-media-file-source.md) (media source, Proposed), [capture port](../specs/capture-port.md) (compressed camera modes)
- Spec: [recording](../specs/recording.md), [io-nodes](../specs/io-nodes.md), [capture-port](../specs/capture-port.md), [control-api](../specs/control-api.md)

## Context

ADR-0086 step 3.2 opens a video file per marked source. Each file gets its own conversion, readback and encoder session, with the master's settings. The Mac measurements of that step (M4 Pro, 1080p60, 30 s takes, 2026-11-14; numbers in PR #348 and `recording.md`, Separate files) showed where this runs out:

- **Render cost is small.** About 0.3–0.4 ms per file per tick, and 99 % of ticks used under a quarter of the period with four files.
- **CPU is not the limit.** The process used about a third of one core of twelve.
- **The limit is the hardware H.264 engine,** which every session shares, the master's included.
  - The master and three 1080p60 files fit.
  - With four files the master skipped frames: about 20 % without protection, and about 0.7 % with the yield rule of 3.2, while the files gave way about 92 % of the time.
- **Two encode workers serve all the files** (`ISO_VIDEO_ENCODERS`, files assigned `i % 2`). So with three files, the third shared a worker with the first and its encoder skipped about 115 of 1,800 frames.

The owner wants the least resource use with a good experience. He asks for:
- a choice of where the files are encoded, chosen from what this machine has;
- honest text on what each choice costs;
- a short test before the take that measures CPU, GPU, encoders and disk;
- first, recording each source without its filters, so that the cheapest paths apply;
- later, encoding that may finish after Stop.

What a source can give without encoding, in this tree:
- **A still source:** a PNG per change (ADR-0117).
- **A media file source:** the file is already on disk. ADR-0108 is Proposed, and the node does not decode real files yet.
- **A camera:** many USB cameras deliver MJPEG or H.264. The capture port leaves compressed modes to a later minor (`capture-port.md`, Scope). AVFoundation decodes them before delivery, so on macOS a camera's compressed stream is not reachable. Media Foundation can offer it on Windows.
- **A screen:** always raw, but often unchanged. ScreenCaptureKit already drops idle frames (`rbs-capture-sck`), and the capture handoff knows a tick with no new frame (`capture-port.md`).

Encoders in this tree:
- `codec.sessions.max` is already a codec capability (`rbs-codec` `caps.rs`).
- macOS has a software H.264 encoder adapter, `org.rbs.codec.videotoolbox-software`.
- No adapter encodes ProRes, and ProRes is not a codec id.
- Thread classes are `Periodic` and `Responsive` only (`rbs-core` `thread.rs`).

Forces:
- The master never degrades (ADR-0086 decision 9).
- Zero cost when unused (P13).
- Capabilities, not vendor or model names (P3).
- Honest estimates before the take (ADR-0087).
- The user decides trade-offs, with clear text.
- No blocking or I/O on real-time threads (P6).
- No new per-frame CPU copy (ADR-0038).

## Decision

1. **First version: sources are recorded without their filters.**
   - ADR-0086 decision 3's default changes from "with filters" to "without filters": `exclude_filters` defaults to true. The source's own picture is what most edits want, and only it can take the cheap paths below.
   - *Include filters* stays a per-source option. A source with it set takes the encode path unless the image path applies. ADR-0117 records a still source's filtered picture as images.
   - No saved profile holds the old default yet (the `recording.iso` section does not exist), so nothing migrates.

2. **Smart mode: each source takes the cheapest honest path.** At the start of each take, the studio resolves a *path* for each marked picture source, in this order, and writes it to the manifest (`path`).

   - **Image** (ADR-0117): a still source whose `video_as` is `auto` or `image`. No encoder. A still source set to `video` takes the encode path.

   - **Reference**: a media file source without filters. The file is not copied.
     - This amends ADR-0086 decision 6 (no absolute path in the manifest). The manifest holds the original's file name, size, modification time and a fingerprint (`source_file`). The fingerprint is a hash of the first and last MiB, read at take start off the control thread. The full content hash is computed after Stop on a worker thread. `recording.md` names both algorithms. It also holds the path relative to the take folder when both are on one volume, and nothing more.
     - The media timeline: `media_in_ns` and `media_out_ns`, plus `seeks`, `pauses`, `loops` and `restarts` (from `loop` and `restart_when_shown`), each with its master time.
     - The editor project export (ADR-0089) is written on the user's machine and locates the original from the source's settings at export time. When the original is missing or its hash differs, the export says so.
     - Option *Copy media files* copies the originals into the take folder after Stop.
     - This path waits for ADR-0108 to be accepted and for the media node to decode real files. Until then, media sources take the encode path.

   - **Pass-through**: a camera without filters whose capture delivers a compressed stream a container can hold (H.264 into MP4, MKV or MOV; MJPEG into MKV or MOV).
     - The packets go to a muxer as they arrive, with no decode and no encode.
     - This needs the capture port's compressed modes, a capture-port minor with its own review, and an adapter capability `capture.compressed_out`.
     - Packet times are mapped to the master clock as capture frames are (ADR-0004). The file starts at the first keyframe at or after `t0`, and the manifest records `start_offset_ns` and the camera's own rate (`frame_rate: source`).
     - A pass-through camera that is also on air needs a decoder node for the picture. The cost estimate counts it.
     - Where the platform delivers only decoded frames (macOS today), the camera takes the encode path.

   - **Encode**: everything else (screens, windows, browsers, filtered sources that are not still, cameras without pass-through).
     - The file is constant frame rate by default, as ADR-0117 argues editors prefer: each tick's picture is encoded (step 3.2's `every_tick`).
     - Option *Skip unchanged frames* (per profile, off by default) keys the export on the frame's `(timestamp, sequence)` again. A capture that delivered no new frame then gives no new file frame, and the file is variable frame rate.
     - With that option, the export re-submits the last frame it took at least once per second, so the gap between file frames is at most one second. The input slot still holds that frame.
     - No new capability is needed: the capture's existing handoff already says "no new frame". The manifest marks the file `variable_rate = true`.

   - The UI lists every marked source with its path and a cost word (*free*, *low*, *uses an encoder*), so the user sees which sources cost something.

3. **Where encode-path files are encoded: the user picks, from what this machine has.** The profile section of ADR-0086 decision 4 gains `recording.iso.encoder`.

   - **`master`** (default, as in step 3.2): the master's encoder and engine. When the files would exceed what the rehearsal (decision 4) or the adapter's `codec.sessions.max` says the engine carries, the ADR-0087 estimate warns before the take.

   - **`engine:<id>`**: a hardware encoder this machine has on an engine other than the master's.
     - Example on Windows: an integrated GPU's QuickSync or AMF next to a discrete NVIDIA GPU.
     - Example on macOS: the ProRes engine, once ProRes exists.
     - A new codec capability, `codec.engine_id`, groups encoders that share hardware. Encoders with the same id share an engine. Vendor and model names are never compared (P3).
     - One NVIDIA GPU's several NVENC engines are one engine here: the driver balances sessions across them and no API picks one.
     - ProRes needs new contracts, each with its own review: a ProRes codec id, ProRes encode in the VideoToolbox adapter, and the MOV muxer rules for ProRes.

   - **`cpu`**: a software encoder.
     - macOS starts with the existing `videotoolbox-software` adapter. Windows needs a new video encoder adapter: the existing `rbs-codec-mf` is AAC only. Either a Media Foundation video adapter using the OS H.264 encoder, or another software encoder, chosen with `add-backend`, licence included.
     - Core budget `cpu_cores`. The default is half the machine's performance cores. Those come from a new `rbs-system-probe` function for performance and total core counts. It returns `Unsupported` where the OS has no core types, in which case the default is half of all cores. It is read off the control thread, like the crate's other blocking functions.
     - Its encoder partitions use a new thread class, `ThreadClass::Background` (a new `rbs-core` variant; the enum is `#[non_exhaustive]`, ADR-0030). `rbs-thread-os` maps it per platform: QoS utility on macOS, below normal on Windows, a positive nice value on Linux. That runs it below the render, audio and master encoder threads. A policy that does not know the variant treats it as `Responsive`, so adding it is semver-minor for out-of-tree policies.

   - **`auto`**: picks only among choices that do not change the codec or the resource kind the user accepted. It never switches to ProRes and never picks `cpu` without the user confirming once per profile. When nothing else fits, it uses `master` with the ADR-0087 warning.

   - Each choice shows one honest line, for example:
     - "Uses up to 4 of 12 CPU cores. Files may drop frames if the CPU is busy; the main recording is not affected."
     - "Uses the ProRes engine. ProRes 422 Proxy and LT files are about 11–25 MB per second each at 1080p60; ProRes 422 about 37 and HQ about 55."

4. **Rehearsal.** Before a take with encode-path files, the user can run *Test recording*, about five seconds.
   - **What it runs:** the real encoders, muxers into a discarding transport, and a write test of the target folder. The write test uses synced writes (`fsync`, or uncached I/O where the platform has it), so the page cache does not hide the disk.
   - **What it reports:**
     - render tick times and the process CPU share;
     - each encoder's frames encoded, skipped and given way, and the master encoder's skips;
     - the disk's sustained write speed and free space.
   - **Verdict:** *comfortable*, *at the limit* or *will not keep up*, with the cheapest change that fixes it (fewer files, another engine, CPU, a lower size or rate for the files). The verdict is a pure function of those measurements, tested deterministically (ADR-0032).
   - **When it may run:** it is refused while any output is live (recording, stream or virtual camera), since its hardware sessions would compete with a live master.
   - **Control API:** `record.rehearse`, in the next free minor. It is long-running: it gives progress events and can be cancelled.
   - **Results are kept** per machine and profile (`recording.iso.last_rehearsal`). This is where measured bounds live, not in capabilities. The results feed the ADR-0087 estimate until the hardware, the drivers or the profile change.
   - A rehearsal is a snapshot. The live protections (decision 5) stay on during the take.

5. **Live protection and workers.**
   - ADR-0086 decision 9 and step 3.2's rules stay: files give way after a long render tick and while a master encoder drops frames.
   - Encode workers become one per file, from the ISO partition pool, up to `MAX_ISO_VIDEO`. One file's encoder then no longer delays another's.
   - A worker exists only while its file is open (P13).

6. **Deferred encode is a separate, later decision.** Encoding that finishes after Stop (spooling frames the live encoder cannot take) is kept out of this ADR. Its own ADR follows after decisions 1–5 are built and measured. These constraints are recorded for it now:
   - It may spool only frames already read back that an encoder queue would otherwise drop. It must not read back frames that gave way.
   - It runs on its own partition, not `ISO_OUTPUT`.
   - It names its compressor crate.
   - It keeps a list of pending jobs, so they resume after a restart.
   - Its jobs yield to any live output that shares their engine.

7. **Zero cost (P13).**
   - No marked source: nothing here runs.
   - A source on the image, reference or pass-through path adds no converter, readback or encoder.
   - The rehearsal runs only when asked.
   - `ThreadClass::Background` is used only by `cpu` encoder partitions.

## Consequences

- **Fewer files need an encoder.** A typical take (screen, camera, a few images and clips) encodes one or two files instead of one per source, once the reference and pass-through paths exist. Images cost almost nothing at once (ADR-0117).
- **The user decides where the encoding runs,** from what the machine has, with honest text and, if wanted, a measured test.
- **ADR-0086 decision 3's default changes** (filters excluded). A user who wants the filtered picture sets *Include filters*. ADR-0086 gets a `## Review` line pointing here (rule 9).
- **New contracts, each reviewed before it is built:**
  - the capture port's compressed modes and `capture.compressed_out`;
  - `codec.engine_id`;
  - a core-type capability in `rbs-system-probe`;
  - `ThreadClass::Background`;
  - a Windows software H.264 adapter;
  - ProRes (codec id, VideoToolbox encode, MOV rules);
  - `record.rehearse`;
  - manifest fields (`path`, `source_file`, the media timeline, `variable_rate`).
- **Some paths wait on other work.** The reference path depends on ADR-0108, and pass-through on the capture-port minor and a platform that delivers compressed packets. Until then those sources take the encode path, which is correct, only not free.
- **Order of work.** Each step is measured on the Mac M4 Pro and the Windows RTX 4090 machine (P15):
  - (a) filters excluded by default, and one encode worker per file;
  - (b) image path (ADR-0117);
  - (c) `cpu` with the existing macOS software encoder and `ThreadClass::Background`; on Windows, after a new software video encoder adapter;
  - (d) `codec.engine_id`, starting with dGPU plus iGPU on Windows;
  - (e) rehearsal;
  - (f) *Skip unchanged frames*;
  - (g) ProRes on macOS;
  - (h) reference path, after ADR-0108;
  - (i) pass-through, after the capture-port minor;
  - (j) the deferred-encode ADR.
- **Spec follow-ups:** `recording.md` (paths, encoder choice, rehearsal, manifest), `io-nodes.md`, `capture-port.md`, `control-api.md`, ADR-0087's estimate table, and the UI proposal.

## Alternatives considered

- **Encode every file on the master's engine (step 3.2 as is).** Simple, but the engine's session limit caps the files (three at 1080p60 on the M4 Pro), and the master pays when it is exceeded. This stays the default, behind the estimate's warning.
- **Always encode files on the CPU.** It spreads the work across cores and keeps the master's engine free. But it costs CPU, heat and battery for sources that could be free.
- **Record raw frames and encode after the take.** No encoder limit during the take, but about 190 MB/s per 1080p60 file of disk traffic and a long wait after every take. A narrower overflow-only spool is left to its own ADR (decision 6).
- **Keep filters on by default.** Matches ADR-0086 as written, but closes the free paths for cameras and media, which would then always encode.
- **Variable frame rate by default for screens.** Cheaper, but editors handle it worse (ADR-0117). It stays an option.
- **Pick the engine automatically with no user choice.** A simpler UI, but the trade-offs (file size for ProRes, CPU heat, battery) belong to the user. `auto` stays within what the user accepted.

## Review

- 2026-11-14, independent AI session (round 1, given only the ADR, the referenced ADRs, specs and code): changes requested.
  - 5 major concerns:
    - deferred encode would read back frames that gave way;
    - no lower thread class exists;
    - `codec.engine_sessions` duplicated `codec.sessions.max`;
    - conflicts with ADR-0117 on filtered stills and `video_as`;
    - the reference path broke the manifest's no-absolute-path rule and assumed ADR-0108 was built.
  - 8 minor concerns and 2 nits: NVENC engines, ProRes contracts, the existing macOS software encoder, damage and VFR defaults, `auto` behaviour, rehearsal rules, pass-through timing, spool discovery, core types, ProRes rates, and the ADR-0086 review line.
  - All resolved in the text above. Deferred encode moved to its own ADR with recorded constraints (decision 6). `ThreadClass::Background` added (decision 3). `codec.sessions.max` reused, with measured bounds in the rehearsal results. ADR-0117 respected (decisions 1 and 2). The reference path amends ADR-0086 decision 6 with relative paths and hashes, and waits for ADR-0108. CFR is the default. `auto` is bounded. The rehearsal is refused while live and uses synced writes, a pure verdict and an API minor.
- 2026-11-14, same reviewer (round 2): approve with minor changes. Three corrections are now applied:
  - `rbs-codec-mf` is AAC only, so Windows needs a new video encoder adapter.
  - Core counts come from a new `rbs-system-probe` function, not a capability.
  - The reference path's hash is a cheap fingerprint at start, with the full hash after Stop.

  Two nits are also applied: how `rbs-thread-os` maps `Background`, and this record. Verdict: approve. Waiting for the owner's acceptance.
- 2026-11-14, owner (Salih) approved acceptance after the review. Status: Accepted.
