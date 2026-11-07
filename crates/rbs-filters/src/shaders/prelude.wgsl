// Filter prelude (spec docs/specs/video-filters.md, "Shader prelude"). Prepended to
// every pass of every filter type. Output: linear light, premultiplied alpha.

struct FilterIn {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

// u[0]..u[14]: the filter type's; u[15]: xy source texel size, w 1.0 when the source
// is sRGB-encoded.
@group(0) @binding(0) var<uniform> u: array<vec4<f32>, 16>;
@group(1) @binding(0) var src: texture_2d<f32>;
@group(1) @binding(1) var src_sampler: sampler;

// One triangle that covers the target; uv 0..1 over it, v down.
@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> FilterIn {
    let x = f32((vi << 1u) & 2u);
    let y = f32(vi & 2u);
    var out: FilterIn;
    out.position = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    out.uv = vec2<f32>(x, y);
    return out;
}

fn srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    let lo = c / 12.92;
    let hi = pow((max(c, vec3<f32>(0.0)) + vec3<f32>(0.055)) / 1.055, vec3<f32>(2.4));
    return select(hi, lo, c <= vec3<f32>(0.04045));
}

fn linear_to_srgb(c: vec3<f32>) -> vec3<f32> {
    let lo = c * 12.92;
    let hi = 1.055 * pow(max(c, vec3<f32>(0.0)), vec3<f32>(1.0 / 2.4)) - vec3<f32>(0.055);
    return select(hi, lo, c <= vec3<f32>(0.0031308));
}

// The source at `uv`, linear light, premultiplied.
fn sample_linear(uv: vec2<f32>) -> vec4<f32> {
    var c = textureSampleLevel(src, src_sampler, uv, 0.0);
    if (u[15].w > 0.5) {
        let a = max(c.a, 1e-6);
        c = vec4<f32>(srgb_to_linear(c.rgb / a) * c.a, c.a);
    }
    return c;
}

// Straight (not premultiplied) colour of a premultiplied sample; black where clear.
fn unpremultiply(c: vec4<f32>) -> vec3<f32> {
    if (c.a <= 1e-6) {
        return vec3<f32>(0.0);
    }
    return c.rgb / c.a;
}

// BT.709 luma and chroma of an sRGB-encoded colour.
fn to_ycc(e: vec3<f32>) -> vec3<f32> {
    let y = dot(e, vec3<f32>(0.2126, 0.7152, 0.0722));
    return vec3<f32>(y, (e.b - y) / 1.8556, (e.r - y) / 1.5748);
}

fn from_ycc(c: vec3<f32>) -> vec3<f32> {
    let r = c.x + 1.5748 * c.z;
    let b = c.x + 1.8556 * c.y;
    let g = (c.x - 0.2126 * r - 0.0722 * b) / 0.7152;
    return vec3<f32>(r, g, b);
}
