# Resource and structure audit (2026-11-19)

Independent pass over the RBS tree. The earlier write-ups under `docs/perf/` and `docs/quality/` were not opened and were not edited. This file is a separate report so the two can be compared.

Nothing here is a code change. Numbers that came from a benchmark sign-off in `docs/architecture/hot-paths.md` or from `docs/specs/performance-budgets.md` are labelled as published. Arithmetic from frame size is labelled as arithmetic. Nothing in this pass was re-measured with `cargo bench`.

## What was reviewed

92 workspace crates, about 469 000 lines of Rust in 1 020 files, and about 24 000 lines of Slint in 29 files. The largest crates are `rbs-app` (107k), `rbs-record` (33k), `rbs-gpu` (26k), `rbs-ui` (23k), and `rbs-nodes` (18k).

The passes covered the audio callback, GPU composition and colour conversion, the graph tick, encode / mux / transport, capture and source edges, idle lifecycle, the scene document and recording orchestration, the desktop UI and control API, and cross-crate duplication.

A finding marked **verified** was opened in source while writing this report. A finding marked **scan** was reported by an area pass with file and line citations and was not re-opened line by line. Several passes found the same cost; each cost is written once.

## How the running system spends time

The engine is a media graph on one master clock. Audio is a fixed 10 ms block (480 frames at 48 kHz). Video is one worker per canvas at the canvas cadence. Encoded packets are `bytes::Bytes` so one encode can feed many outputs. Composition is linear light, at least 16-bit float. Conversion to NV12 or P010 is a GPU pass. The default encode path still reads that picture back to the CPU (`YuvDownload`, ADR-0038). That readback is a known exception. Extra copies around it are in this report.

Published budgets that this report uses as a scale, not as new measurements:

| Path | Published scale |
| --- | --- |
| Audio block | 10 ms. Eight sources and four buses should stay under 10% of that (1 ms). |
| One live input node | Ceiling 50 µs. Published about 27 µs at 48 kHz mono, 35 µs at 44.1 kHz stereo. |
| One live output node | Ceiling 50 µs. Published about 30–32 µs. |
| Mixer, 16 stereo inputs | Ceiling 30 µs. Published about 13 µs, meters included. |
| FFmpeg AAC, twoloop | Published 645 µs per 10 ms stereo block, on the encoder worker. |
| Loudness tap | Published 19.7 µs per block, absent unless shown. |
| Static scene | After the first frame, compositor draws and YUV conversions per second should be 0. The recording still repeats frames. |
| UI idle | 0% CPU and no redraw timer when nothing animates and the show is not live. |
| UI with preview and meters | At most 10% of one core. |

Test and build facts used while scoping the passes (`docs/specs/testing-strategy.md`, `.config/nextest.toml`, `.github/workflows/ci.yml`):

- Fast tests are each well under 1 s. Nextest’s CI profile reports a test at 15 s and kills it at 60 s. No retries. The CI job timeout is 60 minutes.
- A cloud container is about 4 vCPUs. A workspace rebuild after an `rbs-app` change is about 70 s. Building `rbs-app` with `-p` beside a workspace build compiled a second copy of dependencies and took about 248 s (noted 2026-11-17 in the testing spec).

## How to read a finding

Severity is the cost if the path is in use, not the difficulty of the change.

| Level | Meaning |
| --- | --- |
| P0 | A full-frame copy, a GPU readback that a sibling path already avoids, or work that scales with destination count or resolution on the steady path. |
| P1 | A standing per-block, per-frame, or per-edit cost that is real and bounded. |
| P2 | Startup, a rare path, or a split implementation that will drift into a runtime bug. |
| P3 | Small. Worth recording so the next edit does not copy it. |

“Local change” is a patch in the function that pays the cost. “Larger change” is a different shape, used when a local patch leaves the expensive design in place.

## The costs that dominate

1. On Linux, and on QSV, VAAPI, and AMF everywhere, a picture still crosses the CPU on the way to the encoder. NVENC and VideoToolbox already have a native path that skips that upload. See F-01.
2. An Annex B encode is rewritten once per FLV, fragmented MP4, and Matroska output, and RTMP copies the payload again while chunking. See F-02.
3. A browser page is swizzled and then copied twice more before the GPU upload. See F-03.
4. A live 48 kHz input still runs a 64-tap lowpass into 48 kHz. A media file at the same rate does not. See F-05.
5. FFmpeg AAC never selects a coder, so the published 645 µs twoloop figure is the path that runs. See F-06.
6. An empty studio still runs the audio worker at 100 Hz and the render worker at the canvas rate, and startup probes every encoder. See F-08.
7. Every scene command clones and revalidates the whole collection, and the UI rebuilds every source row on each control round before it notices that nothing changed. See F-09 and F-10.

## Findings

### F-01: Linux capture and QSV, VAAPI, and AMF encode through system memory

- **Severity:** P0. **Kind:** runtime. **Confidence:** verified for the preference and the three empty native lists; the PipeWire row copy and `copy_420` bodies are scan.
- **Where:** `crates/rbs-nodes/src/capture.rs` `import_preferences` (Vulkan and any backend other than D3D12 or Metal returns only `MemoryKind::System`). `native_memory: Vec::new()` in `crates/rbs-codec-qsv/src/engine.rs`, `crates/rbs-codec-vaapi/src/engine.rs`, and `crates/rbs-codec-amf/src/engine.rs`. QSV `upload` calls `copy_420` into a system surface. `Engine::attach_native` in `crates/rbs-codec-hw/src/session.rs` refuses native frames unless the engine overrides it. The studio asks for a native memory kind first and falls back to system memory (`crates/rbs-record/src/studio.rs`, scan).
- **What happens:** A Linux screen or camera is mapped, copied by rows, uploaded for the compositor, converted, read back (the known `YuvDownload`), and copied into the encoder. WGC and ScreenCaptureKit already hand off a GPU texture. NVENC and VideoToolbox already skip the encoder upload when the frame is native. AMF, QSV, and VAAPI never report a native kind, so they take the CPU upload even when capture stayed on the GPU. AMF also allocates a host surface per frame (scan).
- **Scale:** 1080p NV12 is about 3.1 MB. Two full copies at 60 fps are about 370 MB/s of memcpy, after the GPU convert. Linux capture adds another full-frame copy and an upload before that. 4K NV12 is about 12.4 MB per copy.
- **Local change:** None that removes the Linux copy. The comment in `import_preferences` says DMA-BUF import stays off until it is verified on hardware. Pooling AMF host surfaces is the failure mode that engine documents (scan).
- **Larger change:** Report the render device’s memory kind and implement `attach_native`, as NVENC (`D3d12`) and VideoToolbox (`Metal`) already do. On Vulkan, offer `MemoryKind::DmaBuf` once the importer is verified, and give `prepare_native` a Linux exporter. PipeWire already has a DMA-BUF branch that runs only when the request asks for it (scan).
- **Benefit:** The steady encode path stops moving the picture through the CPU on every platform that can already share a texture. This is the largest single runtime cut in the tree.
- **Risk:** A wrong fence tears or loses the device. The capture spec already says hosts request system memory for portal targets until a hardware check passes.

### F-02: Each converting output rewrites Annex B, and RTMP copies it again

