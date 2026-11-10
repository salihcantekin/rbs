# Hot paths

Code on these paths runs per frame, per audio block or per packet. Code edited inside these functions and modules, or called directly from them, follows principle [P15](principles.md#p15-hot-paths-get-extra-scrutiny): measurements, the `hot-path-review` checklist and human approval.

For `Node::process`, this covers the graph's call into nodes and the built-in nodes shipped with RBS. A third-party plugin's own `process` is its author's responsibility; the host enforces its time budget.

Keep this list current. When a new crate or module lands on one of these paths, add it here in the same PR; `.github/CODEOWNERS` mirrors the paths so reviews are requested automatically. CODEOWNERS works on whole files and directories, so it is coarser than this list; that is accepted.

| Path | Runs | Location (current or planned) |
| --- | --- | --- |
| Audio thread: device callbacks, audio graph, audio filters (ducker key link: [ADR-0095](adr/0095-audio-ducker-and-key-tap.md)) | Every block (10 ms) | `crates/rbs-audio` (`input.rs`, `resample.rs`, `mixer.rs`, `meter.rs`, `sink.rs`; `encoder.rs` on the encoder worker), `crates/rbs-audio-filters` (DSPs and the filter node), audio adapters in `crates/rbs-audio-io-*` |
| Audio input hand-off: `AudioInputStream::read`, the sample ring (`RingProducer` pushes in OS callbacks, `RingConsumer::read`) and `EdgeClock` | Every OS packet and every block per Active audio input | `crates/rbs-audio-io/src/ring.rs`, `crates/rbs-audio-io/src/edge.rs`, adapters in `crates/rbs-audio-io-*` |
| Render loop: compositor, GPU filters, transitions | Every frame per canvas | `crates/rbs-graph/src/runtime` (executor tick and swap), `crates/rbs-gpu`, `crates/rbs-filters` (`FilterNode::process` and the pass recording it calls; `FilterType::uniforms` of every built-in type) |
| `Node::process` of every node type | Every frame or block | Wherever nodes are implemented |
| Parameter reads by running nodes: `ParamReceiver::update` | Every frame or block per node with parameters | `crates/rbs-graph/src/runtime/param.rs` |
| Queue endpoints between partitions | Every frame, block or packet per crossing link | `crates/rbs-graph/src/runtime/queue.rs` (video, packet and audio queues) |
| Audio values on links: audio slots, `AudioOut`, `Inputs::audio` | Every block per audio pin | `crates/rbs-graph/src/runtime/schedule.rs` |
| Engine worker loop: pacing, doorbell wait, the call into `Executor::run_tick` and `swap_pending` | Every tick per partition | `crates/rbs-engine/src/worker.rs` |
| Capture frame hand-off: `CaptureSession::latest_frame` and the adapter code it calls | Every frame per Active capture source | `crates/rbs-capture`, adapters in `crates/rbs-capture-*` |
| Frame, texture and audio buffer pools | Every frame or block | `crates/rbs-gpu`, `crates/rbs-audio` |
| Colour conversion to encoder formats (NV12, P010), the scaled conversion for the virtual camera (`YuvConverter::scaled`, `YuvDownload::scaled`, ADR-0066), and YUV source decode | Every frame per encoder, virtual camera frame or YUV source | `crates/rbs-gpu` (`yuv.rs`, `shaders/yuv.wgsl`, `download.rs`) |
| Capture, export, encoder, output, replay ring (`replay.rs`, `replay_node.rs`, [ADR-0088](adr/0088-replay-buffer-and-retroactive-recording.md)) and virtual camera nodes: `process` of the built-in port nodes, the virtual camera session's `reserve`/`send` and its sender's doorbell (`rbs-vcam` `Handoff`, ADR-0066), system-memory upload of source frames and the non-blocking NV12 download for encoders (ADR-0038) | Every frame or packet per Active source, encoder and output | `crates/rbs-nodes`, `crates/rbs-gpu/src/upload.rs`, `crates/rbs-gpu/src/download.rs`, `crates/rbs-vcam/src/handoff.rs`, `crates/rbs-vcam/src/mailbox.rs` |
| Preview export to the UI process: `PreviewPublisher::publish` and `tick` (one non-blocking Mach send per frame, ADR-0028; one overlapped pipe write on Windows, ADR-0036), called from the preview node's `process` (ADR-0041) | Every frame per subscribed preview | `crates/rbs-gpu/src/share` (publisher, mach; `win/publisher.rs`, `win/pipe.rs`; the UI-side `layer.rs` files are not on the engine's hot path), `crates/rbs-nodes/src/preview.rs` |
| Encoder submit/drain and packet fan-out | Every frame per encoder | `crates/rbs-codec*`, `crates/rbs-graph` |
| Muxers and transport send loops | Every packet | `crates/rbs-mux*`, `crates/rbs-transport*` |
| Bitstream conversion (Annex B / length-prefixed, FLV tag bodies) | Every packet per converting output | `crates/rbs-media-format` |
| Clock reads, timestamp mapping, rates, cadences, timebases, drift estimation | Everywhere above | `crates/rbs-core/src/time.rs`, `crates/rbs-core/src/timing.rs` |
| Media value types: frame and block construction, lease retain/release | Every frame, block or packet | `crates/rbs-core/src/media/` |
| Plugin call boundaries (native vtable, WASM, IPC) | Every frame or block for active plugins | `crates/rbs-plugin-host` |

