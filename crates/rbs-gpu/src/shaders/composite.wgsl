// Layer compositor (spec docs/specs/gpu-compositor.md).
// One instance per layer; six vertices per instance form the layer quad.
// Output is linear light with premultiplied alpha; blending is "source over".

struct LayerGpu {
    // Affine columns: a, b, c, d.
    m: vec4<f32>,
    // tx, ty, canvas width, canvas height.
    t: vec4<f32>,
    // Crop: u0, v0, u1, v1.
    crop: vec4<f32>,
    // opacity, source is sRGB (1.0) or linear (0.0), source alpha is ignored (1.0),
    // index of the style record (styled pipeline only).
    params: vec4<f32>,
};

@group(0) @binding(0) var<storage, read> layers: array<LayerGpu>;
@group(1) @binding(0) var source: texture_2d<f32>;
@group(1) @binding(1) var source_sampler: sampler;

struct VsOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) @interpolate(flat) layer: u32,
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> VsOut {
    // Two triangles: (0,0) (1,0) (0,1) and (0,1) (1,0) (1,1).
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0),
    );
    let unit = corners[vi];
    let l = layers[ii];
    let px = l.m.x * unit.x + l.m.z * unit.y + l.t.x;
    let py = l.m.y * unit.x + l.m.w * unit.y + l.t.y;
    var out: VsOut;
    out.position = vec4<f32>(px / l.t.z * 2.0 - 1.0, 1.0 - py / l.t.w * 2.0, 0.0, 1.0);
    out.uv = mix(l.crop.xy, l.crop.zw, unit);
    out.layer = ii;
    return out;
}

fn srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    let lo = c / 12.92;
    let hi = pow((c + vec3<f32>(0.055)) / 1.055, vec3<f32>(2.4));
    return select(hi, lo, c <= vec3<f32>(0.04045));
}

// The source's colour at `uv`: linear, premultiplied (XRGB alpha forced to 1, sRGB
// decoded).
fn layer_colour(l: LayerGpu, uv: vec2<f32>) -> vec4<f32> {
    var c = textureSample(source, source_sampler, uv);
    if (l.params.z > 0.5) {
        // XRGB sources: the fourth channel is undefined, the colour is not premultiplied.
        c = vec4<f32>(c.rgb, 1.0);
    }
    if (l.params.y > 0.5) {
        // Sources are premultiplied; decode the colour, not the coverage.
        let a = max(c.a, 1e-6);
        c = vec4<f32>(srgb_to_linear(c.rgb / a) * c.a, c.a);
    }
    return c;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let l = layers[in.layer];
    return layer_colour(l, in.uv) * l.params.x;
}

// Styled layers (ADR-0053): shape, border, antialiased edge. Drawn by their own
// pipeline, so unstyled layers above never pay for any of this.

struct StyleGpu {
    // Kind (1 rectangle, 2 circle, 3 ellipse), corner radius, border width (box units),
    // unused.
    shape: vec4<f32>,
    // Box size in its own units, unused, unused.
    size: vec4<f32>,
    // Border colour, linear, premultiplied.
    border: vec4<f32>,
};

@group(0) @binding(1) var<storage, read> styles: array<StyleGpu>;

struct StyledOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) @interpolate(flat) layer: u32,
    // Position in the cropped box: (0,0) top left, (1,1) bottom right; slightly
    // outside on the grown quad.
    @location(2) unit: vec2<f32>,
};

@vertex
fn vs_styled(@builtin(vertex_index) vi: u32, @builtin(instance_index) ii: u32) -> StyledOut {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0),
    );
    let l = layers[ii];
    // Grow the quad by one canvas pixel on every side, so the antialiasing ramp centred
    // on an edge that touches the box keeps its outer half.
    let grow = vec2<f32>(1.0 / max(length(l.m.xy), 1e-3), 1.0 / max(length(l.m.zw), 1e-3));
    let unit = mix(-grow, vec2<f32>(1.0) + grow, corners[vi]);
    let px = l.m.x * unit.x + l.m.z * unit.y + l.t.x;
    let py = l.m.y * unit.x + l.m.w * unit.y + l.t.y;
    var out: StyledOut;
    out.position = vec4<f32>(px / l.t.z * 2.0 - 1.0, 1.0 - py / l.t.w * 2.0, 0.0, 1.0);
    out.uv = mix(l.crop.xy, l.crop.zw, unit);
    out.layer = ii;
    out.unit = unit;
    return out;
}