- **Severity:** P0. **Kind:** runtime. **Confidence:** verified for fragmented MP4 staging and the RTMP body-then-chunk copy. FLV and Matroska are the same shape (scan).
- **Where:** `crates/rbs-mux-fmp4/src/muxer.rs` (Annex B units are scanned into `track.staging`). The same rewrite is in `crates/rbs-mux-matroska` and `crates/rbs-media-format/src/flv.rs` (scan). `crates/rbs-transport-rtmp/src/io.rs` fills `self.body`, then `chunk::write_message` fills `self.out` from that body. NVENC, QSV, and VAAPI emit Annex B. VideoToolbox is already length-prefixed (scan). One encoder is shared across matching outputs (scan). MPEG-TS wraps the original bytes and does not rewrite them (scan).
- **What happens:** The published Matroska gap is about 4 µs for a length-prefixed IDR against about 45 µs for Annex B, and about 430 µs against about 610 µs for a 1 s GOP. That gap is paid once per FLV, fMP4, and Matroska output, not once per encode. fMP4 and Matroska copy the converted bytes again at flush. RTMP then copies the tag body into the chunk buffer. FLV sizes the tag with one NAL walk and writes it with a second (scan).
- **Scale:** Destination count times packet size, for every Annex B encode. A second and third recording of the same encode multiplies the conversion.
- **Local change:** Convert once, keep the filtered length-prefixed `Bytes`, and let the next output clone it. Write RTMP chunk headers around slices of `body` instead of assembling `out`.
- **Larger change:** Have the hardware encoders emit 4-byte length prefixes when every output wants them, and keep Annex B only for MPEG-TS. Put the filter-and-prefix loop next to `annexb_to_length_prefixed` in `crates/rbs-media-format/src/nal.rs`. fMP4 and Matroska each own a copy of that loop today (scan).
- **Benefit:** Extra recordings and streams become a `Bytes` clone plus container headers, which is the fan-out the output node already uses for the encoded packet. One RTMP payload-sized allocation disappears per packet.
- **Risk:** FLV drops only AUD; fMP4 and Matroska also drop parameter sets. A shared buffer has to be the form each muxer can accept, or each muxer still walks headers.

### F-03: A browser picture is copied three times before upload

- **Severity:** P0. **Kind:** runtime. **Confidence:** verified.
- **Where:** `crates/rbs-browser-cef/src/main.rs` `on_paint` swizzles BGRA into the shared-memory slot. `crates/rbs-browser-shm/src/lib.rs` `take_latest` `copy_nonoverlapping`s that slot into a `Vec`. `crates/rbs-sources/src/browser.rs` `publish_at` `copy_from_slice`s those bytes into `Bytes` (a reuse path exists when the previous buffer is unique).
- **What happens:** The CEF comment says the helper converts while it copies so the studio adds no pass, and that a GPU path replaces this. The studio still copies the full frame twice after that swizzle. `take_latest` is a raw copy because the helper is untrusted and must not be aliased.
- **Scale:** 1080p is about 8.3 MB, 4K about 33 MB, per copy, per painted frame, capped at 60. A 1080p30 page is on the order of 0.5 GB/s of extra CPU traffic beyond the shared-memory write, before the upload.
- **Local change:** `publish_at` can take the `Vec` (`Bytes::from`) and drop the third copy. Replace the per-pixel temporary in the swizzle with straight stores (scan).
- **Larger change:** Let CEF paint into the slot the upload reads, or hand the studio a GPU texture and drop the `Vec` and the `Bytes`.
- **Benefit:** Two full-frame CPU copies leave the steady path. A GPU handoff removes the swizzle as well.
- **Risk:** Dropping `take_latest` needs a protocol where the slot stays immutable until the upload finishes.

### F-04: The virtual camera copies NV12 twice and does not share an encode download

- **Severity:** P0. **Kind:** runtime. **Confidence:** scan, agreed by the GPU pass and the encode pass. The Windows pack and the ring write were not re-opened for this report.
- **Where:** `crates/rbs-nodes/src/vcam.rs` always builds a `YuvDownload`. `crates/rbs-vcam-mf/src/sender.rs` `pack` copies every row into `scratch`. `crates/rbs-vcam-ring` `Writer::write` copies that scratch into the slot. The studio opens the camera as system-memory NV12. Encode exports of the same shape already share one download. The camera does not join it.
- **What happens:** The known readback has already produced NV12. The sender packs it, then the ring copies it again. A camera whose size matches a live encode also runs a second GPU conversion and readback.
- **Scale:** About 3.1 MB at 1080p and about 12.4 MB at 4K, twice, per camera frame, plus the extra download when sizes match.
- **Local change:** When both planes are contiguous and the stride matches the ring, write straight into the slot and drop `scratch`.
- **Larger change:** Feed the camera from an encode download of the same size and format. Write NV12 into the shared mapping from the GPU, the way native export already writes encoder surfaces.
- **Benefit:** One full-frame memcpy per camera frame, and a second GPU convert when the sizes match.
- **Risk:** Padded planes still need `pack`. Sharing a download ties the camera to that export’s lifetime.

### F-05: A live input at the graph rate still runs the 64-tap sinc

- **Severity:** P1. **Kind:** runtime. **Confidence:** verified. The split of the 27 µs figure was not measured.
- **Where:** `crates/rbs-audio/src/resample.rs` (`TAPS` is 64, `PHASES` is 256). `cutoff` at equal rates is `0.5 - 0.035`. `crates/rbs-audio/src/input.rs` `InputAligner::render` calls `coefficients` and `apply` for every output frame, then dots the channel matrix, with no bypass when `nominal` is 1. The output node does the same (scan). `crates/rbs-nodes/src/media/pcm.rs` sets `kernel` to `None` when the rates match and copies.
- **What happens:** 48 kHz to 48 kHz on a live input is a lowpass, not a copy. Steering only needs a few hundred ppm (`MAX_CORRECTION` is 0.002). The coefficient table is already shared per rate pair (`Kernel::shared`); the hot cost is the filter, not building the table. A comment on `Kernel::shared` says that share came from an earlier performance pass. This report does not reopen that share.
- **Scale:** Published 27 µs (48 kHz mono) and about 30 µs (output) per block. Eight matched-rate inputs are a few hundred microseconds. How much of 27 µs is the sinc, versus the ring read and the FIFO slide, is not split.
- **Local change:** A short interpolator when the two rates are equal and the correction stays inside `MAX_CORRECTION`. Keep the 64-tap table for 44.1 kHz and other real ratios.
- **Larger change:** One resampler policy for live devices, browser sound, and media files. Today a 48 kHz file is a channel copy and a 48 kHz microphone is a lowpass.
- **Benefit:** Most of the matched-rate input and output cost, if a short interpolator replaces the sinc. Unproven until the bench is split.
- **Risk:** `docs/specs/audio-graph.md` requires this kernel, including a 48-to-48 level test. A cheaper 1:1 path is a spec change. Drift steering has to stay continuous.

### F-06: FFmpeg AAC never selects a coder

