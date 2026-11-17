# RBS performance audit: audio

Scope: rbs-audio, rbs-audio-filters, rbs-denoise, rbs-audio-io (+ adapters), browser and media sound, meters to the UI, audio encode hand-off.
Method: code reading only (no build, no bench). Every number below is an **estimate** unless it quotes a measured figure from the repo docs (`docs/specs/audio-graph.md` line 297, `docs/architecture/hot-paths.md`, `docs/specs/performance-budgets.md`). Measured reference figures used: input node 27 µs/block (48 kHz mono), output node 32 µs (48 to 48 kHz), mixer of 16 stereo inputs 13 µs, loudness meter 19.7 µs with "the 4x true peak about three quarters of it".

Block = 10 ms = 480 frames at 48 kHz. Stereo block = 960 samples.

Findings are ordered by benefit/effort.

---

### A-01 Mixer gain/meter loops are serial reductions (not vectorised)
- Location: `crates/rbs-audio/src/mixer.rs:506-540` (strip loops: `peak = peak.max(y.abs()); sum_sq += y * y; *o += y;`) and `:553-560` (master loop, same pattern).
- Problem: `sum_sq += y * y` is an in-order f32 reduction; without reassociation LLVM cannot vectorise it, so every loop runs one sample per add latency (3-4 cycles). 960 samples × ~4 cycles ≈ 3,800 cycles ≈ 0.8 µs per stereo strip. That matches the measured 13 µs for 16 stereo inputs (0.8 µs each). The `f32::max` reduction has the same problem. Runs every block for every strip and once for the master, also for strips without a meter.
- Proposal: accumulate into fixed arrays of 8 partial sums/peaks (`chunks_exact(8)` with `[f32; 8]` accumulators, combined after the loop), the same shape `resample::apply` already uses. Alternatively take the peak as integer max on `to_bits()` of `abs()` as the compressor does (`compressor.rs`, "Non-negative floats order like their bits"). Split the loop when the strip has no meter (no reduction at all). Note: the sum order changes, so meter RMS differs in the last bits (no audible effect; tests comparing exact RMS may need a tolerance).
- Benefit: estimate 4-6× on these loops: 13 µs → about 2.5-3 µs per block for 16 stereo strips (≈ 1 % of one core saved at 100 blocks/s per 16 strips; 8 strips ≈ 0.5 %). Reasoning: SSE/NEON 4-wide with 2 accumulators removes the latency chain.
- Effort: S
- Risk: Low (pure arithmetic; bit-exact output samples, only meter sums reorder).
- Kind: quick-win ; Hot path: yes

### A-02 True peak: exact skip bound and phase-vectorised layout
- Location: `crates/rbs-audio/src/loudness/true_peak.rs:82-104` (`for phase in self.coeffs.chunks_exact(TAPS) { let y: f32 = phase.iter().zip(window).map(|(c, s)| c * s).sum(); ...}`), called from `loudness/mod.rs` `LoudnessMeter::process`.
- Problem: per input sample, 4 phases × 12 taps = 48 MACs, each phase an in-order 12-element `.sum()` (serial) plus a max. 480 × 2 ch × 48 = 46k scalar MACs per block. Docs: about 15 µs of the measured 19.7 µs. Runs every block while loudness is on (Mixer loudness bar visible = typical).
- Proposal (two independent parts):
  1. Exact skip: the FIR output is bounded by `max|x| × L1(phase)`. Keep `bound = max over phases of Σ|c|` (computed in `new`). `self.peak` is the maximum since reset (session max). If `block_sample_peak × bound <= self.peak`, the block cannot raise the true peak: skip the oversampling and only copy the last `TAPS` samples into the history. Exact, no approximation. After a reset, the first loud block computes; then nearly all blocks skip.
  2. Store coefficients transposed (`[TAPS][factor]`) so each tap is one 4-wide multiply-add for all 4 phases (12 vector FMAs per sample instead of 48 scalar + 4 serial sums).
- Benefit: estimate (2) alone 15 µs → ~3-4 µs; with (1) typical blocks ~0.5 µs (sample peak scan + 12-sample copy). Loudness meter 19.7 µs → ~5 µs worst, ~3 µs typical.
- Effort: S (1) + S (2)
- Risk: Low (1 is provably exact; 2 changes summation order, within 1e-6).
- Kind: quick-win ; Hot path: yes