Locks on these paths: P6 rules out waiting on a lock. The only accepted exception is a `try_lock` that never waits and falls back to the previous value, with the reasoning written in an ADR (so far only the parameter exchange, [ADR-0023](../adr/0023-node-parameter-channels.md)). Calling `lock()` on a hot path is never allowed.

Not hot: start-up, probing, configuration, the control API, UI, scene editing (these must still not block hot paths).

## Human sign-offs (P15)

| Date | Maintainer | Changes signed off |
| --- | --- | --- |
| 2026-11-03 | Salih | Worker thread setup ([ADR-0030](../adr/0030-worker-thread-setup.md)); hardware encoder session (`rbs-codec-hw`, [ADR-0031](../adr/0031-adapter-support-crates.md), PR #56); audio input port and adapters ([ADR-0033](../adr/0033-audio-input-port.md): PipeWire #71, WASAPI #72, CoreAudio #74); Windows preview publisher ([ADR-0036](../adr/0036-windows-preview-layer.md), #75); Media Foundation AAC encoder (#79); capture, export, encoder and output nodes with the GPU-to-CPU readback exception ([ADR-0038](../adr/0038-io-graph-nodes.md), #81); the PipeWire portal's 60 s stream-hold exception (#62). The audio graph (ADR-0037, #82) was signed off earlier. |
| 2026-11-03 | Salih | Fragmented MP4 media time origin at the earliest sample, no edit list (`rbs-mux-fmp4`, #90). |
| 2026-11-03 | Salih | Preview node and meter sink, and the once-per-start base store in the engine worker ([ADR-0041](../adr/0041-studio-recording-on-a-running-engine.md), #91); the SC-PC numbers follow with the desktop app's hardware run. |
| 2026-11-04 | Salih | Marked final ticks and the video encoder's final-tick flush ([ADR-0043](../adr/0043-end-of-stream-on-final-ticks.md)), Matroska duration and size patches, and the output node's aligned start (#100); SC-PC numbers in the PR. |
| 2026-11-04 | Salih | FFmpeg AAC encoder (`rbs-codec-ffmpeg`, #119): 645 us per 10 ms stereo block with libavcodec's `twoloop` coder in the cloud container (output ports spec). |
| 2026-11-04 | Salih | Output node connection counters; RTMP and SRT `send`: hand-off only while linked, newest media time on its own cache line, stale committed media discarded at connection start; RTMP staging drops old audio before cutting the current GOP (#124). `send` benchmarks within ±7 % machine noise of main (PR). |
| 2026-11-04 | Salih | Recording tail on stop ([ADR-0045](../adr/0045-recording-tail-on-stop.md), #103): the export's end time and oldest-first readbacks while ending, the export's and video encoder's `last_frame` stores, the audio reader's doorbell on final ticks and the audio encoder node's flush on its first empty final tick. SC-PC: exported equals encoded frames in every CLI (NVENC/AMF, MKV/MP4) and app recording, audio ends 13-24 ms before video, stop to file closed about 160 ms. |
| 2026-11-06 | Salih | Styled layers in the compositor ([ADR-0053](../adr/0053-item-style-and-video-filter-chains.md), #141): a second pipeline and style array used only for layers with a shape or border, the style record fill and pipeline switches in `Compositor::render`, and the `fs_styled` shader. Unstyled layers keep the old shader and record; container numbers in the PR, Mac numbers in a PR comment. |
| 2026-11-07 | Salih | Crop editing ([ADR-0058](../adr/0058-crop-editing-of-sources.md), #159): `Compositor::render` skips layers whose crop leaves no picture (one comparison per layer, counted in `RenderStats::empty_crop_layers`); `CaptureSourceNode::process` compares each new frame's size with the last one and stores `CaptureNodeStats::picture_size` only on a resize. Compositor `record_submit` benchmarks within noise of main (PR). |
| 2026-11-10 | Salih | Replay ring node and packet ring ([ADR-0088](../adr/0088-replay-buffer-and-retroactive-recording.md), #271): `PacketRing::push`, `ReplayRingNode::process`, the lock-free save request and the reaper hand-off. Cloud numbers in the PR (push 55 ns per packet, cut of 7500 packets 155 us, 0 allocations per steady tick); output-partition tick time and encoder latency with other outputs follow with the Studio wiring. |
