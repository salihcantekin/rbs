# RBS performance audit: encode, record, stream, output

Auditor area: `rbs-codec*`, `rbs-record`, `rbs-mux*`, `rbs-transport*`, `rbs-media-format`, `rbs-nodes` export/encode/output/replay, `rbs-gpu` `download.rs`/`yuv.rs`/`export.rs`.
Method: code and docs read only (no build, no bench). Every benefit is an **estimate** unless marked measured. Line numbers checked on 2026-11-17.

Reference numbers used below (arithmetic, not measurements):

- 1080p NV12 = 1920 x 1080 x 1.5 = 3.11 MB per frame. The readback staging row is aligned to 256 bytes, so 1920 becomes a 2048-byte stride and one staged frame is 2048 x 1620 = 3.32 MB (+6.7 %).
- 4K NV12 = 3840 x 2160 x 1.5 = 12.44 MB per frame (3840 is already 256-aligned).
- At 60 fps: 1080p is 187 MB/s raw (199 MB/s staged); 4K is 746 MB/s.
- A large `memcpy` is DRAM-bound. Assumed 10-20 GB/s on one desktop core, and somewhat more on the M4 Pro's unified memory.

---

## Today's readback path, end to end (ADR-0038)

Per frame, per video chain, on every platform, the Mac included (`crates/rbs-nodes/src/export.rs:13`, `:306`, `:845-889`):

1. The render thread converts the canvas into NV12 planes on the GPU (`YuvConverter`). That is one render pass.
2. A GPU copy goes from the planes into one of `DOWNLOAD_RING = 4` mappable staging buffers (`crates/rbs-gpu/src/download.rs:35`, `:367-368`). On Windows/RTX 4090 this is a PCIe download.
3. On the **next** render tick, a CPU `memcpy` copies the mapped staging buffer into one of `DOWNLOAD_BUFFERS = 6` recycled `Bytes` (`download.rs:39`, `:475` `buffer.copy_from_slice(&mapped)`).
4. A one-frame mailbox carries the frame to the encoder worker. That worker ticks at the render cadence plus 1/4 frame (`crates/rbs-record/src/studio.rs:1527-1535`).
5. A second CPU `memcpy` copies the frame into the encoder's input buffer:
   - NVENC: `nvEncLockInputBuffer` + `copy_420` (`crates/rbs-codec-nvenc/src/engine.rs:738-779`).
   - AMF: `AMF_MEMORY_HOST` surface (`crates/rbs-codec-amf/src/engine.rs:716-780`).
   - VideoToolbox: `CVPixelBufferPool` plus a row copy (`crates/rbs-codec-videotoolbox/src/mac/session.rs:886-910`).
6. On Windows NVENC, the driver then uploads the frame back to the GPU over PCIe.

Cost per chain:

| | 1080p60 | 4K60 |
| --- | --- | --- |
| CPU `memcpy` (2 copies) | about 386 MB/s, about 2-4 % of a core | about 1.5 GB/s, about 8-15 % of a core |
| Readback memory: (4 staging + 6 frame buffers) x frame | about 33 MB | about 124 MB |
| NVENC input buffers: 8 x frame | about 25 MB | about 100 MB |
| PCIe on Windows/4090 (down + up) | about 0.4 GB/s | about 1.5 GB/s |

The CPU copies also stream about 6 MB per frame through the caches. That evicts working sets that the render, audio and encoder threads need.

Latency: the readback is taken one render tick later, the encoder runs 1/4 frame after that, and NVENC/AMF return a picture on the next encoder tick. The canvas therefore reaches packet output in about 2.25 frame intervals. The budget (`docs/specs/performance-budgets.md:52`) is "glass-to-encoder < 2 frame intervals", and it also has to include capture. To verify on a device.

---

### E-01 Replay buffer opens its own encoders instead of sharing (ADR-0088 decision 2 accepted, not built)
- **Location:** `crates/rbs-record/src/studio/replay.rs:3-9`: "This first version opens its own encoder sessions; sharing the encoders of a running output with equal settings (ADR-0088 decision 2) follows." The defaults are 120 s and 1 GiB (`crates/rbs-app/src/replay_settings.rs:43-53`).
- **Problem:** When the replay buffer is armed next to a recording or stream with the same settings (the default mode is "Match the recording"), RBS runs a second export, a second hardware encoder session and a second AAC encoder. All of them produce bit-identical packets. Streamers typically keep the buffer armed for the whole session.
- **Proposal:** Implement decision 2. The replay ring links to the running chain's `packets` pin (another packet queue on an existing encoder, which is the same fan-out multistream uses), joining at the next keyframe. In the other direction, a recording started while the buffer is armed joins the buffer's chain, so "record and include the last N minutes" (decision 5) gets a continuous file for free.
- **Benefit (estimate):** Per armed buffer at 1080p60:
  - one fewer readback: about 199 MB/s DMA plus about 386 MB/s CPU `memcpy`, about 2-4 % of a core;
  - about 58 MB less RAM (33 MB readback ring + 25 MB NVENC buffers);
  - one fewer hardware session, which counts against the consumer NVENC session cap and the ADR-0087 estimate;
  - one fewer AAC encoder: 6.4 % of a core with FFmpeg on Linux (measured in `docs/specs/output-ports.md:210`); less with MF or AudioToolbox.

  At 4K60, about 4x the readback figures.
- **Effort:** M
- **Risk:** Medium. The ring must cope with a chain that is paused or held (ADR-0124 filters already exist for `start_output`), and the shared chain's lifetime must outlive the output (the multistream "anchor" pattern, ADR-0127 decision 3).
- **Kind:** structural. **Hot path:** yes (packet fan-out from a shared encoder, `ReplayRingNode::process`).

