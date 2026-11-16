# ADR-0130: Voice filters: noise suppression, compressor, limiter and equaliser, with filter latency

- Status: Proposed
- Date: 2026-11-16
- Deciders: Salih (owner)
- Builds on: [ADR-0082](0082-audio-filter-chains-on-sources.md) (audio filter chains), [ADR-0095](0095-audio-ducker-and-key-tap.md) (key input), [ADR-0126](0126-measured-sync-offset-and-device-memory.md) (sync offset), [ADR-0091](0091-per-scene-audio-levels.md), [ADR-0092](0092-loudness-metering-and-leveling.md) (loudness; its look-ahead limiter waits for this latency decision)
- Spec: [audio-filters](../specs/audio-filters.md), [audio-graph](../specs/audio-graph.md) ("Offset"), [control-api](../specs/control-api.md)

## Context

Sources have audio filter chains (ADR-0082) with three types: Gain, Noise gate and Ducker. A microphone in a live production usually needs four more: noise suppression (fans, keyboard, room hum that a gate cannot remove while the person talks), a compressor (even voice level), a limiter (no overs after gain or compression) and an equaliser (low cut, presence). OBS offers RNNoise and Speex suppression, a compressor, a limiter without look-ahead and a 3-band EQ with fixed bands. ADR-0082 named a compressor, limiter and equaliser as later work and kept filters latency-free; ADR-0092 decision 6 defers a look-ahead limiter until "a separate decision adds latency reporting to nodes". This ADR is that decision.

Facts that shape it:

- The graph runs at 48 kHz stereo with 480-frame blocks by default (`AudioGraphConfig`), but the rate and block size are configurable (1 to 8192 frames).
- `AudioDsp::set` is called inside `AudioFilterNode::process` when values change, so it runs on the audio thread and must be real-time safe (P6).
- `Values` holds at most 12 fields (`rbs_params::MAX_FIELDS`).
- An input node already applies a per-source offset (`InputParams`, ADR-0126): a delay up to its `max_offset`, or an *advance* up to `latency - period - 5 ms` (35 ms with the default 50 ms graph latency) by reading its FIFO earlier. The studio sends it through `Studio::set_audio_offset`; the node's block path needs no change to apply a different value.
- RNNoise (Xiph, BSD-3-Clause) is the established light neural denoiser: 48 kHz mono, 480-sample frames, a GRU model of about 88 KB, roughly 1 % of one core. Its pure Rust port `nnnoiseless` 0.5 (BSD-3-Clause) reaches its FFT through `easyfft`, which plans lazily in a **thread-local cache on the first call**: the first `process_frame` on the audio thread allocates and plans, and every call does a map lookup. That breaks P6 as shipped.
- RNNoise's overlap-add synthesis delays its output by one frame (480 samples, 10 ms). A look-ahead limiter delays by its look-ahead. Both are latency the node contract cannot express today.

## Decision

1. **Four new built-in types, in this order:** Noise suppression, Compressor, Limiter, Equaliser. Each is an `AudioFilterType` in the catalogue (ADR-0082 decision 3), with its schema, DSP, conformance run, benchmark and allocation guard. No scene format change: filters are type ids and settings. VST and other plugin hosting is out of scope and needs its own ADR.

2. **Filter latency is part of the type contract.** `AudioFilterType` gains `fn latency_frames(&self, setup: DspSetup) -> u32 { 0 }`. It must depend only on the type and the setup, never on values, so a value edit never changes a chain's latency. The node delays nothing itself; the DSP's output is simply later by that many frames, and `reset` starts the delay line with silence. `PassThrough` reports 0. The chain latency of a source is the sum over its **enabled** filters.