- **Severity:** P1. **Kind:** runtime. **Confidence:** verified that no coder is set. The 645 µs figure is the published sign-off, not a new run.
- **Where:** `crates/rbs-codec-ffmpeg/src/linux.rs` `Context::open` sets bitrate, sample rate, `threads = 1`, channel layout, and time base. The tree has no `aac_coder` and no `twoloop` string. `AudioEncoderConfig::adapter_options` exists and this adapter does not read it (scan).
- **What happens:** libavcodec’s native AAC encoder defaults to the twoloop search. That is the coder behind the published 645 µs per 10 ms stereo block, about 6.5% of one core per stream, on the encoder worker. The device callback does not run it. Media Foundation and AudioToolbox are different encoders.
- **Scale:** One encode per distinct AAC configuration, every 1024-sample frame.
- **Local change:** Set `aac_coder` from an adapter option before `avcodec_open2`. Leave the default until a measurement on the same machine says otherwise.
- **Larger change:** On macOS and Windows, prefer the platform encoder that is already in the tree, and leave libavcodec for Linux.
- **Benefit:** The gap between 645 µs and whatever the fast coder measures. Not a few microseconds. Quality at the same bitrate changes.
- **Risk:** A coder that changes `frame_size` or `initial_padding` fails the open check, which expects 1024.

### F-07: A placed capture stays Active while hidden or off the current scene

- **Severity:** P0 if treated as waste, and it is the written rule. **Kind:** idle. **Confidence:** verified.
- **Where:** `crates/rbs-scene/src/activity.rs` `Run::Placed` is “Active while placed in any scene (capture, stills, audio; the wanted rule).” `crates/rbs-sources/src/registry.rs` `activity_traits`: media and browser run while a shown scene draws them; every other built-in type, and unknown types, is `ActivityTraits::PLACED`.
- **What happens:** A display, window, or camera that sits in another scene, or is hidden in this one, keeps its OS session, its callback, and its GPU surfaces. Media and browser already drop their workers when not shown. WGC still GPU-copies each frame into a shared texture (scan).
- **Scale:** One session per placed capture for as long as the collection is open. A hidden 4K display is the same GPU copy as a visible one.
- **Local change:** None inside the capture node. The binder marks the node Active.
- **Larger change:** A default of `Run::Shown` for capture, or a standby that stops the OS session when the item is not drawn. This is an ADR-0152 decision.
- **Benefit:** Hidden and off-scene captures cost nothing, which is the rule media and browser already follow.
- **Risk:** The current rule exists so the device is already open on the cut. Stopping the session adds the camera or ScreenCaptureKit start, which the spec already treats as over the 500 ms budget, the first time that scene is shown.

### F-08: An open studio ticks audio, video, and the control thread with an empty scene

- **Severity:** P1. **Kind:** idle. **Confidence:** verified for the worker wait, the empty-schedule stop, the 250 ms cap, and `adapters.probe()` at studio start. The “mixer is always adopted” path is scan.
- **Where:** `crates/rbs-engine/src/worker.rs` parks until the next cadence tick whenever the schedule is non-empty. `crates/rbs-engine/src/part.rs` `after_collect` stops a worker whose schedule is empty. Studio start adopts a mixer, a meter sink, and the program compositor, so those schedules are not empty (scan). `IDLE_WAIT` in `crates/rbs-app/src/studio_host.rs` is 250 ms. The comment says an earlier pass cut this from 50 ms. `start` calls `adapters.probe()` before any recording.
- **What happens:**
  - The audio worker wakes about 100 times a second and the mixer zero-fills a block when no source is attached.
  - The render worker wakes every frame. Render-on-change can skip the draw. It does not skip the wake-up.
  - The studio thread uses `recv_timeout` and wakes four times a second even when no driver returned a deadline.
  - Probing every encoder loads NVENC, AMF, oneVPL, VA-API, FFmpeg, and VideoToolbox plug-ins through process-lifetime `OnceLock`s. The sessions are dropped. The libraries stay mapped (scan).
- **Scale:** For the life of the studio, including a scene with no source and no output. Over 60 s that is about 6 000 audio wake-ups, one render wake-up per frame, and 240 studio wake-ups. The idle budget compares a loaded build with a baseline that already includes these threads, so the budget does not see them.
- **Local change:** Leave the audio schedule empty until the first source or the first output that takes audio. When `engine.service()` returns no wait and no driver has a deadline, block on `recv()`. Probe an encoder when the record or stream UI is first opened, and cache the failure.
- **Larger change:** A static program has no frame clock. Capture and recording are what need one. Encoder capabilities for the menu are a manifest. Opening CUDA or VideoToolbox is first use of that encoder.
- **Benefit:** An empty scene drops two cadence threads and the 4 Hz poll. Idle RSS no longer includes unused encoder runtimes. Byte sizes were not measured.
- **Risk:** The first microphone waits for the audio worker, which the apply path already starts. A condition that forgets to arm a deadline stalls until the next UI message. The encoder menu arrives late unless the probe result is cached.

### F-09: Every edit clones the collection, and every save reparses the previous file

- **Severity:** P1. **Kind:** memory, control thread. **Confidence:** verified.
- **Where:** `crates/rbs-scene/src/collection.rs` `apply_with` clones the collection, runs the edit, calls `validate`, and commits. `validate` walks every canvas, source, scene, and item (scan). `crates/rbs-record/src/studio.rs` `with_followers` clones even when no canvas follows another. A studio update clones again and rewalks every shown scene (scan). `crates/rbs-user-data/src/document.rs` `write` renders the new document, reads the previous file, and `parse`s the whole old document, then drops that model.
- **What happens:** A fader sample or a text keystroke duplicates every source’s settings and re-checks invariants that this edit cannot break. The comment on `apply_with` says collections hold hundreds of objects and edits are user-paced. The autosave parse is deliberate: a hand edit that breaks the body must be moved aside, not only an older format. `set_mix` already sends levels without rebinding the graph (scan).
- **Scale:** Two collection clones per studio update with no followers, plus a full TOML parse of the previous file on save. Files are capped at 16 MB.
- **Local change:** For a command that already checked its own field, keep the inverse for undo and skip the full `validate`. Return the borrowed collection from `with_followers` when the follower set is empty.
- **Larger change:** Store undo as the inverse command and validate only the invariants that command can affect. Journal commands and rewrite the document on a timer or on close. Do not skip the old-file parse on the current `write` path. That parse is the guarantee that a broken body is kept.
- **Benefit:** Fader drags and text edits stop allocating the rest of the document. Autosave stops building a second collection that is thrown away, once a journal replaces per-edit rewrite.
- **Risk:** An invariant that only the full walk catches could commit. The volume path already rejects a bad factor before `apply`.
- **Done (2026-11-21):** Field edits apply in place (`rbs-scene` `collection/field.rs`, F-09a). The studio host hands the collection to autosave when its epoch and `generation` changed, instead of comparing it with a kept copy each round (13.7 µs per round) and cloning twice per edit (78 µs each, 300 sources); one clone per edit for the writer remains. The writer thread keeps a `Written` record and does not parse a file it wrote itself (`document::write_known`), and leaves an unchanged file in place: a 77 KiB save drops from 5.7 ms to 2.0 ms p50 (`cargo bench -p rbs-user-data --bench save`, 4 vCPU cloud container). The journal is not built: what is left is one 78 µs copy per user-paced edit and one render per save.

### F-10: The control round builds every source row, then compares

