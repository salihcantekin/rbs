// Stand-in for the engine compositor: an animated test pattern.
struct U { t: f32, w: f32, h: f32, pad: f32 };
@group(0) @binding(0) var<uniform> u: U;

@vertex
fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let x = f32((i << 1u) & 2u);
    let y = f32(i & 2u);
    return vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
}

@fragment
fn fs(@builtin(position) p: vec4<f32>) -> @location(0) vec4<f32> {
    let uv = p.xy / vec2<f32>(u.w, u.h);
    let bars = floor(uv.x * 8.0) / 8.0;
    var c = vec3<f32>(0.5 + 0.5 * sin(6.283 * (bars + u.t * 0.1)), 0.4 * uv.y, 0.6 - 0.4 * uv.x);
    let bx = 0.5 + 0.35 * sin(u.t * 1.3);
    let by = 0.5 + 0.30 * cos(u.t * 0.9);
    if abs(uv.x - bx) < 0.06 && abs(uv.y - by) < 0.1 {
        c = vec3<f32>(1.0, 1.0, 1.0);
    }
    return vec4<f32>(c, 1.0);
}
