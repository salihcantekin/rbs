// Pointer halo (org.rbs.filter.pointer-halo). u[0]: x halo radius, y dimming outside
// the spotlight (0 = off), z spotlight radius; u[1]: halo colour, linear rgb and alpha.
// Effect 0, half 0: the pointer (x, y) in picture pixels.
@fragment
fn fs_main(in: FilterIn) -> @location(0) vec4<f32> {
    var c = sample_linear(in.uv);
    if (effect_count() == 0u) {
        return c;
    }
    let d = distance(in.position.xy, effect(0u, 0u).xy);
    if (u[0].y > 0.0) {
        let lit = 1.0 - smoothstep(u[0].z - 2.0, u[0].z + 2.0, d);
        c = vec4<f32>(c.rgb * mix(1.0 - u[0].y, 1.0, lit), c.a);
    }
    let a = (1.0 - smoothstep(u[0].x - 1.0, u[0].x + 1.0, d)) * u[1].w;
    return c * (1.0 - a) + vec4<f32>(u[1].rgb * a, a);
}