3. **Compensation by input advance, never by delaying other sources.** For a source bound to an input node, the studio sends `effective = offset - chain_latency` (ns, at the graph rate) instead of the stored offset. The stored offset (scene `sync_offset_ms`, device memory of ADR-0126) is unchanged: it stays a property of the device, so a measurement (ADR-0126) made with a denoiser on stores the same value as one made without. The input node clamps as today; when the clamp cuts the advance, the remainder is reported as `uncompensated_ns` (decision 8) and the UI says so. Sources without an input node (media, browser audio) are late by their chain latency and report it the same way. Enabling or disabling a latent filter changes the effective offset in the same apply as the chain-tail change, so its step falls inside the dip ADR-0082 decision 6 already accepts; a value edit never moves it. Other sources and the video are never delayed to match.

4. **Noise suppression** (`org.rbs.audio-filter.noise-suppression`, "Noise suppression").
   - The RNNoise algorithm and model, ported in a new crate **`rbs-denoise`** (pure Rust, no OS or GPU, depends only on `realfft`/`rustfft`) from `nnnoiseless` 0.5 under its BSD-3-Clause notice (the notice is kept in the crate's `LICENSE-RNNOISE` file and source headers; BSD-3-Clause is on the `deny.toml` allow list and compatible with GPL-3.0-or-later). The port owns its FFT plans and scratch buffers, made in `new`; `process_frame` must not allocate, lock, or touch thread-locals (allocation guard test). The model weights are compiled in (`include_bytes!`, about 88 KB, in the binary only, no file I/O).
   - `rbs-audio-filters` depends on `rbs-denoise`; the layering test keeps both free of `rbs-gpu`, `wgpu` and OS crates. `rbs-denoise` is a DSP library, not an adapter: it touches no device and no vendor SDK (P1). It is on the hot-path list and in CODEOWNERS.
   - Only at 48 kHz: at another rate `make` returns the new `ParamError::Unsupported(&'static str)` (`ParamError` is `#[non_exhaustive]`, so adding it is not a break) and the studio's existing fallback (ADR-0082 decision 8) runs a pass-through and reports `FILTER_UNAVAILABLE`, so the source stays audible. A resampling wrapper is a later change.
   - Latency: 480 frames when `block_frames` is a multiple of 480, else 960 (an internal 480-frame FIFO in front of the frame processor); both fixed per setup.
   - Fields: `max_reduction` (6..80 dB, default 30, the floor of RNNoise's per-band gains, so speech is never over-suppressed and the residual noise sounds natural; OBS has no strength control), `mode` (choice `mono` "One pass for all channels" (default: a microphone is one voice; L+R is denoised once and written to every channel) or `separate` "Each channel" (stereo sources, one state per channel, cost × channels)), `voice_gate` (toggle, default off: lowers by `max_reduction` while RNNoise's voice probability stays under `voice_threshold` for 200 ms, a gate that keys on speech, not level), `voice_threshold` (0..100 %, default 60).
   - Meter: `FilterMeter` with the input peak and the current reduction; the voice probability is published as a third relaxed atomic for the editor (not a contract change: `FilterMeter` gains a `voice` field read only by the noise suppression editor).

5. **Compressor** (`org.rbs.audio-filter.compressor`, "Compressor"). Zero latency. Linked detector over all channels (or over the key, below), selectable `detector` (choice `peak` or `rms`, RMS over 10 ms, default `rms`), gain computer with `threshold` (-60..0 dB, -18), `ratio` (1..20, log, 4), `knee` (0..24 dB, 6; the quadratic soft knee of Giannoulis et al.), and a smoothed gain in dB with `attack` (0.1..200 ms, log, 5) and `release` (10..2000 ms, log, 120), then `makeup` (0..24 dB, 0) and `mix` (0..100 %, 100; parallel compression, which OBS lacks). Sidechain: `key_input()` is true and `key_source` (`FieldKind::Source`, none by default) selects a key as for the ducker (ADR-0095); with no key source the compressor detects on its own input; with a key source whose block is missing the key is silence (no gain reduction), the ducker's rule. 10 fields. Meter: input peak and gain reduction (the gate's bar).

