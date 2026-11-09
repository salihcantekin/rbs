# Spec: Video filters

- Status: Draft
- Owner crate(s): `rbs-filters` (filter type trait, filter node, catalogue, conformance suite); `rbs-sources` (scene settings to filter values, node types); `rbs-record` (filter instances in the studio)
- Related ADRs: ADR-0053 (decisions 6 to 9), ADR-0062, ADR-0013, ADR-0020, ADR-0023

## Purpose

Built-in GPU video filters for sources: colour correction, chroma key, blur and sharpen, run as `VideoFilter` nodes between a source and every compositor that draws it. This spec fixes how a filter type is defined, how its settings are checked and reach the running node, how the node renders without allocating, and what happens when its input changes size.

## Scope

In scope: the `FilterType` trait, field schema and values, the `FilterNode`, the first catalogue, the conformance suite, how the scene binder and the studio use them, and where the user edits filters.

Out of scope: audio filters; filters that need resources (LUT, image mask; a later version sends resource handles through the channel); background removal (own port, ADR-0053 decision 6); the drop shadow (an item style, ADR-0053 decision 5); looks (ADR-0053 decision 9); plugin filters (they will implement the same trait through the plugin SDK).

## Contract

### Filter types (`rbs_filters::FilterType`)

```rust
pub trait FilterType: Send + Sync + 'static {
    /// `org.rbs.filter.<name>`; well-formed (`NodeTypeId::is_well_formed`).
    fn type_id(&self) -> NodeTypeId;
    /// English display name, for example "Colour correction".
    fn name(&self) -> &'static str;
    /// The settings schema, in display order. At most `MAX_FIELDS` (8).
    fn fields(&self) -> &'static [Field];
    /// The passes, in order: 1 to `MAX_PASSES` (2).
    fn passes(&self) -> &'static [Pass];
    /// Fills one uniform block per pass from validated `values` and the input's size
    /// in pixels. Pure: no I/O, no allocation; called on the render thread when the
    /// values or the size change.
    fn uniforms(&self, values: &Values, input: [u32; 2], out: &mut [Uniforms]);
}

pub struct Field { pub key: &'static str, pub label: &'static str, pub kind: FieldKind }
#[non_exhaustive] pub enum FieldKind {
    /// A number in `min..=max`; `step` is the UI's increment, `unit` is shown after the value.
    Number { min: f32, max: f32, default: f32, step: f32, unit: &'static str },
    /// An sRGB colour with straight alpha.
    Color { default: [u8; 4] },
}
#[non_exhaustive] pub enum Value { Number(f32), Color([u8; 4]) }
pub struct Pass {
    /// WGSL with `@fragment fn fs_main(in: FilterIn) -> @location(0) vec4<f32>`, using
    /// the prelude's bindings (below).
    pub shader: &'static str,
    /// Size of the pass's target: `Full` (the input's size) or `Half` (each side
    /// rounded up). The last pass is always `Full`: it writes the published texture.
    pub target: Scale,
    /// What the pass samples: `Input` (the node's input) or `Previous` (the previous
    /// pass's target).
    pub source: PassSource,
}
/// 16 `vec4<f32>` (256 bytes), the uniform block of one pass.
pub struct Uniforms(pub [[f32; 4]; 16]);
```

Invariants (conformance tests): the type id is well-formed and starts with `org.rbs.filter.`; field keys are unique, lowercase ASCII with `_`; for numbers `min < max`, `default` and `step > 0` are finite and `default` is in range; there are 1 to 2 passes, the first samples `Input`, the last targets `Full`; `uniforms` of the default values returns finite numbers.

### Values (`rbs_params::Values`, re-exported as `rbs_filters::Values`)

The schema types (`Field`, `FieldKind`, `Value`, `Values`, `ParamError`, `MAX_FIELDS` = 12) live in the dependency-free crate `rbs-params` (ADR-0082), so audio filters share them without depending on the GPU crate. `rbs-filters` re-exports them and keeps its own limit of 8 fields per video filter type (`rbs_filters::MAX_FIELDS`), checked by the conformance suite. `Field`, `FieldKind` and `Value` are `#[non_exhaustive]`; fields are built with `Field::number`, `Field::color`, `Field::toggle`, `Field::choice`, `.with_help` and `.log`. `FieldKind` also has `Toggle` and `Choice`, `Number` has a `curve` (`Linear` or `Log`), and every field has a `help` sentence; the built-in video filters use numbers and colours only. `Values::defaults`, `from_pairs`, `with` and `to_pairs` take anything that is a `Schema` (a filter type or a field list). A `Toggle` is stored in settings as `Bool`, a `Choice` as the option's key (`Text`); a wrong kind or an unknown option is refused, and lenient loading falls back to the default.

