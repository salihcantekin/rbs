// Canvas (linear Rgba16Float, premultiplied, composited over black) to NV12 or P010 planes.
// sRGB curve (SDR video is display-referred, ADR-0174), BT.709 matrix, limited range,
// chroma averaged over each 2x2 block.
// Spec: docs/specs/gpu-compositor.md, "Encoder conversion (NV12, P010)".
//
// `yuv.rs` compiles one pair of fragment entry points (plus `vs_fullscreen`) and
// shares that module. The markers name those parts. They are comments; the math
// is unchanged.

// BEGIN header
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
// END header

// BEGIN oetf
// SDR video is display-referred (ADR-0174): linear light is encoded with the sRGB
// curve, the inverse of the decode, so camera and screen codes pass through unchanged.
fn oetf_video(l: vec3<f32>) -> vec3<f32> {
    let c = clamp(l, vec3<f32>(0.0), vec3<f32>(1.0));
    let lo = 12.92 * c;
    let hi = 1.055 * pow(c, vec3<f32>(1.0 / 2.4)) - vec3<f32>(0.055);
    return select(hi, lo, c <= vec3<f32>(0.0031308));
}
// END oetf

// BEGIN luma
fn luma(p: vec3<f32>) -> f32 {
    return dot(p, vec3<f32>(0.2126, 0.7152, 0.0722));
}
// END luma

