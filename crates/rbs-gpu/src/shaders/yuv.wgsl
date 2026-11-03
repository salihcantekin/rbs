// Canvas (linear Rgba16Float, premultiplied, composited over black) to NV12 or P010 planes.
// BT.709 OETF and matrix, limited range, chroma averaged over each 2x2 block.
// Spec: docs/specs/gpu-compositor.md, "Encoder conversion (NV12, P010)".

// Code levels of the output format, set per pipeline (defaults: 8-bit NV12).
// Limited-range luma is `y_black + y_range * Y'`, chroma `c_mid + c_range * C`.
override y_black: f32 = 16.0;
override y_range: f32 = 219.0;
override c_mid: f32 = 128.0;
override c_range: f32 = 224.0;
// Maps a whole code to the normalized value the render target stores:
// 1/255 for 8-bit; 64/65535 for P010 (10-bit code in the high bits of a 16-bit word).
override code_scale: f32 = 0.00392156862745098;

@group(0) @binding(0) var canvas: texture_2d<f32>;
@group(0) @binding(1) var unused_sampler: sampler;

@vertex
fn vs_fullscreen(@builtin(vertex_index) vi: u32) -> @builtin(position) vec4<f32> {
    // One triangle covering the viewport.
    let x = f32((vi << 1u) & 2u);
    let y = f32(vi & 2u);
    return vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
}

fn oetf_709(l: vec3<f32>) -> vec3<f32> {
    let c = clamp(l, vec3<f32>(0.0), vec3<f32>(1.0));
    let lo = 4.5 * c;
    let hi = 1.099 * pow(c, vec3<f32>(0.45)) - vec3<f32>(0.099);
    return select(hi, lo, c < vec3<f32>(0.018));
}

fn luma(p: vec3<f32>) -> f32 {
    return dot(p, vec3<f32>(0.2126, 0.7152, 0.0722));
}

fn load_prime(x: i32, y: i32) -> vec3<f32> {
    return oetf_709(textureLoad(canvas, vec2<i32>(x, y), 0).rgb);
}

@fragment
fn fs_luma(@builtin(position) pos: vec4<f32>) -> @location(0) f32 {
    let p = load_prime(i32(pos.x), i32(pos.y));
    // Rounded to a whole code first, so P010's low six bits stay zero.
    return round(y_black + y_range * luma(p)) * code_scale;
}

@fragment
fn fs_chroma(@builtin(position) pos: vec4<f32>) -> @location(0) vec2<f32> {
    let x = i32(pos.x) * 2;
    let y = i32(pos.y) * 2;
    let p = (load_prime(x, y) + load_prime(x + 1, y) + load_prime(x, y + 1) + load_prime(x + 1, y + 1)) * 0.25;
    let yp = luma(p);
    let cb = (p.b - yp) / 1.8556;
    let cr = (p.r - yp) / 1.5748;
    return round(vec2<f32>(c_mid) + c_range * vec2<f32>(cb, cr)) * code_scale;
}
