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

// Scaled conversion (ADR-0066 decision 4, kernels in ADR-0102). The canvas is fitted
// into the output with its aspect ratio kept; the rest of the output is black. The
// one-to-one entries above are unchanged and do not sample. This bilinear pair is the
// non-integer (and any other non-box, non-upscale) path. Integer downscale and upscale
// are the box and Catmull-Rom entries further down; a pipeline is built for one pair.
// The picture's rectangle in output pixels, set per pipeline.
override fit_x: f32 = 0.0;
override fit_y: f32 = 0.0;
override fit_w: f32 = 1.0;
override fit_h: f32 = 1.0;
// Integer downscale ratio N (>= 2). Set only on the box pipelines.
override box_n: i32 = 2;

fn sample_prime(p: vec2<f32>) -> vec3<f32> {
    let uv = (p - vec2<f32>(fit_x, fit_y)) / vec2<f32>(fit_w, fit_h);
    if (any(uv < vec2<f32>(0.0)) || any(uv > vec2<f32>(1.0))) {
        return vec3<f32>(0.0);
    }
    return oetf_709(textureSampleLevel(canvas, unused_sampler, uv, 0.0).rgb);
}

@fragment
fn fs_luma_scaled(@builtin(position) pos: vec4<f32>) -> @location(0) f32 {
    let p = sample_prime(pos.xy);
    return round(y_black + y_range * luma(p)) * code_scale;
}

@fragment
fn fs_chroma_scaled(@builtin(position) pos: vec4<f32>) -> @location(0) vec2<f32> {
    // The four luma pixel centres of this chroma sample's 2x2 block.
    let c = floor(pos.xy) * 2.0;
    let p = (sample_prime(c + vec2<f32>(0.5, 0.5)) + sample_prime(c + vec2<f32>(1.5, 0.5))
        + sample_prime(c + vec2<f32>(0.5, 1.5)) + sample_prime(c + vec2<f32>(1.5, 1.5))) * 0.25;
    let yp = luma(p);
    let cb = (p.b - yp) / 1.8556;
    let cr = (p.r - yp) / 1.5748;
    return round(vec2<f32>(c_mid) + c_range * vec2<f32>(cb, cr)) * code_scale;
}

// Integer downscale (ADR-0102): each output pixel is the equal-weight mean of its
// N×N canvas texels in linear light. N and the fit rectangle are pipeline constants.
// Outside the fitted picture the result is black, as on the bilinear path.
fn box_linear(ix: i32, iy: i32) -> vec3<f32> {
    let cx = f32(ix) + 0.5;
    let cy = f32(iy) + 0.5;
    let lx = i32(floor(cx - fit_x));
    let ly = i32(floor(cy - fit_y));
    let nx = i32(round(fit_w));
    let ny = i32(round(fit_h));
    if (lx < 0 || ly < 0 || lx >= nx || ly >= ny) {
        return vec3<f32>(0.0);
    }
    let dims = vec2<i32>(textureDimensions(canvas));
    let origin = vec2<i32>(lx, ly) * box_n;
    var sum = vec3<f32>(0.0);
    for (var j = 0; j < box_n; j++) {
        for (var i = 0; i < box_n; i++) {
            let s = clamp(origin + vec2<i32>(i, j), vec2<i32>(0), dims - vec2<i32>(1));
            sum += textureLoad(canvas, s, 0).rgb;
        }
    }
    return sum / (f32(box_n) * f32(box_n));
}

@fragment
fn fs_luma_box(@builtin(position) pos: vec4<f32>) -> @location(0) f32 {
    let p = oetf_709(box_linear(i32(pos.x), i32(pos.y)));
    return round(y_black + y_range * luma(p)) * code_scale;
}

@fragment
fn fs_chroma_box(@builtin(position) pos: vec4<f32>) -> @location(0) vec2<f32> {
    // The four luma pixels of this chroma sample, each already box-filtered.
    let x = i32(pos.x) * 2;
    let y = i32(pos.y) * 2;
    let p = (oetf_709(box_linear(x, y)) + oetf_709(box_linear(x + 1, y))
        + oetf_709(box_linear(x, y + 1)) + oetf_709(box_linear(x + 1, y + 1))) * 0.25;
    let yp = luma(p);
    let cb = (p.b - yp) / 1.8556;
    let cr = (p.r - yp) / 1.5748;
    return round(vec2<f32>(c_mid) + c_range * vec2<f32>(cb, cr)) * code_scale;
}

