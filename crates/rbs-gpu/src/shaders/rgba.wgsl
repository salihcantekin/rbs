// Picture (linear Rgba16Float, premultiplied) to straight-alpha sRGB RGBA8 for an image
// file (ADR-0117 decision 5). Pixel for pixel: textureLoad, no scaler.
// Spec: docs/specs/gpu-compositor.md, "Image download (RGBA8)".

@group(0) @binding(0) var picture: texture_2d<f32>;
@group(0) @binding(1) var unused_sampler: sampler;

@vertex
fn vs_fullscreen(@builtin(vertex_index) vi: u32) -> @builtin(position) vec4<f32> {
    // One triangle covering the viewport.
    let x = f32((vi << 1u) & 2u);
    let y = f32(vi & 2u);
    return vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
}

fn encode_srgb(l: vec3<f32>) -> vec3<f32> {
    let c = clamp(l, vec3<f32>(0.0), vec3<f32>(1.0));
    let lo = 12.92 * c;
    let hi = 1.055 * pow(c, vec3<f32>(1.0 / 2.4)) - vec3<f32>(0.055);
    return select(hi, lo, c <= vec3<f32>(0.0031308));
}

@fragment
fn fs_rgba(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let p = textureLoad(picture, vec2<i32>(pos.xy), 0);
    let a = clamp(p.a, 0.0, 1.0);
    // Un-premultiply; a fully transparent pixel keeps no colour.
    let rgb = select(vec3<f32>(0.0), p.rgb / max(a, 1e-6), a > 0.0);
    // The target is Rgba8Unorm: these are the stored sRGB codes, rounded to nearest.
    return vec4<f32>(encode_srgb(rgb), a);
}
