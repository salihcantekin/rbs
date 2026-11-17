# Performance audit: capture and sources

Scope: rbs-capture (port), rbs-capture-{sck,wgc,avf,mf,pipewire}, rbs-nodes capture/still/text/media, rbs-sources (registry, browser, browser_sound, still, text worker), rbs-media-{decode,avf,mf,anim}, rbs-browser-{shm,ipc,runner,cef}, rbs-text-parley, rbs-gpu interop/upload (where sources enter the GPU), rbs-vcam*.
Method: code read only (no build, no bench). Every benefit below is an **estimate** with its arithmetic. Line numbers verified against the tree on 2026-11-17.

Reference sizes used below: BGRA/RGBA8 1080p = 8.3 MB, 1440p = 14.7 MB, 4K = 33.2 MB, 5K (5120x2880) = 59 MB; NV12 1080p = 3.1 MB, 4K = 12.4 MB; Rgba16Float 1080p = 16.6 MB, 4K = 66 MB.

Ordered by benefit/effort.

---

### C-01 Media decode worker wakes every 2 ms, also when paused, held or ended
- Location: `crates/rbs-nodes/src/media/worker.rs:34` (`const PARK: Duration = Duration::from_millis(2);`), parks at `:434` (paused), `:443` (held), `:539` (`thread::park_timeout(next.min(PARK))` while playing).
- Problem: each Active media source's worker wakes at least 500 times a second, whatever the content: a 30 fps video needs ~30 picture decodes plus sound chunks per second, a paused source (until `PAUSE_RELEASE` = 30 s, `player.rs:83`) or a held source (non-looping file at its end, a single-picture GIF/WebP, a range shorter than one picture) needs none. Commands already `unpark` the thread (`player.rs:201`, `control.rs:563`), so the cap only covers the clock-based due time and the return ring.
- Proposal: park until the next due time exactly (`next_due() - horizon`, no 2 ms cap) while playing; park with no timeout (or a long one, e.g. 1 s heartbeat) while paused or held. Have the picture node / sound node `unpark` the worker when it pushes to the return rings or frees a slot it was waiting on (one atomic flag + `Thread::unpark`, not on every tick: only when the worker set a "waiting for space" flag).
- Benefit (estimate): 500 to about 30-100 wakeups/s per playing source, 500 to ~0 per paused/held source. Each wakeup is a syscall pair + clock read + atomics (~2-5 µs) and, more importantly, prevents CPU package idle states: on a laptop or the M4 Pro E-cores, 500 Hz timers per source measurably raise idle power (macOS "Energy Impact"; Windows timer coalescing is defeated). 4 media sources = 2000 wakeups/s today.
- Effort: S
- Risk: Low-Med (must not miss a due time; the return-ring free path must still be serviced).
- Kind: quick-win; Hot path: yes (media hand-off row of hot-paths.md includes the worker).

### C-02 Display/window capture runs at the display's native rate unless the source has an `fps` setting
- Location: `crates/rbs-sources/src/registry.rs:1186-1194` (`if let Some(fps) = fps { request = request.with_frame_rate(fps); }`), `crates/rbs-capture-sck/src/mac.rs:1200-1208` (`frame_interval(None)` = `Duration::ZERO`, so ScreenCaptureKit uses the display refresh, `MAX_RATE` 120 at `:64`), `crates/rbs-capture-wgc/src/win.rs:1217-1219` (`SetMinUpdateInterval` only when a rate was requested; `MAX_RATE` 240 at `:80`), `InstanceContext` (`registry.rs:236`) carries no canvas rate.
- Problem: the desktop Add source path fills `fps` with the canvas rate (`rbs-app/src/studio_host/attributes.rs:384-387`), but anything without it (control API / MCP `source.add` without `fps`, OBS import, `rbs-cli`, older documents, to verify per path) captures at up to 120 Hz (ProMotion) or 144/240 Hz (gaming monitors) for a 30/60 fps canvas. Also the stored `fps` is frozen at add time: changing the canvas from 60 to 30 fps keeps captures at 60. On Windows `SetMinUpdateInterval` errors are ignored (`let _ =`); on builds where it does not exist (to verify: it is a recent WinRT API) there is no cap at all.
- Proposal: (1) put the canvas rate (the max rate over canvases that draw the source) in `InstanceContext` and use it when `fps` is absent; treat a stored `fps` above the canvas rate as the canvas rate. (2) In the WGC `FrameArrived` handler add a software gate: if less than ~0.9 x canvas interval passed since the last published frame and the previous one has not been taken, `Close()` the frame without the GPU copy (see C-11).
- Benefit (estimate): 240 Hz monitor, 60 fps canvas: 4x fewer OS frames, 4x fewer full-frame GPU copies on Windows (4K: 33 MB x 180 saved copies/s = 6 GB/s of VRAM read+write saved; on an iGPU laptop with ~60-100 GB/s shared DRAM that is 6-10 % of all memory bandwidth). On Mac ProMotion 120 Hz vs 60: half the WindowServer/replayd capture work for that stream. Only applies while content changes (both OS APIs skip idle frames).
- Effort: S
- Risk: Low (frame rate is already a negotiated upper bound in the port).
- Kind: quick-win; Hot path: no (configuration), the WGC gate is in the capture hand-off: yes.

