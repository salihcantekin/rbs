// NV12 or P010 planes (Y' and interleaved CbCr) to linear Rgba16Float, for source import.
// Spec: docs/specs/gpu-compositor.md, "YUV source decode".

struct Params {
    // R', G', B' = dot(row, vec4(luma, cb, cr, 1)) on the stored normalized values. Folds
    // in the code scale, the range and the matrix (computed on the CPU per colour space).
    rows: array<vec4<f32>, 3>,
    // x: 0 = linear, 1 = BT.709 inverse OETF, 2 = sRGB EOTF.
    transfer: vec4<u32>,
};

@group(0) @binding(0) var luma: texture_2d<f32>;
@group(0) @binding(1) var chroma: texture_2d<f32>;
@group(0) @binding(2) var chroma_sampler: sampler;
@group(0) @binding(3) var<uniform> params: Params;

@vertex
fn vs_fullscreen(@builtin(vertex_index) vi: u32) -> @builtin(position) vec4<f32> {
    let x = f32((vi << 1u) & 2u);
    let y = f32(vi & 2u);
    return vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
}

fn inverse_oetf_709(v: vec3<f32>) -> vec3<f32> {
    let lo = v / 4.5;
    let hi = pow((v + vec3<f32>(0.099)) / 1.099, vec3<f32>(1.0 / 0.45));
    return select(hi, lo, v < vec3<f32>(0.081));
}

fn srgb_eotf(v: vec3<f32>) -> vec3<f32> {
    let lo = v / 12.92;
    let hi = pow((v + vec3<f32>(0.055)) / 1.055, vec3<f32>(2.4));
    return select(hi, lo, v <= vec3<f32>(0.04045));
}

@fragment
fn fs_decode(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let size = vec2<f32>(textureDimensions(luma));
    let y = textureLoad(luma, vec2<i32>(pos.xy), 0).r;
    // Bilinear chroma at the pixel centre (centre siting, as the encoder writes it).
    let c = textureSampleLevel(chroma, chroma_sampler, pos.xy / size, 0.0).rg;
    let v = vec4<f32>(y, c, 1.0);
    let prime = clamp(
        vec3<f32>(dot(params.rows[0], v), dot(params.rows[1], v), dot(params.rows[2], v)),
        vec3<f32>(0.0),
        vec3<f32>(1.0),
    );
    // Uniform control flow: the same branch for every pixel of the draw.
    var lin = prime;
    if params.transfer.x == 1u {
        lin = inverse_oetf_709(prime);
    } else if params.transfer.x == 2u {
        lin = srgb_eotf(prime);
    }
    return vec4<f32>(lin, 1.0);
}
