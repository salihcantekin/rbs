# Spec: Video filters

- Status: Draft
- Owner crate(s): `rbs-filters` (filter type trait, filter node, catalogue, conformance suite); `rbs-sources` (scene settings to filter values, node types); `rbs-record` (filter instances in the studio)
- Related ADRs: ADR-0053 (decisions 6 to 9), ADR-0061, ADR-0013, ADR-0020, ADR-0023, ADR-0024

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
pub enum FieldKind {
    /// A number in `min..=max`; `step` is the UI's increment, `unit` is shown after the value.
    Number { min: f32, max: f32, default: f32, step: f32, unit: &'static str },
    /// An sRGB colour with straight alpha.
    Colour { default: [u8; 4] },
}
pub enum Value { Number(f32), Colour([u8; 4]) }
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

### Values (`rbs_filters::Values`)

`Values` holds one `Value` per field, in field order, always valid for its type. `Values::defaults(ty)` holds every field's default. `Values::from_pairs(ty, pairs: impl IntoIterator<Item = (&str, Value)>) -> Result<Values, FilterError>` starts from the defaults and replaces the given keys; `Values::with(&self, ty, pairs)` does the same from existing values (a patch). Refusals (`FilterError`): `UnknownKey(key)`, `WrongKind(key)` (a colour for a number or the reverse), `OutOfRange(key)` (outside `min..=max`, or not finite). Values never hold a value the type refused, so `process` never validates.

`Values::to_pairs(ty)` gives `(key, Value)` pairs for saving. Colours are written as `#rrggbb` when opaque, else `#rrggbbaa`.

In the scene model a filter's `Settings` map holds numbers as `SettingValue::Float` (an `Int` is read as a number) and colours as `SettingValue::Text` in `#rrggbb` or `#rrggbbaa`. `rbs_sources::filter_values(ty, &Settings) -> Result<Values, FilterError>` converts (any other text or a `Bool` is `WrongKind`) and `rbs_sources::filter_settings(ty, &Values) -> Settings` converts back. Missing keys take their defaults, so a document saved by a build whose type had fewer fields still loads.

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
pub const INPUT_RESIZED: &str = "filter input size changed";
pub struct FilterStats { /* atomics */ }
impl FilterStats {
    pub fn warm_size(&self) -> Option<[u32; 2]>;  // the size the node is warmed for
    pub fn resized(&self) -> bool;                 // waiting for a re-warm at a new size
    pub fn frames(&self) -> u64;                   // frames published
}
```

- `prepare`: one render pipeline per pass (target format `CANVAS_FORMAT`, no blending), one uniform buffer per pass (256 bytes) and its bind group (group 0). Group 1 is the context's sampled-texture layout (`GpuContext::sampled_layout`), so pool textures bind with their own `PooledTexture::sampled_bind_group`.
- `warm`: when a size is known (`FilterStats::warm_size`, set from `new` or from a resize), acquires and releases three `TextureDesc::canvas(w, h)` outputs and one intermediate per non-final pass at its scale, so `process` reuses them from the pool's free list. Without a known size it acquires nothing. Clears `resized`.
- `process` (hot path):
  1. `values.update()`; no input frame, or an input whose texture is gone or not sampleable: output nothing, `Ok`.
  2. Input size differs from the warmed size: output nothing, store the new size and set `resized`, return `Err(NodeError::Backend(INPUT_RESIZED))`. Never publishes the input or a texture of another size.
  3. Values or size or decode flag changed since the last write: `uniforms` into a fixed array, then one `write_buffer` per pass.
  4. Acquire the output (and intermediates), record each pass (one draw of three vertices), submit, publish the output with the input's timestamp, `PixelFormat::Rgba16Float`, `ColorSpace::LINEAR_BT709`, release the textures to the pool (the frame's lease keeps the output alive, as the compositor's canvas).
  It does not allocate, block, lock or read back. `release` drops the pipelines and buffers.
- The executor bypasses the node after its failure limit; the controller releases, prepares and warms it after the back-off (ADR-0024), at the stored size. A composition root may call `Engine::retry` on a node whose `FilterStats::resized` is true to skip the back-off, and does not report such a restart as a failure.

### Catalogue (`rbs_filters::catalogue() -> &'static [&'static dyn FilterType]`)

| Type id | Name | Fields (key: range, default) | Passes |
| --- | --- | --- | --- |
| `org.rbs.filter.color-correction` | Colour correction | `brightness` −1..1, 0; `contrast` −1..1, 0; `saturation` 0..3, 1; `gamma` 0.1..4, 1; `hue` −180..180 °, 0 | 1 |
| `org.rbs.filter.chroma-key` | Chroma key | `key_color` colour, `#00ff00`; `similarity` 0..1, 0.4; `smoothness` 0..1, 0.08; `spill` 0..1, 0.1 | 1 |
| `org.rbs.filter.blur` | Blur | `radius` 0..64 px, 8 | 2: horizontal at half size, vertical at full size |
| `org.rbs.filter.sharpen` | Sharpen | `amount` 0..2, 0.3 | 1 |

Maths, on un-premultiplied colour `c` (alpha `a`), premultiplied again at the end:

- **Colour correction**, in sRGB-encoded values `e = linear_to_srgb(c)`: `e = (e - 0.5) * (1 + contrast) + 0.5 + brightness` (contrast below 0 flattens, at −1 grey), saturation mixes with the Rec. 709 luma (`luma + (e - luma) * saturation`), hue rotates around the grey axis by `hue` degrees, gamma: `e = pow(max(e, 0), 1 / gamma)`; clamped to 0..1 and decoded. The defaults give the input back (within 1/1000 per channel).
- **Chroma key**, in sRGB-encoded values: distance `d` between the pixel's and the key's chroma (BT.709 Cb, Cr). Alpha is multiplied by `smoothstep(similarity, similarity + smoothness, d)` (`smoothness` 0: a hard edge at `similarity`). Spill: the key's dominant channel is limited towards the average of the other two by `spill` times how close the pixel is to the key. A pixel of exactly the key colour becomes transparent at `similarity > 0`.
- **Blur**: Gaussian with `sigma = radius / 2` source pixels. Pass 1 samples the input at half-size texel centres along x with up to 32 taps on each side at half-size spacing; pass 2 samples pass 1 along y the same way and writes full size. `radius` 0 copies the input (one tap). Weights are normalised to 1; premultiplied colours are blurred, so transparent edges do not darken.
- **Sharpen**: `c + amount * (4c - n - s - e - w)` on the four neighbours one source pixel away, clamped to 0..1 per channel and to `a`.

### Binding and the studio

- `rbs_sources::SourceTypes::node_types` installs `descriptor(ty)` for every catalogue type; the binder then binds filters as the scene model spec says.
- `SceneCollection::set_filter_settings(source, filter, settings) -> Result<Undo, SceneError>` replaces a filter's settings (one undo step; `UnknownSource`, `UnknownFilter`, `InvalidSetting` for a non-finite float). The model stores what it is given; callers validate with `filter_values` first.
- The binder keeps each bound filter's settings and reports `BindReport::changed_filters: Vec<(FilterId, NodeId)>` for a kept node whose filter's settings changed.
- `rbs_record::Studio` makes a `FilterNode` (render partition) for every scheduled filter node without an instance, with the source's picture size when it knows it; keeps its `ParamSender<Values>`; sends `filter_values` on `changed_filters`; takes instances back when retired; and `Studio::preview_filter(filter, values) -> bool` sends values to a running filter without changing the collection (slider drags). On `service`, a filter node that failed with `resized` set is retried at once (`Engine::retry`) and left out of the failures it reports.

### Where the user edits filters

The Filters tab of the Properties panel (ADR-0061 decision 9; `ui-design-system.md`, Properties). The control API's `filter.*` methods (`control-api.md`, protocol 1.7).

## Errors and failure behaviour

- Invalid values: refused with `FilterError` before any model edit; the app says so in plain words in the error bar, the API answers `invalid_params`.
- Unknown filter type in a collection: binder problem `UnknownFilterType`; the source is drawn without that filter; the app lists the problem.
- `prepare` failing (shader rejected by the driver): the node stays out of the schedule and is restarted with back-off; the layer is not drawn (P7) rather than drawn unfiltered.
- Input resized: the layer is not drawn until the node is warmed at the new size (one control-loop round with the root's retry).

## Performance requirements

- No allocation in `process` after warm, at a steady size (allocation-counting test, as `rbs-gpu`'s `alloc_per_frame`).
- `benches/filters.rs` in `rbs-filters`: each catalogue type at 1920x1080 on `Rgba16Float` input, `record_submit` (CPU time to record and submit) and `frame` (submit and wait). Numbers go in the PR; regressions follow the performance budgets spec.
- A source without filters, and a disabled filter, cost nothing per frame (binder test: no node in the plan).

## Conformance tests

`rbs_filters::conformance::check_type(&dyn FilterType)` (pure, no GPU) and `check_on_gpu(&dyn FilterType, &GpuContext)` (needs an adapter; software Vulkan in CI):

1. Schema invariants of "Filter types" above.
2. `Values::from_pairs` refuses unknown keys, wrong kinds, out-of-range and non-finite numbers, and fills defaults; `to_pairs` round-trips.
3. `uniforms` gives finite numbers for the defaults, every field at its `min` and at its `max`.
4. Output size equals input size; output is `Rgba16Float`, linear, timestamped like the input.
5. A fully transparent input stays fully transparent (no colour where alpha is 0).
6. The input is never published: on a size change the node publishes nothing and reports `INPUT_RESIZED`; after a re-warm it publishes the new size.
7. No allocation in `process` after warm (counting allocator, own test binary).

Per type (golden values read back in tests only): colour correction with defaults is the identity within 1/1000 and brightness +0.5 lightens; chroma key makes a pure key-coloured input transparent and keeps a far colour opaque; blur of radius 0 is the identity and of a single bright pixel spreads it and keeps the sum within 2 %; sharpen with amount 0 is the identity.

## Open questions

- Fusing adjacent per-pixel filters into one pass (ADR-0053 decision 8): decided on numbers from `benches/filters.rs`.
- A before/after toggle in the Filters tab: needs the source's unfiltered picture in the preview, a second layer or a bypass flag; later.
