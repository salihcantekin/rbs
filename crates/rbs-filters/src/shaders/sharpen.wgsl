// Sharpen (org.rbs.filter.sharpen). u[0].x: amount.
@fragment
fn fs_main(in: FilterIn) -> @location(0) vec4<f32> {
    let t = u[15].xy;
    let c = sample_linear(in.uv);
    let n = sample_linear(in.uv + vec2<f32>(0.0, -t.y));
    let s = sample_linear(in.uv + vec2<f32>(0.0, t.y));
    let w = sample_linear(in.uv + vec2<f32>(-t.x, 0.0));
    let e = sample_linear(in.uv + vec2<f32>(t.x, 0.0));
    let sharp = c.rgb + u[0].x * (4.0 * c.rgb - n.rgb - s.rgb - w.rgb - e.rgb);
    return vec4<f32>(clamp(sharp, vec3<f32>(0.0), vec3<f32>(c.a)), c.a);
}