// Upscale (ADR-0102): Keys Catmull-Rom, a = -0.5, in linear light. Samples outside
// the canvas clamp to the edge texel. Outside the fitted picture the result is black.
fn keys(d: f32) -> f32 {
    let x = abs(d);
    let near = (1.5 * x - 2.5) * x * x + 1.0;
    let far = ((-0.5 * x + 2.5) * x - 4.0) * x + 2.0;
    let w = select(far, near, x <= 1.0);
    return select(0.0, w, x <= 2.0);
}

fn catmull_linear(p: vec2<f32>) -> vec3<f32> {
    let uv = (p - vec2<f32>(fit_x, fit_y)) / vec2<f32>(fit_w, fit_h);
    if (any(uv < vec2<f32>(0.0)) || any(uv > vec2<f32>(1.0))) {
        return vec3<f32>(0.0);
    }
    let dims = vec2<i32>(textureDimensions(canvas));
    let coord = uv * vec2<f32>(dims) - vec2<f32>(0.5);
    let base = floor(coord);
    let f = coord - base;
    var acc = vec3<f32>(0.0);
    for (var y = -1; y <= 2; y++) {
        let wy = keys(f.y - f32(y));
        for (var x = -1; x <= 2; x++) {
            let wx = keys(f.x - f32(x));
            let s = clamp(vec2<i32>(base) + vec2<i32>(x, y), vec2<i32>(0), dims - vec2<i32>(1));
            acc += textureLoad(canvas, s, 0).rgb * (wx * wy);
        }
    }
    return acc;
}

@fragment
fn fs_luma_catmull(@builtin(position) pos: vec4<f32>) -> @location(0) f32 {
    let p = oetf_709(catmull_linear(pos.xy));
    return round(y_black + y_range * luma(p)) * code_scale;
}

@fragment
fn fs_chroma_catmull(@builtin(position) pos: vec4<f32>) -> @location(0) vec2<f32> {
    let c = floor(pos.xy) * 2.0;
    let p = (oetf_709(catmull_linear(c + vec2<f32>(0.5, 0.5)))
        + oetf_709(catmull_linear(c + vec2<f32>(1.5, 0.5)))
        + oetf_709(catmull_linear(c + vec2<f32>(0.5, 1.5)))
        + oetf_709(catmull_linear(c + vec2<f32>(1.5, 1.5)))) * 0.25;
    let yp = luma(p);
    let cb = (p.b - yp) / 1.8556;
    let cr = (p.r - yp) / 1.5748;
    return round(vec2<f32>(c_mid) + c_range * vec2<f32>(cb, cr)) * code_scale;
}

// Open shadows, a grade on the Video path only. Linear light below middle grey
// (0.18) is raised by a power of 0.7 about that knee; black, middle grey and
// everything above are unchanged. The BT.709 OETF and limited-range codes then
// run as on the default path, so the file stays Rec. 709. These entry points are
// built only when the grade is on. The entries above do not call this.
const SHADOW_KNEE: f32 = 0.18;
const SHADOW_GAMMA: f32 = 0.7;

fn open_shadows(l: vec3<f32>) -> vec3<f32> {
    let c = clamp(l, vec3<f32>(0.0), vec3<f32>(1.0));
    let lifted = SHADOW_KNEE * pow(c / SHADOW_KNEE, vec3<f32>(SHADOW_GAMMA));
    // `select` evaluates both sides. Zero stays zero without a negative power.
    return select(c, lifted, (c > vec3<f32>(0.0)) & (c < vec3<f32>(SHADOW_KNEE)));
}

fn load_open(x: i32, y: i32) -> vec3<f32> {
    return oetf_709(open_shadows(textureLoad(canvas, vec2<i32>(x, y), 0).rgb));
}

@fragment
fn fs_luma_open(@builtin(position) pos: vec4<f32>) -> @location(0) f32 {
    let p = load_open(i32(pos.x), i32(pos.y));
    return round(y_black + y_range * luma(p)) * code_scale;
}

@fragment
fn fs_chroma_open(@builtin(position) pos: vec4<f32>) -> @location(0) vec2<f32> {
    let x = i32(pos.x) * 2;
    let y = i32(pos.y) * 2;
    let p = (load_open(x, y) + load_open(x + 1, y) + load_open(x, y + 1) + load_open(x + 1, y + 1))
        * 0.25;
    let yp = luma(p);
    let cb = (p.b - yp) / 1.8556;
    let cr = (p.r - yp) / 1.5748;
    return round(vec2<f32>(c_mid) + c_range * vec2<f32>(cb, cr)) * code_scale;
}

