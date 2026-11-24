// Model input (org.rbs.filter.background-removal): the picture at the session's small
// size, sRGB-encoded, straight colour, opaque, BGRA in memory (a `Bgra8Unorm` target).
// u[0].xy: the small target's texel size in uv units. Four taps cover each target texel.
@fragment
fn fs_main(in: FilterIn) -> @location(0) vec4<f32> {
    let d = u[0].xy * 0.25;
    var sum = vec3<f32>(0.0);
    var weight = 0.0;
    for (var i = 0; i < 4; i = i + 1) {
        let sx = select(-1.0, 1.0, (i & 1) == 1);
        let sy = select(-1.0, 1.0, (i & 2) == 2);
        let c = sample_linear(in.uv + vec2<f32>(sx, sy) * d);
        sum = sum + c.rgb;
        weight = weight + c.a;
    }
    if (weight <= 1e-6) {
        return vec4<f32>(0.0, 0.0, 0.0, 1.0);
    }
    return vec4<f32>(clamp(linear_to_srgb(sum / weight), vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}