### E-02 Separate-file (ISO) exports read back an unchanged picture on every tick
- **Location:** `crates/rbs-nodes/src/export.rs:871-875`. With `every_tick`, the key is `(tick.time, tick.index)`, always new, so the export calls `download.submit_at(...)` every tick even when the source's picture (`canvas.sequence()`) has not changed. The comment reads "An ISO track stamps the tick, so it takes the same picture again." ISO files are constant frame rate (ADR-0119 decision 2).
- **Problem:** Some ISO sources produce fewer new pictures than ticks:
  - a 30 fps camera on a 60 fps canvas: 50 % of its pictures are repeats;
  - a ScreenCaptureKit or WGC display that only delivers changed frames: about 100 % repeats on a static desktop;
  - images and slides: about 100 % repeats.

  Each repeat still costs a GPU conversion, a 3.3 MB DMA, a 3.3 MB CPU copy, and a ring slot that can make `busy`/`no_buffer` drop real frames.
- **Proposal:** In the export node, when `every_tick` is set and the input's `(timestamp, sequence)` equals the last converted one, send the **last delivered `Bytes` frame again** with the new tick timestamp (`Bytes` is reference-counted, so there is no copy) instead of submitting a new readback. The node already keeps `self.last`. The encoder still receives one frame per tick, so the file stays CFR. The hardware encoder turns a repeated frame into a nearly empty P-frame.
- **Benefit (estimate):** 30 fps camera ISO at 1080p60: −30 conversions, −100 MB/s DMA and −100 MB/s CPU copy per file. Static screen ISO: about −95 % of that file's export work. Up to 16 ISO files are allowed (`crates/rbs-record/src/iso.rs:19`), so this compounds, and the ADR-0087 estimate could then allow more ISO files.
- **Effort:** S
- **Risk:** Low. Keep the frame's `sequence` new, so the encoder node's `(timestamp, sequence)` de-duplication does not skip it, and the hold, pause and end paths do not change.
- **Kind:** quick-win. **Hot path:** yes (`VideoExportNode::process`).

### E-03 Zero-copy encode on macOS: the exporter and VideoToolbox native input exist, only the queue blocks them
- **Location:**
  - The exporter (IOSurface ring, Metal-aliased planes, no readback) is in `crates/rbs-gpu/src/export.rs:1-19`, with `export` at `:263` and `take_ready` at `:305`. No node uses it.
  - The VideoToolbox adapter already accepts `NativeSurface::IoSurface` frames (`crates/rbs-codec-videotoolbox/src/mac/session.rs:1259-1264`, wrapper cache `:758`, `:840`).
  - The blocker is `VideoFrame::share` returning `None` for native payloads (`crates/rbs-core/src/media/video.rs:950`). The video queue writer then counts the frame as `unshareable` and drops it (`crates/rbs-graph/src/runtime/queue.rs:330-337`).
  - `docs/status.md:18` still lists "Default video encode: CPU readback".
- **Problem:** On the M4 Pro every recording and stream pays steps 2-5 above, although ADR-0025's zero-copy path is built. Unified memory makes the copies cheaper than on a discrete GPU, but they still cost DRAM bandwidth (shared with the GPU), power and RAM.
- **Proposal (ADR-0038 exit criteria b and c):**
  1. Let a native frame cross a **single-consumer** video mailbox. The queue moves the frame with its `Lease` instead of `share()`. A replaced frame drops its lease on the writer thread, which lease owners already allow (`queue.rs:40-41`). That stays safe only if `Lease::clone` counts holders or the move is exclusive; to verify.
  2. Let the export node build a `SurfaceExporter` instead of a `YuvDownload` when the chosen encoder declares `input.memory` with `metal`. That is a capability check (P3), not a platform check.

  This is a contract change to the queue and needs `architecture-review`. It also needs a P15 sign-off.
- **Benefit (estimate):** Per chain:
  - 1080p60: −3 copies per frame (GPU blit to staging, staging to `Bytes`, `Bytes` to `CVPixelBuffer`), about −570 MB/s of memory traffic, about 1-2 % of a P-core;
  - 4K60: about −2.2 GB/s, about 5-8 % of a P-core;
  - −33 MB RAM at 1080p (−124 MB at 4K) for the readback ring, plus the VideoToolbox pixel-buffer pool;
  - likely −1 frame of latency if the exporter's `take_ready` runs in the same tick; to verify.

  It applies to every chain, ISO files and own-encoding destinations included.
- **Effort:** M
- **Risk:** Medium. Lease semantics across threads; exporter ring sizing against `FRAMES_IN_FLIGHT = 16` in VideoToolbox (`crates/rbs-codec-videotoolbox/src/lib.rs:59`). ADR-0025 decision 5 limits it to Apple silicon, which matches the owner's Mac.
- **Kind:** architectural. **Hot path:** yes.

### E-04 Share the export (conversion + readback) between chains that differ only in encoder settings
- **Location:** `crates/rbs-record/src/studio.rs:4027-4067`. `VideoPlan::New` always adds an export node *and* an encoder node, and sharing is all-or-nothing on `VideoSettings` plus size (`:3816-3820`). ADR-0127 decision 3 accepts "a second readback and encoder session" for an own encoding.
- **Problem:** Two chains often differ only in bitrate, codec or keyframe interval at the **same output size**. Examples: a 40 Mbps HEVC recording next to a 6 Mbps H.264 stream; multistream own encodings at 1080p for Twitch and YouTube. Each chain then reads back the same NV12 picture again.
- **Proposal:** Split the chain into two shareable parts. The export is keyed by (canvas, output size, pixel format, colour, tone); the encoder is keyed by the full settings. One export's `video` pin feeds N encoder mailboxes. System frames are `Bytes` and already shareable (`video.rs:949`), and the graph already allows several links per output pin. After E-03 or E-05, a native frame needs one surface per consumer or a multi-reader lease. Until then this works on the readback path today.
- **Benefit (estimate):** Each extra chain of the same size saves one readback: 1080p60 −199 MB/s DMA, −199 MB/s CPU copy and −33 MB RAM; 4K60 −746 MB/s and −124 MB. It also frees 1 of `MAX_CHAINS_PER_CANVAS` = 4 render-side GPU conversions.
- **Effort:** M
- **Risk:** Medium. The hold and pause gates live in the export (ADR-0124), so an export can only be shared by chains with the same gate. That fits the anchor rule of ADR-0127 decision 13.
- **Kind:** structural. **Hot path:** yes (new fan-out from the export on the render thread).