`Values` holds one `Value` per field, in field order, always valid for its type. `Values::defaults(ty)` holds every field's default. `Values::from_pairs(ty, pairs: impl IntoIterator<Item = (&str, Value)>) -> Result<Values, ParamError>` starts from the defaults and replaces the given keys; `Values::with(&self, ty, pairs)` does the same from existing values (a patch). Refusals (`ParamError`): `UnknownKey(key)`, `WrongKind(key)` (a colour for a number or the reverse), `OutOfRange(key)` (outside `min..=max`, or not finite). Values never hold a value the type refused, so `process` never validates.

`Values::to_pairs(ty)` gives `(key, Value)` pairs for saving. Colours are written as `#rrggbb` when opaque, else `#rrggbbaa`.

In the scene model a filter's `Settings` map holds numbers as `SettingValue::Float` (an `Int` is read as a number) and colours as `SettingValue::Text` in `#rrggbb` or `#rrggbbaa`. `rbs_sources::filter_values(ty, &Settings) -> Result<Values, ParamError>` converts (any other text or a `Bool` is `WrongKind`) and `rbs_sources::filter_settings(ty, &Values) -> Settings` converts back, widening each number to the shortest decimal that reads back as the same `f32` (as documents write floats), so 0.1 is stored as 0.1. Whether an edit changes anything is decided on `Values`, not on stored settings. Missing keys take their defaults, so a document saved by a build whose type had fewer fields still loads. `rbs_sources::filter_values_lenient(ty, &Settings) -> (Values, Vec<String>)` is for settings read from a document (ADR-0062 decision 8): unknown keys are ignored, numbers clamped into range, wrong kinds and non-finite numbers replaced by the default, and the keys it adjusted are returned so the studio reports `FilterSettingsAdjusted { filter, key }`. Edits from the app and the API always use the strict check, on top of what is on air: the base of a patch, the Filters tab's fields and the API's `settings` rows are the lenient values the node runs, never the raw stored map. Every edit writes the full set (`filter_settings` of the patched `Values`), so the first edit repairs the stored settings, drops keys the type does not have and clears that filter's `FilterSettingsAdjusted`; a patch is refused only for keys it sends. A filter whose type is unknown keeps its stored settings untouched: it can be moved, enabled, disabled and removed, and an edit of its settings is refused.

The trait evolves only by methods with default bodies; `FieldKind` and `Value` are `#[non_exhaustive]`. Identifiers and API values spell `color`; UI text says "colour".

### Shader prelude

`rbs-filters` prepends to every pass:

```wgsl
struct FilterIn { @builtin(position) position: vec4<f32>, @location(0) uv: vec2<f32> };
@group(0) @binding(0) var<uniform> u: array<vec4<f32>, 16>;
@group(1) @binding(0) var src: texture_2d<f32>;
@group(1) @binding(1) var src_sampler: sampler;
// The sample at `uv` in linear light, premultiplied (decoded when the input is
// sRGB-encoded; the node sets `decode` in the last vector of `u`, `u[15].w`).
fn sample_linear(uv: vec2<f32>) -> vec4<f32>;
fn srgb_to_linear(c: vec3<f32>) -> vec3<f32>;
fn linear_to_srgb(c: vec3<f32>) -> vec3<f32>;
```

`u[15]` is reserved for the node: `xy` the source texel size (`1 / source width`, `1 / source height`), `z` unused, `w` 1.0 when the pass samples an sRGB-encoded input, else 0.0. Types fill `u[0]` to `u[14]`. The vertex stage draws one full-target triangle; `uv` is 0..1 over the target. `src_sampler` is the context's linear, clamp-to-edge sampler.

### Filter node (`rbs_filters::FilterNode`)

