// Background removal, blurred background, step 2: a Gaussian along x over the 1/8 picture
// (17 taps spread over u[2].y, the radius in uv; sigma is half the radius).
@fragment
fn fs_main(in: FilterIn) -> @location(0) vec4<f32> {
    var sum = vec4<f32>(0.0);
    var norm = 0.0;
    for (var i = 0; i < 17; i = i + 1) {
        let t = (f32(i) - 8.0) / 8.0;
        let g = exp(-2.0 * t * t);
        sum = sum + sample_linear(in.uv + vec2<f32>(t * u[2].y, 0.0)) * g;
        norm = norm + g;
    }
    return sum / norm;
}
