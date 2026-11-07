// Chroma key (org.rbs.filter.chroma-key).
// u[0]: key Cb, key Cr, similarity, smoothness. u[1]: spill, key chroma direction xy.
@fragment
fn fs_main(in: FilterIn) -> @location(0) vec4<f32> {
    let c = sample_linear(in.uv);
    if (c.a <= 1e-6) {
        return vec4<f32>(0.0);
    }
    let e = linear_to_srgb(unpremultiply(c));
    var ycc = to_ycc(e);
    let d = distance(ycc.yz, u[0].xy);
    let keep = smoothstep(u[0].z, u[0].z + max(u[0].w, 1e-4), d);
    // Spill: take away the chroma that points towards the key.
    let dir = u[1].yz;
    let along = max(dot(ycc.yz, dir), 0.0);
    ycc = vec3<f32>(ycc.x, ycc.yz - dir * along * u[1].x);
    let out = clamp(from_ycc(ycc), vec3<f32>(0.0), vec3<f32>(1.0));
    let a = c.a * keep;
    return vec4<f32>(srgb_to_linear(out) * a, a);
}
