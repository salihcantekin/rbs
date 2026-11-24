// Background removal (org.rbs.filter.background-removal), Exact mode: the picture copied
// into a texture the node owns, as linear light, premultiplied. u[15]: texel size, 0,
// transfer code (the prelude's `sample_linear` decodes it).
@fragment
fn fs_main(in: FilterIn) -> @location(0) vec4<f32> {
    return sample_linear(in.uv);
}
