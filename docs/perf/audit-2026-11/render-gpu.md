# Render loop and GPU work: performance audit

Scope: `rbs-gpu` (compositor, pool, yuv/download, upload, share, shaders), `rbs-filters`, `rbs-graph` runtime, `rbs-engine/src/worker.rs`, video nodes in `rbs-nodes`, and their wiring in `rbs-record/src/studio.rs`. Every finding was checked against the code in this tree (wgpu 30.0.1). Builds and benchmarks were not run, so every number is an **estimate** unless it says otherwise.

Every change here except R-17, R-20 and R-23 touches a path in `docs/architecture/hot-paths.md`. Those changes need **P15: before/after numbers, the `hot-path-review` checklist and a human maintainer's sign-off.**

Reference sizes used below:
- 1080p `Rgba16Float` canvas: 1920·1080·8 B = **16.6 MB**. At 4K: **66.4 MB**.
- 1080p RGBA8: 8.3 MB. 1080p NV12 with the 256-byte row alignment of `download.rs` (stride 2048): **3.3 MB**. 4K NV12 (stride 3840): **12.4 MB**.
- "Per second" figures assume 60 fps.

---

### R-01 Share one canvas→NV12 conversion and readback across encoders with equal size, format and colour
- Location: `crates/rbs-record/src/studio.rs:754` (`let export = VideoExportNode::with_tone(`), plus the sharing test at `:764` (`let shared_video = running.video.iter().position(|c| { ... c.settings == *video ...`).
- Problem: An export node (`YuvDownload`) is built for every video chain that does not share an *encoder*. Encoders are shared only when *all* video settings are equal, including bitrate, codec and preset. Two typical cases are a recording at high bitrate plus a stream at 6 Mb/s, and a multistream with 4 encodings per canvas (ADR-0127). Both convert the same canvas to the same NV12 size N times, read it back N times and memcpy it N times. This happens whenever two or more outputs differ only in encoder settings.
- Proposal: Key the export by `(canvas, output size, YuvFormat, ColorSpace, open_shadows, pause/hold policy)`. Build one `YuvDownload` per key and fan its `SystemFrame` out to every encoder chain. `Bytes` is refcounted, so sharing costs nothing. Needs one reader per chain on the render→encoder mailbox, or an export node with N video outputs. Chains that pause or hold keep their own export, as today.
- Benefit (estimate), per extra output at 1080p60:
  - GPU: about 33 MB canvas read + 3.3 MB plane write + 3.3 MB copy per frame, about **2.4 GB/s** of memory traffic.
  - PCIe: 3.3 MB per frame, about **200 MB/s** on a discrete GPU.
  - Render thread: one 3.3 MB memcpy, about **0.3 ms** per tick.
  - Memory: about 31 MB (4 staging buffers + 6 CPU buffers + planes).
  - Everything is about 4× at 4K (12.4 MB frames, about 1.2 ms memcpy).
- Effort: M
- Risk: Low/Med. The pause, hold and end (ADR-0045, ADR-0124) state machines are per export today. Keep sharing to the plain run path first.
- Kind: structural ; Hot path: yes (P15)

### R-02 The virtual camera converts and reads back every frame even when no app is reading it
- Location: `crates/rbs-nodes/src/vcam.rs:218-219` (`} else if let Some(slot) = self.session.reserve() { match download.submit(gpu, ...`). The drop happens later, in `crates/rbs-vcam-mf/src/sender.rs:117` (`Sent::Absent => c.count_consumer_absent()`).
- Problem: With the virtual camera started and no consumer (for example Zoom not open), each due frame is still:
  - scaled and converted on the GPU;
  - read back over PCIe;
  - memcpy'd into a `Bytes` on the render thread;
  - packed into the sender's scratch buffer;
  - then dropped as `Absent`.

  This runs all the time while the camera is on.
- Proposal: The sender thread already knows the consumer is absent (`write` returns `Absent`). Publish that as an `AtomicBool` "consumer present" with a short hysteresis, for example absent for 1 s. `VirtualCameraNode::process` reads it once per tick and skips `reserve`/`submit` while it is false. That is one relaxed load and no feature check in a hot loop, since it is the node's own input. Keep the heartbeat.
- Benefit (estimate), per idle camera tick at 1080p30 NV12:
  - 33 MB of GPU canvas reads;
  - 3.3 MB of PCIe;
  - two CPU copies of 3.3 MB, about **0.5 ms CPU per frame**;
  - about **1 GB/s** of GPU traffic.

  All of it drops to zero while no app reads the camera.
- Effort: S
- Risk: Low. The first frame after a consumer appears is late by at most one frame. To verify: the macOS side has no `rbs-vcam` sender adapter here.
- Kind: quick-win ; Hot path: yes (P15)

### R-03 Render on change: skip compositing, conversion and readback when no input changed
- Location: `crates/rbs-gpu/src/node.rs:366` (`self.layers.update();`, whose `bool` result is ignored) and `:412` (`let canvas = local.pool.acquire(gpu, &desc)` runs every tick). The export keys on `(timestamp, sequence)` at `crates/rbs-nodes/src/export.rs:887`, and the compositor output gets a new `sequence` every tick (`frame.with_sequence(tick.index)`).
- Problem: Every tick the compositor clears and redraws every layer into a fresh canvas. Every export then converts it, reads it back and copies it, and the preview downscales it. This happens even when every input slot still holds the same frame:
  - capture nodes keep the old frame when `latest_frame` is `None`;
  - stills and text are static;
  - window and display capture deliver only on change.

  Common cases: a 30 fps camera on a 60 fps canvas makes **half the ticks redundant**. Slides, a code editor or an idle desktop with no camera make **nearly all ticks redundant**.
