# Spec: Audio filters

- Status: Draft (crate, nodes, binder, studio wiring and control API 1.16 exist; the UI follows in a later PR of ADR-0082). Latency contract and compensation (ADR-0130 step 1) and Noise suppression (step 2, `rbs-denoise`), the compressor (step 3), the limiter (step 4) and the equaliser (step 5) built; the API fields are planned.
- Owner crate(s): `rbs-audio-filters` (audio filter type trait, DSP trait, filter node, catalogue, conformance suite); `rbs-denoise` (RNNoise, ADR-0130); `rbs-params` (settings schema, shared with video filters)
- Related ADRs: ADR-0130 (latency, voice filters), ADR-0082, ADR-0062, ADR-0023, ADR-0020, ADR-0013

## Purpose

Built-in audio filters for sources: Gain and Noise gate, run as `AudioFilter` nodes between an audio source and the mixer. This spec fixes how a filter type is defined, what the DSP may and may not do, and the exact behaviour of the two filters so it can be tested without a clock.

## Scope

In scope: `AudioFilterType`, `AudioDsp`, `FilterMeter`, `AudioFilterNode`, the catalogue and the conformance suite.

Out of scope here (later PRs of ADR-0082): the editor UI, a limiter and look-ahead filters (they need latency reporting in the node contract). The ducker and its key input are decided in [ADR-0095](../adr/0095-audio-ducker-and-key-tap.md) (Accepted).

## Contract

### Types

```rust
pub trait AudioFilterType: Send + Sync + 'static {
    fn type_id(&self) -> NodeTypeId;                 // org.rbs.audio-filter.<name>
    fn name(&self) -> &'static str;
    fn fields(&self) -> &'static [Field];            // at most 12, rbs_params schema
    fn make(&self, setup: DspSetup, values: &Values) -> Result<Box<dyn AudioDsp>, ParamError>;
    fn key_input(&self) -> bool { false }            // a second input `key` (ADR-0095)
    fn latency_frames(&self, setup: DspSetup) -> u32 { 0 } // display only (ADR-0130)
}

pub trait AudioDsp: Send {
    fn set(&mut self, values: &Values);              // never resets, never steps
    fn reset(&mut self);                             // initial state (closed gate)
    fn process(&mut self, channels: &mut [&mut [f32]], frames: usize);
    // `key`: the key's planar channels, or None (no key linked or no block): silence.
    fn process_keyed(&mut self, channels: &mut [&mut [f32]], key: Option<&[&[f32]]>, frames: usize);
    fn meter(&self) -> Option<Arc<FilterMeter>>;     // once, before adoption
    fn latency_frames(&self) -> u32 { 0 }            // fixed at make (ADR-0130)
}
```

`DspSetup { format, block_frames }` is the graph's audio format and block size. `make`, `set` and `reset` run off the audio thread or in `prepare`; `process` never allocates, blocks, locks or does I/O (P6). All state is sized in `make`. A DSP sanitises its output: no NaN, no infinity, no denormals.

### Node