### C-03 Browser helper un-premultiplies every pixel on the CPU
- Location: `crates/rbs-browser-cef/src/main.rs:147-151` (`SlotLayout::Rgba8Straight => bgra_premultiplied_to_rgba(pixels, slot, transparent)`), `crates/rbs-browser-ipc/src/page.rs:54-73` (scalar per-pixel loop with a divide), `crates/rbs-app/src/browser_tree.rs:72` (the studio asks for `Rgba8Straight`).
- Problem: every CEF paint runs a scalar loop over all pixels with branches and, for transparent pages, a u32 division per partial-alpha pixel. 1080p = 2.07 M pixels; at ~1-2 ns/pixel that is ~2-4 ms per paint, 60-120 ms CPU per second at 30 fps (6-12 % of a core), 4x that at 4K. It also loses precision (premultiplied -> straight -> premultiplied again in the compositor). ADR-0109 decision 9 already says conversion happens on the GPU, "never with a per-frame CPU pass".
- Proposal: use the existing `SlotLayout::Bgra8Premultiplied` (the helper then does a plain `copy_from_slice`), upload as `Bgra8Unorm` and mark the frame premultiplied so the compositor skips its premultiply (or a tiny swizzle/unpremultiply in the upload shader). Opaque pages: alpha forced to 1 in the shader, not per pixel.
- Benefit (estimate): 2-4 ms CPU per 1080p paint -> ~0.3-0.8 ms memcpy; ~5-10 % of one core per 1080p30 transparent overlay; better edge quality.
- Effort: M (needs a premultiplied flag or Bgra8 premultiplied path in `SourceUpload`/compositor; contract text in ADR-0109 decision 9 says the source picture is straight Rgba8, so the spec sentence about the in-graph format may need a note).
- Risk: Low-Med (alpha handling correctness; covered by pixel tests).
- Kind: quick-win; Hot path: yes (browser picture hand-off).

### C-04 Browser picture is copied three times on the CPU before the GPU upload
- Location: `crates/rbs-browser-shm/src/lib.rs:250-273` (`take_latest` copies the slot into `child.frame`), `crates/rbs-browser-runner/src/lib.rs:70-71` and `:316-318` (supervisor calls `publish(&child.frame, ...)`), `crates/rbs-sources/src/browser.rs:403-455` (`publish_at` copies again into reused `Bytes`), then `SourceUpload::upload` -> `Queue::write_texture` copies into wgpu staging (`rbs-gpu/src/upload.rs:112`) on the render thread.
- Problem: shm -> Vec -> Bytes -> wgpu staging -> GPU. Plus the helper's own CEF buffer -> shm copy. 1080p30: 8.3 MB x 3 studio-side CPU copies x 30 = ~750 MB/s of memcpy per browser source (4K: ~3 GB/s).
- Proposal: (S) let the supervisor copy straight from the shm slot into the reusable `Bytes` of `BrowserFrameSink` (one raw copy, keeps the "never form a reference to helper memory" rule): removes one copy. (M/L, see C-14) let the supervisor write directly into a mapped wgpu staging buffer so the render thread only records `copy_buffer_to_texture`.
- Benefit (estimate): -250 MB/s memcpy per 1080p30 source for the S step (~2-4 % of a core); -500 MB/s and the render-thread memcpy (C-14) for the full step.
- Effort: S (first step)
- Risk: Low.
- Kind: quick-win; Hot path: yes (publish_at is supervisor-side, upload is render side).

### C-05 Displays are always captured at full native resolution, whatever the canvas or the drawn size
- Location: `crates/rbs-capture/src/session.rs:98-99` (`size`: "Requested camera size; ignored for displays and windows"), `crates/rbs-capture-sck/src/mac.rs:1220-1226` (`stream_config.setWidth(size.width)` with the native size from `follow_size`, `:1096-1115`), `crates/rbs-capture-wgc/src/win.rs:961-1030` (full-size `CopySubresourceRegion` into a full-size shared texture).
- Problem: an M4 Pro MacBook's built-in display captures at 3024x1964 (23.8 MB/frame), a 5K Studio Display at 5120x2880 (59 MB/frame), a 4K Windows monitor at 33 MB/frame, for a 1080p canvas, or for a 480p picture-in-picture. Every frame is composited at that size by the OS, held in 7 surfaces (C-11/C-12), and sampled by the compositor with a large minification (bandwidth plus aliasing on text, since sources have no mips).
- Proposal: extend the port so displays/windows accept a target size (or a max long side): ScreenCaptureKit scales in WindowServer for free (`setWidth/setHeight` + `setScalesToFit`), WGC can replace the plain copy in `write_slot` with a downscale blit (D3D11 video processor `VideoProcessorBlt`, already used in `rbs-media-mf/src/gpu.rs:590-610`) into a smaller shared texture, PipeWire can negotiate `VideoSize`. Default target: the largest size any canvas draws the item at (rounded up, capped at native), recomputed on a scale edit with hysteresis (reconfigure is non-real-time and already exists as `CaptureChange`). Keep native when a filter or region asks for full detail. Needs a spec change and an independent review (port contract, rule 11).
- Benefit (estimate): 5K -> 1080p: 59 -> 8.3 MB per frame, 7x less capture bandwidth and surface memory (7 surfaces: 413 MB -> 58 MB, see C-12); at 60 fps 3.5 GB/s -> 0.5 GB/s of IOSurface traffic on unified memory, which is GPU/CPU shared bandwidth on Apple Silicon and iGPU laptops. On Windows 4K -> 1080p: the per-frame copy shrinks 4x (2 GB/s -> 0.5 GB/s at 60 fps) and VRAM per session 232 MB -> 58 MB. Better downscale quality than compositor bilinear (OS scalers filter).
- Effort: M (SCK) / M (WGC) / L total with spec, review and the "drawn size" plumbing.
- Risk: Med (resize churn while dragging an item; region/crop in source pixels must be rescaled; ADR-0058/0103 assume source pixels).
- Kind: architectural; Hot path: yes (capture hand-off).