// Signed distance to a polygon of `n` points (at most 7). The loop always runs 7 times
// so it stays uniform; points past `n` are ignored. Negative inside.
fn polygon_distance(p: vec2<f32>, v: array<vec2<f32>, 7>, n: i32) -> f32 {
    var d = dot(p - v[0], p - v[0]);
    var s = 1.0;
    var j = n - 1;
    for (var i: i32 = 0; i < 7; i++) {
        if (i < n) {
            let e = v[j] - v[i];
            let w = p - v[i];
            let b = w - e * clamp(dot(w, e) / max(dot(e, e), 1e-6), 0.0, 1.0);
            d = min(d, dot(b, b));
            let c0 = p.y >= v[i].y;
            let c1 = p.y < v[j].y;
            let c2 = e.x * w.y > e.y * w.x;
            if ((c0 && c1 && c2) || (!c0 && !c1 && !c2)) {
                s = -s;
            }
            j = i;
        }
    }
    return s * sqrt(d);
}

// Signed distance from `p` (relative to the box centre, box units) to the shape's edge,
// negative inside. `half` is half the box size.
fn shape_distance(kind: f32, radius: f32, p: vec2<f32>, half: vec2<f32>) -> f32 {
    let short = min(half.x, half.y);
    if (kind < 1.5) {
        // Rounded rectangle; a radius past half the shorter side is clamped there.
        let r = clamp(radius, 0.0, short);
        let q = abs(p) - half + vec2<f32>(r);
        return length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0) - r;
    }
    if (kind < 2.5) {
        // The largest circle centred in the box.
        return length(p) - short;
    }
    if (kind < 3.5) {
        // Ellipse filling the box: the implicit function over its gradient's length, a
        // close distance estimate near the edge, where antialiasing needs it.
        let h = max(half, vec2<f32>(1e-3));
        let k = length(p / h);
        if (k < 0.5) {
            return -short;
        }
        return (k - 1.0) * k / max(length(p / (h * h)), 1e-6);
    }
    // Triangle (tip on the top edge) and arrow (pointing right). Both fill the box.
    var v: array<vec2<f32>, 7>;
    var n: i32 = 3;
    if (kind < 4.5) {
        v[0] = vec2<f32>(0.0, -half.y);
        v[1] = vec2<f32>(half.x, half.y);
        v[2] = vec2<f32>(-half.x, half.y);
    } else {
        let neck = half.x * 0.15;
        let shaft = half.y * 0.38;
        v[0] = vec2<f32>(half.x, 0.0);
        v[1] = vec2<f32>(neck, half.y);
        v[2] = vec2<f32>(neck, shaft);
        v[3] = vec2<f32>(-half.x, shaft);
        v[4] = vec2<f32>(-half.x, -shaft);
        v[5] = vec2<f32>(neck, -shaft);
        v[6] = vec2<f32>(neck, -half.y);
        n = 7;
    }
    return polygon_distance(p, v, n);
}

@fragment
fn fs_styled(in: StyledOut) -> @location(0) vec4<f32> {
    let l = layers[in.layer];
    let st = styles[u32(l.params.w)];
    // Screen-space derivatives of the box position, taken before any branch (WGSL
    // uniformity).
    let local = in.unit * st.size.xy;
    let dx = dpdx(local);
    let dy = dpdy(local);
    // The grown quad samples past the crop; clamp to it.
    let lo = min(l.crop.xy, l.crop.zw);
    let hi = max(l.crop.xy, l.crop.zw);
    let c = layer_colour(l, clamp(in.uv, lo, hi));
    let half = st.size.xy * 0.5;
    let p = local - half;
    let kind = st.shape.x;
    let radius = st.shape.y;
    let d = shape_distance(kind, radius, p, half);
    // Unit normal by central differences, then how many box units one canvas pixel
    // spans along it: exact under rotation, non-uniform scale and shear.
    let e = max(0.5 * max(length(dx), length(dy)), 1e-4);
    let g = vec2<f32>(
        shape_distance(kind, radius, p + vec2<f32>(e, 0.0), half)
            - shape_distance(kind, radius, p - vec2<f32>(e, 0.0), half),
        shape_distance(kind, radius, p + vec2<f32>(0.0, e), half)
            - shape_distance(kind, radius, p - vec2<f32>(0.0, e), half),
    );
    let n = g / max(length(g), 1e-6);
    let w = max(length(vec2<f32>(dot(n, dx), dot(n, dy))), 1e-6);
    let outer = clamp(0.5 - d / w, 0.0, 1.0);
    let inner = clamp(0.5 - (d + st.shape.z) / w, 0.0, 1.0);
    // Border over the picture inside the shape, nothing outside it.
    let t = select(0.0, (outer - inner) / outer, outer > 0.0);
    return (st.border * t + c * (1.0 - st.border.a * t)) * outer * l.params.x;
}