`AudioFilterNode` (kind `AudioFilter`, input `in`, output `out`, no latency; a type with `key_input()` has a second audio input `key`, whose block counts only in the graph's format and size and is silence otherwise). In `process` it takes the newest values from its `ParamSender<Values>` (ADR-0023), copies the input block to the output buffer with the input's timestamp, and runs the DSP in place. No input block: it publishes nothing. A block whose format or length is not the graph's is dropped and counted (`AudioFilterStats::invalid_inputs`). `prepare` applies the current values and calls `reset`, so a filter joining the schedule starts clean; a value edit never resets. `AudioFilterNode::pass_through` builds a node that passes audio unchanged, for a filter that could not be made.

### Latency (ADR-0130 decisions 2 to 4)

A DSP's output may be later than its input by `AudioDsp::latency_frames()` frames. The value is fixed when the DSP is made: `set` never changes it, so a value edit never changes a chain's latency. `reset` fills the delay with silence. `AudioFilterType::latency_frames(setup)` is for display (control API, help text); it equals what `make` would build for `setup` and is 0 where `make` refuses. The conformance suite checks both, at the defaults and at every number field's extreme. A pass-through (a filter that could not be made) has latency 0. `AudioFilterStats::latency_frames()` carries the built DSP's latency to the studio.

**Reset after a gap.** A node whose DSP has latency above 0 (fixed when the node is built) resets the DSP before the first block that follows a tick without an input block, so stale audio never plays after a gap. A joining upstream node with no output on its first tick causes one more reset right after `prepare`; it is harmless. Zero-latency DSPs are never reset by a gap.

**Compensation.** The chain latency `L` of a source is the sum over its audio filter instances that the plan schedules (enabled, not Standby). For a source with an input node the studio sends the input `stored - L`; the input clamps it to `a` (`audio-graph.md`, "Offset"), and the program hears the source at `h = a + L`. When the clamp does not bite, `h` is the stored offset; otherwise `uncompensated = h - clamp(stored)` is reported. A source without an input node is late by `L`. `Studio::apply_input_offsets` sends the new value when a filter instance is made, enabled, disabled or removed, or when the stored offset changes, and never writes the stored offset or the device memory (ADR-0126). `Studio::audio_latency(source)` returns `AudioLatency { filter_latency_ns, heard_offset_ns, uncompensated_ns }`; `rbs_record::compensate_filter_latency` is the pure calculation. The sync measurement's effective offset is `h` (ADR-0126 decision 4 as amended by ADR-0130).

### Catalogue v1

| Type id | Name | Fields (key: range, default; unit) |
| --- | --- | --- |
| `org.rbs.audio-filter.gain` | Gain | `gain` -30..+30, 0; dB, step 0.5 |
| `org.rbs.audio-filter.noise-gate` | Noise gate | `open_threshold` -80..0, -40; dB. `close_threshold` -80..0, -46; dB. `attack` 0.1..100, 5; ms, log. `hold` 0..500, 100; ms. `release` 5..1000, 150; ms, log. `range` -80..0, -80; dB |
| `org.rbs.audio-filter.noise-suppression` | Noise suppression | `max_reduction` 6..80, 30; dB. `mode` choice `mono` ("One pass for all channels", default) or `separate` ("Each channel"). `voice_gate` toggle, off. `voice_threshold` 0..100, 60; % |
| `org.rbs.audio-filter.compressor` | Compressor | `key_source` a source (none). `threshold` -60..0, -18; dB. `ratio` 1..20, 4; :1, log. `knee` 0..24, 6; dB. `attack` 0.1..200, 5; ms, log. `release` 10..2000, 120; ms, log. `detector` choice `rms` ("Average (RMS)", default) or `peak` ("Peak"). `makeup` 0..24, 0; dB. `mix` 0..100, 100; % |
| `org.rbs.audio-filter.limiter` | Limiter | `ceiling` -30..0, -1; dB, step 0.1. `release` 1..1000, 60; ms, log |
| `org.rbs.audio-filter.equaliser` | Equaliser | `low_cut` toggle, off. `low_cut_freq` 20..400, 80; Hz, log. `low_shelf_freq` 20..1000, 120; Hz, log. `low_shelf_gain` -18..18, 0; dB. `band1_freq` 40..16000, 400; Hz, log. `band1_gain` -18..18, 0; dB. `band1_q` 0.1..10, 1; log. `band2_freq` 40..16000, 3000; Hz, log. `band2_gain` -18..18, 0; dB. `band2_q` 0.1..10, 1; log. `high_shelf_freq` 1000..20000, 8000; Hz, log. `high_shelf_gain` -18..18, 0; dB |
| `org.rbs.audio-filter.ducker` | Ducker | `key_source` a source (none), `threshold` -80..0, -30; dB. `amount` 0..60, 12; dB. `attack` 1..200, 20; ms, log. `hold` 0..2000, 300; ms. `release` 20..3000, 500; ms, log |

**Noise suppression (ADR-0130 decision 5).** RNNoise from `rbs-denoise` (a vendored, patched copy of `nnnoiseless` 0.5.2; its README lists the patches). 48 kHz only: at another rate `make` returns `ParamError::Unsupported` and the studio runs a pass-through (latency 0). Samples are scaled by 32768 into RNNoise's i16 range and back. `mode` `mono` denoises the mean of the channels once and writes it to every channel; `separate` runs one RNNoise state per channel. `make` builds one state per channel of the graph format, so a `mode` change in `set` allocates nothing; it resets every state (about 10 ms of silence while they converge; the one value edit that is audible). `max_reduction` is a floor for the applied band gains (`10^(-max_reduction/20)`). With `voice_gate` on, a frame whose voice probability stays under `voice_threshold` for more than 20 frames (200 ms) is lowered by `max_reduction` too, ramped linearly across the frame; a frame at or above the threshold opens it at once (the probability belongs to the input a frame ahead of the output, so the opening leads the speech). Latency for a block of `B` frames: `960 - gcd(B, 480)` (480 for multiples of 480): input collects in a 480-frame FIFO, every full frame is denoised, and the output queues start with `480 - gcd(B, 480)` frames of silence so they never run dry; `process` reads the whole block before writing any output. It publishes a `FilterMeter`: the block's input peak, the last frame's reduction (energy ratio, dB) and the voice probability (`FilterMeter::voice`, 0..1; 0 for every other filter).

**Compressor (ADR-0130 decision 6).** Has a `key` input, bound like the Ducker's; with a key the detector reads only the key. Per sample the detector level is linked over the channels: `rms` is a mean square with a 10 ms time constant, `peak` the largest square; NaN, infinity and squares below 1e-30 count as silence. A detector value below the bottom of the knee gives no reduction without a dB conversion. The static curve is the quadratic soft knee of Giannoulis, Massberg and Reiss (2012): with `over = level - threshold`, 0 below `-knee/2`, `(1/ratio - 1) * (over + knee/2)^2 / (2 * knee)` inside the knee and `(1/ratio - 1) * over` above it. The gain reduction in dB follows the curve with the `attack` time constant while it deepens and `release` while it eases; a reduction above -1e-6 dB counts as none. A block is processed in passes over a buffer made in `make` (detector, dB conversion, smoothing, gain), so only the two smoothers run sample by sample; a block longer than the setup's is processed in parts with the same result. The output gain is `1 - mix + mix * 10^((reduction + makeup) / 20)`. The dB conversions use polynomial log2 and exp2 (error below 0.001 dB). No latency. Budget 5 µs per 480-frame stereo block (`cargo bench -p rbs-audio-filters --bench dsp -- compressor`). At the defaults with a quiet input, or with `mix` 0, the output is bit-exact. It publishes a `FilterMeter`: the input peak and the block's deepest reduction.

**Limiter (ADR-0130 decision 7).** Latency `W`: 2 ms rounded to whole frames (96 at 48 kHz), whatever the block size. Per input sample `j` the needed gain is `g = min(1, ceiling / max_ch |x|)` (NaN and infinity count as silence) with the ceiling lowered by 2^-20 (about -0.00001 dB) so float rounding never lifts an output sample over it. The release stage keeps `1 - g'`, which shrinks by `exp(-1 / (release * rate))` per sample and never leaves `g' > g`; within 1e-6 of 1 it is 1. `m[j]` is the minimum of `g'` over `[j - W, j]` (a monotonic queue in a ring made in `make`); the gain for output sample `n` (input `n - W`) is the mean of `m` over `[n - W + 1, n]` (an `f64` running sum, recomputed every `W` samples). Every window of that mean contains sample `n - W`, so no output sample exceeds the ceiling. A final clamp stays as a guard and counts when it acts (`LimiterDsp::guard_hits`); the tests require 0 for full-scale squares, rising impulses within one window, an impulse at every offset of the window, noise at +12 dB and noise bursts, at block sizes 480, 64 and 37. A part with nothing to limit while at rest skips the queue. Sample peak, not true peak. Below the ceiling and at rest the output is the input delayed by `W`, bit-exact. The guarantee covers this filter's output only. It publishes a `FilterMeter`: the input peak and the block's deepest reduction.

**Equaliser (ADR-0130 decision 8).** Five RBJ biquads ("Audio EQ Cookbook") in transposed direct form II, per channel, in this order: low cut (high-pass, Q 0.707, 12 dB per octave), low shelf (slope 1), band 1 and band 2 (peaking), high shelf (slope 1). Coefficients are computed in `f64` and run in `f32`. Frequencies above 0.45 × the rate are clamped. No latency, no meter. `set` only stores targets: frequency and Q glide in octaves, gain in dB, with a 20 ms time constant, snapping within 1e-4 octave or 1e-3 dB; while anything glides the coefficients of the gliding sections are recomputed every 32 frames. A shelf or band whose gain is 0 dB, and the low cut while off, are skipped (their state is cleared), so the defaults, and a gain that glided back to 0, are bit-exact. The low cut's switch acts at once and starts from a clean state. Input and output are sanitised; a state that is not finite after a block restarts at 0. Two channels run in one loop so their recursions overlap.

**Gain** ramps linearly in amplitude over 10 ms to a new value; at 0 dB the output is bit-exact.

**Noise gate.** The detector is a peak envelope follower over all channels (linked): per sample `env = max(|x|, env * decay)` with a fixed 40 ms time constant. The gate is *open* while `env >= open_threshold` (opening from closed at that moment) and stays open while `env >= close_threshold`; `hold` is the time since `env` was last at or above `close_threshold`; when it has run out the gate closes. The gain moves linearly in dB at `|range| / attack` dB per ms when opening and `|range| / release` dB per ms when closing, so a partial transition reverses from the current gain with no jump. `range` is at most 0 dB (0 means no effect). A `close_threshold` above `open_threshold` is clamped to it inside `set`. A change of `hold`, `attack` or `release` during a transition changes only the rate. The ripple of the envelope on a sine of frequency `f` is `8.7 / (2 * f * 0.040)` dB, so with `hold` 0 a gap between the thresholds smaller than that can chatter; the help text says so. The gate has no sidechain input and publishes a `FilterMeter` (input peak and gain reduction in dB, once per block).

**Ducker (ADR-0095).** Has a `key` input. `key_source` is a source id stored as an `Int` setting; none is the setting being absent (`FieldKind::Source`, `Value::Source(Option<u32>)`). The binder links `key` from the tail of the key source's chain (pre-fader); a missing, deleted, non-audio, own-source or cyclic key leaves it unlinked, and an unlinked or block-less key is silence. The detector is a linked peak follower over the key's channels with a 10 ms time constant. The duck is *active* from `env >= threshold` and stays active while `env >= threshold - 3 dB`; `hold` runs from the last such sample. The gain moves linearly in dB toward `-amount` (active) or 0 (inactive) at `max(amount, 1) / attack` and `max(amount, 1) / release` dB per ms, so a partial transition reverses without a jump and a lowered gain always returns, also after `amount` was set to 0. There is no look-ahead and no latency. With 0 dB of gain the block is unchanged bit for bit. It publishes a `FilterMeter`: the key's peak as `input_peak_db` and the reduction as `gain_reduction_db`.

### Conformance

`conformance::check_type` checks the id, the schema (`rbs_params::check_fields`) and that the DSP output stays clean for silence, full scale, NaN, infinity and denormal input at the defaults and at every number field's extremes.

### Wiring

The scene binder puts an audio filter node in its source's partition and links the mixer to the chain tail (`scene-model.md`, "Graph binding"). `rbs-sources` installs the catalogue's descriptors in `node_types()` and converts stored settings with `filter_values`, `filter_values_lenient` and `filter_settings`, which take any `Schema`. The studio (`rbs-record`) makes an `AudioFilterNode` for every scheduled audio filter on the audio partition, keeps its `ParamSender`, sends values on edits (`update`, `preview_filter`, `restore_filter`) and exposes the meter (`Studio::audio_filter_meter`). If `make` fails, the studio runs a pass-through node and reports the filter in `filter_problems` with the key `FILTER_UNAVAILABLE` (`"*"`), so the source stays audible and unfiltered.

## Performance

Hot path (P15, `hot-paths.md`). `cargo bench -p rbs-audio-filters` measures one block of 480 frames at 48 kHz stereo; the PR that introduced the crate records the numbers. `--bench node` measures one executor tick of a source, a Gain node and a sink (the node's own per-block cost, ADR-0130).

## Tests

`tests/dsp.rs`, `tests/ducker.rs` and `tests/noise.rs` (block-driven DSP behaviour, allocation guards; noise suppression: latency for 480-, 960-, 256-, 1024-, 441- and 1-frame blocks, output equal to RNNoise delayed by the reported latency, reduction bounded by `max_reduction`, the voice gate, the 48 kHz rule, a `mode` switch without allocation, separate channels stay apart); `rbs-denoise/tests/denoise.rs` (agreement with `nnnoiseless` below -90 dBFS, one frame of latency, the gain floor, NaN and infinity, `reset`, no allocation including the first call) and `tests/node.rs` (node in a real executor: timestamp kept, value edit without reset, missing input, wrong format, allocation guard including a value update).

## Noise gate level bar (UI)

The Properties editor of a noise gate starts with a `GateBar`: the input level as a bar from -80 to 0 dB (`meter.low`), the open threshold (`accent`) and the close threshold (`text.secondary`) as lines on it, and under it a thin bar for the gain reduction (`meter.mid`). The bar reads `FilterMeter` through the `filter-levels` callback from a 30 Hz timer that exists only while the gate's section is open and shown (P13); no section, no timer, no reads.
