// A change between two slide show pictures (ADR-0171 decision 7). Both pictures and
// the target are the node's output size, Rgba8Unorm holding sRGB codes with straight
// alpha. Pixel for pixel: textureLoad, no scaler.
// Spec: docs/specs/gpu-compositor.md, "Slide show change".

@group(0) @binding(0) var from_pic: texture_2d<f32>;
@group(0) @binding(1) var from_unused: sampler;
@group(1) @binding(0) var to_pic: texture_2d<f32>;
@group(1) @binding(1) var to_unused: sampler;

struct Params {
    // 0 fade, 1 slide (the incoming picture pushes the outgoing one), 2 swipe (the
    // incoming picture moves in over the outgoing one).
    mode: u32,
    // Progress, 0 to 1.
    t: f32,
    width: f32,
    _pad: f32,
};
@group(2) @binding(0) var<uniform> params: Params;

@vertex
fn vs_fullscreen(@builtin(vertex_index) vi: u32) -> @builtin(position) vec4<f32> {
    let x = f32((vi << 1u) & 2u);
    let y = f32(vi & 2u);
    return vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
}

fn decode_srgb(c: vec3<f32>) -> vec3<f32> {
    let lo = c / 12.92;
    let hi = pow((c + vec3<f32>(0.055)) / 1.055, vec3<f32>(2.4));
    return select(hi, lo, c <= vec3<f32>(0.04045));
}

fn encode_srgb(l: vec3<f32>) -> vec3<f32> {
    let c = clamp(l, vec3<f32>(0.0), vec3<f32>(1.0));
    let lo = 12.92 * c;
    let hi = 1.055 * pow(c, vec3<f32>(1.0 / 2.4)) - vec3<f32>(0.055);
    return select(hi, lo, c <= vec3<f32>(0.0031308));
}

// Linear light, premultiplied; transparent outside the picture.
fn load(pic: texture_2d<f32>, x: i32, y: i32) -> vec4<f32> {
    let size = vec2<i32>(textureDimensions(pic));
    if x < 0 || x >= size.x || y < 0 || y >= size.y {
        return vec4<f32>(0.0);
    }
    let p = textureLoad(pic, vec2<i32>(x, y), 0);
    return vec4<f32>(decode_srgb(p.rgb) * p.a, p.a);
}

// `top` over `bottom`, premultiplied.
fn over(top: vec4<f32>, bottom: vec4<f32>) -> vec4<f32> {
    return top + bottom * (1.0 - top.a);
}

@fragment
fn fs_slide(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let x = i32(floor(pos.x));
    let y = i32(floor(pos.y));
    let t = clamp(params.t, 0.0, 1.0);
    // The incoming picture's left edge, in pixels from the left.
    let edge = i32(round((1.0 - t) * params.width));
    var c: vec4<f32>;
    if params.mode == 0u {
        c = mix(load(from_pic, x, y), load(to_pic, x, y), t);
    } else if params.mode == 1u {
        let w = i32(round(params.width));
        c = over(load(to_pic, x - edge, y), load(from_pic, x - edge + w, y));
    } else {
        c = over(load(to_pic, x - edge, y), load(from_pic, x, y));
    }
    let a = clamp(c.a, 0.0, 1.0);
    let rgb = select(vec3<f32>(0.0), c.rgb / max(a, 1e-6), a > 0.0);
    return vec4<f32>(encode_srgb(rgb), a);
}