fn sample_open(p: vec2<f32>) -> vec3<f32> {
    let uv = (p - vec2<f32>(fit_x, fit_y)) / vec2<f32>(fit_w, fit_h);
    if (any(uv < vec2<f32>(0.0)) || any(uv > vec2<f32>(1.0))) {
        return vec3<f32>(0.0);
    }
    return oetf_709(open_shadows(textureSampleLevel(canvas, unused_sampler, uv, 0.0).rgb));
}

@fragment
fn fs_luma_open_scaled(@builtin(position) pos: vec4<f32>) -> @location(0) f32 {
    let p = sample_open(pos.xy);
    return round(y_black + y_range * luma(p)) * code_scale;
}

@fragment
fn fs_chroma_open_scaled(@builtin(position) pos: vec4<f32>) -> @location(0) vec2<f32> {
    let c = floor(pos.xy) * 2.0;
    let p = (sample_open(c + vec2<f32>(0.5, 0.5)) + sample_open(c + vec2<f32>(1.5, 0.5))
        + sample_open(c + vec2<f32>(0.5, 1.5)) + sample_open(c + vec2<f32>(1.5, 1.5)))
        * 0.25;
    let yp = luma(p);
    let cb = (p.b - yp) / 1.8556;
    let cr = (p.r - yp) / 1.5748;
    return round(vec2<f32>(c_mid) + c_range * vec2<f32>(cb, cr)) * code_scale;
}

@fragment
fn fs_luma_open_box(@builtin(position) pos: vec4<f32>) -> @location(0) f32 {
    let p = oetf_709(open_shadows(box_linear(i32(pos.x), i32(pos.y))));
    return round(y_black + y_range * luma(p)) * code_scale;
}

@fragment
fn fs_chroma_open_box(@builtin(position) pos: vec4<f32>) -> @location(0) vec2<f32> {
    let x = i32(pos.x) * 2;
    let y = i32(pos.y) * 2;
    let p = (oetf_709(open_shadows(box_linear(x, y)))
        + oetf_709(open_shadows(box_linear(x + 1, y)))
        + oetf_709(open_shadows(box_linear(x, y + 1)))
        + oetf_709(open_shadows(box_linear(x + 1, y + 1)))) * 0.25;
    let yp = luma(p);
    let cb = (p.b - yp) / 1.8556;
    let cr = (p.r - yp) / 1.5748;
    return round(vec2<f32>(c_mid) + c_range * vec2<f32>(cb, cr)) * code_scale;
}

@fragment
fn fs_luma_open_catmull(@builtin(position) pos: vec4<f32>) -> @location(0) f32 {
    let p = oetf_709(open_shadows(catmull_linear(pos.xy)));
    return round(y_black + y_range * luma(p)) * code_scale;
}

@fragment
fn fs_chroma_open_catmull(@builtin(position) pos: vec4<f32>) -> @location(0) vec2<f32> {
    let c = floor(pos.xy) * 2.0;
    let p = (oetf_709(open_shadows(catmull_linear(c + vec2<f32>(0.5, 0.5))))
        + oetf_709(open_shadows(catmull_linear(c + vec2<f32>(1.5, 0.5))))
        + oetf_709(open_shadows(catmull_linear(c + vec2<f32>(0.5, 1.5))))
        + oetf_709(open_shadows(catmull_linear(c + vec2<f32>(1.5, 1.5))))) * 0.25;
    let yp = luma(p);
    let cb = (p.b - yp) / 1.8556;
    let cr = (p.r - yp) / 1.5748;
    return round(vec2<f32>(c_mid) + c_range * vec2<f32>(cb, cr)) * code_scale;
}

// Screen delivery (ColorSpace::SRGB_VIDEO): sRGB OETF, full range. A separate entry point
// from the BT.709 limited path above, so that path does not branch on this mode.
// `code_max` is the largest legal code (255 or 1023). Full-range chroma can round one
// code past that end; it is clamped here and not on the limited-range path.
override code_max: f32 = 255.0;

