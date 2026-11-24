// Background removal (org.rbs.filter.background-removal): the picture times the refined
// matte. u[0]: low edge, high edge (matte values), 0, 0. Group 2 is the matte, one
// channel, any size, sampled bilinearly. Output: linear light, premultiplied.
@group(2) @binding(0) var matte: texture_2d<f32>;
@group(2) @binding(1) var matte_sampler: sampler;

@fragment
fn fs_main(in: FilterIn) -> @location(0) vec4<f32> {
    let c = sample_linear(in.uv);
    let m = textureSampleLevel(matte, matte_sampler, in.uv, 0.0).r;
    let a = smoothstep(u[0].x, u[0].y, m);
    return c * a;
}
