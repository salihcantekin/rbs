// Canvas (linear Rgba16Float, premultiplied) to the preview surface (Rgba8UnormSrgb).
// Scaled bilinearly to the target size and composited over opaque black; the render
// target applies the sRGB encoding. Spec: docs/specs/gpu-compositor.md, "Preview export".

@group(0) @binding(0) var canvas: texture_2d<f32>;
@group(0) @binding(1) var linear_clamp: sampler;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn vs_fullscreen(@builtin(vertex_index) vi: u32) -> VsOut {
    // One triangle covering the viewport; uv is 0..1 over the visible part.
    let x = f32((vi << 1u) & 2u);
    let y = f32(vi & 2u);
    var out: VsOut;
    out.pos = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    out.uv = vec2<f32>(x, y);
    return out;
}

@fragment
fn fs_preview(in: VsOut) -> @location(0) vec4<f32> {
    let c = textureSample(canvas, linear_clamp, in.uv);
    // Premultiplied over black: the colour channels are already the result.
    return vec4<f32>(clamp(c.rgb, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}

// For targets without an sRGB format (the Windows preview surfaces, `R8G8B8A8_UNORM`,
// ADR-0035): the same result with the sRGB encoding applied here.
fn srgb_encode(c: vec3<f32>) -> vec3<f32> {
    let lo = c * 12.92;
    let hi = 1.055 * pow(c, vec3<f32>(1.0 / 2.4)) - 0.055;
    return select(hi, lo, c <= vec3<f32>(0.0031308));
}

@fragment
fn fs_preview_encode(in: VsOut) -> @location(0) vec4<f32> {
    let c = textureSample(canvas, linear_clamp, in.uv);
    return vec4<f32>(srgb_encode(clamp(c.rgb, vec3<f32>(0.0), vec3<f32>(1.0))), 1.0);
}