- Proposal, three steps:
  1. The compositor builds a cheap per-tick key: the layer list changed (`update()` result), each drawn layer's `(TextureId, frame sequence)`, and "any fade or slide in progress". If the key equals the last one, it publishes the **same canvas frame** again (same `TextureId` and `sequence`, new timestamp) and skips acquire, render and submit.
  2. The export keeps the last delivered `SystemFrame` per `sequence`. When the canvas sequence did not change, it resends that `Bytes` with the new timestamp (refcount clone, CFR kept) and skips the GPU conversion and readback.
  3. The preview keys on `sequence` only and skips the publish.

  The encoder still encodes a frame each tick; that cost is a P-frame with no residual.
- Benefit (estimate), per unchanged tick at 1080p with 1 export and the preview: about **16.6 MB × (1 + layers)** compositor traffic, 33 MB conversion reads, 6.6 MB plane/copy, 3.3 MB PCIe, 3.3 MB memcpy, and about 20 MB preview traffic. That is roughly **≥100 MB per tick, ≥6 GB/s** saved at 60 fps for a static scene and about half that for a 30 fps camera scene. It also removes 3+ `queue.submit` calls and their `device.maintain` per skipped tick (see R-12). On an iGPU sharing DDR (about 60-100 GB/s) this is several percent of the bus, plus GPU clocks drop.
- Effort: M (compositor and preview), M (export resend)
- Risk: Med. Every source must bump `sequence` on a new picture; the filter node already does this with `next_generation()`. Sources that render in place into the same texture with the same sequence would freeze. Audit the capture, media, browser and text nodes.
- Kind: architectural ; Hot path: yes (P15)

