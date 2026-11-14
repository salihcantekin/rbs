// Normalise (ADR-0086 decision 9): the source's picture as the canvas format, linear
// light and premultiplied, the same decode that puts it on the canvas. No settings.
@fragment
fn fs_main(in: FilterIn) -> @location(0) vec4<f32> {
    return sample_linear(in.uv);
}