### A-03 Resampler runs a 64-tap sinc even when device and graph rates match
- Location: `crates/rbs-audio/src/input.rs:255` (`kernel: Kernel::new(src.sample_rate, config.format.sample_rate)` unconditionally) and render loop `:544-570`; `crates/rbs-audio/src/output.rs:189` and `:414-444`; also every browser source via `InputAligner` (`crates/rbs-sources/src/browser_sound.rs:683`). Contrast: `crates/rbs-nodes/src/media/pcm.rs:24` already bypasses equal rates.
- Problem: per output frame: 64 lerps in `Kernel::coefficients` + 64 MACs per source channel, even for 48→48 kHz (the common case for USB mics, the monitor device, CEF). Measured: input node 27 µs (mono), output node 32 µs (stereo) per block; nearly all of it is the resampler. The drift steering keeps the step at `1 ± ppm`, so the fractional position never stays at 0 and the full kernel is always needed.
- Proposal: a matched-rate mode with hysteresis. When `src == dst`: while the steering error is within a dead band (e.g. ±0.25 ms), run with step exactly 1 and an integer position (a plain copy plus channel map). When the error leaves the band (device drift of 20-50 ppm reaches 0.25 ms after 5-12 s), steer with the existing sinc until the error is back near 0 and the fractional part crosses 0, then return to copy mode (switch only at frac ≈ 0 so there is no step). Keep the full kernel for unequal rates. The spec (`docs/specs/audio-graph.md`, "Resampler"/"Input node") must describe the mode.
- Benefit: estimate input node 27 µs → ~3 µs most of the time, output node 32 µs → ~3 µs. With 6 mics + monitor at 48 kHz: ~170 µs/block → ~25 µs (≈ 1.5 % of a core). Also lower power (less work per wake-up).
- Effort: M
- Risk: Med (audio quality at the mode switches; needs listening test and a test that frac=0 at switch; steering stability).
- Kind: structural ; Hot path: yes

### A-04 No flush-to-zero / denormals-are-zero on audio threads
- Location: no `FTZ/DAZ/MXCSR/FPCR` anywhere (`grep -rn -i "ftz\|denormal\|mxcsr\|fpcr" crates` finds only per-sample guards). Thread setup: `crates/rbs-thread-os/src/*.rs`, engine worker `crates/rbs-engine/src/worker.rs`.
- Problem: IIR state can decay into subnormals after digital silence (loopback, media, browser, muted device): EQ section state `z` is not flushed (`equaliser.rs:460` `run_lanes` cleans only the samples, not `st`), RNNoise's high-pass memory and the GRU accumulate, the ducker/gate envelopes are guarded but other code is not. On x86 (Windows RTX 4090 box), each subnormal operand costs ~100+ cycles (microcode assist); a 5-section stereo EQ is 480 × 2 × 15 ops → can go from ~3 µs to 100+ µs per block while silent. Apple M-series handle subnormals in hardware, so the gain there is small.
- Proposal: set MXCSR FTZ|DAZ (bits 15 and 6) on x86 and FPCR.FZ (bit 24) on aarch64 at the start of the audio partition's worker thread (and the WASAPI/CoreAudio callback threads that convert samples), in `rbs-thread-os` (a marked low-level module, `// SAFETY:` comment). Keep `clean()` for NaN/inf. Add a bench case "EQ, tone then 2 s digital silence" to prove it.
- Benefit: estimate removes worst-case 10-100× slowdowns of IIR filters in silence on x86; 0 cost otherwise. Also lets the per-sample denormal guards be simplified later.
- Effort: S
- Risk: Low-Med (thread-wide FP mode; only audio code runs on those threads; changes results only below 1.2e-38).
- Kind: quick-win ; Hot path: yes