### C-06 Off-air display/window captures stay fully Active
- Location: `docs/adr/0107-temporal-sources-sleep-when-hidden.md` decision 1 ("Capture sources and stills keep today's rule ... The device stays open"), `crates/rbs-scene/src/flatten.rs:331` (`wanted_sources`), `docs/status.md:51`.
- Problem: a display or window capture placed in any scene, including scenes that are not shown and hidden items, keeps its OS stream at full rate, the WGC GPU copy per frame, the importer work and the surfaces. A typical collection has the same desktop or game window in 3-6 scenes and several different windows in off-air scenes: each costs as if on air. Keeping the device open is right for cameras (reopen latency, exclusive access), but displays and windows can be throttled without closing.
- Proposal (new ADR, supersedes part of ADR-0107 decision 1 for display/window kinds only): when no shown scene (program, preview, hold, on any canvas) draws the source, send `CaptureChange::FrameRate(1 fps)` (ScreenCaptureKit `updateConfiguration` is asynchronous and cheap; WGC `SetMinUpdateInterval`; PipeWire renegotiate or drop) instead of stopping. On show, restore the canvas rate; the last frame (at most 1 s old) is on air at once and the next live frame arrives within one interval. Alternatively a "keep warm at full rate" per-source option for users who want it.
- Benefit (estimate): per off-air 4K60 capture on Windows: ~2 GB/s GPU copy (33 MB x 60) -> 33 MB/s; on Mac the replayd/WindowServer cost of one stream (typically 1-5 % CPU plus GPU per 4K60 stream, to measure) -> ~2 %. With 3 off-air window captures that is the largest steady idle cost in this area.
- Effort: M (binder already computes shown scenes for temporal sources; the change is a reconfigure on membership change).
- Risk: Med (first frame after a cut may be up to 1 s old for one interval; game captures may prefer full warm).
- Kind: architectural; Hot path: no (control-path reconfigure).

### C-07 Linux: PipeWire DMA-BUF import exists but the node never asks for it
- Location: `crates/rbs-nodes/src/capture.rs:90-109` (`import_preferences`: "Vulkan: system memory only until DMA-BUF import is verified on hardware", `_ => (vec![MemoryKind::System], None)`); importer side ready in `crates/rbs-gpu/src/interop/vulkan.rs` and `mod.rs:226-231`; adapter side ready in `crates/rbs-capture-pipewire/src/linux.rs:1275-1300`.
- Problem: every Linux display/camera frame takes the shared-memory path: PipeWire copy into the slot (`linux.rs:1306-1326`), then `write_texture` (wgpu staging copy) and GPU upload. 1080p60 BGRA: 8.3 MB x 2 CPU copies x 60 = ~1 GB/s plus 0.5 GB/s PCIe/upload.
- Proposal: verify DMA-BUF import on one AMD and one Intel Mesa machine (the conformance suite already exists) and return `[DmaBuf, System]` with the modifiers for Vulkan; keep System as fallback.
- Benefit (estimate): ~1 GB/s memcpy and ~0.5 GB/s upload per 1080p60 capture removed; ~5-10 % of a core.
- Effort: S-M (mostly hardware verification).
- Risk: Med (driver/modifier differences; fallback exists).
- Kind: quick-win; Hot path: yes.