- **Severity:** P1. **Kind:** runtime, UI. **Confidence:** verified for the rebuild. The meter-row reset on `Update::Heard` is scan.
- **Where:** `crates/rbs-app/src/studio_host.rs` `publish`. Every round calls `scene_list()`, `rows_of`, `global_rows`, and `rows()`, then compares with the previous vectors. `rows_of` clones source names. A fader command publishes, and the round publishes again (scan). `cue_remaining_ms` is part of row equality, so a running Show cue dirties the list every round (scan).
- **What happens:** The notify is skipped when the vectors compare equal. The allocation already happened. While an output runs the cap is 50 ms, about 20 full builds a second, about 40 during a fader drag. `Update::Heard` builds new meter rows with default bar fields, so painted peaks differ and every strip is replaced. The 30 Hz meter tick then paints the bars back (scan).
- **Scale:** Linear in sources, two to four walks per round. An equaliser curve of 101 points is rebuilt even when Properties is closed (scan).
- **Local change:** Build rows only when a generation on the collection or the runtime status changes. On `Heard`, patch name, mute, and fader and leave bar fields alone. Do not call `publish` inside a command when `round` always publishes at the end. Keep cue time off `SourceRow`.
- **Larger change:** One snapshot per real edit. Volume patches one row. Sources, Properties, and the Mixer should not share one row type that includes layout, filters, browser settings, and the cue clock.
- **Benefit:** Idle control-thread allocations drop out. Fader drags and timed shows stop redrawing the window at the control rate.
- **Risk:** A missed dirty bit shows a stale Sources list until the next edit.
- **Status (2026-11-21):** local change done. The round already skipped the rebuild when a stamp of the collection's generation and the runtime state was unchanged, and `Heard` already kept the painted bars. What remained: the stamp itself cloned every source state (`Studio::sources`) and looked up device peers on each round, `publish_levels` rebuilt its list each round, and the Show cue clock was part of the stamp and of `SourceRow`. Now the stamp reads the borrowed `Studio::source_health`, device peers are left to the generation, the levels list is rebuilt only with the rows, and cue time travels in `Update::Cues`, which the window patches into the rows it changes. An unchanged `publish` makes 0 allocations, down from 25 at 10 sources and 112 at 50 (`studio_host/publish_cost.rs`, a test asserts 0), and a running cue no longer rebuilds the lists. Open: the larger change. A fader step still rebuilds every row, about 1,000 allocations at 50 sources (debug build, same file).

### F-11: A shared export readback locks on every repeated frame

- **Severity:** P0 for the rule it breaks (a blocking lock on the render tick), P1 for the uncontended cost. **Confidence:** verified.
- **Where:** `crates/rbs-gpu/src/download.rs` `YuvDownload::take`. The function returns before the lock only when the oldest entry is `Kind::Free`. Any other kind, including `Repeat`, calls `Store::lock`. `Store::Shared` uses `Mutex::lock`. `Store::Own` does not. A comment says the shared readback came from an earlier performance pass so two outputs convert one picture once. This report does not undo that share.
- **What happens:** A constant-rate export of a static canvas takes the shared mutex once per frame per joined download. On the single render thread the lock does not wait. It is still `lock()`, which the hot-path rule does not allow, and a real readback copies pixels while the guard is held.
- **Scale:** One lock per shared export per tick for as long as repeats are queued. Two outputs of the same shape join one readback.
- **Local change:** If the oldest entry is `Kind::Repeat` and it does not touch a mapped slot, share `last` and return before `lock`, the way `Kind::Free` already returns.
- **Larger change:** Each download keeps its own entry ring. Take the shared readback only when a `Kind::Readback` slot is mapped.
- **Benefit:** The steady repeat path of a shared stream-plus-recording export stops taking a mutex. A new frame’s copy no longer sits inside that lock.
- **Risk:** Repeats must stay ordered behind in-flight readbacks. Skipping the lock is valid only when the entry does not touch `Readback`.

### F-12: NV12 conversion reads the canvas twice, and upscale chroma runs Catmull four times

- **Severity:** P1. **Kind:** GPU. **Confidence:** verified.
- **Where:** `crates/rbs-gpu/src/yuv.rs` `YuvConverter::record` starts one render pass for luma and one for chroma. `crates/rbs-gpu/src/shaders/yuv.wgsl` `fs_chroma_catmull` calls `catmull_linear` four times and averages. The same four calls exist for the open-shadows and sRGB copies.
- **What happens:** Luma and chroma cannot share a load because they are separate render passes writing different sizes. Any output larger than the canvas on both axes selects Catmull-Rom, including a small step such as 1080p to 1440p. Four overlapping 4×4 neighbourhoods cover about 25 unique texels, not 64. A static scene does not enter this path.
- **Scale:** Per converted frame. A 1080p `Rgba16Float` canvas is about 16 MB, read twice. Chroma upscale does about four times the luma pass’s loads on a quarter of the pixels.
- **Local change:** Keep the two passes. Sample the unique Catmull taps once per chroma texel.
- **Larger change:** One compute dispatch writes both planes from a single canvas load. Use the existing bilinear kernel for mild upscales.
- **Benefit:** About one full canvas read removed per converted frame, if the cache does not already catch the second pass. Mild upscales drop to one sample per luma pixel. Quality of a bilinear upscale is softer.
- **Risk:** Chroma siting has to stay matched to the luma kernel or edges fringe.

### F-13: Windows cameras copy the frame, then the node uploads it

- **Severity:** P0. **Kind:** runtime. **Confidence:** scan.
- **Where:** `crates/rbs-capture-mf` declares `MemoryKind::System` only. The reader callback copies every row out of the locked Media Foundation buffer, then sets alpha on BGRA. `CaptureSourceNode` uploads that system frame on the render thread.
- **What happens:** The spec says the ring copy stands in for the upload. The node still uploads. USB cameras have no GPU buffer today, so one transfer is required. The ring copy is a second full copy.
- **Scale:** Every Active camera, at the negotiated rate. 1080p30 is about 8 MB of memcpy plus an 8 MB upload, thirty times a second.
- **Local change:** Upload from the locked buffer on a dedicated thread and publish a shared NT handle, the way WGC does. Until that exists, keep one copy and stop describing the ring as a substitute for the upload.
- **Larger change:** A D3D11 frame-server path, which the spec already lists as a follow-up.
- **Benefit:** One transfer instead of two. A shared texture removes the CPU picture for cameras that can deliver one.
- **Risk:** Media Foundation reuses the sample. The upload must finish or copy before the buffer is unlocked.

### F-14: Animated images clone the canvas, and the media worker wakes every 2 ms

- **Severity:** P0 for the clone on a large animation, P1 for the park. **Kind:** runtime. **Confidence:** the GIF `canvas.clone()` is verified. The 2 ms park is scan.
- **Where:** `crates/rbs-media-anim/src/lib.rs` publishes `self.canvas.clone()` for each GIF frame. APNG and WebP do the same class of full-frame copy (scan). `crates/rbs-nodes/src/media/worker.rs` caps every wait at 2 ms, including a frame that is 30 ms away and a paused file (scan). `queue_picture` drops a sample when the ring is full, after `next_picture` has already decoded it (scan).
- **Scale:** A 1080p animation is two full CPU copies per frame on the decode thread, plus the upload. A playing file wakes about 500 times a second. Hidden media does not hit this. Media’s default run is `Shown`.
- **Local change:** Swap the canvas with a spare `Vec` and let the picture path take that `Vec`. Park for the real wait, and unpark on the command path that already exists. If the ring is full, do not decode the next picture.
- **Benefit:** One CPU copy per animated frame instead of two. A 24 fps file wakes about 24 times a second. A paused file sleeps until the next command or the 30 s release.
- **Risk:** Disposal methods need the canvas intact for the next frame, so the published buffer must be a snapshot. A seek that forgets to unpark delays the next frame.