### A-05 Each input/output node builds its own 64 KB kernel table
- Location: `crates/rbs-audio/src/resample.rs:63-95` (`table = vec![0.0f32; (PHASES + 1) * TAPS]` = 257 × 64 × 4 B = 65,792 B), created per `State::new` in `input.rs:255`, `output.rs:189`, and per browser source aligner.
- Problem: 8 sources at the same rate pair hold 8 identical tables (≈ 0.5 MB) and each one competes for L1/L2 during the block (the 4090 box's 48 KB L1D cannot hold even one; the M4's 128 KB holds two). The table is also rebuilt (257 × 64 Bessel evaluations in f64) on every `prepare`.
- Proposal: a process-wide cache `Mutex<Vec<((src, dst), Weak<Kernel>)>>` on the control path (`Kernel::shared(src, dst) -> Arc<Kernel>`), used by `State::new` (control thread, allocation allowed).
- Benefit: estimate −64 KB per extra node at the same rate pair (−450 KB for 8 sources), fewer L2 misses on the audio thread (to measure), faster `prepare`.
- Effort: S
- Risk: Low.
- Kind: quick-win ; Hot path: no (control path; the table read in the hot path is unchanged)

### A-06 Noise suppression output ring uses `%` per sample
- Location: `crates/rbs-audio-filters/src/noise.rs:118` (`let at = (self.head + self.len) % self.buf.len();`), `:129` (`self.head = (self.head + 1) % self.buf.len();`), `:274-278` (gate scaling).
- Problem: a 64-bit integer division per pushed and per popped sample per channel: 480 × 2 ch × 2 ≈ 1,900 divisions per block; the `pop` chain is serial (`head` depends on the previous division). x86 64-bit `div` ≈ 15-40 cycles latency, M4 ≈ 7-9.
- Proposal: copy whole spans (`copy_from_slice` of up to two slices around the wrap), or `if at >= len { at -= len }`. Pop directly into the channel slice in one or two `copy_from_slice` calls.
- Benefit: estimate 1-3 µs per block per noise filter on x86, ~0.5-1 µs on M4.
- Effort: S
- Risk: Low.
- Kind: quick-win ; Hot path: yes

### A-07 RNNoise runs the pitch search and a second FFT before deciding the frame is silent
- Location: `crates/rbs-denoise/src/features.rs:127-183`: `transform_input(...)` (band energy `ex`), then `find_pitch()` (`:142`, pitch downsample + search + remove_doubling), a second `transform_input` at the pitch lag, band correlation and a DCT, and only then `if e < 0.04 { ... return true; }` (`:178`). `lib.rs:267-268` then skips only the RNN on silence; `frame_synthesis` (inverse FFT) still runs.
- Problem: on quiet frames (between words, a muted talent, room tone below the threshold) the most expensive non-RNN parts (pitch search, 2nd FFT) are computed and thrown away. Speech has roughly 30-50 % pauses.
- Proposal: compute `e = Σ ex` right after the first `transform_input` and return early when `e < 0.04` (features zeroed as today); keep the pitch buffer shift (already in `shift_and_filter_input`). Second step: an exact digital-zero fast path in `Denoiser::process_frame` (input all 0 and synthesis memory all 0 → output 0, skip both FFTs). Record the patch in `crates/rbs-denoise/README.md` (vendored-patch list). Pitch state (`last_period`) is not advanced on skipped frames; upstream advances it, so outputs differ slightly at the first voiced frame after silence (to verify by listening/A-B).
- Benefit: estimate 40-60 % less denoise time on silent frames; for a typical voice mic 15-30 % less in total. Digital zero: ~95 % less.
- Effort: M (vendored code, A/B comparison against upstream needed)
- Risk: Med (behaviour change vs upstream at speech onsets).
- Kind: structural ; Hot path: yes

### A-08 Mono microphones are processed as stereo through the whole filter chain
- Location: `crates/rbs-audio/src/input.rs:155-162` (mono feeds L and R), graph format fixed per partition (`config.rs`, default stereo 48 kHz), filters run on `format.channels` (`crates/rbs-audio-filters/src/node.rs:147`). Noise suppression in `mono` mode then averages the two identical channels back to one (`noise.rs` `process`, `sum / n`), and writes the same output to both rings.
- Problem: most voice sources are mono devices. Every filter (gate, compressor, EQ, limiter, ducker, noise suppression ring + scaling) and every copy between nodes does 2× the work and memory traffic of the source's real channel count. In `separate` noise mode a mono mic runs RNNoise twice on identical data.
- Proposal: let an input node's chain run at the source's own channel count (mono links within the source chain), and upmix once at the mixer strip (the mixer already maps per channel). Needs an audio-graph spec change (links of a source chain may carry a narrower layout) and an ADR (contract change to "every audio link carries the graph format"). Smaller step without the ADR: `NoiseDsp` detects identical channels (or the source layout passed in `DspSetup`) and runs one state; filters get a `DspSetup` with the source's layout.
- Benefit: estimate ~50 % of filter-chain CPU for mono sources (e.g. EQ+compressor+limiter+gate ≈ 15 µs → 7.5 µs per mic per block), and 50 % of noise suppression in `separate` mode (one RNNoise frame ≈ tens to 250 µs budget).
- Effort: L (spec + ADR + graph/format plumbing) / S for the NoiseDsp-only step
- Risk: Med (contract change; channel-count mismatch handling on every node)
- Kind: architectural ; Hot path: yes