```rust
impl FilterNode {
    /// A node of `ty` with `values`; `size` is the input size when known (warms at it).
    pub fn new(ty: Arc<dyn FilterType>, values: Values, size: Option<[u32; 2]>)
        -> (FilterNode, ParamSender<Values>, Arc<FilterStats>);
}
pub fn descriptor(ty: &dyn FilterType) -> NodeDescriptor; // kind VideoFilter, in "in", out "out", video
pub struct FilterStats { /* atomics */ }
impl FilterStats {
    pub fn warm_size(&self) -> Option<[u32; 2]>;  // the size its textures are for
    pub fn resizes(&self) -> u64;                  // size changes handled in process
    pub fn frames(&self) -> u64;                   // frames published
}
```

- `prepare`: one render pipeline per pass (target format `CANVAS_FORMAT`, no blending), one uniform buffer per pass (256 bytes) and its bind group (group 0). Group 1 is the context's sampled-texture layout (`GpuContext::sampled_layout`), so pool textures bind with their own `PooledTexture::sampled_bind_group`.
- `warm`: when a size is known (from `new`), acquires and releases two `TextureDesc::canvas(w, h)` outputs and one intermediate per non-final pass at its scale, so `process` reuses them from the pool's free list. Without a known size it acquires nothing. Two outputs suffice: the frame published last tick stays leased in the output slot while the next is drawn, and filter outputs are read only within the render tick (never queued).
- `cool` (leaving the schedule: disabled, removed, or after a failed `warm`): gives back what the node kept for its size, as on a size change (step 2 of `process`): `forget_texture` on the output it published last and `forget` for its spare output and intermediates. So its memory goes at once even when the compositor acquires the same description every tick (a source as large as the canvas).
- `process` (hot path):
  1. `values.update()`; no input frame, or an input whose texture is gone or not sampleable: output nothing, `Ok`.
  2. Input size differs from the size of its textures (or none is known yet): give back the old size's textures (`TexturePool::forget_texture` on the output published last, `TexturePool::forget` for the spare output and each intermediate), store the new size and count it in `resizes`, then go on: step 4 acquires textures of the new size, which the pool creates on that first frame (the one allocation `process` may make, once per size change, as `CompositorNode` does on a pool miss; ADR-0062 decision 5). Never publishes the input or a texture of another size.
  2a. The input frame (texture id and time), the values, the size and the decode flag are those of the frame published last: the output slot still holds that frame (video slots keep their frame), so `touch` each target description and return `Ok` without recording anything.
  3. Values or size or decode flag changed since the last write: `uniforms` into a fixed array, then one `write_buffer` per pass.
  4. Acquire the output (and intermediates), record each pass (one draw of three vertices), submit, publish the output with the input's timestamp, `PixelFormat::Rgba16Float`, `ColorSpace::LINEAR_BT709`, release the textures to the pool (the frame's lease keeps the output alive, as the compositor's canvas).
  It does not block, lock or read back. At a steady size it makes no allocation of its own; the wgpu calls it shares with the compositor are checked as `alloc_per_frame` does (the count per frame is bounded and does not grow). A failing `acquire` (pool full, size above the device limit) publishes nothing and returns `NodeError::Backend`, an ordinary failure with restart and back-off (ADR-0024). `release` drops the pipelines and buffers.
- **Freeing memory.** `TexturePool::trim_idle(idle_ticks)` (rbs-gpu), called by `GpuRuntime::end_tick` once per tick with `POOL_IDLE_TICKS` (120), advances the pool's own clock and drops the free textures of each description nobody acquired or touched in the last `idle_ticks` calls; descriptions acquired every tick, or touched by a filter whose input is unchanged, are never dropped. A full pool evicts the least recently used free textures before refusing (`gpu-compositor.md`, invariant 9). Dropping textures on the render thread is the P6 exception of ADR-0062 decision 5. A source whose first frame comes more than `POOL_IDLE_TICKS` after `warm` allocates once on that frame (accepted). So the textures of a removed or disabled filter, or of a size no input has any more, are freed within about two seconds at 60 fps, on the render thread (P13).

### Catalogue (`rbs_filters::catalogue() -> &'static [&'static dyn FilterType]`)