fn oetf_srgb(l: vec3<f32>) -> vec3<f32> {
    let c = clamp(l, vec3<f32>(0.0), vec3<f32>(1.0));
    let lo = 12.92 * c;
    let hi = 1.055 * pow(c, vec3<f32>(1.0 / 2.4)) - vec3<f32>(0.055);
    return select(hi, lo, c <= vec3<f32>(0.0031308));
}

fn load_srgb(x: i32, y: i32) -> vec3<f32> {
    return oetf_srgb(textureLoad(canvas, vec2<i32>(x, y), 0).rgb);
}

fn store_y(p: vec3<f32>) -> f32 {
    return clamp(round(y_black + y_range * luma(p)), 0.0, code_max) * code_scale;
}

fn store_c(p: vec3<f32>) -> vec2<f32> {
    let yp = luma(p);
    let cb = (p.b - yp) / 1.8556;
    let cr = (p.r - yp) / 1.5748;
    return clamp(
        round(vec2<f32>(c_mid) + c_range * vec2<f32>(cb, cr)),
        vec2<f32>(0.0),
        vec2<f32>(code_max),
    ) * code_scale;
}

@fragment
fn fs_luma_srgb(@builtin(position) pos: vec4<f32>) -> @location(0) f32 {
    return store_y(load_srgb(i32(pos.x), i32(pos.y)));
}

@fragment
fn fs_chroma_srgb(@builtin(position) pos: vec4<f32>) -> @location(0) vec2<f32> {
    let x = i32(pos.x) * 2;
    let y = i32(pos.y) * 2;
    let p = (load_srgb(x, y) + load_srgb(x + 1, y) + load_srgb(x, y + 1) + load_srgb(x + 1, y + 1))
        * 0.25;
    return store_c(p);
}

fn sample_srgb(p: vec2<f32>) -> vec3<f32> {
    let uv = (p - vec2<f32>(fit_x, fit_y)) / vec2<f32>(fit_w, fit_h);
    if (any(uv < vec2<f32>(0.0)) || any(uv > vec2<f32>(1.0))) {
        return vec3<f32>(0.0);
    }
    return oetf_srgb(textureSampleLevel(canvas, unused_sampler, uv, 0.0).rgb);
}

@fragment
fn fs_luma_srgb_scaled(@builtin(position) pos: vec4<f32>) -> @location(0) f32 {
    return store_y(sample_srgb(pos.xy));
}

@fragment
fn fs_chroma_srgb_scaled(@builtin(position) pos: vec4<f32>) -> @location(0) vec2<f32> {
    let c = floor(pos.xy) * 2.0;
    let p = (sample_srgb(c + vec2<f32>(0.5, 0.5)) + sample_srgb(c + vec2<f32>(1.5, 0.5))
        + sample_srgb(c + vec2<f32>(0.5, 1.5)) + sample_srgb(c + vec2<f32>(1.5, 1.5)))
        * 0.25;
    return store_c(p);
}

@fragment
fn fs_luma_srgb_box(@builtin(position) pos: vec4<f32>) -> @location(0) f32 {
    return store_y(oetf_srgb(box_linear(i32(pos.x), i32(pos.y))));
}

@fragment
fn fs_chroma_srgb_box(@builtin(position) pos: vec4<f32>) -> @location(0) vec2<f32> {
    let x = i32(pos.x) * 2;
    let y = i32(pos.y) * 2;
    let p = (oetf_srgb(box_linear(x, y)) + oetf_srgb(box_linear(x + 1, y))
        + oetf_srgb(box_linear(x, y + 1)) + oetf_srgb(box_linear(x + 1, y + 1))) * 0.25;
    return store_c(p);
}

@fragment
fn fs_luma_srgb_catmull(@builtin(position) pos: vec4<f32>) -> @location(0) f32 {
    return store_y(oetf_srgb(catmull_linear(pos.xy)));
}

@fragment
fn fs_chroma_srgb_catmull(@builtin(position) pos: vec4<f32>) -> @location(0) vec2<f32> {
    let c = floor(pos.xy) * 2.0;
    let p = (oetf_srgb(catmull_linear(c + vec2<f32>(0.5, 0.5)))
        + oetf_srgb(catmull_linear(c + vec2<f32>(1.5, 0.5)))
        + oetf_srgb(catmull_linear(c + vec2<f32>(0.5, 1.5)))
        + oetf_srgb(catmull_linear(c + vec2<f32>(1.5, 1.5)))) * 0.25;
    return store_c(p);
}