### A-09 Muted strips still run the full mix loop
- Location: `crates/rbs-audio/src/mixer.rs:481-544`.
- Problem: a muted strip at steady gain 0 (`g0 == g1 == 0`, `plain`) multiplies every sample by 0, adds it to the output and computes peak/RMS of zeros. ≈ 0.8 µs per muted stereo strip per block (see A-01).
- Proposal: if `plain && g0 == 0.0 && g1 == 0.0`: skip the loop and record a zero block on the meter (or nothing; post-fader meter of a muted strip is silent).
- Benefit: estimate 0.8 µs per muted strip per block (0.2 µs after A-01).
- Effort: S
- Risk: Low.
- Kind: quick-win ; Hot path: yes

### A-10 UI meter refresh rewrites every row 30 times a second, even in silence
- Location: `crates/rbs-app/src/studio_ui.rs:6935-6984` (`tick_meters`), timer `METER_PERIOD = 33 ms` (`:66`), started in `main.rs:2301-2311` while the Mixer shows meters.
- Problem: per tick, for each source slot it linearly scans all rows (`for i in 0..count { row_data(i) ... }`, O(slots × rows) with a struct clone each) and calls `set_row_data` unconditionally, which marks the model dirty and makes Slint re-render the meter rows 30 times a second even when every source is silent and the ballistics have settled. On a laptop with the window open and nothing playing, this is a steady 30 Hz redraw.
- Proposal: keep a `slot → row index` map rebuilt when rows change; build the new row, compare with the old (`MeterRow` is a generated Slint struct with `PartialEq`, to verify) and only `set_row_data` when it differs; stop the timer once every meter has settled at the floor and restart it when any `LevelMeter` records a non-zero peak (cheap check: `take()` peaks > 0). No visual change (rule 16 does not apply; `ui-review` still applies as UI code).
- Benefit: estimate idle redraws 30/s → 0/s with the Mixer visible and silent sources; less CPU/GPU in the UI process (likely 1-3 % of a core of UI/render work on a busy Mixer, to measure).
- Effort: S
- Risk: Low.
- Kind: quick-win ; Hot path: no

### A-11 The meter sink keeps the whole audio graph running with no consumer
- Location: `crates/rbs-record/src/studio.rs:1601-1628` (mixer + `MeterSinkNode` marked essential whenever `settings.audio`), `crates/rbs-audio/src/sink.rs`. App side: `main.rs:2731-2733` stops only the UI meter timer when the app is hidden.
- Problem: with no recording, stream, monitor, loudness or API subscriber, and the window hidden or the Mixer closed, the audio partition still ticks 100 times a second and runs every input node, filter chain and noise suppression; every device stays open (macOS microphone indicator stays on). With no audio sources at all, the partition still wakes 100 times/s to mix silence. (To verify on device that nothing else keeps it alive.)
- Proposal: make the meter sink a demand: add it only while something reads meters (Mixer visible, `audio.levels` subscribed, loudness shown); otherwise the plan drops it and the mixer, inputs and filters fall out of the schedule (Standby, P13). Needs an ADR-0041 follow-up: device reopen latency when the Mixer is shown again, and the sync/offset state of inputs. Cheaper step: no meter sink while the mixer has no strips.
- Benefit: estimate hidden, idle app: 100 audio wake-ups/s → 0, plus all per-source DSP (e.g. 6 sources × ~30-300 µs per block = 0.2-3 % of a core) and device callbacks (another ~100/s per device). Matters most on battery (M4 Pro laptop).
- Effort: M
- Risk: Med (meter latency on show; device reopen; ADR needed).
- Kind: architectural ; Hot path: no (schedule change)

