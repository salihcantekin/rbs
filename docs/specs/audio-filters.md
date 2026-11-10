# Spec: Audio filters

- Status: Draft (crate, nodes, binder, studio wiring and control API 1.16 exist; the UI follows in a later PR of ADR-0082)
- Owner crate(s): `rbs-audio-filters` (audio filter type trait, DSP trait, filter node, catalogue, conformance suite); `rbs-params` (settings schema, shared with video filters)
- Related ADRs: ADR-0082, ADR-0062, ADR-0023, ADR-0020, ADR-0013

## Purpose

Built-in audio filters for sources: Gain and Noise gate, run as `AudioFilter` nodes between an audio source and the mixer. This spec fixes how a filter type is defined, what the DSP may and may not do, and the exact behaviour of the two filters so it can be tested without a clock.

## Scope

In scope: `AudioFilterType`, `AudioDsp`, `FilterMeter`, `AudioFilterNode`, the catalogue and the conformance suite.

Out of scope here (later PRs of ADR-0082): the editor UI, a limiter and look-ahead filters (they need latency reporting in the node contract), the ducker.

## Contract

### Types

```rust
pub trait AudioFilterType: Send + Sync + 'static {
    fn type_id(&self) -> NodeTypeId;                 // org.rbs.audio-filter.<name>
    fn name(&self) -> &'static str;
    fn fields(&self) -> &'static [Field];            // at most 12, rbs_params schema
    fn make(&self, setup: DspSetup, values: &Values) -> Result<Box<dyn AudioDsp>, ParamError>;
}

pub trait AudioDsp: Send {
    fn set(&mut self, values: &Values);              // never resets, never steps
    fn reset(&mut self);                             // initial state (closed gate)
    fn process(&mut self, channels: &mut [&mut [f32]], frames: usize);
    fn meter(&self) -> Option<Arc<FilterMeter>>;     // once, before adoption
}
```

`DspSetup { format, block_frames }` is the graph's audio format and block size. `make`, `set` and `reset` run off the audio thread or in `prepare`; `process` never allocates, blocks, locks or does I/O (P6). All state is sized in `make`. A DSP sanitises its output: no NaN, no infinity, no denormals.

### Node

`AudioFilterNode` (kind `AudioFilter`, input `in`, output `out`, no latency). In `process` it takes the newest values from its `ParamSender<Values>` (ADR-0023), copies the input block to the output buffer with the input's timestamp, and runs the DSP in place. No input block: it publishes nothing. A block whose format or length is not the graph's is dropped and counted (`AudioFilterStats::invalid_inputs`). `prepare` applies the current values and calls `reset`, so a filter joining the schedule starts clean; a value edit never resets. `AudioFilterNode::pass_through` builds a node that passes audio unchanged, for a filter that could not be made.

### Catalogue v1

| Type id | Name | Fields (key: range, default; unit) |
| --- | --- | --- |
| `org.rbs.audio-filter.gain` | Gain | `gain` -30..+30, 0; dB, step 0.5 |
| `org.rbs.audio-filter.noise-gate` | Noise gate | `open_threshold` -80..0, -40; dB. `close_threshold` -80..0, -46; dB. `attack` 0.1..100, 5; ms, log. `hold` 0..500, 100; ms. `release` 5..1000, 150; ms, log. `range` -80..0, -80; dB |

**Gain** ramps linearly in amplitude over 10 ms to a new value; at 0 dB the output is bit-exact.

**Noise gate.** The detector is a peak envelope follower over all channels (linked): per sample `env = max(|x|, env * decay)` with a fixed 40 ms time constant. The gate is *open* while `env >= open_threshold` (opening from closed at that moment) and stays open while `env >= close_threshold`; `hold` is the time since `env` was last at or above `close_threshold`; when it has run out the gate closes. The gain moves linearly in dB at `|range| / attack` dB per ms when opening and `|range| / release` dB per ms when closing, so a partial transition reverses from the current gain with no jump. `range` is at most 0 dB (0 means no effect). A `close_threshold` above `open_threshold` is clamped to it inside `set`. A change of `hold`, `attack` or `release` during a transition changes only the rate. The ripple of the envelope on a sine of frequency `f` is `8.7 / (2 * f * 0.040)` dB, so with `hold` 0 a gap between the thresholds smaller than that can chatter; the help text says so. The gate has no sidechain input and publishes a `FilterMeter` (input peak and gain reduction in dB, once per block).

### Conformance

`conformance::check_type` checks the id, the schema (`rbs_params::check_fields`) and that the DSP output stays clean for silence, full scale, NaN, infinity and denormal input at the defaults and at every number field's extremes.

### Wiring

The scene binder puts an audio filter node in its source's partition and links the mixer to the chain tail (`scene-model.md`, "Graph binding"). `rbs-sources` installs the catalogue's descriptors in `node_types()` and converts stored settings with `filter_values`, `filter_values_lenient` and `filter_settings`, which take any `Schema`. The studio (`rbs-record`) makes an `AudioFilterNode` for every scheduled audio filter on the audio partition, keeps its `ParamSender`, sends values on edits (`update`, `preview_filter`, `restore_filter`) and exposes the meter (`Studio::audio_filter_meter`). If `make` fails, the studio runs a pass-through node and reports the filter in `filter_problems` with the key `FILTER_UNAVAILABLE` (`"*"`), so the source stays audible and unfiltered.

## Performance

Hot path (P15, `hot-paths.md`). `cargo bench -p rbs-audio-filters` measures one block of 480 frames at 48 kHz stereo; the PR that introduced the crate records the numbers.

## Tests

`tests/dsp.rs` (block-driven DSP behaviour, allocation guard) and `tests/node.rs` (node in a real executor: timestamp kept, value edit without reset, missing input, wrong format, allocation guard including a value update).
