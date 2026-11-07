// Blur (org.rbs.filter.blur): one direction of a separable Gaussian.
// u[0..8]: weights 0..31 (four per vector); u[8]: taps on each side, step in uv (x, y).
fn weight(i: i32) -> f32 {
    let k = u32(abs(i));
    return u[k / 4u][k % 4u];
}

@fragment
fn fs_main(in: FilterIn) -> @location(0) vec4<f32> {
    let n = i32(u[8].x);
    let step = u[8].yz;
    var sum = vec4<f32>(0.0);
    for (var i = -n; i <= n; i = i + 1) {
        sum = sum + sample_linear(in.uv + step * f32(i)) * weight(i);
    }
    return sum;
}
