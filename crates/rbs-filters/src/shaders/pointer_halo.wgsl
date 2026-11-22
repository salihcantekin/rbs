// Pointer halo (org.rbs.filter.pointer-halo). u[0]: x halo radius, y dimming outside
// the spotlight (0 = off), z spotlight radius, w 1 for the laser look; u[1]: halo
// colour, linear rgb and alpha. The laser is a red dot (a sixth of the radius, at
// least 2 px) in a soft glow three times its size; it ignores the colour.
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
    if (u[0].w > 0.5) {
        let core = max(u[0].x / 6.0, 2.0);
        let disc = 1.0 - smoothstep(core - 1.0, core + 1.0, d);
        let glow = (1.0 - smoothstep(core, core * 3.0, d)) * 0.45;
        // Linear red with a white-hot centre.
        let red = vec3<f32>(1.0, 0.02, 0.02);
        let hot = mix(red, vec3<f32>(1.0, 0.75, 0.75), 1.0 - smoothstep(0.0, core, d));
        let a = max(disc, glow);
        let rgb = mix(red, hot, disc);
        return c * (1.0 - a) + vec4<f32>(rgb * a, a);
    }
    let a = (1.0 - smoothstep(u[0].x - 1.0, u[0].x + 1.0, d)) * u[1].w;
    return c * (1.0 - a) + vec4<f32>(u[1].rgb * a, a);
}