### F-15: Mixer meters run while nothing is reading them

- **Severity:** P1. **Kind:** idle on the audio thread. **Confidence:** verified for the arithmetic and the attach. The UI timer gate is the design-system rule, not re-measured.
- **Where:** `crates/rbs-audio/src/mixer.rs` `mix_lanes` always tracks eight lanes of peak and sum of squares. `record_block` runs when a meter is attached. `LevelMeter::record_block` does two `fetch_max` and two compare-exchange loops that add energy as `f64` bits. Studio creates a `LevelMeter` when an audio source is adopted and attaches it for the life of the source. The master meter is attached for the life of the mixer. The lane split exists so the mix vectorises. A comment credits an earlier pass for that split. Do not delete the lanes.
- **Scale:** Every heard source, every 10 ms block, including a hidden Mixer tab. The published 13 µs mix includes meters, so the idle saving is not in that number.
- **Local change:** When no reader is subscribed, call a vectorised add-only loop and skip `record_block`. Loudness already leaves the graph when it is off. Meters can follow that: no `LevelMeter` unless the Mixer is shown or a control client subscribed.
- **Benefit:** A fraction of the 13 µs, plus the atomic traffic. Unsplit until a bench runs with meters detached.
- **Risk:** A clip between two reads must still be kept while any reader is active. The two banks exist so the UI and the API do not clear each other.
- **Status (2026-11-21):** done. F-15a (add-only loops without a meter, `rbs-audio`) on main c10a429. The reader gate landed an hour after this pass (9c75965, 2026-11-19): the studio host puts meters on the mixer only while the Mixer is shown or a client subscribes to `audio.levels`. F-15b then turned them off while every window is hidden and cleared peaks kept from before they came off. The saving when unread is F-15a's measured no-meter figure (16 inputs 9.00 to 4.27 µs per block, cloud; `hot-paths.md`).

### F-16: Sample rings are atomic per sample, and input FIFOs slide every block

- **Severity:** P1. **Kind:** runtime. **Confidence:** verified.
- **Where:** `crates/rbs-audio-io/src/ring.rs` stores `to_bits()` into an `AtomicU32` per sample and loads them one by one, including a run that does not wrap. The output ring does the same in reverse (scan). `crates/rbs-audio/src/input.rs` `drop_front` `copy_within`s the unread tail every block, per channel.
- **What happens:** The single-producer protocol already publishes the index with a release store. The compiler cannot turn the atomic loop into a vector copy. PipeWire is planar, then interleaved into atomics, then deinterleaved again (scan). The FIFO capacity for a studio input is on the order of 19 000 frames per channel. The steady slide is the unread window, not the whole capacity.
- **Scale:** Per packet on the device thread and per block on the audio thread, times channels, times each live input and each monitor. The slide is on the order of a microsecond per stereo input at a 50 ms fill. The atomic loop is likely low single-digit microseconds per endpoint. Neither is split out of the 27 µs node figure.
- **Local change:** Atomic indexes only. Plain `f32` in the ring so a non-wrapping run is one copy. A ring index in the FIFO so dropping the front is an add.
- **Larger change:** A planar ring. PipeWire would stop interleaving and deinterleaving.
- **Benefit:** Small beside F-05, and it is pure overhead on every live endpoint.
- **Risk:** A plain-float ring needs an invariant comment and the risky-review checklist. The resampler window must not read torn frames.

### F-17: A media picture republishes every tick, and an audio queue copies the block twice

- **Severity:** P1. **Kind:** runtime. **Confidence:** verified for the media share. The audio queue is scan.
- **Where:** `crates/rbs-nodes/src/media/picture.rs` `dequeue_texture`: when the picture did not change and a texture is showing, it still returns `VideoFrame::share()`. `process` then `set_video`s that share. `share` retains the lease and clones an `Arc`. Still and capture leave the slot alone until the picture changes (scan). `crates/rbs-graph/src/runtime/queue.rs` copies an audio block into a queue buffer and the reader copies it again into the slot. Packet queues clone `Bytes` and do not copy the payload.
- **Scale:** Once per visible media picture per render tick, including a held last frame. Stereo 48 kHz at 10 ms is about 3.8 KB, copied twice, per audio queue that crosses partitions.
- **Local change:** When the picture identity did not change, return without `share`, matching a still. Let the audio reader publish the queued buffer by pointer, or hand the slot across by index.
- **Benefit:** A paused or slowly changing media source drops lease traffic. One of the two sample copies on every inter-partition audio hop goes away.
- **Risk:** A consumer that treats “slot replaced” as “new picture” would stop seeing repeats. Current consumers key on timestamp and sequence.

### F-18: Six video clock edges, and they already disagree

- **Severity:** P1. **Kind:** structure, timestamps. **Confidence:** verified for AVFoundation and Windows Graphics Capture. The other four sites are scan.
- **Where:** AVFoundation `crates/rbs-capture-avf/src/mac.rs` rejects a mapped time more than `MAX_PTS_SKEW` (1 s) from arrival and stamps arrival instead. Windows Graphics Capture `crates/rbs-capture-wgc/src/win.rs` maps, clamps to arrival, and enforces a 1 ns floor, with no skew rejection. PipeWire drops the mapping only when it is more than 1 s before arrival (scan). ScreenCaptureKit and the test adapter have no skew rejection (scan). Each site calls `DriftEstimator` itself.
- **What happens:** A jumped device clock is stamped as arrival on a camera and left up to a second off on a screen capture. Lip-sync and the replay buffer then depend on which adapter produced the frame. Audio capture does not do this. WASAPI, CoreAudio, and PipeWire audio use `EdgeClock`.
- **Scale:** One block per capture callback. The cost is wrong timestamps, not bandwidth.
- **Local change:** One function next to `DriftEstimator`: observe, map, clamp to now, enforce the 1 ns floor, and apply one skew policy.
- **Larger change:** Put that function in `rbs-capture` as the video counterpart of `EdgeClock`, and delete the per-adapter copies.
- **Benefit:** One timestamp rule. A skew fix lands once.
- **Risk:** Unifying on the 1 s rejection changes ScreenCaptureKit and WGC timestamps for clocks that are genuinely offset by more than a second. That needs the drifting-clock test the spec already describes.

### F-19: Repeated frames still upload on AMF, QSV, and VAAPI, and AMF waits by sleeping