// BEGIN prime
fn load_prime(x: i32, y: i32) -> vec3<f32> {
    return oetf_video(textureLoad(canvas, vec2<i32>(x, y), 0).rgb);
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
// END prime

// BEGIN cs_store
// Equal-size NV12, one dispatch (audit F-12). Each invocation loads a 4×2 block of
// canvas texels once and writes whole 32-bit words: four luma codes per row and the
// two chroma pairs of the block's two 2×2 halves. No atomics and no clear: every
// word of the picture is written by exactly one invocation. A block that reaches past
// an odd number of 2-pixel columns repeats the last column into the row padding
// (the stride is a multiple of 256, so the word stays inside the row).
// Scaled kernels stay on the render passes above.
override out_stride: u32 = 256u;

@group(1) @binding(0) var<storage, read_write> packed: array<u32>;

fn pack4(a: u32, b: u32, c: u32, d: u32) -> u32 {
    return (a & 0xffu) | ((b & 0xffu) << 8u) | ((c & 0xffu) << 16u) | ((d & 0xffu) << 24u);
}

/// Writes the luma words of rows `y` and `y + 1` and the chroma word at column `x`.
fn store_words(x: u32, y: u32, height: u32, luma0: u32, luma1: u32, chroma: u32) {
    let stride = out_stride;
    packed[(y * stride + x) >> 2u] = luma0;
    packed[((y + 1u) * stride + x) >> 2u] = luma1;
    packed[(stride * height + (y / 2u) * stride + x) >> 2u] = chroma;
}

fn code_y(p: vec3<f32>) -> u32 {
    return u32(round(y_black + y_range * luma(p)));
}

fn code_c(p: vec3<f32>) -> vec2<u32> {
    let yp = luma(p);
    let cb = (p.b - yp) / 1.8556;
    let cr = (p.r - yp) / 1.5748;
    let c = round(vec2<f32>(c_mid) + c_range * vec2<f32>(cb, cr));
    return vec2<u32>(u32(c.x), u32(c.y));
}

/// Limited-range codes of a 4×2 block: `a*` is row `y`, `b*` row `y + 1`.
fn write_block(
    x: u32,
    y: u32,
    height: u32,
    a0: vec3<f32>, a1: vec3<f32>, a2: vec3<f32>, a3: vec3<f32>,
    b0: vec3<f32>, b1: vec3<f32>, b2: vec3<f32>, b3: vec3<f32>,
) {
    let left = code_c((a0 + a1 + b0 + b1) * 0.25);
    let right = code_c((a2 + a3 + b2 + b3) * 0.25);
    store_words(
        x,
        y,
        height,
        pack4(code_y(a0), code_y(a1), code_y(a2), code_y(a3)),
        pack4(code_y(b0), code_y(b1), code_y(b2), code_y(b3)),
        pack4(left.x, left.y, right.x, right.y),
    );
}
// END cs_store

// BEGIN cs_nv12
@compute @workgroup_size(8, 8)
fn cs_nv12(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = vec2<i32>(textureDimensions(canvas));
    let x = i32(gid.x) * 4;
    let y = i32(gid.y) * 2;
    if (x + 1 >= dims.x || y + 1 >= dims.y) {
        return;
    }
    let x2 = min(x + 2, dims.x - 1);
    let x3 = min(x + 3, dims.x - 1);
    write_block(
        u32(x),
        u32(y),
        u32(dims.y),
        load_prime(x, y), load_prime(x + 1, y), load_prime(x2, y), load_prime(x3, y),
        load_prime(x, y + 1), load_prime(x + 1, y + 1), load_prime(x2, y + 1), load_prime(x3, y + 1),
    );
}
// END cs_nv12

// BEGIN fit
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
// END fit
// Integer downscale ratio N (>= 2). Set only on the box pipelines.
// BEGIN box_n
override box_n: i32 = 2;
// END box_n

// BEGIN sample
fn sample_prime(p: vec2<f32>) -> vec3<f32> {
    let uv = (p - vec2<f32>(fit_x, fit_y)) / vec2<f32>(fit_w, fit_h);
    if (any(uv < vec2<f32>(0.0)) || any(uv > vec2<f32>(1.0))) {
        return vec3<f32>(0.0);
    }
    return oetf_video(textureSampleLevel(canvas, unused_sampler, uv, 0.0).rgb);
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
// END sample

// BEGIN box_fn
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
// END box_fn

// BEGIN box_fs
@fragment
fn fs_luma_box(@builtin(position) pos: vec4<f32>) -> @location(0) f32 {
    let p = oetf_video(box_linear(i32(pos.x), i32(pos.y)));
    return round(y_black + y_range * luma(p)) * code_scale;
}

@fragment
fn fs_chroma_box(@builtin(position) pos: vec4<f32>) -> @location(0) vec2<f32> {
    // The four luma pixels of this chroma sample, each already box-filtered.
    let x = i32(pos.x) * 2;
    let y = i32(pos.y) * 2;
    let p = (oetf_video(box_linear(x, y)) + oetf_video(box_linear(x + 1, y))
        + oetf_video(box_linear(x, y + 1)) + oetf_video(box_linear(x + 1, y + 1))) * 0.25;
    let yp = luma(p);
    let cb = (p.b - yp) / 1.8556;
    let cr = (p.r - yp) / 1.5748;
    return round(vec2<f32>(c_mid) + c_range * vec2<f32>(cb, cr)) * code_scale;
}
// END box_fs

// BEGIN catmull_fn
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

// The four luma centres of one chroma texel. Their 4×4 Catmull-Rom neighbourhoods
// overlap (about 25 canvas texels, not 64), so each of those texels is loaded once
// and the four kernels then run in the same order as `catmull_linear`. A centre
// outside the fitted picture is black. Luma still calls `catmull_linear` itself.
fn catmull_quad(c: vec2<f32>) -> array<vec3<f32>, 4> {
    var out: array<vec3<f32>, 4>;
    var valid: array<bool, 4>;
    var base: array<vec2<i32>, 4>;
    var frac: array<vec2<f32>, 4>;
    var centre: array<vec2<f32>, 4>;
    centre[0] = c + vec2<f32>(0.5, 0.5);
    centre[1] = c + vec2<f32>(1.5, 0.5);
    centre[2] = c + vec2<f32>(0.5, 1.5);
    centre[3] = c + vec2<f32>(1.5, 1.5);

    var min_base = vec2<i32>(0);
    var max_base = vec2<i32>(0);
    var any_valid = false;
    let dims = vec2<i32>(textureDimensions(canvas));
    let fit_origin = vec2<f32>(fit_x, fit_y);
    let fit_size = vec2<f32>(fit_w, fit_h);

    for (var i = 0; i < 4; i++) {
        let uv = (centre[i] - fit_origin) / fit_size;
        if (any(uv < vec2<f32>(0.0)) || any(uv > vec2<f32>(1.0))) {
            valid[i] = false;
        } else {
            let coord = uv * vec2<f32>(dims) - vec2<f32>(0.5);
            let b = floor(coord);
            valid[i] = true;
            base[i] = vec2<i32>(b);
            frac[i] = coord - b;
            if (!any_valid) {
                min_base = vec2<i32>(b);
                max_base = vec2<i32>(b);
                any_valid = true;
            } else {
                min_base = min(min_base, vec2<i32>(b));
                max_base = max(max_base, vec2<i32>(b));
            }
        }
    }
    if (!any_valid) {
        return out;
    }

    // Upscale centres sit less than one canvas texel apart, so the union of the
    // four clamped 4×4 windows is a rectangle of at most 5×5. A wider window
    // (not an upscale) loads each tap on its own.
    let min_s = clamp(min_base - vec2<i32>(1), vec2<i32>(0), dims - vec2<i32>(1));
    let max_s = clamp(max_base + vec2<i32>(2), vec2<i32>(0), dims - vec2<i32>(1));
    let span = max_s - min_s;
    let cached = span.x < 5 && span.y < 5;
    var taps: array<vec3<f32>, 25>;
    if (cached) {
        for (var j = 0; j <= span.y; j++) {
            for (var i = 0; i <= span.x; i++) {
                taps[j * 5 + i] = textureLoad(canvas, min_s + vec2<i32>(i, j), 0).rgb;
            }
        }
    }

    for (var i = 0; i < 4; i++) {
        if (!valid[i]) {
            continue;
        }
        let f = frac[i];
        let b = base[i];
        var acc = vec3<f32>(0.0);
        for (var y = -1; y <= 2; y++) {
            let wy = keys(f.y - f32(y));
            for (var x = -1; x <= 2; x++) {
                let wx = keys(f.x - f32(x));
                let s = clamp(b + vec2<i32>(x, y), vec2<i32>(0), dims - vec2<i32>(1));
                var texel: vec3<f32>;
                if (cached) {
                    let ix = s.x - min_s.x;
                    let iy = s.y - min_s.y;
                    texel = taps[iy * 5 + ix];
                } else {
                    texel = textureLoad(canvas, s, 0).rgb;
                }
                acc += texel * (wx * wy);
            }
        }
        out[i] = acc;
    }
    return out;
}
// END catmull_fn

// BEGIN catmull_fs
@fragment
fn fs_luma_catmull(@builtin(position) pos: vec4<f32>) -> @location(0) f32 {
    let p = oetf_video(catmull_linear(pos.xy));
    return round(y_black + y_range * luma(p)) * code_scale;
}

@fragment
fn fs_chroma_catmull(@builtin(position) pos: vec4<f32>) -> @location(0) vec2<f32> {
    let q = catmull_quad(floor(pos.xy) * 2.0);
    let p = (oetf_video(q[0]) + oetf_video(q[1]) + oetf_video(q[2]) + oetf_video(q[3])) * 0.25;
    let yp = luma(p);
    let cb = (p.b - yp) / 1.8556;
    let cr = (p.r - yp) / 1.5748;
    return round(vec2<f32>(c_mid) + c_range * vec2<f32>(cb, cr)) * code_scale;
}
// END catmull_fs

// BEGIN open_fn
// Open shadows, a grade on the Video path only. Linear light below middle grey
// (0.18) is raised by a power of 0.7 about that knee; black, middle grey and
// everything above are unchanged. The Video curve and limited-range codes then
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
// END open_fn

// BEGIN open_prime
fn load_open(x: i32, y: i32) -> vec3<f32> {
    return oetf_video(open_shadows(textureLoad(canvas, vec2<i32>(x, y), 0).rgb));
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
// END open_prime

// BEGIN cs_nv12_open
@compute @workgroup_size(8, 8)
fn cs_nv12_open(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = vec2<i32>(textureDimensions(canvas));
    let x = i32(gid.x) * 4;
    let y = i32(gid.y) * 2;
    if (x + 1 >= dims.x || y + 1 >= dims.y) {
        return;
    }
    let x2 = min(x + 2, dims.x - 1);
    let x3 = min(x + 3, dims.x - 1);
    write_block(
        u32(x),
        u32(y),
        u32(dims.y),
        load_open(x, y), load_open(x + 1, y), load_open(x2, y), load_open(x3, y),
        load_open(x, y + 1), load_open(x + 1, y + 1), load_open(x2, y + 1), load_open(x3, y + 1),
    );
}
// END cs_nv12_open

// BEGIN open_sample
fn sample_open(p: vec2<f32>) -> vec3<f32> {
    let uv = (p - vec2<f32>(fit_x, fit_y)) / vec2<f32>(fit_w, fit_h);
    if (any(uv < vec2<f32>(0.0)) || any(uv > vec2<f32>(1.0))) {
        return vec3<f32>(0.0);
    }
    return oetf_video(open_shadows(textureSampleLevel(canvas, unused_sampler, uv, 0.0).rgb));
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
// END open_sample

// BEGIN open_box
@fragment
fn fs_luma_open_box(@builtin(position) pos: vec4<f32>) -> @location(0) f32 {
    let p = oetf_video(open_shadows(box_linear(i32(pos.x), i32(pos.y))));
    return round(y_black + y_range * luma(p)) * code_scale;
}

@fragment
fn fs_chroma_open_box(@builtin(position) pos: vec4<f32>) -> @location(0) vec2<f32> {
    let x = i32(pos.x) * 2;
    let y = i32(pos.y) * 2;
    let p = (oetf_video(open_shadows(box_linear(x, y)))
        + oetf_video(open_shadows(box_linear(x + 1, y)))
        + oetf_video(open_shadows(box_linear(x, y + 1)))
        + oetf_video(open_shadows(box_linear(x + 1, y + 1)))) * 0.25;
    let yp = luma(p);
    let cb = (p.b - yp) / 1.8556;
    let cr = (p.r - yp) / 1.5748;
    return round(vec2<f32>(c_mid) + c_range * vec2<f32>(cb, cr)) * code_scale;
}
// END open_box

// BEGIN open_catmull
@fragment
fn fs_luma_open_catmull(@builtin(position) pos: vec4<f32>) -> @location(0) f32 {
    let p = oetf_video(open_shadows(catmull_linear(pos.xy)));
    return round(y_black + y_range * luma(p)) * code_scale;
}

@fragment
fn fs_chroma_open_catmull(@builtin(position) pos: vec4<f32>) -> @location(0) vec2<f32> {
    let q = catmull_quad(floor(pos.xy) * 2.0);
    let p = (oetf_video(open_shadows(q[0])) + oetf_video(open_shadows(q[1]))
        + oetf_video(open_shadows(q[2])) + oetf_video(open_shadows(q[3]))) * 0.25;
    let yp = luma(p);
    let cb = (p.b - yp) / 1.8556;
    let cr = (p.r - yp) / 1.5748;
    return round(vec2<f32>(c_mid) + c_range * vec2<f32>(cb, cr)) * code_scale;
}
// END open_catmull

// BEGIN srgb
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
// END srgb

// BEGIN srgb_prime
fn load_srgb(x: i32, y: i32) -> vec3<f32> {
    return oetf_srgb(textureLoad(canvas, vec2<i32>(x, y), 0).rgb);
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
// END srgb_prime

// BEGIN cs_nv12_srgb
fn code_y_full(p: vec3<f32>) -> u32 {
    return u32(clamp(round(y_black + y_range * luma(p)), 0.0, code_max));
}

fn code_c_full(p: vec3<f32>) -> vec2<u32> {
    let yp = luma(p);
    let cb = (p.b - yp) / 1.8556;
    let cr = (p.r - yp) / 1.5748;
    let c = clamp(
        round(vec2<f32>(c_mid) + c_range * vec2<f32>(cb, cr)),
        vec2<f32>(0.0),
        vec2<f32>(code_max),
    );
    return vec2<u32>(u32(c.x), u32(c.y));
}

@compute @workgroup_size(8, 8)
fn cs_nv12_srgb(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = vec2<i32>(textureDimensions(canvas));
    let x = i32(gid.x) * 4;
    let y = i32(gid.y) * 2;
    if (x + 1 >= dims.x || y + 1 >= dims.y) {
        return;
    }
    let x2 = min(x + 2, dims.x - 1);
    let x3 = min(x + 3, dims.x - 1);
    let a0 = load_srgb(x, y);
    let a1 = load_srgb(x + 1, y);
    let a2 = load_srgb(x2, y);
    let a3 = load_srgb(x3, y);
    let b0 = load_srgb(x, y + 1);
    let b1 = load_srgb(x + 1, y + 1);
    let b2 = load_srgb(x2, y + 1);
    let b3 = load_srgb(x3, y + 1);
    let left = code_c_full((a0 + a1 + b0 + b1) * 0.25);
    let right = code_c_full((a2 + a3 + b2 + b3) * 0.25);
    store_words(
        u32(x),
        u32(y),
        u32(dims.y),
        pack4(code_y_full(a0), code_y_full(a1), code_y_full(a2), code_y_full(a3)),
        pack4(code_y_full(b0), code_y_full(b1), code_y_full(b2), code_y_full(b3)),
        pack4(left.x, left.y, right.x, right.y),
    );
}
// END cs_nv12_srgb

// BEGIN srgb_sample
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
// END srgb_sample

// BEGIN srgb_box
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
// END srgb_box

// BEGIN srgb_catmull
@fragment
fn fs_luma_srgb_catmull(@builtin(position) pos: vec4<f32>) -> @location(0) f32 {
    return store_y(oetf_srgb(catmull_linear(pos.xy)));
}

@fragment
fn fs_chroma_srgb_catmull(@builtin(position) pos: vec4<f32>) -> @location(0) vec2<f32> {
    let q = catmull_quad(floor(pos.xy) * 2.0);
    let p = (oetf_srgb(q[0]) + oetf_srgb(q[1]) + oetf_srgb(q[2]) + oetf_srgb(q[3])) * 0.25;
    return store_c(p);
}
// END srgb_catmull