### C-08 Every 4:2:0 picture (cameras, macOS video files, NV12 uploads) is expanded to a full Rgba16Float texture each frame
- Location: `crates/rbs-gpu/src/interop/mod.rs:160-195` (import of NV12/P010: `pool.acquire(ctx, &decoded_desc(w, h))` + `decoder.decode(...)`), `crates/rbs-gpu/src/yuv_decode.rs:41-43` (`TextureDesc::canvas` = `CANVAS_FORMAT` = `Rgba16Float`, `rbs-gpu/src/lib.rs:63`), same in `rbs-gpu/src/upload.rs:180-212`.
- Problem: per new picture a full-screen pass reads NV12 (1.5 B/px) and writes 8 B/px. 1080p30 camera: 16.6 MB write + 3.1 MB read = ~600 MB/s; 4K30 camera or 4K60 video: 66 MB x 60 = ~4 GB/s of GPU write bandwidth, plus 66 MB of VRAM per held frame (frames in flight x 66 MB). Then the compositor reads that RGBA16F again.
- Proposal: (a) for a layer with no filter chain, let the compositor sample the two planes directly (YCbCr -> RGB in the layer shader, a second pipeline like the styled-layer one in ADR-0053), so no intermediate texture exists; (b) otherwise decode SDR 8-bit sources into `Rgba8UnormSrgb` (or `Rgb10a2Unorm`) instead of `Rgba16Float`: half the bytes. Overlaps the compositor auditor.
- Benefit (estimate): (a) removes ~600 MB/s (1080p30) to ~4 GB/s (4K60) GPU traffic and 16-66 MB VRAM per frame in flight per source; (b) halves it. On an M4 Pro (~270 GB/s) this is a few %, on a weak iGPU laptop (~50 GB/s shared) a 4K60 video alone is ~8 % of bandwidth today.
- Effort: M (b) / L (a)
- Risk: Med (colour accuracy: linear blending is the reason for 16F; (b) changes precision for HDR/10-bit, so keep 16F for P010/HDR).
- Kind: structural; Hot path: yes (import and colour conversion).