### E-05 Zero-copy NVENC on Windows/RTX 4090 through CUDA external memory (reuse the existing CUDA session)
- **Location:**
  - NVENC opens a CUDA context (`NV_ENC_DEVICE_TYPE_CUDA`, `crates/rbs-codec-nvenc/src/engine.rs:112`) with system-memory input buffers (`:594-609`, `:738-779`).
  - The D3D12 interop module only *imports* (`crates/rbs-gpu/src/interop/d3d12.rs:1`).
  - ADR-0025 decision 6 (D3D12 exporter) is a follow-up (issue #83).
- **Problem:** Render (4090) → PCIe download → 2 CPU copies → PCIe upload → NVENC on the same 4090. The pixels leave the GPU only to come back to it.
- **Proposal, smallest step:** Keep `YuvDownload`'s structure, but `copy_texture_to_buffer` into a **device-local, exportable D3D12 buffer** (heap flag `SHARED`, NT handle, created through wgpu-hal) instead of a `MAP_READ` staging buffer. Then:
  1. NVENC's existing CUDA context imports that buffer with `cuImportExternalMemory` + `cuExternalMemoryGetMappedBuffer`;
  2. it registers the pointer once per ring slot (`nvEncRegisterResource`, `NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR`, pitch = the 256-aligned stride);
  3. it syncs on the render queue's D3D12 fence imported with `cuImportExternalSemaphore`.

  This avoids NV12 texture views in wgpu, keeps one NVENC code path for Windows and Linux (Vulkan opaque FD instead of an NT handle), and needs no D3D11 device. Alternatives: an NVENC D3D12 session (`NV_ENC_DEVICE_TYPE_DIRECTX` with D3D12 fences, SDK ≥ 11.1), or D3D11 with `OpenSharedResource1`. Both add a second device type to the adapter.

  Capability: the encoder declares `input.memory = d3d12` or `cuda`, and the export node chooses from it (ADR-0038 exit criterion c).
- **Benefit (estimate):**
  - 1080p60: −386 MB/s CPU `memcpy` (about 2-4 % of a core); −0.4 GB/s of PCIe traffic; −58 MB host RAM (ring + NVENC system input buffers); the GPU-to-GPU copy is about 3.3 MB at about 1 TB/s, about 3 µs.
  - 4K60: −1.5 GB/s of CPU copies (about 8-15 % of a core); −224 MB host RAM.
  - Probably −1 frame of latency (a fence wait instead of a map callback seen on the next tick).
- **Effort:** L
- **Risk:** High: new FFI, `unsafe`, cross-API synchronisation (`risky-review`, `architecture-review`, P15). The AMF case on this machine is the AMD **iGPU**, a different adapter from the render GPU. There the readback is the right path (or a cross-adapter shared heap later), so the export choice has to compare adapter identity (LUID) as a capability. `rbs-record/src/select.rs` has no device-affinity rule today.
- **Kind:** architectural. **Hot path:** yes.

  Per-platform summary of zero-copy hand-off:

  | Encoder | Input today | What zero copy needs | State |
  | --- | --- | --- | --- |
  | VideoToolbox (M4 Pro) | System NV12 into a CVPixelBuffer pool | Queue moves a native frame; export node picks `SurfaceExporter` | Exporter and adapter both built (E-03) |
  | NVENC (4090) | CUDA session, system input buffers | Exportable D3D12/Vulkan buffer or texture, CUDA external memory + semaphore, `nvEncRegisterResource` | Nothing built; D3D12 interop only imports (E-05) |
  | AMF (AMD iGPU on SC-PC) | DX11 device, `AMF_MEMORY_HOST` | Same adapter as render: D3D11/D3D12 shared texture plus `CreateSurfaceFromDX11Native`. Cross-adapter: readback stays | Keep the readback while render is on the 4090 |
  | QSV / VA-API (Linux) | System memory | Vulkan DMA-BUF export → `vaCreateSurfaces` DRM PRIME 2 / `MFX_MEMTYPE_VIDEO` | ADR-0038 says DMA-BUF is unverified on hardware |

### E-06 Interim: drop one of the two CPU copies on the readback path
- **Location:** `crates/rbs-gpu/src/download.rs:459-498` copies staging into recycled `Bytes`; `crates/rbs-codec-hw/src/copy.rs:32-56` copies `Bytes` into the encoder input.
- **Problem:** The staging → `Bytes` copy exists only so the frame can cross threads as `Bytes`. The mapped staging memory itself could be what the encoder reads.
- **Proposal:** Hand the encoder a `System` frame whose `Bytes` points into the **mapped staging buffer**. Use `Bytes::from_owner` (bytes ≥ 1.9) with an owner that holds an `Arc` of the slot. The owner's `Drop` marks the slot for unmapping, and the render thread unmaps it in `take`/`recycle`. Unmapping from the drop thread also works, because `wgpu::Buffer::unmap` takes `&self`; to verify wgpu's threading rules. Raise `DOWNLOAD_RING` to cover the slots the encoder holds, and drop the 6 frame buffers. This belongs in a marked low-level module with `SAFETY` comments.
- **Benefit (estimate):** About −50 % of the CPU copy work on every readback chain until E-03/E-05 land, and for good on cross-adapter AMF: 1080p60 −199 MB/s (about 1-2 % of a core); 4K60 −746 MB/s (about 4-8 %). −20 MB RAM per chain at 1080p (−75 MB at 4K) from the 6 frame buffers.
- **Effort:** M
- **Risk:** Medium. Mapped-range lifetime across threads, which `unsafe` code must uphold. On D3D12 the READBACK heap is CPU-cached, so reading from it is fast; on Metal it is shared memory.
- **Kind:** structural. **Hot path:** yes.

### E-07 Hardware encoders allow no B-frames, so the user's B-frames setting does nothing
- **Location:**
  - Every hardware adapter advertises `codec.video.b_frames.max = 0`: `crates/rbs-codec-hw/src/device.rs:222` and `crates/rbs-codec-videotoolbox/src/mac/mod.rs:209`.
  - NVENC sets `frameIntervalP = 1` (`engine.rs:494`) and `enableLookahead(0)` (`:394-395`).
  - VideoToolbox sets `AllowFrameReordering=false` (`session.rs:673-678`).
  - AMF sets `H264_B_PIC_PATTERN = 0` (`crates/rbs-codec-amf/src/params.rs:174`).
  - The shared session refuses output out of submit order (`crates/rbs-codec-hw/src/session.rs:212-215`) and stamps `dts = pts` (`:222-226`).
  - The profile has `b_frames` (`crates/rbs-app/src/record_profile.rs:668`), default 0.
- **Problem:** H.264 without B-frames needs roughly 10-15 % more bits for the same quality on NVENC Turing+/Ada, and on VideoToolbox H.264/HEVC as well (estimate from common encoder BD-rate results; less for HEVC/AV1 on NVENC). For a fixed-bitrate stream such as Twitch at 6 Mbps, that is visible quality left on the table. OBS defaults NVENC to 2 B-frames.
- **Proposal:**
  1. Generate DTS in `Session`: either a decode-order queue of PTS (dts = pts of the frame `n - reorder_depth`) or the adapter's own DTS (VideoToolbox gives `CMSampleBuffer` decode time; NVENC gives `outputTimeStamp` plus the picture type).
  2. Accept `NEED_MORE_INPUT` (NVENC) and repeated output (AMF) by holding a reorder depth.
  3. Advertise `b_frames.max = 2..4` per codec.
  4. Default 2 for recordings and non-low-latency streams; keep 0 for `low_latency` (WHIP, virtual camera).

  The muxers already carry pts ≠ dts: fMP4 composition offset with `MAX_COMPOSITION_DELAY`, FLV `cts` in `write_body`, MPEG-TS PTS/DTS (`crates/rbs-mux-mpegts/src/muxer.rs:492-498`). Matroska `SimpleBlock` and the replay ring cut at keyframes need a check.
- **Benefit (estimate):** About 10-15 % fewer bits at the same quality (H.264), or the same file and stream size at visibly better quality, at about 2 frames (33 ms) of extra latency and a small increase in encoder memory.
- **Effort:** L (session + 4 adapters + conformance)
- **Risk:** Medium. Timestamp contract (ADR-0043 final ticks, ADR-0124 cut gate re-stamping, `KeyframeAt` windows).
- **Kind:** architectural. **Hot path:** yes (encoder submit/drain).

### E-08 NVENC and AMF quality-per-bit defaults: preset P3, no AQ, no multipass
- **Location:**
  - NVENC preset mapping: Speed → P1, Balanced (the default) → P3, Quality → P4 (`engine.rs:451-455`).
  - Tuning is `HIGH_QUALITY` unless `low_latency` (`:456-460`).
  - No `enableAQ` or `enableTemporalAQ` and no `multiPass` anywhere in `engine.rs` (grep).
  - `EncoderPreset::Balanced` is the default (`crates/rbs-codec/src/config.rs:80-88`; `crates/rbs-record/src/recording.rs:236`).
- **Problem:** On an RTX 4090 a single 1080p60 or 1440p60 encode at P5-P6 with spatial AQ and quarter-resolution two-pass uses a small share of one of its NVENC engines. P3 without AQ leaves visible quality unused at stream bitrates (flat-area banding, smeared motion). AMF and QSV have similar knobs (pre-analysis, VBAQ, high-motion quality boost).
- **Proposal:**
  - Map Balanced → P5 and Quality → P6/P7.
  - Enable spatial AQ (strength auto) by default in CBR/VBR.
  - Use `multiPass = NV_ENC_TWO_PASS_QUARTER_RESOLUTION` for CBR streams.
  - Keep `low_latency` lean.
  - Let the ADR-0087 calibration lower the preset if the measured per-frame encode time exceeds its budget.
  - Equivalent for AMF: `PREANALYSIS_ENABLE` and VBAQ, where `params.rs` marks them optional.
- **Benefit (estimate):** 5-10 % BD-rate improvement (VMAF/PSNR) at the same bitrate, at the cost of more NVENC engine time but not CPU. To verify with a 60 s VMAF A/B on the 4090.
- **Effort:** S (config) + measurement
- **Risk:** Low-Medium. Multipass adds latency inside the encoder; check that ISO files on the same engine (ADR-0119) still fit.
- **Kind:** quick-win. **Hot path:** no (session open), but the encode cost changes per frame, so measure.

### E-09 Readback ring and encoder slots are over-provisioned for RAM
- **Location:**
  - `DOWNLOAD_RING = 4` staging buffers and `DOWNLOAD_BUFFERS = 6` frame buffers (`crates/rbs-gpu/src/download.rs:35`, `:39`), also used for ISO (`crates/rbs-record/src/studio/iso_video.rs:504`).
  - NVENC `FRAMES_IN_FLIGHT = 8` input and bitstream buffers allocated up front (`crates/rbs-codec-nvenc/src/lib.rs:50`, `engine.rs:594-619`); AMF and QSV 4; VideoToolbox 16 (pool, allocated lazily).
- **Problem:** The video queue is a one-frame mailbox, and every hardware session copies on `submit`. A frame buffer is therefore held by at most the mailbox plus the frame being submitted, about 2-3 in steady state. NVENC drains everything except the newest picture each tick (`engine.rs:819-830`), so about 2 slots are in use.
- **Proposal:** `DOWNLOAD_BUFFERS` 6 → 3 and NVENC `FRAMES_IN_FLIGHT` 8 → 4. Keep `DOWNLOAD_RING` at 3-4, since GPU latency spikes are real. Make the values per chain and size them from the measured `busy`/`no_buffer` counters (they already exist in `DownloadStats` and `ExportNodeStats`).
- **Benefit (estimate):** Per chain: 1080p −10 MB (download) −12 MB (NVENC) = about −22 MB; 4K about −87 MB. With 3 ISO files and record + stream on the 4090 at 1080p, about −110 MB.
- **Effort:** S
- **Risk:** Low-Medium. More `no_buffer` drops under jitter, but they are counted, so verify with a 2-minute run.
- **Kind:** quick-win. **Hot path:** yes (ring constants on the readback path).

### E-10 FFmpeg AAC uses the slow `twoloop` coder (Linux)
- **Location:** `crates/rbs-codec-ffmpeg/src/lib.rs` (no `aac_coder` option set); measured in `docs/specs/output-ports.md:210`: "645 us per 10 ms block … twoloop (6.4 % of one core) … the `fast` coder is about twice as fast (not offered yet)".
- **Problem:** Each AAC chain on Linux costs 6.4 % of a core: master, stream with other audio settings, replay (E-01), and each ISO AAC track.
- **Proposal:** Set `aac_coder=fast` by default (`av_opt_set` on the codec context). Keep `twoloop` behind an adapter option for archival recordings, or use it only above 192 kbps where the difference is audible.
- **Benefit (estimate):** About −3.2 % of a core per AAC encoder (measured base, factor from the spec). Linux only; Windows uses MF and macOS uses AudioToolbox.
- **Effort:** S
- **Risk:** Low. Slight quality loss at low bitrates; check with a listening test at 128 kbps.
- **Kind:** quick-win. **Hot path:** yes (audio encoder submit).

### E-11 AMF output wait polls with `sleep(1 ms)`
- **Location:** `crates/rbs-codec-amf/src/engine.rs:836-851`: `QueryOutput` loop with `std::thread::sleep(Duration::from_millis(1))` up to a 10 s deadline. The binding already has `AMF_VIDEO_ENCODER_QUERY_TIMEOUT` (`crates/rbs-codec-amf/src/sys/amf_api.rs:147`).
- **Problem:** Hit when all 4 slots are in flight (`wait = true`). Each iteration is a timer sleep. `timeBeginPeriod(1)` makes it about 1 ms, but on a coarse timer it is 15.6 ms. ADR-0030 notes Windows 11 can ignore the period for a windowless process, and the CLI is headless. It also burns wake-ups.
- **Proposal:** Set `QueryTimeout` (for example 50 ms) at init when `QueryTimeoutSupport` is true, so `QueryOutput` blocks inside the driver. Keep the sleep loop only as a fallback.
- **Benefit (estimate):** Up to −15 ms per stalled wait (fewer encoder `Busy` drops on the iGPU), and about −1000 wake-ups/s while stalled. No effect in steady state.
- **Effort:** S
- **Risk:** Low.
- **Kind:** quick-win. **Hot path:** yes (encoder drain).

### E-12 File transport: wake-up per packet, and the page cache grows with the recording
- **Location:**
  - `crates/rbs-transport-file/src/session.rs:228-234`: `wake` unparks the I/O thread for each packet sent while it sleeps.
  - `:133`: `BufWriter` 256 KiB.
  - `:160-170`: park with a 500 ms safety timeout.
  - `:180-183`: `sync_data` only at the end.
- **Problem:**
  1. About 107 wake-ups/s per file (60 video + about 47 AAC packets), each to copy a few KB into a 256 KiB buffer. A `write` syscall then happens only every few hundred ms.
  2. Hours of 50-100 Mbps recording fill the page cache with dirty, then clean, pages: about 22-45 GB/h. On Linux, `dirty_ratio` write-back can stall writers in bursts. On the M4 Pro's unified memory, the system's "memory used" and compression pressure rise.
- **Proposal:**
  1. Wake the I/O thread only when queued bytes ≥ about 128 KiB, or on a keyframe chunk, or every 250 ms (the producer already tracks `queued_bytes`), plus on stop.
  2. Every few MB, write back and drop what was written:
     - Linux: `sync_file_range(WRITE)` then `posix_fadvise(DONTNEED)` on the older range;
     - macOS: `fcntl(F_NOCACHE)`, or `F_FULLFSYNC`-free `fcntl(F_FLUSH_DATA)` plus `F_NOCACHE`;
     - Windows: keep it cached, or `FILE_FLAG_SEQUENTIAL_SCAN`.

     This also bounds data loss at a crash to a few MB instead of whatever the OS holds (seconds to minutes).
- **Benefit (estimate):** About −100 wake-ups/s per file output (power on the laptop; about 0.05 % of a core). Page cache held by the recording goes from GBs to a few MB, a smoother disk write rate, and a tighter crash bound for `mux.live` formats.
- **Effort:** S (1), M (2, per OS)
- **Risk:** Low (1). Medium (2): per-OS behaviour, and the ADR-0090 disk-lag protection reads `written_timestamp`, so keep it updated.
- **Kind:** quick-win (1), structural (2). **Hot path:** yes (transport send loop).

### E-13 RTMP and SRT toggle the socket blocking mode around every poll
- **Location:**
  - RTMP `poll_reads`: `set_nonblocking(true)` … read … `set_nonblocking(false)` (`crates/rbs-transport-rtmp/src/io.rs:293-319`), called after every idle wake (`:922`) and every 100 ms while busy (`:902-906`).
  - SRT `poll` does the same (`crates/rbs-transport-srt/src/io.rs:490-509`) on every loop iteration while busy (`:1041-1045`).
- **Problem:** Two extra `fcntl`/`ioctlsocket` syscalls per poll. RTMP wakes per packet, so about 200 extra syscalls/s per destination. SRT polls every burst while busy, so at 20 Mbps (about 1900 datagrams/s, bursts of 32) about 120 polls/s, or 240 syscalls/s. Multistreaming multiplies these by up to 7.
- **Proposal:** Use per-call non-blocking reads: `recv` with `MSG_DONTWAIT` on Unix (via `socket2::Socket::recv_with_flags`), or `peek`/`FIONREAD` on Windows. Alternatively keep the socket non-blocking and wait with `poll`/`WSAPoll` with a timeout instead of `SO_RCVTIMEO`.
- **Benefit (estimate):** About −200 to −300 syscalls/s per destination, about 0.05 % of a core each. Tiny alone, but it scales with destinations.
- **Effort:** S
- **Risk:** Low (the TLS wrapper needs the same treatment).
- **Kind:** quick-win. **Hot path:** yes (transport send loop).

### E-14 SRT sender: a 5 ms idle read slice, no pacing, one syscall per 1316-byte datagram
- **Location:**
  - `SLICE = 5 ms` read timeout (`crates/rbs-transport-srt/src/io.rs:28`, `:733`). When idle, the loop blocks in `read` (`:1046`). The producer's `wake` cannot interrupt a blocking `recv`, so a new packet waits up to 5 ms.
  - `BURST = 32` sent back-to-back (`:50`, `:936-990`).
  - `send_datagram` per packet (`:328-331`).
- **Problem:**
  1. 0-5 ms of added latency (about 2.5 ms on average) plus about 200 timer wake-ups/s while idle.
  2. A 150 KB keyframe leaves as roughly 114 datagrams in back-to-back bursts. libsrt paces against `maxbw`/`inputbw`; unpaced bursts overflow shallow router and Wi-Fi queues, which causes loss, then retransmission, then more bandwidth.
  3. At 20-50 Mbps that is 1900-4750 `sendto` calls/s.
- **Proposal:**
  - Wake the loop on new data by sending a 1-byte datagram to its own socket from a loopback-connected wake socket, or by waiting on `poll` over the UDP socket plus a wake pipe or event. Then raise the idle timeout to the ACK and keepalive deadlines.
  - Pace new data at about 1.25x the measured input rate (libsrt's `inputbw` mode), with retransmissions exempt.
  - Batch with `sendmmsg` or UDP GSO (`UDP_SEGMENT`) on Linux and `UDP_SEND_MSG_SIZE` (USO) on Windows. macOS has no batched send; keep the loop there.
- **Benefit (estimate):** About −2.5 ms average latency and about −200 wake-ups/s idle; fewer retransmissions on lossy links (to verify by a `retransmitted` count A/B); at 50 Mbps about −1 % of a core in syscalls on Linux/Windows.
- **Effort:** M
- **Risk:** Medium (pacing must not starve the latency window; conformance tests with a lossy peer).
- **Kind:** structural. **Hot path:** yes.

### E-15 Matroska and fMP4 allocate a new buffer for every cluster or fragment and hold the media twice
- **Location:**
  - fMP4: `let mut chunk = Vec::with_capacity(moof_size + mdat_size)` per fragment (`crates/rbs-mux-fmp4/src/muxer.rs:848`).
  - Matroska: the same per cluster (`crates/rbs-mux-matroska/src/muxer.rs:540`).
  - A fragment or cluster ends at each keyframe (2 s by default), and the crate docs state "one allocation per fragment".
- **Problem:** At a 4K 80 Mbps recording, a 2 s cluster is about 20 MB. Each cluster is a fresh `mmap` (glibc) or `VirtualAlloc`, about 5000 first-touch page faults, then `munmap` after the file thread writes it. The payload is held twice until flush: the packets queued in `samples` plus the cluster copy. At 1080p 6 Mbps it is only about 1.5 MB, so the effect is mainly for high-bitrate and ISO recordings.
- **Proposal:**
  - Either recycle the cluster buffer with the same `Bytes::try_into_mut` trick as `download.rs:504-510`, since the file thread drops its clone after writing;
  - or emit a fragment as a small header chunk plus the samples' own `Bytes` as further chunks. That costs no copy at all, and the file transport already writes chunk by chunk into its `BufWriter`. For Annex B input this also removes the second copy (E-17).
- **Benefit (estimate):** 4K80 recording: about −2500 page faults/s and −20 MB of peak RSS per output; about 0.2-0.5 % of a core.
- **Effort:** S (recycle), M (chunk list)
- **Risk:** Low.
- **Kind:** quick-win. **Hot path:** yes (muxer `write` and `flush`).

### E-16 One output worker plus one transport I/O thread per output (and per multistream destination)
- **Location:** Output partitions are doorbell-paced, one worker each (`crates/rbs-record/src/studio.rs:1537-1542`, `crates/rbs-engine/src/worker.rs:44-53`), and every transport spawns its own I/O thread (file `crates/rbs-transport-file/src/session.rs:303`; RTMP; SRT). ADR-0127's "Alternatives" defers merging.
- **Problem:** Record + stream + replay gives about 10-14 threads: render, audio, 1-3 video encoders, 1-3 audio encoders, 3 outputs, 2 transports. Each packet causes 2 wake-ups (output worker, then I/O thread), about 214/s per output. With 6 destinations, about 1300 wake-ups/s and 12 mostly idle threads, each with a stack (about 2-8 MB virtual, small RSS).
- **Proposal:**
  1. For file outputs, let the output worker write directly through its `BufWriter`. It is already a dedicated, isolated worker whose queue drops GOPs on overflow, so the separate file thread adds no isolation.
  2. Ring the output doorbell from the audio encoder only every N packets, or on video packets. The output does not need 47 wakes/s for audio.
  3. Revisit the ADR-0127 deferral with measured numbers.
- **Benefit (estimate):** −1 thread and about −100 to −200 wake-ups/s per file output; on a 6-destination multistream, about −600 wake-ups/s. That is mostly power and idle C-state residency on the laptop.
- **Effort:** M (1), S (2), L (3)
- **Risk:** Medium. (1) changes the transport contract ("send never blocks") into "the output worker may block"; it needs a new ADR or supersession and an `architecture-review`.
- **Kind:** architectural. **Hot path:** yes.

### E-17 Annex B to length-prefixed conversion is repeated in every muxer for the same packet
- **Location:** NVENC and AMF emit Annex B (`engine.rs:526` `repeatSPSPPS`). Each fMP4, MOV or MKV muxer converts again (`crates/rbs-mux-fmp4/src/muxer.rs:1050-1078`; Matroska "two copies for Annex B input" at `crates/rbs-mux-matroska/src/lib.rs:15-17`), and so does RTMP's `write_body` (`crates/rbs-transport-rtmp/src/io.rs:937-939`).
- **Problem:** With a recording plus 1-7 stream destinations sharing an encoder, the same NAL scan and copy runs once per output. Bench (`hot-paths.md`, 2026-11-16): Matroska GOP 1 s Annex B 612.6 µs vs length-prefixed 431.6 µs, so about +40 % mux time.
- **Proposal:** Convert once at the encoder session, `Bytes` in `StreamDescriptor.bitstream = LengthPrefixed(4)` with avcC/hvcC extradata, since every container except MPEG-TS wants it. MPEG-TS and SRT convert back, which is a cheaper direction because the lengths are known.
- **Benefit (estimate):** About 180 µs/s per Matroska output (measured delta, about 0.02 % of a core) plus one payload copy per output. Small; worth it mainly for multistream.
- **Effort:** S-M
- **Risk:** Low.
- **Kind:** quick-win. **Hot path:** yes.

### E-18 The encoder delivers each packet as a fresh exact-size `Vec`
- **Location:** NVENC `...to_vec()` in `lock_output` (`crates/rbs-codec-nvenc/src/engine.rs:696-697`), then `Bytes::from(output.data)` (`crates/rbs-codec-hw/src/session.rs:229`). VideoToolbox `copy_block` (`session.rs:340-356`). AMF `buffer_bytes`.
- **Problem:** About 60 heap allocations/s per video encoder, and keyframes of 100 KB-1 MB fall above the `mmap` threshold. The port accepts this ("the one allocation per packet the port allows"). The packets go on to live in the replay ring, so a pool would need to be a slab.
- **Proposal:** A per-session slab (`BytesMut` of about 4 MB, `split_to(len).freeze()`, reclaimed when unique). A replay ring holding minutes of packets would pin slabs, so use a slab only for chains without a replay ring, or accept the pinning.
- **Benefit (estimate):** About −60 allocations/s per encoder, mostly fewer page faults on keyframes; ≪ 0.1 % of a core.
- **Effort:** S
- **Risk:** Low.
- **Kind:** quick-win. **Hot path:** yes.

### E-19 Video reaches the encoder by cadence, not by doorbell: up to 1 extra frame of latency and drops from phase jitter (to verify)
- **Location:** `crates/rbs-graph/src/runtime/queue.rs:8-15` ("Video writers never wake the consumer: encoders run at frame cadence"); encoder partitions `Pacing::cadence_after(frames, fps.frame_duration() / 4)` (`crates/rbs-record/src/studio.rs:1527-1535`); readback taken on the next render tick (`export.rs:845-850`).
- **Problem:** The encoder tick runs 1/4 frame (4.2 ms at 60 fps) after the render tick's *start*. If the render tick (compositor + filters + export) takes about 4 ms, ticks jitter around the encoder's sampling instant. The mailbox then replaces a frame (counted `skipped`) and the next tick finds nothing new, which gives a duplicate/drop pair. Latency from canvas to submit is about 1.25 frames.
- **Proposal:**
  - Measure the queue's `skipped` count under a heavy scene first.
  - If it is non-zero, let the video writer ring the encoder doorbell when the mailbox gets a frame (`Pacing::doorbell` for encoder partitions, with the frame-rate timer as a heartbeat). The encoder then runs as soon as its frame exists.
  - Optionally poll the map completion right after the export's `queue.submit` within the same tick (`device.poll(Maintain::Poll)`) so a fast GPU delivers in the same tick.
- **Benefit (estimate):** Removes phase-induced drop/duplicate pairs and about 0.25-1 frame (4-17 ms at 60 fps) of glass-to-encoder latency, which helps the < 2 frame budget.
- **Effort:** M
- **Risk:** Medium (the worker pacing contract, ADR-0026/0027; needs review).
- **Kind:** structural. **Hot path:** yes.

### E-20 VideoToolbox always runs `RealTime=true`, without power hints for outputs where speed does not matter
- **Location:** `crates/rbs-codec-videotoolbox/src/mac/session.rs:669-670` (`RealTime` always true); `PrioritizeEncodingSpeedOverQuality` only for Speed (`:693-699`).
- **Problem:** For ISO files and recordings where a few frames of delay do not matter, `RealTime` plus the default power policy keep the media engine at a higher clock. macOS 13+ has `kVTCompressionPropertyKey_MaximizePowerEfficiency`.
- **Proposal:** Set `MaximizePowerEfficiency=true` for the `Speed` preset and for ISO and replay encoders, behind a capability so it stays optional. Leave the master stream as it is.
- **Benefit (estimate):** Lower package power on the M4 Pro during long ISO takes. To verify with `powermetrics` in 60 s.
- **Effort:** S
- **Risk:** Low (an optional key; ignore errors as for `PrioritizeEncodingSpeedOverQuality`).
- **Kind:** quick-win. **Hot path:** no.

### E-21 Windows: global 1 ms timer resolution per worker, and MMCSS network throttling
- **Location:** `crates/rbs-thread-os/src/windows.rs:1-10`, `:38` (`timeBeginPeriod(1)` for every registered thread, MMCSS "Games" for periodic ones).
- **Problem:**
  1. A 1 ms system timer raises idle power on laptops. RBS could use per-wait high-resolution timers instead.
  2. While any thread is registered with MMCSS, Windows applies `NetworkThrottlingIndex` (10 packets/ms, about 120 Mbps at a 1500-byte MTU) to non-multimedia traffic. A local high-bitrate SRT or multistream to 7 destinations at 6-10 Mbps each is near that ceiling.
- **Proposal:** For waits that need precision, use `CreateWaitableTimerExW(CREATE_WAITABLE_TIMER_HIGH_RESOLUTION)` (Windows 10 1803+) and drop `timeBeginPeriod`. Check the throttling effect with a 7-destination run (this is cross-area with the engine auditor).
- **Benefit (estimate):** Lower idle power; avoids silent throttling of high-bitrate egress. To verify.
- **Effort:** M
- **Risk:** Medium.
- **Kind:** structural. **Hot path:** yes (worker pacing).

### E-22 The 256-byte stride padding at 1920 wide moves 6.7 % more per readback (minor)
- **Location:** `crates/rbs-gpu/src/download.rs:226-229`.
- **Problem:** At 1920 wide the stride is 2048, so +0.21 MB per frame, +12.6 MB/s at 1080p60 in DMA and in the first CPU copy. `copy_420` drops the padding on the second copy anyway.
- **Proposal:** No change needed if E-03, E-05 or E-06 land. Otherwise accept it; this entry only documents where the 3.32 MB comes from.
- **Benefit:** About 6 % of the readback cost (estimate). **Effort:** S. **Risk:** Low. **Kind:** quick-win. **Hot path:** yes.

---

## Already good (verified)

- **No tokio, no async runtime** in any encode, mux or transport crate (`grep tokio` over `crates/*/Cargo.toml`: none). Transports are one blocking I/O thread each, with park/unpark and a fenced sleep handshake.
- **Packets fan out without copying:** `EncodedPacket` payloads are `Bytes`, and transport queues hold references (ADR-0127 decision 16; RTMP `Item` holds the packet).
- **Encoder sharing:** video and audio chains are shared **independently** when settings and size match (`studio.rs:4027-4100`). Multistream defaults to one shared encoder for all destinations (ADR-0127 decision 3), an advantage over obs-multi-rtmp.
- **Unused means free:** partitions get no worker until something is scheduled (ADR-0013/0027), and the replay buffer has no node, thread or memory while disarmed.
- **Readback never blocks the render thread:** the ring is non-blocking with counted drops, and frame buffers are recycled with `Bytes::try_into_mut` (`download.rs:459-510`, with a test).
- **File writes:** a 256 KiB `BufWriter` and `sync_data` only at the end (`crates/rbs-transport-file/src/session.rs:133`, `:180-183`). Fragmented MP4/MOV and Matroska clusters keep the file playable after a crash.
- **Muxers size each fragment or cluster exactly** (no `Vec` regrowth). FLV and MPEG-TS write one exact buffer per packet. The WAV size patch runs about once per MiB, not per write.
- **NVENC drain locks only pictures older than the newest** (`engine.rs:819-830`), so `nvEncLockBitstream` almost never waits.
- **RTMP and SRT:** GOP-aware dropping, an 8 MiB / 3 s queue, stale media discarded on reconnect, newest timestamp on its own cache line (sign-off 2026-11-04), RTMP `TCP_NODELAY`, and one `write` per message.
- **ADR-0119:** one encode worker per ISO file and a `Background` thread class for CPU encoders. ISO audio is tapped without a second mixer.
- **VideoToolbox** caches `CVPixelBuffer` wrappers per IOSurface (`session.rs:758`, `:840`) and allocates pool buffers lazily up to a threshold.

## Open questions / measurements needed (each about 2 min or less on a device)

1. **Ring depth (E-09):** a 2-minute 1080p60 record + stream on the 4090 and on the M4. Read `ExportNodeStats` `busy`, `lost` and `no_buffer` and `GpuMetrics::frames_cpu_copied`. If `no_buffer` and `busy` stay at 0, 3 buffers are enough.
2. **Readback cost today (E-03/E-05/E-06):** 60 s 4K60 recording. Per-thread CPU of the render and encoder workers (Mac: `top -l` with threads or Instruments Time Profiler; Windows: Process Explorer thread view) and `powermetrics --samplers cpu_power,gpu_power` on the Mac.
3. **NVENC sessions (E-01, E-04):** `nvidia-smi encodersessions` while recording, streaming at another bitrate and with replay armed. Expected 3 sessions today; 1-2 after E-01 and E-04.
4. **Quality per bit (E-07/E-08):** a 60 s capture of the same gameplay at 6 Mbps with NVENC P3/no AQ against P5 + spatial AQ (+ 2 B-frames once E-07 exists). `ffmpeg -lavfi libvmaf` against a lossless reference. Also check `nvidia-smi dmon -s u` encoder utilisation.
5. **Phase drops (E-19):** a heavy scene with render tick ≥ 4 ms at 60 fps for 60 s. Read the video queue's `QueueStats::skipped` for the encoder link and the encoder's `last`-key repeats.
6. **Glass-to-encoder latency:** the budget is < 2 frames (`performance-budgets.md:52`). Measure with the existing sync probe or a flashing source plus packet timestamps.
7. **ISO duplicates (E-02):** a 60 s ISO of a static desktop. The fraction of export ticks whose source `sequence` did not change, which equals the readbacks E-02 saves.
8. **SRT pacing (E-14):** 20 Mbps to a listener behind `tc netem loss 1% delay 30ms` (Linux) or Clumsy (Windows). Count retransmitted packets before and after.
9. **AMF on the iGPU:** is the iGPU ever chosen while render is on the 4090? Registration order puts NVENC first (`crates/rbs-app/src/platform.rs:71-75`), but ISO `engine:<id>` (ADR-0119) can choose AMF. In that case the readback is required, and E-06 is the only saving.
10. **WHIP:** `rbs-transport-whip` does not exist in this tree (the `crates/` listing). Its low-latency defaults (no B-frames, `low_latency` tuning, small pacing window) should be specified there when it lands.
11. **`Lease` semantics for E-03/E-04:** does `Lease::clone` count holders so that the exporter ring waits for every holder? If not, native frames can only be moved, never shared, and E-04 then needs one surface per consumer.