6. **Limiter** (`org.rbs.audio-filter.limiter`, "Limiter"). Look-ahead of 2 ms rounded to whole frames (96 at 48 kHz), reported as its latency. Per sample the required gain is `min(1, ceiling / peak)` over all channels (linked); a sliding-window minimum over the look-ahead (a monotonic queue sized in `make`) gives the gain the delayed sample needs, the gain falls linearly across the window so it arrives in time, and rises with `release` (1..1000 ms, log, 60). Guarantee: no output sample exceeds `ceiling` (-30..0 dBFS, default -1); a final clamp to the ceiling enforces it if rounding ever leaves a residue, and a test counts that the clamp never acts on the conformance signals. Sample peak, not true peak; a true-peak option (4x oversampled detector) is a later field. The same type serves the master chain of ADR-0092 when that chain exists. Meter: input peak and gain reduction.

7. **Equaliser** (`org.rbs.audio-filter.equaliser`, "Equaliser"). Zero latency, biquads in transposed direct form II per channel (RBJ cookbook formulas). Exactly 12 fields: `low_cut` (toggle, off) and `low_cut_freq` (20..400 Hz, log, 80; 12 dB/oct high-pass, Q 0.707), `low_shelf_freq` (20..1000 Hz, log, 120) and `low_shelf_gain` (-18..18 dB, 0), two peaking bands `band1_freq` (40..16000 Hz, log, 400), `band1_gain` (-18..18, 0), `band1_q` (0.1..10, log, 1), `band2_freq` (40..16000, log, 3000), `band2_gain` (0), `band2_q` (1), `high_shelf_freq` (1000..20000 Hz, log, 8000) and `high_shelf_gain` (0). A frequency above 0.45 × the sample rate is clamped. A band at 0 dB (or a low cut that is off) is skipped, so the defaults pass audio bit-exact. A value edit glides: the target parameters are smoothed with a 20 ms constant and the coefficients recomputed every 32 frames, so `set` stays cheap (no trigonometry in `set`; it stores targets) and a sweep makes no zipper noise or step. The editor's response curve is a later visual proposal; the generated editor of ADR-0082 is the first UI.