### A-12 The real-time audio thread rings an encoder doorbell (unpark syscall) every block
- Location: `crates/rbs-graph/src/runtime/queue.rs:659` (audio writer: `self.doorbell.ring()` after every `try_send`), `:147-166` (`ring` = swap + `fence(SeqCst)` + `try_lock` + `thread.unpark()`); encoder partitions are `Pacing::doorbell` (`crates/rbs-record/src/studio.rs:1551-1560`).
- Problem: per block per audio encoder chain, the audio thread may do a futex wake syscall (≈1-5 µs, and a possible context switch on the same core), 100 times a second. An AAC frame is 1024 frames, so 2 of every ~2.1 wake-ups find no full frame to encode. The reader also rings itself while more blocks are queued.
- Proposal: ring only when the queued frames reach the encoder's frame size (AAC 1024, Opus 960; pass `frame_frames` to the queue), on a final tick, or after a bound (e.g. 3 blocks). The doorbell's timer (100 ms) stays as a safety net. Alternatively pace audio encoder partitions with the audio cadence offset by half a block and drop the doorbell.
- Benefit: estimate ~55 % fewer encoder wake-ups (100/s → ~47/s per chain) and one syscall less on the RT thread for most blocks.
- Effort: M
- Risk: Low-Med (latency of the last packet; ADR-0045 drain rules must hold).
- Kind: structural ; Hot path: yes

