// Background removal, blurred background, step 1: the picture at 1/8 size, each texel the
// average of its 8x8 source pixels (16 bilinear taps), each source pixel weighted by how
// much of it is background so the person's colours do not bleed into the room.
// u[0]: low edge, high edge (matte values). u[15]: source texel size, transfer code.
// Output: (colour * weight, weight).
@group(2) @binding(0) var matte: texture_2d<f32>;
@group(2) @binding(1) var matte_sampler: sampler;

@fragment
fn fs_main(in: FilterIn) -> @location(0) vec4<f32> {
    var sum = vec4<f32>(0.0);
    for (var j = 0; j < 4; j = j + 1) {
        for (var i = 0; i < 4; i = i + 1) {
            let p = in.uv + vec2<f32>(f32(i) * 2.0 - 3.0, f32(j) * 2.0 - 3.0) * u[15].xy;
            let s = sample_linear(p);
            let c = s.rgb / max(s.a, 1e-4);
            let m = textureSampleLevel(matte, matte_sampler, p, 0.0).r;
            let w = 1.0 - smoothstep(u[0].x, u[0].y, m);
            sum = sum + vec4<f32>(c * w, w);
        }
    }
    return sum / 16.0;
}
