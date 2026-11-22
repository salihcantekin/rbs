// Click rings (org.rbs.filter.click-rings). u[0].x: ring line width in pixels.
// Effect i: half 0 = (x, y, radius, alpha) in picture pixels; half 1 = linear rgb.
@fragment
fn fs_main(in: FilterIn) -> @location(0) vec4<f32> {
    var c = sample_linear(in.uv);
    let half_w = max(u[0].x * 0.5, 0.5);
    let px = in.position.xy;
    for (var i = 0u; i < effect_count(); i++) {
        let at = effect(i, 0u);
        let rgb = effect(i, 1u).rgb;
        let d = abs(distance(px, at.xy) - at.z);
        let a = (1.0 - smoothstep(half_w - 0.75, half_w + 0.75, d)) * at.w;
        c = c * (1.0 - a) + vec4<f32>(rgb * a, a);
    }
    return c;
}