- **Severity:** P1. **Kind:** runtime. **Confidence:** verified for the default `keeps_input` and the AMF sleep. The “only NVENC returns true” claim is scan.
- **Where:** `Engine::keeps_input` in `crates/rbs-codec-hw/src/session.rs` defaults to `false`. The session skips `upload` when reuse is on and the picture id matches. AMF `output` with `wait == true` sleeps 1 ms until the picture appears or the deadline passes (`crates/rbs-codec-amf/src/engine.rs`). QSV waits inside `MFXVideoCORE_SyncOperation` (scan). VideoToolbox implements its own in-flight slots and does not go through this session (scan).
- **What happens:** Render-on-change still memcpy’s a full frame on the three engines that leave `keeps_input` false. AMF wakes the encode thread every millisecond while a full queue drains. Fixes to ordering and reuse in `rbs-codec-hw` do not apply to VideoToolbox.
- **Scale:** Every repeated frame on those three encoders. Up to 1 ms of extra delay per poll on AMF when the queue is full.
- **Local change:** Turn `keeps_input` on only after the bit-exact hardware test named in the trait docs. Replace the AMF sleep with the driver’s wait.
- **Larger change:** Route VideoToolbox submit, busy, keyframe, and reuse through the same session type so the port has one implementation.
- **Benefit:** Repeated frames stop copying on every hardware encoder that can keep its input buffer.
- **Risk:** `keeps_input` without that hardware test encodes a stale picture.

### F-20: Filters and invisible layers still draw

- **Severity:** P1 for a filtered camera or a hidden fullscreen layer. P2 for a default colour grade. **Kind:** GPU. **Confidence:** scan.
- **Where:** The compositor pushes a layer at opacity 0 and the shader samples before multiplying by opacity (`composite.wgsl`, `compositor.rs`). There is no test for a layer fully covered by an opaque layer above. A filter on a 4:2:0 source calls `YuvToRgb` before its own passes. The compositor samples NV12 in place and does not decode an unfiltered layer. Colour correction always converts to sRGB and back, including at the default uniforms, which are a mathematical no-op.
- **Scale:** One fullscreen render per new picture of a filtered 4:2:0 source, plus the filter. One extra sample-and-blend per opacity-0 layer on every redraw. One extra fullscreen pass per default colour-correction filter on every new source frame.
- **Local change:** Skip the draw when opacity is 0. If colour-correction uniforms are the identity, publish the input frame.
- **Larger change:** Let a one-pass filter sample the planar texture. One pipeline with the kind in the layer record, and skip layers under an opaque fullscreen layer. Coverage culling is wrong if a later layer is transparent.
- **Benefit:** A filtered camera loses one full-frame decode when the filter can sample planar data. A default colour filter stops being a full-frame copy.
- **Risk:** Colour has to match the in-place YUV shader or a filtered camera will not match an unfiltered one. Skipping the sRGB round trip changes pixels slightly versus today’s defaults.

### F-21: A fully transparent source keeps its filter chain scheduled

- **Severity:** P1. **Kind:** idle. **Confidence:** scan.
- **Where:** `visible_sources` includes every visible item at any opacity, including 0. The binder marks the source and every filter in its chain `Active` together. A hidden temporal source is already left out. A hidden capture is warm on purpose (F-07) and its filters stay scheduled too.
- **What happens:** A media or browser source drawn at opacity 0 keeps decoding. Filters in front of a hidden device run even though the layer is not drawn.
- **Local change:** Leave opacity 0 linked for the fade, and do not mark filters `Active` unless the layer is in the flattened list or the filter is on a path that is heard.
- **Larger change:** A sink edge that keeps a device open without pulling the filter chain into the plan.
- **Benefit:** Fully transparent media stops decoding. Filters on hidden devices stop running while the device can stay open.
- **Risk:** A fade from 0 needs the source already producing. Unlinking at the moment opacity hits 0 pops the start of the fade.

### F-22: The same job has two or six copies, and they have started to drift

- **Severity:** P2. **Kind:** structure. **Confidence:** the two clock sites in F-18 are verified. The rest of this list is scan.
- **Where and what:**
  - Video clock edge, six adapters. See F-18.
  - Media sound and browser sound each own a block ring, a half-period due test, and the same deinterleave (`crates/rbs-nodes/src/media/sound.rs`, `crates/rbs-sources/src/browser_sound.rs`). Media adds underrun rules. Browser sound does not.
  - `crates/rbs-app/src/platform.rs` and `crates/rbs-cli/src/platform.rs` each register muxers, transports, capture, and encoders. The app list continues with text and media decoders. A new encoder added in one file is invisible to the other binary.
  - fMP4 and Matroska each own the Annex B rewrite. See F-02.
  - Capture and playback each implement an atomic interleaved ring (`crates/rbs-audio-io` `ring.rs` and `output.rs`).
  - ScreenCaptureKit and AVFoundation each own an IOSurface `latest_frame` consumer that does not copy pixels. Media Foundation and PipeWire then add their own system-memory slots, which is where F-01 and F-13 get their cost.
  - UI delete is implemented in `Command::DeleteSource` and again in the API match. Volume already goes through one function, `run_ui_api`.
  - `RecordError` becomes a `String` at `rbs-cli` `record` and at `StudioUi::record_all`. `Document::from_body` returns `Result<Self, String>` while the scene crate has `SceneError`.
- **Local change:** One clock function, one deinterleave, one adapter table both binaries call, one Annex B helper, one ring, one IOSurface consumer, and route delete through `run_ui_api`.
- **Benefit:** The next fix lands once. Headless and the app probe the same encoders. A busy canvas stays a `RecordError::CanvasBusy` instead of a string.
- **Risk:** A shared adapter table must not make the CLI register media decoders unless it asks for them. Media’s underrun rules must stay out of the browser sound path.

### F-23: Startup work that runs before the window, and a second scene in the CLI

- **Severity:** P2. **Kind:** startup. **Confidence:** scan.
- **Where:** If `browser-profiles/` is non-empty, `sweep_at_start` loads every scene collection on the UI thread before `MainWindow::new`, including the collection just parsed. `look::load` reads `appearance.toml` again after `forget_missing`. `rbs-cli` `record` builds a display plus the first camera unless `--no-camera`, and with `--encoder` calls `probe` and then `probe_all`. On macOS, studio start asks ScreenCaptureKit for the display list when permission is already granted, even when the scene has no display source. Windows spawns `rbs-os-scheme` to watch the theme registry for the life of the process.
- **Scale:** Once per launch, plus one extra capture for every default CLI record.
- **Local change:** Parse profile ids on the sweep thread. Pass the appearance value that was just read. Default the CLI camera off, or require `--camera`. Read the main display size from Core Graphics and call ScreenCaptureKit when the user adds a display. Take `WM_SETTINGCHANGE` on the UI thread.
- **Benefit:** Window creation no longer waits on every collection. A headless record of a display matches the app’s single composite. Opening the app does not touch ScreenCaptureKit until a display source is asked for.
- **Risk:** Scripts that relied on the automatic corner camera need `--camera`. A collection saved during the sweep can be missed until the next start.

### F-24: Smaller hot-path and control-path items

Each line is a real cost. None of them moves the frame budget by itself.