8. **Control API** (one minor version, numbered at merge; next free today 1.68). `filter.types` entries gain `latency_ms` (at the running graph's setup). Source rows (`source.list`) gain `filter_latency_ms` and, when not zero, `uncompensated_ms` (decision 3). New filter type ids appear only for `domain` `"audio"` or `"all"` (ADR-0082 decision 9), so older clients see no change.

9. **UI.** The four editors are generated from their schemas (ADR-0082 decision 10); the compressor and limiter reuse the gate's level bar for gain reduction. A filter with latency shows its delay in its help line ("Adds 10 ms; compensated" or "Adds 10 ms; 4 ms could not be compensated"). Every visible change is proposed to the owner with dark and light screenshots before merge (CLAUDE.md rules 15, 16, 18). A one-click "Voice chain" preset (noise suppression, gate, EQ low cut, compressor, limiter) and the EQ curve are later visual proposals.

10. **Zero cost and hot path.** A source with none of these filters runs exactly as before: no node, no `rbs-denoise` state, no compensation arithmetic beyond the existing offset send. A disabled filter is Standby (ADR-0082 decision 11) and counts zero latency. Budgets per 480-frame stereo block at 48 kHz, measured with `cargo bench -p rbs-audio-filters` and recorded in each PR: compressor, limiter and equaliser at most 5 µs; noise suppression at most 250 µs in `mono` mode (2.5 % of the block). Each PR has the allocation guard, the `hot-path-review` checklist and Salih's sign-off (P15).

## Consequences

- Microphones get the four filters a voice chain needs, with controls OBS lacks: a reduction limit and a speech gate for noise suppression, parallel compression, a limiter that never overshoots, and a parametric EQ with Q.
- Latency becomes a declared, tested property; ADR-0092's look-ahead limiter is unblocked, and A/V sync stays as measured because the input advance absorbs it.
- `rbs-denoise` is a ported codebase (about 2,500 lines) that the project now maintains; its output is compared against `nnnoiseless` in a test (same input, same model, difference below -90 dBFS) so a porting error is caught.
- Noise suppression works only at 48 kHz in this version; at other rates it is reported unavailable and passes audio through.
- A source whose offset already uses the advance budget gets only partial compensation; this is reported, not hidden.
- Specs: `audio-filters.md` (latency in the contract, the four types, compensation), `audio-graph.md` (effective offset), `control-api.md`, `hot-paths.md`, `docs/architecture/overview.md` (`rbs-denoise`), CODEOWNERS, `rbs-params` (`ParamError::Unsupported`).

## Implementation order

1. Latency contract and compensation (decisions 2, 3, 8 API fields), tested with a test DSP that declares latency.
2. `rbs-denoise` port and Noise suppression.
3. Compressor. 4. Limiter. 5. Equaliser.
6. UI proposal with screenshots for the four editors; then merges one at a time after Salih's approval.

## Tests

- **Contract:** `latency_frames` is value-independent (conformance: same answer for defaults and every field extreme); the chain latency sums enabled filters only; the effective offset sent to an input node equals `offset - chain_latency` and changes only on enable, disable, add, remove; `uncompensated_ns` when the advance budget is exceeded; a non-input source reports its latency.
- **Alignment:** an impulse through a chain with noise suppression and a limiter arrives at the mixer at the same graph time as without the chain (input node with the default budget).
- **Noise suppression:** frame-exact agreement with `nnnoiseless` as a dev-dependency; reported latency equals the measured impulse delay for 480- and 256-frame blocks; white noise is reduced by at least 20 dB and by no more than `max_reduction`; the voice gate closes on noise and opens on a speech-like signal; `make` refuses 44.1 kHz; allocation guard including the first call.
- **Compressor:** static curve (threshold, ratio, knee) within 0.1 dB on steady sines; attack and release times in blocks; `mix` 0 is bit-exact; key silence gives no reduction; allocation guard.
- **Limiter:** no sample above the ceiling for full-scale square waves, impulses and noise at +12 dB; the clamp never acts; latency equals the measured delay; release time.
- **Equaliser:** defaults bit-exact; magnitude response at the band centres within 0.1 dB of the RBJ formulas; a sweep of a frequency during a sine makes no step above a bound; stable at every field extreme.
- The ADR-0082 conformance suite (clean output for silence, full scale, NaN, infinity, denormals) for all four.

## Alternatives considered

- **Use `nnnoiseless` as a dependency.** Least code, but its lazy thread-local FFT planning allocates on the audio thread and costs a lookup per frame (P6); fixing it needs a fork anyway. Rejected in favour of an owned port.
- **Speex preprocessor denoiser.** C library (FFI in a filter crate) and weaker on non-stationary noise. Rejected.
- **DeepFilterNet or another larger model.** Better quality, but tens of MB and a neural runtime (ONNX or tract), ten times the CPU; it would break the lean rule (CLAUDE.md rule 17) as a default. A later optional type, ideally through the plugin SDK.
- **Delay every other source and the video by the largest chain latency.** Keeps everything aligned without advance budget limits, but adds delay to the whole programme and to every output for one filter. Rejected; input advance costs nothing where it applies.
- **Ignore latency (as OBS does).** 10 ms is small, but it adds to the measured offset silently and a look-ahead limiter on top makes it worse; the measurement of ADR-0126 would depend on which filters were on. Rejected.
- **Latency depending on values (adjustable look-ahead).** A slider that moves the offset would make a value edit audible as a jump. Rejected; the look-ahead is fixed.
- **Raise `MAX_FIELDS` for a larger EQ.** `Values` is copied on every value update of every filter (ADR-0082 decision 2); 12 fields give a usable parametric EQ. A full graphic EQ is a later type with its own decision.
- **A limiter without look-ahead (OBS).** It overshoots on transients and cannot keep a ceiling. Rejected.

## Review

Independent review per ADR-0014. Pending.