| Type id | Name | Fields (key: range, default) | Passes |
| --- | --- | --- | --- |
| `org.rbs.filter.color-correction` | Colour correction | `brightness` −1..1, 0; `contrast` −1..1, 0; `saturation` 0..3, 1; `gamma` 0.1..4, 1; `hue` −180..180 °, 0 | 1 |
| `org.rbs.filter.chroma-key` | Chroma key | `key_color` color, `#00ff00`; `similarity` 0..1, 0.4; `smoothness` 0..1, 0.08; `spill` 0..1, 0.1 | 1 |
| `org.rbs.filter.blur` | Blur | `radius` 0..64 px, 8 | 2: horizontal at half size, vertical at full size |
| `org.rbs.filter.sharpen` | Sharpen | `amount` 0..2, 0.3 | 1 |

Maths, on un-premultiplied colour `c` (alpha `a`), premultiplied again at the end:

- **Colour correction**, in sRGB-encoded values `e = linear_to_srgb(c)`: `e = (e - 0.5) * (1 + contrast) + 0.5 + brightness` (contrast below 0 flattens, at −1 grey), saturation mixes with the Rec. 709 luma (`luma + (e - luma) * saturation`), hue rotates around the grey axis by `hue` degrees, gamma: `e = pow(max(e, 0), 1 / gamma)`; clamped to 0..1 and decoded. The defaults give the input back (within 1/1000 per channel).
- **Chroma key**, in sRGB-encoded values: distance `d` between the pixel's and the key's chroma (BT.709 Cb, Cr). Alpha is multiplied by `smoothstep(similarity, similarity + smoothness, d)` (`smoothness` is floored at 0.0001, because `smoothstep` with equal edges is undefined in WGSL; at 0 the edge is hard). Spill: the key's dominant channel is limited towards the average of the other two by `spill` times how close the pixel is to the key. A pixel of exactly the key colour becomes transparent at `similarity > 0`.
- **Blur**: Gaussian with `sigma = radius / 2` source pixels. Pass 1 samples the input at half-size texel centres along x with up to 31 taps on each side (`MAX_BLUR_TAPS`; a radius above about 40 px is cut at the 31st tap, about 1.9 sigma at radius 64, and the weights renormalised) at half-size spacing; pass 2 samples pass 1 along y the same way and writes full size. `radius` 0 uses one tap per pass; the half-size intermediate still softens the picture by about one source pixel, so the smallest blur is not the identity (remove the filter or turn it off for none). Weights are normalised to 1; premultiplied colours are blurred, so transparent edges do not darken.
- **Sharpen**: `c + amount * (4c - n - s - e - w)` on the four neighbours one source pixel away, clamped to 0..1 per channel and to `a`.

### Binding and the studio

- `rbs_sources::SourceTypes::node_types` installs `descriptor(ty)` for every catalogue type; the binder then binds filters as the scene model spec says.
- `SceneCollection::set_filter_settings(source, filter, settings) -> Result<Undo, SceneError>` replaces a filter's settings (one undo step; `UnknownSource`, `UnknownFilter`, `InvalidSetting` for a non-finite float). The model stores what it is given; callers validate with `filter_values` first.
- The binder keeps each bound filter's settings and reports `BindReport::changed_filters: Vec<(FilterId, NodeId)>` for a kept node whose filter's settings changed.
- `rbs_record::Studio` makes a `FilterNode` (render partition) for every scheduled filter node without an instance, with the source's picture size when it knows it (every filter of the chain gets it); keeps its `ParamSender<Values>`; sends values on `changed_filters`; takes instances back when retired; and `Studio::preview_filter(filter, values) -> bool` sends values to a running filter without changing the collection (slider drags); `Studio::restore_filter(filter) -> bool` sends the collection's values again (a cancelled drag); `Studio::filter_stats(filter)` gives a running instance's `FilterStats`. Values come from `filter_values_lenient`; its adjustments are listed in `Studio::filter_problems()`.

### Where the user edits filters

The Filters tab of the Properties panel (ADR-0062 decision 9; `ui-design-system.md`, Properties). The control API's `filter.*` methods (`control-api.md`, protocol 1.8).

## Errors and failure behaviour