### C-09 Windows video files: decode -> video-processor blit to BGRA -> copy into shared texture -> CPU fence wait, with up to 24 BGRA slots
- Location: `crates/rbs-media-mf/src/gpu.rs:1-25` (module doc), `:83` (`const SLOTS: usize = 24;`), `:415-432` (`CopySubresourceRegion` then `wait_for_gpu()`), `:436-455` (`WaitForSingleObject(self.event, COPY_WAIT_MS)`), `crates/rbs-media-mf/src/win.rs:114` (`MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING`).
- Problem: two GPU passes per picture (VP colour conversion NV12->BGRA into MF's pool, then a copy into a shareable texture), and BGRA is 2.7x the bytes of NV12. VRAM: slots are lazy but the worker decodes `LEAD` (150 ms, `rbs-nodes/src/media/mod.rs:62`) ahead, so ~9-12 BGRA textures are live at 60 fps: 1080p ~100 MB, 4K ~400 MB per media source, plus MF's own pools. The worker thread blocks on a CPU fence wait per picture.
- Proposal: ask the decoder for shareable NV12 output (`MF_SA_D3D11_SHARED_WITHOUT_MUTEX` and `MF_SA_D3D11_BINDFLAGS` on the output stream attributes) and hand the decoder's own textures out as NV12 native pictures (needs NV12 support in the D3D12 importer, `rbs-gpu/src/interop/d3d12.rs:154` has `chroma: None` today; wgpu supports `TextureFormat::NV12` on DX12). Hand out with `SurfaceSync::TimelineFence` instead of a CPU wait (the importer already waits on shared fences, `interop/mod.rs:217-219`). Keep the VP path only for rotation.
- Benefit (estimate): 4K60 video: one 33 MB BGRA write + one 33 MB copy per frame (~4 GB/s) removed, VRAM per source ~400 MB -> ~150 MB (NV12 12.4 MB x 12); no worker stall per frame.
- Effort: L
- Risk: Med-High (MF output pool lifetimes, sharing decoder surfaces across devices; risky-review).
- Kind: architectural; Hot path: yes.

### C-10 Media pictures are decoded 150 ms ahead, the same lead as sound
- Location: `crates/rbs-nodes/src/media/mod.rs:62` (`pub const LEAD: Duration = Duration::from_millis(150);`), `:65` (`PICTURE_SLOTS: usize = 16`), `worker.rs:402-403,456` (one `horizon` for both streams).
- Problem: pictures are queued up to 150 ms ahead: 9 at 60 fps. Each is a native surface (AVF NV12 or MF BGRA slot, `rbs-media-avf/src/surfaces.rs:33` and `rbs-media-mf/src/gpu.rs:83`, 24 slots each) or a CPU RGBA frame. Sound needs the lead for the audio latency; pictures need only enough to hide decode jitter (2-3 frames).
- Proposal: a separate picture lead (e.g. max(3 frame periods, 50 ms)), sound keeps `LEAD`; slots (24) sized from it.
- Benefit (estimate): decoded pictures in flight 9 -> 3 at 60 fps: 4K BGRA (Windows) ~300 MB -> ~100 MB VRAM per source; 4K NV12 (Mac) ~110 MB -> ~37 MB.
- Effort: M
- Risk: Med (a slow hardware decode of a 4K HEVC GOP start could miss; measure the decode time distribution first).
- Kind: structural; Hot path: yes.

### C-11 WGC: full-frame GPU copy on every arrival and 7 full-size surfaces per session
- Location: `crates/rbs-capture-wgc/src/win.rs:902-911` (claim then copy for every arrived frame), `:1022-1028` (`CopySubresourceRegion` + `Signal` + `Flush`), `:77` (`POOL_BUFFERS: i32 = 2`), `crates/rbs-capture-wgc/src/lib.rs:56` (`RING_SLOTS: usize = 5`).
- Problem: the copy exists because WGC pool surfaces are not shareable (correct). But a frame that arrives while the previous one is still untaken is copied and then replaced before the render tick reads it (wasted copy). Memory: 5 shared + 2 pool = 7 x 33 MB = 232 MB VRAM for one 4K display capture; two monitors or a monitor and a game window ~0.5 GB.
- Proposal: (1) rate gate (C-02). (2) Measure the max of `handed_out` slots over a session; if the consumer never holds more than 2, `RING_SLOTS` 4 is enough (`FRAMES_IN_FLIGHT` 2). (3) Combine with C-05 so the copy is a downscale and the slots are canvas-size.
- Benefit (estimate): (2) -33 MB VRAM per 4K session; (1)+(3) see C-02/C-05.
- Effort: S (measure + constant)
- Risk: Low-Med (more drops if the compositor does hold 3).
- Kind: quick-win; Hot path: yes.

### C-12 ScreenCaptureKit queue depth 7 at native size
- Location: `crates/rbs-capture-sck/src/lib.rs:58-67` (`RING_SLOTS = 5`, `QUEUE_DEPTH = RING_SLOTS + 2`), `mac.rs:1230` (`setQueueDepth`).
- Problem: up to 7 IOSurfaces at native size per stream: 5K = 413 MB, built-in 3024x1964 = 167 MB, per display capture (unified memory on Apple Silicon: counted against the system's RAM). Apple's default is 3. The doc comment itself says free slots could release early.
- Proposal: measure peak handed-out slots on the M4 Pro at 60 and 120 Hz; try `QUEUE_DEPTH` 5 (ring 4) and release a free slot's pixel buffer back to SCK once it is not the latest. Together with C-05 the surfaces also become canvas-size.
- Benefit (estimate): -2 surfaces = -118 MB (5K) / -48 MB (built-in) per display capture; with C-05 total < 60 MB.
- Effort: S
- Risk: Low-Med (drops at 120 Hz; the hardware run decides).
- Kind: quick-win; Hot path: yes.

### C-13 Region capture in adapters (ADR-0103 defers it)
- Location: `docs/adr/0103-...md:12,18,27` ("Capture adapters still deliver the whole surface", "Bandwidth still captures the whole window until a later region-capture ADR"); SCK config `mac.rs:1219-1233` (no `setSourceRect`); WGC `win.rs:1009-1016` (copy box = whole frame).
- Problem: a source region (e.g. the video pane of a browser window, a quarter of a 4K display) still captures, copies, imports and filters the whole surface.
- Proposal: a follow-up ADR: adapters accept a source rect. SCK `setSourceRect` (+ the C-05 output size), WGC copies only the region box into a region-size shared texture (the copy already takes a `D3D11_BOX`), PipeWire uses the crop meta. Filters then also run on the region only.
- Benefit (estimate): proportional to the region: a quarter of a 4K display = 4x less copy (2 GB/s -> 0.5 GB/s at 60 fps on Windows) and 4x less per-pixel filter work.
- Effort: M-L
- Risk: Med (region edits must reconfigure without restarting the stream; ADR-0103 decision 1 says an edit must not replace the node).
- Kind: architectural; Hot path: yes.

### C-14 One shared "producer writes into GPU-visible staging" path for all CPU sources
- Location: `crates/rbs-gpu/src/upload.rs:7,112` (`Queue::write_texture` on the render thread), callers: browser `rbs-sources/src/browser.rs:822-833`, MF cameras via `rbs-nodes/src/capture.rs:262-268`, PipeWire shm, animated pictures `rbs-nodes/src/media/picture.rs:530-535`.
- Problem: every CPU source pays producer copy -> (ring/Bytes) -> `write_texture` memcpy into wgpu's staging on the render thread -> GPU copy. The render-thread memcpy is 8.3 MB per new 1080p picture (~0.8-1.5 ms at 6-10 GB/s), 33 MB at 4K (~3-5 ms), inside a 16.7 ms tick. ADR-0109 decision 9 even says "The fallback upload runs off the render thread", which the browser node does not do today (`browser.rs:822-833`).
- Proposal: a small staging pool in `rbs-gpu`: N persistently mapped upload buffers (or `map_async` recycled), handed to producer threads; the producer copies once from its source (MF locked buffer, PipeWire shm, browser shm slot, decoder canvas) into the mapped buffer, publishes the slot lock-free; the render thread only records `copy_buffer_to_texture`. On Apple Silicon (unified memory) and Windows iGPUs, use a host-visible buffer or an IOSurface the GPU samples directly: zero GPU copy. Needs an ADR (crosses rbs-gpu, rbs-capture adapters, sources).
- Benefit (estimate): per 1080p30 CPU source: -1 to -2 memcpy of 8.3 MB (250-500 MB/s) and ~1 ms/new-frame off the render thread; at 4K30 ~3-5 ms of render-thread time per new frame removed, which directly protects frame pacing on weak laptops.
- Effort: L
- Risk: Med-High (lifetimes of mapped buffers vs GPU use, wgpu mapping rules; risky-review, hot-path-review).
- Kind: architectural; Hot path: yes.

### C-15 Windows cameras: copy in the MF callback, no MJPEG, alpha written per pixel
- Location: `crates/rbs-capture-mf/src/lib.rs:1-22` ("native uncompressed formats in system memory", one copy), `:183-245` (`copy_picture`, per-pixel alpha loop at `:238-242`), `:348-360` (`drop(data.clone())` allocates the shared `Bytes` header per frame on the MF thread), `lib.rs:56-59` (NV12, YUY2, I420, BGRA only); `rbs-gpu/src/upload.rs:61-66` (`UPLOAD_FORMATS` has no YUY2 or I420).
- Problem: (a) one CPU copy into the ring plus `write_texture`'s copy: 1080p30 NV12 = 2 x 3.1 MB x 30 = 190 MB/s, 4K30 = 750 MB/s. (b) Many USB webcams offer 1080p30/60 only as MJPEG; uncompressed YUY2 over USB2 tops out around 1080p5 / 720p10, so the camera falls back to a lower mode (quality, not CPU). YUY2/I420 modes are listed by the adapter but the upload path does not take them (format negotiation picks NV12 first; a YUY2-only camera fails with FormatUnsupported, to verify). (c) BGRX cameras: alpha set per pixel on the CPU; the compositor already has `Layer::opaque` for X formats (`interop/vulkan.rs:177`).
- Proposal: (a) keep the `IMFSample` (locked buffer) as the frame's lease instead of copying, or write straight into the C-14 staging; (b) add MJPEG with the Media Foundation hardware MJPEG decoder on a D3D11 device of the render adapter, output as a shared NV12 texture (same scheme as `rbs-media-mf`); YUY2 upload as an `Rg8`/`Rgba8` texture decoded by the YUV shader; (c) drop the alpha loop and draw BGRX as opaque.
- Benefit (estimate): (a) -95 MB/s (1080p30) to -375 MB/s (4K30) memcpy; (b) 1080p60 instead of 720p-ish on USB2 webcams, with no CPU decode; (c) ~1 ms per 1080p BGRX frame.
- Effort: S (c) / M (a) / L (b)
- Risk: Low (c) / Med (a: sample lifetime vs MF pool) / Med (b).
- Kind: structural; Hot path: yes.

### C-16 Animated GIF/APNG/WebP: full decode and upload on every loop, one full-canvas allocation per picture
- Location: `crates/rbs-media-anim/src/lib.rs:345,449,495` (`rgba: self.canvas.clone()` per composed frame), `:187-200` (`seek` reopens the file and composes forward), picture upload per frame through `SourceUpload` (`rbs-nodes/src/media/picture.rs:530-535`).
- Problem: a looping 1080p animated overlay re-decodes (LZW/deflate/VP8L + compositing) and re-uploads every picture on every pass, and allocates + frees 8.3 MB per picture (the free happens on the worker, good). Typical overlay GIFs loop forever on air.
- Proposal: when the decoded frames of the whole animation fit a budget (e.g. <= 64 MB GPU, i.e. 8 frames at 1080p or 60 frames at 480p), keep them as GPU textures after the first pass and only swap texture ids afterwards (no decode, no upload, worker parked). Otherwise reuse a pool of `Vec<u8>` buffers instead of `clone()`.
- Benefit (estimate): a 480x270 30-frame 25 fps GIF: ~10-20 ms CPU per second of decode + 13 MB/s upload -> ~0 after the first loop; 1080p animated WebP at 30 fps: 250 MB/s alloc+copy -> 0.
- Effort: M
- Risk: Low-Med (memory budget policy; seek/range must still work).
- Kind: structural; Hot path: yes (media picture hand-off).

### C-17 Still images: full-resolution CPU copy kept for the node's life, full-res GPU texture without mips; solid colours stored as 1920x1080 textures
- Location: `crates/rbs-nodes/src/still.rs:65,94` (`pixels: Bytes` kept), `:118-138` (`warm` uploads), `crates/rbs-sources/src/still.rs:10-14` (`MAX_SIDE` 8192, `MAX_BYTES` 128 MiB, `DEFAULT_SIZE` 1920x1080), `:68-81` (`color_picture` fills w x h pixels), `registry.rs:1532-1547`.
- Problem: an 8192x8192 PNG costs 268 MB RAM (kept, ADR-0074) + 268 MB VRAM, even if drawn as a 300 px logo; a 4000x3000 photo 48 + 48 MB. Strong minification of a texture with no mip chain shimmers and reads much more memory than needed. A solid colour source costs 8.3 MB RAM + 8.3 MB VRAM for one colour.
- Proposal: (1) after the upload, drop the CPU pixels and re-decode on the next `warm` (control thread, file still there; keep CPU pixels only for generated pictures, which are cheap to regenerate anyway). (2) At load, downscale to the largest size any canvas could draw it at (e.g. min(native, 2 x canvas long side)) or generate mips. (3) Solid colour: a 1x1 texture stretched by the layer (gradients: a small texture, e.g. 256 px, stretched with linear filtering, or analytic in the shader), keeping the "natural size" in `picture_size` separate from the texture size.
- Benefit (estimate): (1) -48 to -268 MB RAM per large image; (2) up to 10-50x less VRAM for oversized images; (3) -16 MB per colour source (RAM + VRAM).
- Effort: S (1) / M (2, 3)
- Risk: Low (1: an edited/removed file then fails on rewarm, report as fault; ADR-0074 states the CPU-pixels rule, so (1) needs a new ADR or a Review note) / Med (3: compositor/layout assume texture size = natural size, to verify).
- Kind: quick-win / structural; Hot path: no (control thread, warm).

### C-18 Text rasterisation works on full-box f32 buffers
- Location: `crates/rbs-text-parley/src/lib.rs:350-363` (`Mask::new(bw, bh)` for fill and stroke), `compose.rs:19` (f32 mask), `:74` (blur temp), `:156` (`vec![[0.0; 4]; w * h]`, 16 B/pixel), `:219` (final RGBA).
- Problem: each render allocates ~4 + 4 + 16 + 4 = 28 B per box pixel and touches every pixel of the box even when the ink covers a few percent. A full-canvas text box (1920x1080): ~58 MB allocated and ~8 MB uploaded per render. Live texts render once per second (`text_live.rs:345-360`), so this is ~1 % of a core per full-canvas clock, not a hot problem, but a scrolling/ticker or many live texts multiply it.
- Proposal: compose in u8/u16 premultiplied, restrict compose and upload to the ink bounding box (plus outline/shadow extents) and give the node the offset; reuse buffers across renders in the worker.
- Benefit (estimate): 5-20x less work and upload for typical lower-thirds/clocks (ink ~5-20 % of the box).
- Effort: M
- Risk: Low (pixel tests exist in the conformance suite).
- Kind: structural; Hot path: no (text worker thread); the upload in `TextSourceNode::process` is hot (smaller writes).

### C-19 Browser supervisor polls instead of waiting on a doorbell
- Location: `crates/rbs-browser-runner/src/lib.rs:263` (`SOUND_POLL` 5 ms), `:297-322` (period = 1/(2 x fps), `park_timeout(period)`).
- Problem: 60 wakeups/s for a 30 fps page and 200/s with page sound, per helper, also when the page is static (CEF paints nothing then).
- Proposal: a cross-process doorbell (eventfd/futex on Linux, a named event on Windows, a Mach semaphore or kqueue user event on macOS) signalled by the helper after `publish_with` and after a sound packet; keep a 500 ms timeout for the heartbeat check.
- Benefit (estimate): 200 -> ~30-100 wakeups/s per page with sound, ~0 for a static page; power on laptops.
- Effort: M
- Risk: Low-Med (untrusted helper can spam the doorbell: rate-limit).
- Kind: structural; Hot path: no (supervisor thread).

### C-20 Virtual camera: NV12 readback then three CPU copies
- Location: `crates/rbs-vcam-mf/src/sender.rs:195` (download -> ring copy), `crates/rbs-vcam-ring/src/lib.rs:537` (ring writes), `crates/rbs-vcam-mf-source/src/win/stream.rs:255-260` (`running.pump.next` returns a picture; to verify whether the pump copies out of the ring) and `:284-345` (`copy_nv12` into the MF sample).
- Problem: 1080p30 NV12 = 3.1 MB x (readback + 2-3 copies) x 30 = ~280-370 MB/s while the virtual camera is on.
- Proposal: (S) copy straight from the ring slot into the locked MF buffer (one copy fewer, if the pump copies). (L, new ADR, ADR-0066/0038 constraints) hand a shared D3D11 NV12 texture to the frame-server source (Windows 11 virtual cameras accept DXGI-backed samples), removing the readback.
- Benefit (estimate): S: -93 MB/s at 1080p30; L: the whole readback path (~370 MB/s and a GPU sync) removed.
- Effort: S / L
- Risk: Low / High
- Kind: quick-win / architectural; Hot path: yes.

### C-21 Camera mode choice without a requested size or rate picks the largest and fastest
- Location: `crates/rbs-capture/src/camera.rs:126-170` (`None => of_format().rev().max_by_key(|m| m.area())`, rate: highest), `registry.rs:1150-1166` (size/rate only from settings). The desktop app defaults to 1280x720@30 (`rbs-app/src/studio_host.rs:112`), so the cost hits API-created and imported sources.
- Problem: a 4K webcam opened without settings delivers 4K30 (or 4K60): 4x the bytes of 1080p into copy, upload and the NV12 expansion of C-08 (66 MB Rgba16Float per frame).
- Proposal: in `instantiate`, default size = the smallest mode that covers the canvas size (or the drawn size), rate = canvas rate; keep explicit settings untouched.
- Benefit (estimate): 4K30 -> 1080p30 = 4x less on every per-frame stage (e.g. C-08: 2 GB/s -> 0.5 GB/s of GPU writes).
- Effort: S
- Risk: Low.
- Kind: quick-win; Hot path: no.

### C-22 Media sound resampling done with an interpolated 64-tap kernel on the worker
- Location: `crates/rbs-nodes/src/media/pcm.rs:85-113`, `crates/rbs-audio/src/resample.rs:12-14,100-123` (`TAPS` 64, `PHASES` 256, coefficients interpolated per output frame).
- Problem: per output frame 64 lerps for the coefficients + 64 MACs per channel. 44.1 -> 48 kHz stereo: ~48 k x 192 = ~9 M flops/s (~2-5 ms CPU per second). Small. Also `input.drain(..)` memmoves the history buffer each push.
- Proposal: ask the decoder for the graph rate where the OS resamples in its own pipeline (AVAssetReader `AVSampleRateKey`, MF audio resampler DSP), or use an exact rational polyphase for 147/160 (no coefficient interpolation, halves the work); use a ring for the history.
- Benefit (estimate): ~1-3 ms CPU per second per playing media source with 44.1 kHz sound.
- Effort: S-M
- Risk: Low (quality must match: the resampler's own measured response).
- Kind: quick-win; Hot path: no (worker), yes for the shared `rbs-audio` kernel if touched.

### C-23 Media file is opened twice at instantiate
- Location: `crates/rbs-sources/src/registry.rs:1290-1310` ("Probe without keeping the decoder (ADR-0108 decision 3)", `probe_media` per candidate), then the worker opens it again on start.
- Problem: startup only: two `AVAssetReader`/MF source reader creations (tens of ms each for a network-share or large MP4 moov). Explicit design.
- Proposal: keep the probed decoder for a short grace (e.g. hand it to the first `start` within 2 s), else drop it.
- Benefit (estimate): ~20-100 ms less time-to-first-picture per media source add/scene build.
- Effort: S-M
- Risk: Low (P13: the decoder must still be gone if the source stays Standby).
- Kind: quick-win; Hot path: no.

---

## Already good
- Capture hand-off: `latest_frame` is a lock-free ring take on SCK, WGC, AVF, MF and PipeWire; replaced OS objects are released on producer/janitor threads, never on the render thread; graveyards keep handle values alive for the grace.
- Zero-copy on macOS: SCK displays/windows (BGRA IOSurface) and AVF cameras (NV12 IOSurface) are imported, not uploaded; the importer caches by surface and reopens only on a size change (`rbs-gpu/src/interop/mod.rs:139-150`).
- SCK and WGC deliver only on content change: an idle desktop costs almost nothing.
- Standby is real: `CaptureFactory::open` holds no device or thread; `CaptureSourceNode::release` stops the session and drops the upload's GPU objects (`rbs-nodes/src/capture.rs:296-305`).
- `CaptureSourceNode::process` with no new frame is one ring check; resize detection is one compare per new frame.
- Still source `process` does nothing after the first publish; the upload happens in `warm`, off the render tick.
- Text node: one atomic load when nothing is new; two reserved textures written in place; the text worker threads exist only while text sources exist and stop after 30 s idle; live texts redraw once per second, at the second boundary.
- Media and browser sources sleep when no shown scene draws them (ADR-0107); browser helper processes exist only while Active and are capped at 2.
- CEF paints only when the page changes; the studio does no per-tick work for a static page beyond one `try_lock`.
- One node per source however many scenes place it (ADR-0065), so the same display in five scenes is one capture session.
- Desktop defaults: display/window `fps` = canvas rate, camera 1280x720@30.
- Media worker frees returned buffers on its own thread; sound conversion allocations stay off the audio thread.

## Open questions / measurements needed (each <= 2 min on a device)
1. M4 Pro: Activity Monitor CPU of `replayd`/WindowServer and RBS GPU % with one 5K (or built-in) display capture at native size vs `setWidth/Height` 1920x1080 (temporary patch) at 60 fps with a moving window. Validates C-05.
2. Windows 4K 144/240 Hz monitor: GPU "Copy" engine and 3D % in Task Manager with a display capture and no `fps` setting vs `fps = 60`. Validates C-02/C-11.
3. Peak `Ring::handed_out` count over 1 minute for WGC and SCK at 60 and 120 Hz (add a debug counter). Decides C-11/C-12 slot counts.
4. `powermetrics --samplers tasks` (macOS) or `perf trace -s` wakeups for RBS with one paused media source and one held GIF. Validates C-01 (expect ~500 wakeups/s per source).
5. Windows: VRAM (Task Manager "Dedicated GPU memory") with one 4K60 H.264/HEVC media file playing, before/after a 50 ms picture lead. Validates C-09/C-10.
6. Time of `bgra_premultiplied_to_rgba` on a 1920x1080 transparent page (`cargo bench` style micro-test on the device). Validates C-03.
7. Render-thread time of `BrowserSourceNode::process` with a 4K30 page (tick trace). Validates C-14.
8. Does `GraphicsCaptureSession::SetMinUpdateInterval` succeed on the oldest supported Windows build? (log the HRESULT once). C-02.
9. A YUY2-only USB webcam on Windows: does `open` fail with FormatUnsupported (UPLOAD_FORMATS lacks YUY2)? C-15.
10. Linux AMD/Intel: run the capture conformance with `import_preferences` returning `[DmaBuf, System]`. C-07.