### R-04 Previews, the vertical canvas and the studio-mode compositor render even when nobody looks
- Location:
  - `crates/rbs-record/src/studio.rs:1575` (`essential.insert(id, "preview");`, always Active);
  - `:1589-1593` (the further canvas's compositor and preview marked essential);
  - the studio-mode preview at `:2954` (always Active while `scene_preview` is set).
- Problem: The scheduler already drops nodes with no path to an `Output` (`SkipReason::NoSink`, `rbs-graph/src/description.rs:206`). However, the preview node is an always-Active `Output`, so the compositor and every source stay scheduled at full rate even when:
  - the main window is minimised or hidden;
  - the vertical panel is collapsed;
  - studio mode's preview is not visible;
  - no recording, stream or virtual camera runs.

  The preview publisher returns early when no UI is subscribed, but the compositor work above it does not stop. The UI stays subscribed while minimised.
- Proposal: Let the UI report "preview visible" per canvas, from window occlusion or minimise (macOS `NSWindow.occlusionState`, Windows `WM_SIZE SIZE_MINIMIZED` / DWM cloaked) and from panel visibility. The host moves the preview node to Standby when it is hidden; it is no longer "essential". With no outputs, the compositor, filters and sources then fall out of the plan by `NoSink`, which is exactly P13. Bring it back on show; the first frame takes one tick.
- Benefit (estimate): GPU and CPU render cost drops to **zero** while the app sits minimised and not live, a common state for a long-running studio app. With a vertical canvas added but its panel hidden, it saves a 1080×1920 compositor (16.6 MB × layers per tick, about 1-2 GB/s) plus its preview. Laptop battery and fan benefit most.
- Effort: M
- Risk: Med. Temporal sources (ADR-0107) and the projector, scene-switch and transition paths need care, and the "essential" fault rule needs changing. A source restart on show may take longer than one tick (cameras), so a hidden-only Standby of the *preview* may be preferable to stopping sources: see the open questions.
- Kind: structural ; Hot path: yes, scheduling (P15)

### R-05 Render preview-only compositors at preview resolution
- Location: `crates/rbs-record/src/studio.rs:2954` (studio-mode compositor `CompositorNode::new(CanvasDesc { width, height }, ...)` at **canvas** size). The preview is 1280 px on its long side (`crates/rbs-app/src/canvas_profile.rs:140`).
- Problem: The studio-mode preview compositor exists only to feed a 1280×720 preview, yet it renders at the full canvas size. The program compositor also renders at full canvas size while no output, projector or vcam needs it (setup time, which is most of the time).
- Proposal:
  - Build the studio-mode compositor at preview size and scale every layer transform by `preview/canvas`. Layer transforms are already affine in canvas pixels, so this is one multiply on the control thread.
  - Optionally, while no full-size consumer of the program canvas is active, run the program compositor at preview size, and switch to full size when an output or a full-size projector starts.
- Benefit (estimate):
  - 4K canvas: 8.3 MP → 0.92 MP, **9×** fewer fragments; per-layer traffic 66 MB → 7.4 MB. Canvas ring memory 3×66 MB → 3×7.4 MB, **about 175 MB saved**.
  - 1080p canvas: 2.25× fewer fragments, 16.6 → 7.4 MB per layer pass.
- Effort: M (studio mode), M/L (program compositor resize on output start)
- Risk: Med. Styled layers, borders and radii are in box units (they scale with the transform, so this is OK). Text and stills sampled at a lower size look the same after the downscale. A program resize on output start must not drop a frame: compositor `prepare` runs on the control thread, which is fine.
- Kind: structural ; Hot path: yes (P15)

### R-06 Opaque-layer occlusion culling and a no-blend pipeline in the compositor
- Location: `crates/rbs-gpu/src/compositor.rs:264` (`blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING)` for every layer) and the draw loop around `:478-493`.
- Problem: Every layer is alpha-blended, which reads the destination, and every layer is drawn even when a later opaque, full-canvas, unstyled layer at opacity 1 covers it. A typical case is a background image or colour under a full-screen game or display capture, with a camera and overlays on top.
- Proposal: In the CPU loop that already walks the layers, find the topmost index whose layer:
  - is `opaque` (or has a known-opaque source format, such as an NV12 decode);
  - has opacity ≥ 1;
  - has no style and no rotation;
  - has a transform that covers `[0,w]×[0,h]`.

  Start drawing there, and skip earlier layers (count them in `RenderStats`). Draw opaque, opacity-1, unstyled layers with a third pipeline with `blend: None`. This costs only CPU compares per layer, which already run.
- Benefit (estimate), at 1080p:
  - Each culled full-screen layer saves source read (8.3-16.6 MB) plus destination read and write (33 MB), **about 2.5-3 GB/s**.
  - Each opaque full-screen layer drawn without blending saves the 16.6 MB destination read, **about 1 GB/s**.
- Effort: S/M
- Risk: Low. `opaque` already means "alpha is ignored". Rounding at canvas edges needs a small tolerance.
- Kind: quick-win ; Hot path: yes (P15)

### R-07 One-pass compute NV12/P010 conversion that writes the download layout directly
- Location: `crates/rbs-gpu/src/yuv.rs:602` (`for (pipeline, view) in [(&self.luma, luma), (&self.chroma, chroma)]`: two render passes) and `crates/rbs-gpu/src/shaders/yuv.wgsl:42-58` (`fs_luma` reads one texel plus one OETF; `fs_chroma` reads four texels plus four OETFs). Then `download.rs` copies both planes texture→buffer.
- Problem: The canvas is read twice per export and the BT.709 OETF (`pow`) runs twice per canvas pixel. On the download path the result is first written to two plane textures, then copied into the staging buffer: an extra write and read of the NV12 frame.
- Proposal: Use one compute pass in which one invocation handles a 2×2 block (or 4×2):
  - load 4 texels once and apply the OETF 4 times;
  - write 4 luma values plus 1 CbCr pair.

  For `YuvDownload`, write packed `u32` words straight into a `STORAGE | COPY_SRC` buffer in the final NV12 stride layout, so no plane textures and no texture→buffer copy are needed. Copy that buffer into the `MAP_READ` staging buffer. On unified memory (M4, iGPUs) with `MAPPABLE_PRIMARY_BUFFERS`, map it directly. The surface path (`convert_to_surface`) keeps render passes where storage writes to `R8Unorm` are not supported.
- Benefit (estimate), per export at 1080p:
  - canvas reads 33 → 16.6 MB;
  - OETF evaluations 4.1M → 2.1M per frame;
  - minus 3.3 MB plane write and 3.3 MB copy read.

  That is about **1.4 GB/s** less traffic at 60 fps, about 5.5 GB/s at 4K. On unified memory, one more 3.3 MB copy goes away.
- Effort: M
- Risk: Med. The output must stay bit-exact with the golden tests (`rbs-gpu/tests/yuv.rs`, `golden.rs`). Workgroup and edge handling are needed for odd strides.
- Kind: structural ; Hot path: yes (P15)

### R-08 Per-frame staging-buffer creation in `write_texture` / `write_buffer` (system-memory sources and compositor records)
- Location:
  - `crates/rbs-gpu/src/upload.rs:112` (`ctx.queue().write_texture(` per system frame);
  - `crates/rbs-gpu/src/compositor.rs:442` (`.write_buffer(&self.layers_buffer, ...)` every tick).

  In wgpu-core 30.0.1, every `write_buffer` and `write_texture` call runs `StagingBuffer::new`, which is a HAL `create_buffer` plus `map_buffer` (`wgpu-core/src/device/queue.rs:662`, `:998`, `:1013`; `resource.rs:1255-1266`).
- Problem: Each system-memory frame allocates a new mapped staging buffer of the frame's size, then frees it after submission. That is 8.3 MB per 1080p BGRA frame, used by PipeWire capture, browser frames, media RGBA uploads and still re-uploads. The compositor allocates a small one every tick per compositor. Allocator churn on the render thread costs time, and on Vulkan and D3D12 it creates a buffer object plus a sub-allocation each time.
- Proposal:
  - Uploads: keep a per-source ring of persistently allocated `MAP_WRITE | COPY_SRC` buffers, refilled after `map_async`, and record `copy_buffer_to_texture` (`wgpu::util::StagingBelt` does the same).
  - Compositor: skip `write_buffer` when the staged records equal last tick's (a memcmp of 64 B × layers; with R-03 this is a no-op anyway). Otherwise use a small belt.
- Benefit (estimate): Removes one 8.3 MB allocation per uploaded 1080p frame, about **20-200 µs CPU per upload** depending on backend, which should be measured. It also removes one small allocation per compositor tick. No GPU traffic change.
- Effort: M
- Risk: Low/Med. The ring must respect `UploadBudget` and must not block: drop or reuse when no mapped buffer is free.
- Kind: structural ; Hot path: yes (P15)

### R-09 Move the 3.3-12.4 MB per-frame memcpy off the render thread
- Location: `crates/rbs-gpu/src/download.rs:475` (`Ok(mapped) => buffer.copy_from_slice(&mapped),`), called from `VideoExportNode::process` (`crates/rbs-nodes/src/export.rs:848`) on the render partition.
- Problem: On the default (readback) path the full NV12 frame is copied from the mapped staging buffer into a recycled `Bytes` on the **render thread**. On NVENC and AMF the encoder thread then copies it again into the encoder's locked input buffer (`crates/rbs-codec-nvenc/src/engine.rs:740-782`), so there are two CPU copies per frame. The first copy lengthens every render tick. This happens always, per export.
- Proposal: Give the encoder a frame whose payload is a lease on the mapped staging slot, with unmap and recycle on drop, handed back through the pool-style lease. The encoder adapter then copies once, from the staging memory into `nvEncLockInputBuffer`, on its own thread. Interim option: keep the copy but do it on the encoder partition, by sending the slot index plus lease through the mailbox.
- Benefit (estimate), with memcpy at about 10 GB/s from a cached readback heap:
  - render tick shorter by **about 0.3 ms per export at 1080p** and **about 1.2 ms at 4K**;
  - one fewer CPU copy per frame in total (200-750 MB/s of memcpy).
- Effort: M
- Risk: Med. wgpu's mapped view lifetime is tied to the buffer; the lease must keep the `wgpu::Buffer` (Clone in wgpu 30) and unmap only on the render thread, through a return queue.
- Kind: structural ; Hot path: yes (P15)

### R-10 Zero-copy encoder hand-off (exit ADR-0038 per platform)
- Location: `crates/rbs-record/src/studio.rs:750-753` (comment: "System memory: the export reads the canvas back (the P4 exception of ADR-0038)") and `docs/status.md` row "Default video encode: CPU readback". The macOS exporter exists but is unused: `crates/rbs-gpu/src/export.rs` (`SurfaceExporter`, renders NV12/P010 straight into IOSurfaces).
- Problem: On every platform today, each frame goes canvas → NV12 planes → staging → CPU copy → (encoder) CPU copy → back to the GPU encoder. That is 2 PCIe transfers and 2 memcpys on a discrete GPU, and 2 extra memcpys on unified memory. It also adds about 1-2 frames of latency, because the readback completes a tick later.
- Proposal, per ADR-0038 exit criteria (b) and (c):
  - **macOS:** let the render→encoder mailbox carry a native frame to its single consumer, and choose `SurfaceExporter` when VideoToolbox declares IOSurface input. All the parts exist; it is mostly wiring.
  - **Windows NVENC:** `NV_ENC_INPUT_RESOURCE_D3D12` (or D3D11 through the NT handle `interop::d3d12` already opens), with a fence. Render the NV12 surface on the wgpu D3D12 device (`CAP_NV12_SURFACE` already exists).
  - **Windows AMF:** `AMFSurface` from a D3D11/D3D12 texture.
  - **Linux VAAPI:** export a DMA-BUF NV12 from Vulkan.
- Benefit (estimate), per encoder:
  - 1080p60: removes 3.3 MB GPU→CPU + 3.3 MB CPU→GPU per frame (**about 200 MB/s each way over PCIe**), 2 × 3.3 MB memcpy (**about 0.6 ms CPU per frame**), and about 31 MB of staging and CPU buffers.
  - 4K60: 750 MB/s each way and **about 2.5 ms CPU per frame**.
  - Encode latency drops by about 1 frame.
  - This is the biggest single win for the "beyond OBS" goal; OBS already does zero-copy NVENC on Windows.
- Effort: L (macOS M)
- Risk: Med/High. FFI, fences, cross-API sync. Needs an ADR per ADR-0038 and the risky-review skill.
- Kind: architectural ; Hot path: yes (P15)

### R-11 NV12 sources: decoded every frame to a full `Rgba16Float` texture
- Location:
  - `crates/rbs-gpu/src/yuv_decode.rs:41` (`pub fn decoded_desc(...) -> TextureDesc { TextureDesc::canvas(width, height) }`);
  - `interop/mod.rs` `import` (decode per frame for NV12 surfaces);
  - `upload.rs:175` (decode per frame for system NV12).
- Problem: Each camera or decoder NV12 frame gets an extra full-screen pass that writes a 16-bit float RGBA texture of the source size. The compositor then reads it back. For an 8-bit source, half of those bytes carry no information. Cost: one pass, one submit and 16.6 MB written plus 16.6 MB read per 1080p frame, versus 3.1 MB of NV12. This runs always, per active NV12 source.
- Proposal:
  - (a) Short term: decode 8-bit NV12 into `Rgba8UnormSrgb`. Hardware stores sRGB-encoded values and decodes them for free on sample, and blending still happens in linear in the Rgba16F canvas. That halves write and read. Keep Rgba16F for P010.
  - (b) Structural: when a source has no filter, let the compositor sample the NV12 planes directly with a YUV→linear `fs_yuv` pipeline (matrix, range and transfer in a per-layer record, like the styled pipeline's record). That removes the decode pass entirely.
- Benefit (estimate), per 1080p60 camera:
  - (a) saves about 16.6 MB per frame, **about 1 GB/s**, plus 8.3 MB of pool memory per in-flight texture;
  - (b) saves about 30 MB per frame, **about 1.8 GB/s**, one submit per frame and the decoded texture memory.
  - At 4K, 4×.
- Effort: (a) S/M, (b) M/L
- Risk: (a) Low/Med: 8-bit quantisation after the matrix, about 0.5 LSB, is invisible after the 8-bit NV12 encode. (b) Med: crop and bilinear filtering of chroma need care, and filters still need RGB input.
- Kind: (a) quick-win, (b) architectural ; Hot path: yes (P15)

### R-12 Many submits per tick, each with a full `device.maintain`
- Location: one `queue().submit` per node call:
  - `compositor.rs:496`;
  - `rbs-filters/src/node.rs:525`;
  - `yuv_decode.rs` decode;
  - `yuv.rs` `convert_*` and `download.rs` submit;
  - `share/mod.rs` `render_preview`;
  - importer retire;
  - plus `GpuRuntime::end_tick` → `shared.poll()` (`node.rs:57`).

  In wgpu-core 30.0.1, `Queue::submit` itself ends with `self.device.maintain(wgt::PollType::Poll, ...)` (`wgpu-core/src/device/queue.rs:1541`). In addition, the preview publisher and importer call `device.poll(Wait{timeout: 0})` per pending item.
- Problem: A typical scene is 2 NV12 cameras with 1 filter each, a display capture, stream plus record exports, and the preview. That is about 8 submits per tick, each with validation, tracker merge, fence signal and a full maintain (triage, resource cleanup, map callbacks), plus 1-4 extra polls. This happens always.
- Proposal: Give `RenderLocal` a per-tick `CommandEncoder`. GPU nodes record into it, and `end_tick` does one submit and then runs a small fixed-capacity list of post-submit actions (`map_async` of download slots, the preview's pending submission index, importer hand-back). Keep the single `poll` at end of tick. Needs a spec and ADR update, since it changes how GPU nodes hand work to the runtime.
- Benefit (estimate): about 7 fewer submit-plus-maintain calls per tick at about 30-100 µs CPU each, so **about 0.2-0.7 ms per tick** on the render thread (1-4 % of a 16.7 ms frame). That depends on backend and should be measured. Fewer command-buffer boundaries also help drivers on iGPUs.
- Effort: L
- Risk: Med. Error isolation: today a failing node does not poison others' submits. With one encoder, a validation error affects the whole tick, so keep error scopes per node. Fence waits from the importer must still precede reads, which is OK because they are queued before the submit.
- Kind: architectural ; Hot path: yes (P15)

### R-13 Fuse single-pass point filters and drop the sRGB decode/encode round trip
- Location:
  - `crates/rbs-filters/src/node.rs:467-525` (each filter node: own pass, own Rgba16F target of source size, own submit);
  - `shaders/color_correction.wgsl:5-7` and `chroma_key.wgsl:5-9` (`sample_linear` decodes sRGB with `pow`, then `linear_to_srgb(unpremultiply(c))` encodes it again with `pow`, then `srgb_to_linear` once more).
- Problem: A chain of k filters on a source makes k full-size Rgba16F passes (16.6 MB write plus 16.6 MB read each at 1080p) and k submits. For an sRGB-encoded 8-bit source (`u[15].w = 1`), colour correction and chroma key evaluate 3 vec3 `pow`s per pixel where 1 is needed. This happens only while filters are active.
- Proposal:
  - (a) When `u[15].w > 0.5`, work on `unpremultiply(raw sample)` directly; that is already the sRGB-encoded colour. Encode to linear once at the end.
  - (b) Build one fused pipeline per chain of consecutive `Input`-sourced single-pass filters (colour correction, chroma key, sharpen as the first stage only), generated at `prepare` by concatenating WGSL functions. The uniforms sit in one buffer at fixed offsets.
- Benefit (estimate):
  - (a) 2 fewer vec3 `pow` per pixel per filter, about 12M transcendental ops per 1080p frame. Small on a discrete GPU, about 0.1 ms on an iGPU.
  - (b) For a chain of 3 point filters on a 1080p60 camera: −2 passes, −66 MB per frame (**about 4 GB/s**), −2 submits.
- Effort: (a) S, (b) M/L
- Risk: (a) Low: math is identical up to f32 rounding (conformance tests). (b) Med: shader generation and validation errors, and the per-filter enable toggle changes the fused pipeline, which then rebuilds on the control thread.
- Kind: (a) quick-win, (b) structural ; Hot path: yes (P15)

### R-14 Blur: second pass at full size with up to 63 taps; no linear-sampling tap merge
- Location: `crates/rbs-filters/src/catalogue.rs:139-146` (`BLUR_PASSES`: pass 1 at `Scale::Half`, pass 2 `target: Scale::Full, source: PassSource::Previous`), `MAX_BLUR_TAPS: usize = 31` (`:133`), `shaders/blur.wgsl:13-15` (loop of `2n+1` `sample_linear` calls).
- Problem: At radius 64 (n = 31), pass 1 does 0.52 MP × 63 fetches and pass 2 does **2.07 MP × 63** fetches, about 163M fetches per 1080p frame. Pass 2 runs at full resolution only to upsample.
- Proposal:
  - Run both directions at half size, then add a 1-tap bilinear upsample pass to full size (or let the compositor do the upsample once filters can output a scaled picture).
  - Use the standard linear-sampling trick: pairs of taps merged into one bilinear fetch at the weighted offset, so 63 → 32 fetches.
- Benefit (estimate): about 0.52 MP × 32 × 2 + 2.07 MP × 1 ≈ 35M fetches, versus 163M: **about 4.5× fewer**, and a full-size pass with 63 taps becomes a 1-tap one.
- Effort: S/M
- Risk: Low. A tiny difference against current output; the golden/conformance thresholds need updating.
- Kind: quick-win ; Hot path: yes (P15)

### R-15 Per-canvas 8-bit canvas format for SDR 8-bit outputs
- Location: `crates/rbs-gpu/src/lib.rs:63` (`pub const CANVAS_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;`), used by the compositor, filters, decode, yuv and preview.
- Problem: Everything works in 16-bit float even when every output is 8-bit NV12 SDR. Canvas rings, filter targets and decoded sources cost 8 B per pixel. At 4K there are 3 canvas slots × 66 MB per compositor (program, studio preview, incoming during a transition, vertical), so 200-800 MB of VRAM.
- Proposal: Make the canvas format per canvas: `Rgba8UnormSrgb` (hardware linear blending, like OBS's linear-sRGB mode) when no P010 or HDR output and no float-only filter is attached; otherwise `Rgba16Float`. Needs a spec and ADR change, because `CANVAS_FORMAT` is a contract.
- Benefit (estimate): Halves canvas bandwidth across compositing, conversion and preview reads (about 50 MB per frame less at 1080p with 1 layer, 1 export and the preview) and halves canvas memory (**about 25 MB per compositor at 1080p, about 100 MB at 4K**).
- Effort: L
- Risk: Med/High. Banding in dark gradients after several blend stages, and filters that use `pow` on 8-bit intermediates. Needs visual A/B tests and an ADR.
- Kind: architectural ; Hot path: yes (P15)

### R-16 Windows timer: process-wide `timeBeginPeriod(1)` on every worker thread
- Location: `crates/rbs-thread-os/src/windows.rs:38` (`if unsafe { timeBeginPeriod(1) } == TIMERR_NOERROR {`), used by the cadence loop `park_timeout` in `crates/rbs-engine/src/worker.rs` (`thread::park_timeout(due.saturating_since(now))`).
- Problem: A 1 ms global timer resolution raises system-wide timer interrupts and package power while RBS runs, even idle. The module's own comment notes that Windows 11 ignores it for a process without a visible window. A minimised app (or the headless engine) then falls back to 15.6 ms granularity, which means pacing jitter exactly when the window is hidden.
- Proposal: Have periodic workers wait on a per-thread `CreateWaitableTimerExW(..., CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, ...)`: sub-millisecond precision, no global side effect, and not subject to the Windows 11 visibility rule. Keep `timeBeginPeriod` only as a fallback before Windows 10 1803.
- Benefit (estimate): Lower idle and system power (Microsoft documents that a raised global timer resolution costs battery) and steady pacing when minimised. Should be measured with `powercfg /energy` and the `late` counter.
- Effort: S/M
- Risk: Low/Med. An FFI change on the worker (hot path, ADR-0030).
- Kind: quick-win ; Hot path: yes (P15)

### R-17 Solid-colour (and gradient) stills are full-size textures
- Location: `crates/rbs-sources/src/registry.rs:1598` (`Ok((w, h, still::color_picture(w, h, first, second, angle)))`), with `still::DEFAULT_SIZE` when no size is given. `StillSourceNode` keeps `self.pixels` (the CPU copy) for re-warm (`crates/rbs-nodes/src/still.rs:121-125`).
- Problem: A solid colour uses w×h×4 bytes of GPU memory plus the same in CPU memory, and full-size texture fetches for every pixel it covers. For a 1080p colour that is 8.3 MB + 8.3 MB. This holds for as long as the source exists.
- Proposal: Upload a 1×1 texture for a solid colour; the layer transform still sizes it, and clamp plus linear sampling gives an exact colour. For gradients, upload a 256×1 (or 1×256, rotated by angle) ramp, or keep full size only for angled gradients.
- Benefit (estimate): −16.6 MB per 1080p solid colour source, and near-zero sampling bandwidth (a 1×1 texture stays in cache).
- Effort: S
- Risk: Low. The picture size reported to the UI must stay the logical w×h.
- Kind: quick-win ; Hot path: no (warm-up only)

### R-18 Texture pool keeps its high-water mark while a size is in use
- Location: `crates/rbs-gpu/src/pool.rs:509-523` (`trim_idle`: drops free textures only of a **description** unused for 120 ticks). `crates/rbs-gpu/src/node.rs:29` (`const CANVAS_RING: usize = 3;` warmed in `warm`). `acquire` pops LIFO (`f.slots.pop()`).
- Problem: Because recycling is LIFO and idleness is tracked per description, a texture that was needed once stays allocated for as long as its size is in use. Examples: the third warmed canvas, or a burst of decode targets during a stall. In steady state the compositor seems to need 2 canvases: one in the output slot and one being drawn. To verify with `pool` stats on a device.
- Proposal: Track a per-free-list low-water mark of `slots.len()` over the idle window and drop that many free textures when it stays above 0. That costs one compare per acquire. Warm `CANVAS_RING = 2` unless a measured consumer needs 3.
- Benefit (estimate): −16.6 MB per compositor at 1080p and −66 MB at 4K, if the third slot is indeed never used. Similar savings for filter outputs and decode targets after spikes.
- Effort: S
- Risk: Low. A later spike recreates a texture once.
- Kind: quick-win ; Hot path: yes, pool (P15)

### R-19 Catmull-Rom upscale conversion recomputes the 4×4 kernel four times per chroma sample
- Location: `crates/rbs-gpu/src/shaders/yuv.wgsl:180-190` (`fs_chroma_catmull`: 4 × `catmull_linear` = 64 `textureLoad` plus 4 OETF per chroma pixel); `fs_chroma_box` similarly runs 4 × N² loads.
- Problem: When the output is larger than the canvas (ADR-0102 upscale), the chroma pass costs (W/2·H/2)·64 loads. That equals the luma pass (W·H·16) again, so all upscaled pixels are filtered twice. This happens only for an upscaled output.
- Proposal: With R-07 (compute, one invocation per 2×2 block), evaluate the 4 Catmull-Rom samples once, using them for both luma and chroma. Otherwise use the 9-tap bilinear Catmull-Rom approximation (9 fetches instead of 16).
- Benefit (estimate): For 1080p→4K, about 265M → about 133M loads per frame (2×). With 9-tap, about 75M (3.5×).
- Effort: S (9-tap) / folded into R-07
- Risk: Low/Med. The 9-tap version is an approximation; golden tests need updating.
- Kind: quick-win ; Hot path: yes (P15)

### R-20 Hardware sRGB decode instead of `pow` after filtering for opaque 8-bit sources
- Location: `crates/rbs-gpu/src/shaders/composite.wgsl:59-63` (`if (l.params.y > 0.5) { ... srgb_to_linear(c.rgb / a) * c.a }`, applied after the bilinear sample); `crates/rbs-gpu/src/upload.rs:47-52` (`PixelFormat::Bgra8 => Some(wgpu::TextureFormat::Bgra8Unorm)`).
- Problem: For BGRA or RGBA capture and uploads, the compositor (and the filter prelude) decodes sRGB with `pow` per fragment, and it filters in sRGB space before decoding, which is slightly wrong at edges when scaled. This happens always for sRGB sources.
- Proposal: For opaque or XRGB sources (or straight-alpha uploads such as stills), upload or alias as `Bgra8UnormSrgb` / `Rgba8UnormSrgb`; the bytes are identical. Imported surfaces can use a `view_formats` sRGB view where the backend allows it. Then the sampler decodes for free and correctly, and `Transfer::Linear` is used. Premultiplied-encoded sources (browser) keep the shader path.
- Benefit (estimate): One vec3 `pow` less per fragment per sRGB layer, which is small on discrete GPUs. Also a correctness gain: linear-light filtering when scaling.
- Effort: S/M
- Risk: Med. A visual change (better scaling), so it needs owner sign-off. Premultiplied sources must not take this path.
- Kind: quick-win ; Hot path: yes (P15)

### R-21 Readback memory footprint per export
- Location: `crates/rbs-gpu/src/download.rs:35` (`DOWNLOAD_RING: usize = 4`), `:39` (`DOWNLOAD_BUFFERS: usize = 6`), `:510` (`BytesMut::zeroed(len + 1)`).
- Problem: Each export holds 4 mapped staging buffers, 6 CPU frame buffers and 2 plane textures: about **33 MB at 1080p and about 130 MB at 4K**, per export. With R-01 not done, that is multiplied per output.
- Proposal: Size the CPU buffers from the encoder's declared queue depth (NVENC's async depth plus the mailbox's one frame). Once R-07 lands, drop the plane textures. Consider a staging ring of 3: one in flight, one mapped, one spare.
- Benefit (estimate): −10 to −15 MB per 1080p export and −40 to −60 MB at 4K.
- Effort: S
- Risk: Low/Med. A lower ring raises `busy` drops under GPU stalls; watch `DownloadStats`.
- Kind: quick-win ; Hot path: yes (P15)

### R-22 Idle render rate when nothing is live
- Location: `crates/rbs-record/src/studio.rs:1525` (`Pacing::cadence(frames)` for the render partition, always the canvas rate).
- Problem: With no output running, the program renders at the full canvas rate (60 fps) only for the preview. The UI shows it on a display that may be 60 Hz, but the user often only arranges scenes.
- Proposal: An optional "preview rate while not live" (for example 30 fps). The render worker swaps cadence when the first output starts, at a tick boundary. Combine with R-03 and R-04.
- Benefit (estimate): About 50 % of render GPU and CPU while idle at 30 fps.
- Effort: M
- Risk: Med. Sources timed by tick (media players) must still use master time; a cadence change must keep tick indices monotonic.
- Kind: structural ; Hot path: yes (P15)

### R-23 Preview and layer minification without mips (quality at equal or lower cost)
- Location: `crates/rbs-gpu/src/shaders/preview.wgsl` (`textureSample(canvas, linear_clamp, in.uv)` from a 4K canvas to 1280 px), the compositor sampling with `textureSample` on mip-less pool textures (`pool.rs` `mip_level_count: 1`), and the bilinear-only `fs_*_scaled`.
- Problem: Downscaling more than 2× with a single bilinear tap aliases and shimmers (4K → 1280 is 3×; a 4K display capture in a small picture-in-picture is 4-8×). Large-stride sampling is also cache-inefficient. This is quality first; performance is neutral to slightly positive.
- Proposal: For the preview, use a 2-pass box/bilinear prefilter at 2× steps, or reuse the YUV box kernel. For layers minified more than 2×, generate one or two mip levels on a new picture only, which pairs with R-03. That costs about +33 % of one write per new frame, so make it per layer and only where it is minified.
- Benefit (estimate): Visible quality gain. The texture cache hit rate improves for heavily minified layers. Cost about +2-5 MB of writes per new 1080p frame per minified layer.
- Effort: M
- Risk: Low. Owner visual approval is needed.
- Kind: structural ; Hot path: yes (P15)

### R-24 Adapter choice on hybrid laptops is always the discrete GPU
- Location: `crates/rbs-app/src/main.rs:5899` (`GpuContext::new(&rbs_gpu::GpuRequest::default())`, where the default is `PowerPreference::HighPerformance`); ranking in `crates/rbs-gpu/src/context.rs:404-415`.
- Problem: On an iGPU-plus-dGPU laptop the dGPU always powers up while RBS runs, including idle. Display capture surfaces come from the iGPU, which needs cross-adapter copies (the D3D12 importer requires the render adapter's LUID: `d3d12::validate`). To verify: what WGC does when the LUIDs differ.
- Proposal: Expose a "GPU: auto / power saving / performance" setting. In "auto", pick the adapter that owns the primary display when no hardware encoder exists only on the other one, and re-evaluate when outputs start (needs a device restart). This is a capability decision, not a vendor one (P3).
- Benefit (estimate): Several watts at idle on hybrid laptops (dGPU idle power), plus avoided cross-adapter copies for display capture. To measure.
- Effort: M/L
- Risk: Med. A device restart moves every pool texture.
- Kind: architectural ; Hot path: no

### R-25 Full-screen passes clear before overwriting every pixel
- Location: `crates/rbs-filters/src/node.rs:511`, `yuv.rs` `record` (`LoadOp::Clear(wgpu::Color::BLACK)`), `yuv_decode.rs` decode, and `share/mod.rs:240`. Each draws a full-viewport triangle with `blend: None`.
- Problem: An unnecessary clear before a pass that writes every pixel. On TBDR (Apple M4) a clear is free; on immediate-mode GPUs it is a fast clear (metadata). The cost is very small.
- Proposal: Use `LoadOp::DontCare` where wgpu 30 offers it. To verify the API: it may be gated as unsafe or experimental.
- Benefit (estimate): Negligible to small (a metadata clear per pass, roughly 6-12 per tick).
- Effort: S
- Risk: Low
- Kind: quick-win ; Hot path: yes (P15)

---

## Dropped after checking (not a finding)
- **"Readback completes one tick late because the device is polled only at end of tick".** Not true. wgpu-core 30's `Queue::submit` runs `device.maintain(Poll)` (`queue.rs:1541`), so the compositor's submit at the start of the next tick already fires the download's map callback before the export's `take_ready`.

## Already good (do not redo)
- The compositor records one pass with one draw per layer, keeps pre-reserved staging `Vec`s, and has no per-frame pipeline or bind group creation. The styled pipeline is used only for styled layers (ADR-0053).
- Bind groups for sampled textures live in `PooledTexture` and are created once per texture. The YUV decode bind groups are cached by plane ids.
- The filter node skips all passes when the input frame and values are unchanged (`node.rs:373`), and rewrites uniforms only on change.
- Capture, still and text nodes keep the previous frame in the slot without work when nothing is new. Text uploads only on change, under `UploadBudget`.
- The preview publisher does no GPU work while no UI is subscribed. Windows and macOS previews are zero-copy shared textures (D3D12 / IOSurface), never pixels.
- `YuvConverter` chooses the kernel and entry points at creation (no per-frame branch). The scaled conversion scales once from the canvas, never from another output.
- The scheduler drops nodes with no sink (`SkipReason::NoSink`): good P13 machinery, which R-04 just needs to use for previews.
- The export never blocks on the GPU (non-blocking `map_async`, ring with counted drops), and its frame buffers recycle without allocation (`Bytes::try_into_mut`).
- `ParamReceiver::update` is one atomic load when nothing is new. Per-tick executor overhead is minimal (`catch_unwind` is free on the happy path).
- Native capture import caches opened surfaces (`MAX_SURFACES` 64) and does not reopen per frame.
- Encoders are already shared when all settings match (ADR-0047). R-01 extends the same idea to the conversion.

## Open questions and measurements needed (each ≤ 2 minutes on a device)
1. **Submit and maintain cost (R-12):** with `RBS_` tracing or a Tracy/Instruments capture, measure the CPU time of `queue.submit` and of `device.poll` on the render thread in the reference scene on M4 (Metal), RTX 4090 (D3D12) and an iGPU. Count submits per tick.
2. **Static-tick share (R-03):** log per tick whether every compositor input `(TextureId, sequence)` and the layer list were unchanged. Report the percentage over 60 s for (a) a camera at 30 fps on a 60 fps canvas and (b) a slides/desktop scene.
3. **Export cost (R-01, R-09, R-10):** GPU time of `YuvDownload::submit` (timestamp queries; `CAP_TIMESTAMP_QUERY` exists) and wall time of `take_ready`'s memcpy at 1080p and 4K on SC-PC. Also the render worker's `TickTimes` with 1 vs 2 outputs of equal size.
4. **Pool high-water (R-18):** after 60 s of steady recording, dump `TexturePool` free-list sizes per description, to check whether the third canvas slot or extra decode targets were ever used.
5. **Upload allocation (R-08):** time `SourceUpload::upload` for a 1080p BGRA frame (PipeWire or browser) and count allocations in it (wgpu's internal `StagingBuffer::new`). Compare with a pre-allocated `StagingBelt`.
6. **Hidden-window cost (R-04, R-16):** with the window minimised and nothing live, measure GPU busy %, package power and render-thread wake-ups for 60 s. On Windows 11 also measure the render worker's `late` counter (timer resolution ignored when the window is hidden?).
7. **Vcam idle (R-02):** with the virtual camera on and no consumer, read `frames_cpu_copied` and the vcam `consumer_absent` counter over 10 s. Expect both to rise at the camera rate today.
8. **Filter chains (R-13, R-14):** GPU time of a 3-filter chain and of a radius-64 blur on a 1080p camera (timestamp queries per pass).
9. **NV12 decode target (R-11):** GPU time and bytes of `YuvDecoder::decode` for a 1080p camera, Rgba16F versus Rgba8UnormSrgb target.
10. **Hybrid adapter (R-24):** on a hybrid laptop, read power with RBS idle on dGPU versus iGPU, and check whether WGC display capture still imports zero-copy when the render adapter differs from the display adapter.
