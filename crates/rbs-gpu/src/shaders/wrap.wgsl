// Scrolling layers (ADR-0150). Appended to composite.wgsl in the wrap shader module,
// which is built once per device, only after something scrolls; layers without a scroll
// never use these entry points.
//
// The box is a window onto a strip of copies of the (cropped) picture: copy k covers
// [k * period, k * period + picture) along the direction, the window covers
// [start, start + window). Across the direction the picture fills the box as usual.

struct ScrollGpu {
    // Window start on the strip (wrapped for Loop), window length, picture length,
    // period (picture + gap). Lengths in canvas pixels.
    a: vec4<f32>,
    // Axis (0 horizontal, 1 vertical), once (1), fade length, unused.
    b: vec4<f32>,
};

@group(2) @binding(0) var<uniform> scroll: ScrollGpu;

@vertex
fn vs_wrap(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> StyledOut {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0),
    );
    let unit = corners[vi];
    let l = layers[ii];
    let px = l.m.x * unit.x + l.m.z * unit.y + l.t.x;
    let py = l.m.y * unit.x + l.m.w * unit.y + l.t.y;
    var out: StyledOut;
    out.position = vec4<f32>(px / l.t.z * 2.0 - 1.0, 1.0 - py / l.t.w * 2.0, 0.0, 1.0);
    out.uv = mix(l.crop.xy, l.crop.zw, unit);
    out.layer = ii;
    out.unit = unit;
    return out;
}

// Where the window shows the picture at box position `unit`: x = position along the
// direction in the picture (0..1), y = coverage (1 on the picture, 0 in a gap or outside
// a Once copy), times the edge fade.
fn wrap_along(unit: vec2<f32>) -> vec2<f32> {
    let s = scroll;
    let a = clamp(select(unit.x, unit.y, s.b.x > 0.5), 0.0, 1.0);
    let c = s.a.x + a * s.a.y;
    let picture = max(s.a.z, 1e-6);
    var m = c;
    if (s.b.y < 0.5) {
        m = c - s.a.w * floor(c / max(s.a.w, 1e-6));
    }
    var cover = select(0.0, 1.0, m >= 0.0 && m < picture);
    let fade = s.b.z;
    if (fade > 0.0) {
        let edge = min(a, 1.0 - a) * s.a.y;
        cover = cover * clamp(edge / fade, 0.0, 1.0);
    }
    return vec2<f32>(clamp(m / picture, 0.0, 1.0), cover);
}

// The texture coordinates of picture position `along` on layer `l`: the crop's range
// along the direction, the interpolated `uv` across it; kept half a texel of a plane of
// `size` texels inside the crop, so the seam between copies never blends with the
// clamped edge.
fn wrap_uv(l: LayerGpu, uv: vec2<f32>, along: f32, size: vec2<f32>) -> vec2<f32> {
    let lo = min(l.crop.xy, l.crop.zw) + vec2<f32>(0.5) / size;
    let hi = max(l.crop.xy, l.crop.zw) - vec2<f32>(0.5) / size;
    var p = clamp(uv, lo, max(lo, hi));
    if (scroll.b.x > 0.5) {
        p.y = clamp(mix(l.crop.y, l.crop.w, along), lo.y, max(lo.y, hi.y));
    } else {
        p.x = clamp(mix(l.crop.x, l.crop.z, along), lo.x, max(lo.x, hi.x));
    }
    return p;
}

// RGB colour at a wrapped position; level 0, so no derivatives across the seam.
fn wrap_colour(l: LayerGpu, uv: vec2<f32>, along: f32) -> vec4<f32> {
    let size = vec2<f32>(textureDimensions(source));
    var c = textureSampleLevel(source, source_sampler, wrap_uv(l, uv, along, size), 0.0);
    if (l.params.z > 0.5) {
        c = vec4<f32>(c.rgb, 1.0);
    }
    if (l.params.y > 1.5) {
        c = vec4<f32>(srgb_to_linear(c.rgb) * c.a, c.a);
    } else if (l.params.y > 0.5) {
        let a = max(c.a, 1e-6);
        c = vec4<f32>(srgb_to_linear(c.rgb / a) * c.a, c.a);
    }
    return c;
}

// 4:2:0 colour at a wrapped position: each plane clamped with its own texel size.
fn wrap_yuv_colour(ii: u32, l: LayerGpu, uv: vec2<f32>, along: f32) -> vec4<f32> {
    let luma_size = vec2<f32>(textureDimensions(source));
    let chroma_size = vec2<f32>(textureDimensions(chroma));
    let y = textureSampleLevel(source, source_sampler, wrap_uv(l, uv, along, luma_size), 0.0).r;
    let cc = textureSampleLevel(chroma, source_sampler, wrap_uv(l, uv, along, chroma_size), 0.0).rg;
    let p = yuvs[ii];
    let v = vec4<f32>(y, cc, 1.0);
    let prime = clamp(
        vec3<f32>(dot(p.rows[0], v), dot(p.rows[1], v), dot(p.rows[2], v)),
        vec3<f32>(0.0),
        vec3<f32>(1.0),
    );
    var lin = prime;
    if (p.transfer.x == 2u) {
        lin = srgb_to_linear(prime);
    }
    return vec4<f32>(lin, 1.0);
}

@fragment
fn fs_wrap(in: StyledOut) -> @location(0) vec4<f32> {
    let l = layers[in.layer];
    let w = wrap_along(in.unit);
    return wrap_colour(l, in.uv, w.x) * (w.y * l.params.x);
}

@fragment
fn fs_wrap_styled(in: StyledOut) -> @location(0) vec4<f32> {
    let l = layers[in.layer];
    let w = wrap_along(in.unit);
    return styled(in, wrap_colour(l, styled_uv(l, in.uv), w.x) * w.y);
}

@fragment
fn fs_yuv_wrap(in: StyledOut) -> @location(0) vec4<f32> {
    let l = layers[in.layer];
    let w = wrap_along(in.unit);
    return wrap_yuv_colour(in.layer, l, in.uv, w.x) * (w.y * l.params.x);
}

@fragment
fn fs_yuv_wrap_styled(in: StyledOut) -> @location(0) vec4<f32> {
    let l = layers[in.layer];
    let w = wrap_along(in.unit);
    return styled(in, wrap_yuv_colour(in.layer, l, styled_uv(l, in.uv), w.x) * w.y);
}