- Invalid values: refused with `ParamError` before any model edit; the app says so in plain words in the error bar, the API answers `invalid_params`.
- Unknown filter type in a collection: binder problem `UnknownFilterType`; the source is drawn without that filter; the app lists the problem.
- `prepare` failing (shader rejected by the driver): the node stays out of the schedule and is restarted with back-off; the layer is not drawn (P7) rather than drawn unfiltered.
- Input resized: the frame of the new size is filtered and drawn; the textures for it are created on that frame and the old size's are given back at once, so a window resized every frame holds one size's textures plus at most one frame still being read.
- Settings for a filter whose type is unknown: `filter.set_settings` answers `invalid_params`; the Filters tab offers only Remove.

## Performance requirements

- At a steady size, the allocations per frame are bounded and do not grow (allocation-counting test, as `rbs-gpu`'s `alloc_per_frame`); a size change allocates on that frame only.
- `TexturePool::trim_idle` adds one pass over the retired list and one comparison per description per tick; the compositor benchmark is run before and after. The benchmark also measures a tick that creates the textures of a new size and a tick that drops them.
- An unchanged input costs no pass: one `touch` per target.
- `benches/filters.rs` in `rbs-filters`: each catalogue type at 1920x1080 on `Rgba16Float` input, `record_submit` (CPU time to record and submit) and `frame` (submit and wait). Numbers go in the PR; regressions follow the performance budgets spec.
- A source without filters, and a disabled filter, cost nothing per frame (binder test: no node in the plan).

## Conformance tests

`rbs_filters::conformance::check_type(&dyn FilterType)` (pure, no GPU) and `check_on_gpu(&dyn FilterType, &GpuContext)` (needs an adapter; software Vulkan in CI):

1. Schema invariants of "Filter types" above, and every pass compiled with the prelude through naga (`wgpu::naga`), without a GPU.
2. `Values::from_pairs` refuses unknown keys, wrong kinds, out-of-range and non-finite numbers, and fills defaults; `to_pairs` round-trips.
3. `uniforms` gives finite numbers for the defaults, every field at its `min` and at its `max`.
4. Output size equals input size; output is `Rgba16Float`, linear, timestamped like the input.
5. A fully transparent input stays fully transparent (no colour where alpha is 0).
6. The input is never published: on a size change the node publishes the filtered frame of the new size at once (the input's texture id never appears on its output), and counts the resize.
7. Allocations per frame at a steady size are bounded and do not grow (counting allocator, own test binary).
8. An sRGB-encoded input (`Rgba8UnormSrgb` content in an `Rgba8Unorm` texture tagged sRGB) is decoded; a partly transparent input keeps its alpha where the type does not change it.
9. Odd input sizes (7x3) give an output of that size (half-size intermediates round up).
10. A chain of two filters: both publish at the input's size, also after a size change, with no frame missing.
11. Memory: after a filter is disabled, `TexturePool::allocated` drops by its textures within a few ticks, also while another node acquires the same description every tick; during a 120-frame resize sweep (a new size every frame) under a chain of two filters, it stays within the steady set plus 4 textures and no frame fails.
12. An unchanged input (same frame, same values) is not filtered again and the output stays; new values filter it again.

Outside this crate: the scene model tests `set_filter_settings` (undo, `UnknownFilter`) and that `set_source_volume` / `set_source_muted` keep `sync_offset_ms`; the binder tests `changed_filters` and that a disabled-then-enabled filter keeps its settings and node; the user-data tests a format 3 document loading as format 4 and the offset round trip; `rbs-sources` tests `filter_values_lenient`; `rbs-api` tests the 1.8 methods, the `filter.set_settings` patch semantics (on the lenient values; a stored out-of-range key does not refuse a patch of another key; the full set is written), `invalid_params` for settings of an unknown type and that a 1.7 client sees `method_not_found` for them; `rbs-app` tests that a cancelled drag restores the model's values.

Per type (golden values read back in tests only): colour correction with defaults is the identity within 4/1000 and brightness +0.1 adds 0.1 to the sRGB-encoded value; an sRGB-encoded input is decoded; chroma key makes a pure key-coloured input transparent and keeps a far colour opaque; blur keeps a uniform colour and spreads a single bright pixel; sharpen with amount 0 is the identity.

## Open questions

- Fusing adjacent per-pixel filters into one pass (ADR-0053 decision 8): decided on numbers from `benches/filters.rs`.
- A before/after toggle in the Filters tab: needs the source's unfiltered picture in the preview, a second layer or a bypass flag; later.