### A-13 Replay buffer runs its own AAC encoder next to an identical recording/stream encoder
- Location: `crates/rbs-record/src/studio/replay.rs:4-9` ("This first version opens its own encoder sessions; sharing ... (ADR-0088 decision 2) follows"), `:123-129`.
- Problem: with replay armed while streaming or recording at the same audio settings, the mix is copied, queued and AAC-encoded twice (and video twice: out of this audit's scope, flag for the video auditor).
- Proposal: implement ADR-0088 decision 2 for audio: `AudioPlan::Shared` already exists for outputs (`studio.rs:4074-4081`); let the replay ring node connect to an existing audio chain's `packets` pin when settings match.
- Benefit: estimate one AAC-LC 48 kHz stereo encoder ≈ 0.1-0.3 % of a core (AudioToolbox/MF), 100 wake-ups/s, one queue copy per block. Small for audio; the video share is the big win.
- Effort: M
- Risk: Low-Med (pause/cut gates on shared chains, ADR-0124).
- Kind: structural ; Hot path: no

### A-14 x86 builds use SSE2 only for the audio kernels
- Location: `Cargo.toml:34-37` (`[profile.release]` no `target-cpu`), no `.cargo/config.toml` rustflags; kernels: `resample.rs:100-123`, `true_peak.rs:82`, mixer loops.
- Problem: on x86-64 the baseline is SSE2 (4-wide, no FMA). The resampler, true peak, mixer and EQ lanes could use AVX2+FMA (8-wide, fused) on every supported Windows CPU. aarch64 (M4) already has NEON+FMA by default.
- Proposal: runtime dispatch for the 3-4 hottest kernels (`#[target_feature(enable = "avx2,fma")]` copies selected once at `prepare`, or the `multiversion` crate; adapter/low-level module rules for the `unsafe` call), or ship an x86-64-v3 Windows build if the minimum hardware allows (owner decision).
- Benefit: estimate 1.3-2× on those kernels on Windows (e.g. a 44.1 kHz input's resampler 35 µs → ~20 µs).
- Effort: M
- Risk: Low-Med (`unsafe` for the dispatch; results differ in rounding with FMA).
- Kind: structural ; Hot path: yes

### A-15 Input FIFO is compacted with a memmove every block
- Location: `crates/rbs-audio/src/input.rs:273-282` (`drop_front` → `shift` → `copy_within` per channel) called from `render` `:574-586`; same in `output.rs:205-216`.
- Problem: after each block the remaining FIFO content moves to index 0. The FIFO holds roughly the latency plus the sync delay: 50 ms (2,400 frames) up to 250 ms (12,000 frames) per channel → 10-48 KB memmove per channel per block per source.
- Proposal: keep a start index and compact only when the free tail is smaller than the next read (amortised: one move every N blocks), or a ring with `TAPS` frames mirrored past the end so the kernel window is always contiguous.
- Benefit: estimate 0.5-3 µs per source per block (larger with sync delays).
- Effort: S-M
- Risk: Low.
- Kind: quick-win ; Hot path: yes

### A-16 LevelMeter does six atomic read-modify-writes per channel per strip
- Location: `crates/rbs-audio/src/meter.rs:55-99` (`fetch_max` × 2, CAS loop on f64 bits × 2, `fetch_add` × 2 per `record_block`).
- Problem: 16 stereo strips + master ≈ 34 × 6 ≈ 200 locked RMWs per block (x86 `lock` ≈ 20 cycles each; CAS loops can retry against the UI's `swap`). ≈ 4k cycles ≈ 1 µs per block.
- Proposal: single-writer design: the audio thread keeps cumulative values in plain relaxed stores (cumulative energy, cumulative count, a peak with an epoch), and each reader keeps its own previous snapshot and subtracts (UI and API banks need no second write). Peak since last read: writer stores `max(peak, current_epoch_peak)`, reader bumps an epoch the writer sees at the next block.
- Benefit: estimate ~1 µs per block with 16 strips (x86), less on M4 (LSE atomics).
- Effort: M
- Risk: Low-Med (lock-free protocol; a missed clip between reads must stay impossible).
- Kind: structural ; Hot path: yes

### A-17 K-weighting filters one channel at a time (latency-bound f64 recursion)
- Location: `crates/rbs-audio/src/loudness/filter.rs:63-84` (`sum_squares`), called per channel from `loudness/mod.rs` `process`.
- Problem: two cascaded f64 biquads per sample; the recursion is latency-bound (~8-10 cycles per sample). 960 samples ≈ 9k cycles ≈ 2 µs per block, half the time the core waits.
- Proposal: process L and R in lanes like `equaliser.rs:460 run_lanes::<2>` (independent recursions overlap). Keep f64 (needed for the 38 Hz high-pass precision).
- Benefit: estimate ~1 µs per block (2 µs → 1 µs).
- Effort: S
- Risk: Low.
- Kind: quick-win ; Hot path: yes

### A-18 Gate and ducker detector loops are per-frame with per-channel indexing
- Location: `crates/rbs-audio-filters/src/gate.rs:163-210` (`for f in 0..frames { for ch in channels.iter() { level = level.max(ch[f].abs()); } ...`), ducker `ducker.rs:161+` (same structure).
- Problem: bounds-checked gather per channel per frame, NaN check and envelope in one loop, so nothing vectorises. The compressor already splits detector, gain computer and smoothing into vectorisable passes.
- Proposal: same structure as the compressor: pass 1 `w[f] = max_c |x|` (vectorised, integer max on bits), pass 2 scalar envelope/state machine on `w`, pass 3 gains; fast path when the gate is open and fully settled (gain 1, `hold_left` refreshed) or closed at `range` (constant gain): apply a constant.
- Benefit: estimate gate 2-4 µs → ~1 µs per stereo block.
- Effort: S-M
- Risk: Low (bit-exact possible).
- Kind: quick-win ; Hot path: yes

### A-19 Each audio filter copies its input block before processing
- Location: `crates/rbs-audio-filters/src/node.rs:176` (`out.write(&block)?;`), `crates/rbs-graph/src/runtime/schedule.rs:547-578`.
- Problem: one 3.8 KB copy per filter per block (≈ 0.1-0.2 µs each), plus each node's output slot touched in cache. A 5-filter chain = 5 copies.
- Proposal: in-place chains: when a producer's output has exactly one consumer in the same partition, let the consumer take the slot mutably (slot hand-over in the schedule), so filters process in place. Graph runtime change (schedule.rs), spec note in audio-graph.
- Benefit: estimate 0.5-1 µs per 5-filter source per block; fewer cache lines.
- Effort: M
- Risk: Med (schedule ownership rules; meters/taps reading the pre-filter block must keep their own copy).
- Kind: structural ; Hot path: yes

### A-20 Audio slots are sized for 8 channels regardless of the format
- Location: `crates/rbs-graph/src/runtime/schedule.rs:40-50` (`vec![0.0; capacity * MAX_CHANNELS]`, `MAX_CHANNELS = 8`).
- Problem: each audio output pin holds 480 × 8 × 4 = 15 KB; stereo uses 3.75 KB. With ~4 audio pins per source and 20 sources plus encoders: ≈ 0.9 MB of never-touched memory (it is zeroed at build).
- Proposal: size slots by the partition's channel count (known when `set_audio_block_frames` is called; add `set_audio_channels`).
- Benefit: estimate −11 KB per audio pin (−0.5 to −1 MB typical).
- Effort: S
- Risk: Low (`AudioOut::commit` already rejects more channels than fit; must keep that error path).
- Kind: quick-win ; Hot path: no

### A-21 Browser sound: allocation per packet, a mutex, four copies, a 5 ms poll
- Location: `crates/rbs-sources/src/browser_sound.rs:476-505` (`pending.packets.push_back((stamp, samples[..].to_vec()))` under `aligned.pending.lock()`), `:582-635` (planar render, then interleave into the block), `crates/rbs-browser-runner/src/lib.rs:298-316` (supervisor loop `park_timeout(min(period, SOUND_POLL = 5 ms))`).
- Problem: per CEF packet a heap allocation and a lock (supervisor thread, not RT, so allowed but wasteful); samples go shm → Vec → aligner FIFO → planar scratch → interleaved block → graph slot (deinterleaved again in the node). The supervisor wakes 200 times/s per browser source with sound.
- Proposal: a fixed pool of packet buffers (recycled like the media return ring); hand planar blocks to the node (or interleave once and let the node copy planar); wake on the helper's sound doorbell or poll at 10 ms (the graph block period) instead of 5 ms.
- Benefit: estimate −100 allocations/s and −100 wake-ups/s per browser source with sound; −2 copies per block.
- Effort: S-M
- Risk: Low (ARRIVAL margin of 20 ms covers a 10 ms poll; verify).
- Kind: quick-win ; Hot path: yes (browser sound hand-off is listed in hot-paths.md)

### A-22 Noise suppression in `mono` mode still builds one RNNoise state per channel; FFT plans not shared
- Location: `crates/rbs-audio-filters/src/noise.rs` `NoiseDsp::new` (`for _ in 0..channels { Denoiser::new() }`), `crates/rbs-denoise/src/lib.rs:151-175` (`Fft::new` makes a new `RealFftPlanner` and plans per denoiser).
- Problem: the second state (features, pitch buffers, GRU state, FFT plans and scratch; estimate 30-40 KB) is unused in the default `mono` mode. Each denoiser plans its own 960-point FFT (twiddles duplicated).
- Proposal: put the forward/inverse plans (`Arc<dyn RealToComplex>` are `Sync`) in `Shared` beside the model; keep only scratch per denoiser. The unused state is needed for an instant mode switch without allocation on the audio thread, so keep it, or build it on the control thread when `separate` is set (filter re-make).
- Benefit: estimate −10-20 KB per denoiser (shared plans), −30-40 KB per mono noise filter if the second state is dropped.
- Effort: S
- Risk: Low.
- Kind: quick-win ; Hot path: no

### A-23 Sample ring stores each sample as a separate atomic, interleaved, then deinterleaves
- Location: `crates/rbs-audio-io/src/ring.rs` `push_with` (per-sample `AtomicU32::store` through a closure, wrap test per frame) and `RingConsumer::read` (per-sample load into planar).
- Problem: atomics block vectorisation; a planar device (CoreAudio non-interleaved, PipeWire planar) is interleaved on push and deinterleaved on read. ≈ 2 scalar passes over 960 samples per block per stereo device.
- Proposal: keep the safe atomic design but store planar per channel (contiguous runs → simple loops LLVM can at least unroll), or, inside this adapter-support module, use `UnsafeCell<[f32]>` with the existing acquire/release indices and `copy_from_slice` for the two wrap segments (`// SAFETY:` single producer/consumer, indices published with release).
- Benefit: estimate 0.3-1 µs per device per block (callback side and audio thread side).
- Effort: S-M
- Risk: Med if `unsafe` is used (risky-review); Low for the planar-only change.
- Kind: quick-win ; Hot path: yes

### A-24 Encoder hand-off: three copies of every mixed block per encoder
- Location: mixer slot → `AudioWriter` `buffer.fill(&block)` (`queue.rs` ~`:654`) → `AudioReader::process` copy into its slot (`queue.rs:760-764`) → `EncoderNode` submit → adapter staging (e.g. `rbs-codec-audiotoolbox/src/mac.rs:321` `staging`).
- Problem: 3 × 3.8 KB copies per block per encoder chain; small, but the reader copy exists only to present a slot.
- Proposal: let `AudioReader` publish the queue buffer as the slot's block (borrow, return after the tick) instead of copying.
- Benefit: estimate ~0.2 µs and one cache-line stream per block per chain. Low priority.
- Effort: M
- Risk: Low-Med (buffer lifetime across the tick).
- Kind: structural ; Hot path: yes

### A-25 Mixer looks up each strip's meter by a linear search over params per input per block
- Location: `crates/rbs-audio/src/mixer.rs:491-497` (`self.params.get().strips.iter().find(|s| s.producer == producer)`) and `:457` (strip `find`).
- Problem: O(N²) per block (16 strips → up to 256 compares + pointer chasing). Tiny today; grows with MAX_LAYERS strips.
- Proposal: store the `Option<Arc<LevelMeter>>` (or its index) in `StripState` in `apply_params`.
- Benefit: estimate <0.3 µs per block at 16 strips.
- Effort: S
- Risk: Low.
- Kind: quick-win ; Hot path: yes

---

## Already good

- Disabled audio filters are Standby (not scheduled, zero latency) (`docs/specs/audio-filters.md:54`, `studio.rs:2700`); loudness tap and sync probe exist only while on (hot-paths sign-offs).
- Compressor: vectorisable passes, integer-bit peak, 4-step RMS recursion, idle fast path (`compressor.rs:270-370`). Limiter: "at rest" fast path (`limiter.rs:156`). EQ: skips inactive sections, runs stereo as two lanes, glides in steps (`equaliser.rs:404-520`). Gain: unity path.
- RNNoise model and tables are shared once (`rbs-denoise/src/lib.rs:137` `OnceLock`); the RNN is skipped on silent frames; no allocation per frame.
- Audio encoders are shared between outputs with equal settings (`AudioPlan::Shared`, `studio.rs:4074`).
- The media sound converter skips resampling at equal rates (`rbs-nodes/src/media/pcm.rs:24`) and its node only copies the due block.
- The UI meter timer runs only while the Mixer shows meters and stops while the app is hidden (`main.rs:2301-2311`, `:2731`); `audio.levels` samples only while a client is subscribed.
- Lock-free meters with separate UI/API banks; lock-free, allocation-free sample ring; pre-allocated audio queue buffers; partitions have no worker until a schedule is applied (ADR-0013).
- Device threads: WASAPI event-driven with MMCSS "Pro Audio"; macOS periodic threads use `THREAD_TIME_CONSTRAINT_POLICY`.
- K-weighting in f64 with explicit state flush (`loudness/filter.rs:78`): correct choice for the 38 Hz high-pass; keep.

## Open questions / measurements needed (each ≤ 2 min on a device)

1. `cargo bench -p rbs-audio --bench nodes` and `--bench loudness` on the M4 Pro and the 4090 box: confirm the share of the resampler in the input/output node and of true peak in loudness (A-02, A-03, A-14).
2. macOS Activity Monitor "Idle Wake Ups" (or `powermetrics --samplers tasks -n 1`) for RBS with the window hidden, no output, no audio sources; then with the Mixer visible and silent (A-10, A-11, A-12).
3. Windows: `cargo bench -p rbs-audio-filters --bench dsp -- equaliser` with a case "tone, then 2 s of digital zeros" before/after FTZ/DAZ (A-04).
4. `cargo bench -p rbs-denoise` with a silent (−70 dBFS noise) frame vs speech: time of pitch search vs RNN (A-07).
5. Is the macOS microphone indicator on while RBS is hidden and idle? (confirms A-11's "devices stay open").
6. Does the generated `MeterRow` implement `PartialEq`, and does Slint repaint the window at 30 Hz with silent meters (Quartz Debug "flash screen updates" or Windows PresentMon)? (A-10)
7. The audio partition's MMCSS task on Windows is "Games" (`rbs-thread-os/src/windows.rs:79`) while device threads use "Pro Audio": is that intended? (No measured cost; worth a one-line decision.)