| Item | Where | Change | Benefit |
| --- | --- | --- | --- |
| Identity channel matrix still dotted inside the sinc loop | `rbs-audio` `input.rs`, `output.rs` | Write through when the matrix is a permutation of ones | A small fraction of the 27 µs. Do it with F-05. |
| Unity gain still walks every sample to flush non-normals | `rbs-audio-filters` `gain.rs` | One sanitiser at the input, or skip the walk when the block is already clean | Well under 1 µs per stereo block. |
| Loudness biquads and true peak stay in `f64` | `rbs-audio` `loudness/` | `f32` biquads after the Tech 3341 tests, and a manual dot for the 12-tap phases | A share of the published 19.7 µs, only while loudness is on. |
| Browser and media sound are interleaved and then deinterleaved | `browser_sound.rs`, `media/sound.rs` | A planar ring and one `copy_from_slice` | About 1 µs per stereo source per block. |
| CoreAudio copies a planar HAL buffer into scratch, then into the atomic ring | `rbs-audio-io-coreaudio` `packets.rs` | Push the HAL slices when each buffer is one channel | One less pass on the IO thread. Goes away with F-16. |
| Noise suppression builds one denoiser per channel and runs one of them | `rbs-audio-filters` `noise.rs` | Build the second denoiser off the audio thread when the mode changes | One denoiser’s FFT plans per stereo instance left on the default mode. No per-block change. |
| RNNoise slides its history by assignment | `rbs-denoise` `features.rs` | `copy_within` | A fraction of a microsecond beside the FFT. |
| Media resampler `push`es one sample and `drain`s history | `rbs-nodes` `media/pcm.rs` | `reserve`, `Kernel::shared`, a ring index | Removes reallocations on the decoder worker when the file rate differs. |
| AAC copies each frame into staging and then into the `AVFrame` | `rbs-codec-ffmpeg` | Point the frame at staging after it is writable | A few microseconds beside F-06. |
| PCM `split_off` allocates the tail every packet | `rbs-codec-pcm` | A read index | One heap allocation per 10 ms PCM packet, beside the accepted `Bytes` copy. |
| Audio encoder partitions are doorbell / `Responsive`, not the periodic encoder class | `rbs-record` `studio.rs`, `rbs-core` `thread.rs` | `ThreadClass::Periodic` at the block period | The 645 µs encode competes as encode work. It does not make the coder faster. |
| `catch_unwind` around every node every tick | `rbs-graph` `executor.rs` | One boundary for the tick, or only around untrusted nodes | Thinner calls. Nanoseconds unknown. A panic then unwinds the rest of that tick unless the worker catches it. |
| Image-track readback allocates a `Vec` inside `process` | `rbs-gpu` `rgba.rs` | Recycle one buffer, as `YuvDownload::read` does | One allocator hit per saved image, on the render thread. |
| Static-scene check builds the draw list before it returns | `rbs-gpu` `node.rs` | Compare the key, then build the list | Well under 1 µs per layer. Does not change the draw count. |
| Export stats `fetch_add` of zero every tick | `rbs-nodes` `export.rs` | Add only when the delta is non-zero | Three atomics off a quiet export tick. |
| Replay status stores on empty ticks | `replay_node.rs` | Publish when a packet was pushed or a clip was cut | A few relaxed stores. The push cost is untouched. |
| Browser picture `try_lock`s an empty mailbox every tick | `browser.rs` `take_due` | A `fresh` flag, the way the video queue already loads one before `try_lock` | Idle browser sources stop touching the mutex. |
| Mixer finds each strip with a linear scan | `mixer.rs` | A side table filled when parameters change | Noise at 16 strips. |
| Item ids and scene order are linear scans | `rbs-scene` `collection.rs` | An id-to-scene map for the sort and for item lookup | Small beside F-09. Sources are already a map. |
| Recording service clones every source state about 10 times a second and scans it | `rbs-record` `recording.rs` | Update failures from events, keyed by node id | The control tick stops cloning names for the whole take. |
| Graph `plan` allocates a map and clones the alive set on every apply | `rbs-graph` `description.rs` | Walk by index and reuse the link buffer | Apply time on large scenes. The tick is unchanged. |
| Loudness strings formatted at 30 Hz | `studio_ui/loudness.rs` | Format when the shown tenth changes | The meter tick stops allocating while the reading is steady. |
| Meter tick scans every row to find an id, and clones the row to compare | `studio_ui.rs` `tick_meters` | An id-to-index map, compare the snapped floats | The meter tick stays a few stores. Only while the Mixer is shown. |
| `scene.list` and `audio.levels` rebuild a snapshot `publish` already has | `studio_host.rs`, `rbs-api` | Answer from the cache. Skip levels when the decibel values match. | Remote polls stop copying the scene. A 30 Hz subscriber stops encoding identical JSON. |
| Colour stills fill 1080p one pixel at a time | `rbs-sources` `still.rs` | `repeat` for a solid colour, or a 1×1 texture the compositor scales | Once per create, about 8 MB of tiny copies. |
| PNG decode copies the decoded buffer | `still.rs` | Move the buffer when it is already RGBA | One copy the size of the picture, at open. |
| Each YUV converter compiles the whole of `yuv.wgsl` | `rbs-gpu` `yuv.rs` | Compile the two entry points in use, and share the module | Prepare time. wgpu may already cache identical WGSL. Not measured. |
| Each GPU node submits its own command buffer | compositor, yuv, filters, preview | One encoder for the render tick | Microseconds to tens of microseconds per submit. Not measured. |
| WGC frame-rate cap ignores a failed `SetMinUpdateInterval` | `rbs-capture-wgc` | On failure, drop publishes younger than the interval | A 30 fps canvas on a 144 Hz display stops importing every refresh when the call fails. The OS copy still happens. |
| A window capture with the cursor polls the window list every 50 ms | `rbs-capture-sck` | Skip the window-server query while the pointer is still and the last query is young | Less window-server traffic. The thread remains while that capture is Active, including while only placed (F-07). |
| Media Foundation CPU file path swizzles into a new `Vec`, and the worker copies it | `rbs-media-mf` | A reused buffer taken by `Bytes::from`. The GPU path is the default when a LUID is present. | The fallback drops from two CPU passes to one. |
| Page sound is copied out of shared memory and then `to_vec`’d | `browser-shm` `sound.rs`, `browser_sound.rs` | Swap a spare `Vec` of packet size | One copy per packet, no allocation after the spare exists. A 10 ms stereo packet is about 4 KB. |
| Text measure blocks the control thread up to 200 ms, and quit joins it on the UI thread | `studio_host.rs` | Apply a default box and patch the size when the measure arrives. Join from a detached shutdown thread. | Text add no longer stalls meter publishing. Quit stays responsive. |
| `check-layers` ignores dev-dependencies | `xtask` `read_edges` | Scan them, or move the encoder examples out of `rbs-nodes` | An example can link NVENC, AMF, and VideoToolbox into an engine crate’s dev graph without the check failing. |
| Hardware audio tests sleep 2–20 ms while polling a fixture | CoreAudio, WASAPI, and PipeWire conformance | Wait on the fixture’s event. Keep the deadline. | The suites stop depending on a sleep slot. `RealTimePacer` stays the hardware-only sleeper. |

## Counts from the structure pass

A walk of `crates/**/*.rs` (1 020 files), reported by that pass and not re-run here:

