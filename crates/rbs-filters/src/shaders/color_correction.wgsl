// Colour correction (org.rbs.filter.color-correction).
// u[0]: brightness, contrast, saturation, 1 / gamma. u[1]: cos(hue), sin(hue).
@fragment
fn fs_main(in: FilterIn) -> @location(0) vec4<f32> {
    let c = sample_linear(in.uv);
    if (c.a <= 1e-6) {
        return vec4<f32>(0.0);
    }
    var e = linear_to_srgb(unpremultiply(c));
    e = (e - vec3<f32>(0.5)) * (1.0 + u[0].y) + vec3<f32>(0.5) + vec3<f32>(u[0].x);
    var ycc = to_ycc(e);
    let ch = vec2<f32>(ycc.y * u[1].x - ycc.z * u[1].y, ycc.y * u[1].y + ycc.z * u[1].x);
    ycc = vec3<f32>(ycc.x, ch * u[0].z);
    e = clamp(from_ycc(ycc), vec3<f32>(0.0), vec3<f32>(1.0));
    e = pow(e, vec3<f32>(u[0].w));
    return vec4<f32>(srgb_to_linear(e) * c.a, c.a);
}
