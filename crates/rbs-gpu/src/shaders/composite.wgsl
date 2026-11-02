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
    // opacity, source is sRGB (1.0) or linear (0.0), unused, unused.
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

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let l = layers[in.layer];
    var c = textureSample(source, source_sampler, in.uv);
    if (l.params.y > 0.5) {
        // Sources are premultiplied; decode the colour, not the coverage.
        let a = max(c.a, 1e-6);
        c = vec4<f32>(srgb_to_linear(c.rgb / a) * c.a, c.a);
    }
    return c * l.params.x;
}