| What | Count |
| --- | --- |
| `.unwrap(` / `.expect(` in all crate Rust | 9 837 |
| of those under tests, benches, examples | 7 848 |
| outside `#[cfg(test)]`, `#[test]`, and `mod tests` | 14 |
| of those 14, in `rbs-api` conformance expects | 13 |
| the remaining one | `rbs-app/src/look/drawn.rs`, a UTF-8 invariant after blanking comments |
| `unsafe {` blocks | 3 282 |
| `unsafe {` outside test trees and `sys/` | 1 262, mostly adapters (`rbs-gpu` 182, the virtual-camera Media Foundation source 115, `rbs-app` 84) |
| `SAFETY:` or `# Safety` notes | 1 558 |
| `thread::sleep` in `tests/` | 15, in six files |
| `TODO` / `FIXME` | 1 (`rbs-denoise` `pitch.rs`) |

`rbs-graph`, `rbs-engine`, `rbs-nodes` `src`, `rbs-audio` `src`, and the compositor and download files have no `.unwrap(` or `.expect(` outside tests. `rbs-core`, `rbs-graph`, `rbs-engine`, `rbs-nodes`, `rbs-scene`, and `rbs-audio` have no `unsafe {` outside tests. `VideoFrame::native` and `LeaseOwner` are `unsafe` fn and trait with a `# Safety` section. The pass found a safety note within the previous eight lines of every non-test, non-`sys` `unsafe {` block it scanned. `sys/` modules are pages of `unsafe extern` signatures without a comment per signature.

No non-English identifier turned up. `Instant` in engine crates is a hang guard, a benchmark, or a deadline, not a second media clock.

## Already in good shape

These are easy to “optimise” into a regression. Leave them.

- Render-on-change returns before drawing when the layer key is unchanged. Filters do the same. Export and the camera then submit a repeat with no conversion. Preview publish returns before GPU work when nobody is subscribed.
- Loudness and the sync probe are nodes added only while they are used. The equaliser skips a band at 0 dB. The compressor and limiter have rest paths.
- Kernel tables are shared per rate pair. Parameter update is one atomic load until a value is sent, then a `try_lock`.
- Encoded packets fan out by `Bytes` clone. Matching video and audio settings share one encoder. NVENC and VideoToolbox skip the frame copy on the native path.
- The tick does not rebuild the schedule. Only Active nodes that planning kept are in the list. A hidden temporal source is not scheduled. Empty partitions do not start a worker. A disarmed replay has no node. Recording protection starts its probe thread only while a recording or stream is watched.
- Audio capture uses one `EdgeClock`. Annex-B scanning is `annexb_units`. The latest-frame ring in `rbs-capture` is shared by the GPU handoff adapters. `latest_frame` does not wait for a previous frame to be released.
- The meter timer runs only while the Mixer is on screen. The live timer runs only while live or recording. Panel bodies are behind the visible tab. The preview is a native layer.
- `set_mix` sends levels and does not rebind the graph. Plugin binaries are not loaded. Capture and audio factories do not open devices in `new`.

## Draft rules

These are not applied. `CLAUDE.md`, the principles, and the ADRs stay as they are until this report is accepted and a separate change writes the rules. New ADRs still need an independent review.

1. **A full-frame CPU copy is an exception with a name.** Capture, encode, the virtual camera, and the browser each have one allowed transfer into or out of system memory. A second copy of the same frame in the same hop is a defect. Hardware encoders report native memory kinds they can attach. `MemoryKind::System` is the fallback, not the steady path, once the platform importer exists.
2. **Convert a bitstream once per encode, not once per output.** Muxers and RTMP consume a shared `Bytes`. A container that must strip parameter sets says so in one helper.
3. **The 1:1 audio path is a copy plus steering, unless a spec says otherwise.** A 64-tap kernel is for a real rate change. Changing F-05 updates `docs/specs/audio-graph.md` in the same change.
4. **Video sources use one clock-edge function.** New capture adapters call it. They do not paste `DriftEstimator` again. Audio already has `EdgeClock`.
5. **`Mutex::lock` does not appear on a hot path.** A repeat, an empty mailbox, and a stats counter use an atomic or `try_lock`. `Kind::Free` in the downloader is the pattern.
6. **An unchanged picture does not publish.** Slots stay put, as still and capture already do. `submit_repeat` is only for a consumer that needs a new timestamp.
7. **Installed, hidden, and off-scene are measured states.** A feature that is not on screen has no thread, no timer, and no OS capture session, or an ADR names the warm-up and its cost. F-07 is the current named exception for capture, stills, and audio.
8. **Control-plane edits do not clone a document to discover that one field changed.** A generation counter or the inverse command is the cache key. Building a UI row is allowed when that counter moves.
9. **One implementation per port behaviour.** A second ring, resampler, deinterleave, Annex B rewrite, or adapter registration list is a finding in review. The app and the CLI register adapters through one table.
10. **Hot-path `unwrap`, `expect`, allocation, and `thread::sleep` stay out of library `process` and device callbacks.** The current library `unwrap` count outside tests is 14, and 13 of those are conformance expects. New code does not add to the other one without a comment that names the invariant.
11. **A repeated picture does not upload.** `keeps_input` is on only after the bit-exact hardware test. A wait on the encode thread blocks on the driver, not on `sleep`.
12. **Every new feature names its idle cost in the spec.** Threads, wake-ups, and copies while the feature is off. The static-scene budget counts draws. It also counts wake-ups of the render and audio workers.

## What to measure before changing code

Run these on one machine, release, and record the machine. Do not claim a win from this report alone.

| Question | Command or observation | Decides |
| --- | --- | --- |
| How much of the 27 µs input is the sinc, at 48 kHz and at 44.1 kHz? | `cargo bench -p rbs-audio --bench nodes`, split ring, FIFO, and sinc | Whether F-05 is most of the audio budget |
| Meters attached versus detached | The same bench | F-15 |
| `aac_coder` fast versus the current open | `cargo bench -p rbs-codec-ffmpeg --bench encode`, plus a quality check | F-06 |
| One Annex B encode into 1 versus 3 Matroska or FLV outputs | Mux benches, IDR and a 1 s GOP | F-02 |
| RTMP `write_body` versus `write_message` on one IDR | `cargo bench -p rbs-transport-rtmp --bench send` | The second RTMP copy |
| `copy_420` plus AMF or VAAPI submit versus one NVENC native submit | Encoder benches, 1080p and 4K | F-01 |
| Bytes copied per browser frame | `cargo bench -p rbs-sources --bench browser_picture` | F-03 |
| Shared `YuvDownload` `take` on a static canvas, with and without the repeat lock | A few thousand ticks | F-11 |
| `publish` at idle and while recording, 10 and 50 sources | Control-thread time | F-10 |
| Threads and wake-ups for 60 s with an empty scene | `rbs-render` at frame rate, audio at 100 Hz, `rbs-studio` at 4 Hz | F-08 |
| RSS after probing every encoder versus probing on first record | Same binary, two start paths | F-08 |

## What this pass did not do

- No `cargo bench`, no `cargo test`, no app launch, no OBS comparison on a reference machine.
- Shader occupancy, GPU cache hits, and command-buffer submit cost are reasoned, not traced.
- Filter `process` bodies were not timed. The cost of a hidden filter chain (F-21) is “it stays scheduled,” not a microsecond figure.
- `rbs-ui` layout code, the generated Slint crates, and OBS import mappers were sampled, not read line by line.
- The earlier audit and its plan files were not opened. Comments in `resample.rs`, `mixer.rs`, `download.rs`, and `studio_host.rs` cite that work. Those comments were used only as labels on code that already changed. This report does not grade that work.
