// Background removal (org.rbs.filter.background-removal): the picture over a new
// background, cut by the refined matte. Group 2 is the matte, one channel, any size,
// sampled bilinearly. Output: linear light, premultiplied.
// u[0]: low edge, high edge (matte values), 0, 0.
// u[1]: background colour, linear light, premultiplied (mode 2).
// u[2]: mode (0 transparent, 1 blur, 2 colour), blur radius in uv along x and along y.
@group(2) @binding(0) var matte: texture_2d<f32>;
@group(2) @binding(1) var matte_sampler: sampler;

// The blurred room (colour * weight, weight) at 1/8 size, from the picture being drawn
// (the one the matte was made from).
@group(3) @binding(0) var room: texture_2d<f32>;
@group(3) @binding(1) var room_sampler: sampler;

// The matte is smaller than the picture, so its edge is blocky. Each matte value is the
// average of the 3x3 matte texels around it, weighted by how alike the picture is there
// and here (a joint bilateral upsample): the edge follows the picture's own edges.
fn guided_matte(uv: vec2<f32>, c: vec4<f32>) -> f32 {
    let step = 1.0 / vec2<f32>(textureDimensions(matte));
    let here = c.rgb / max(c.a, 1e-4);
    var sum = 0.0;
    var weight = 0.0;
    for (var j = -1; j <= 1; j = j + 1) {
        for (var i = -1; i <= 1; i = i + 1) {
            let p = uv + vec2<f32>(f32(i), f32(j)) * step;
            let s = sample_linear(p);
            let d = s.rgb / max(s.a, 1e-4) - here;
            let w = exp(-dot(d, d) * 40.0 - 0.5 * f32(i * i + j * j));
            sum = sum + textureSampleLevel(matte, matte_sampler, p, 0.0).r * w;
            weight = weight + w;
        }
    }
    return sum / weight;
}

@fragment
fn fs_main(in: FilterIn) -> @location(0) vec4<f32> {
    let c = sample_linear(in.uv);
    let m = guided_matte(in.uv, c);
    let a = smoothstep(u[0].x, u[0].y, m);
    if (u[2].x < 0.5) {
        return c * a;
    }
    var bg = u[1];
    if (u[2].x < 1.5) {
        let r = textureSampleLevel(room, room_sampler, in.uv, 0.0);
        // The smallest strengths fade to the sharp picture: the room is drawn at 1/8 size,
        // so it cannot be less blurred than about 8 pixels.
        let keep = smoothstep(0.006, 0.03, u[2].z);
        bg = vec4<f32>(mix(c.rgb, r.rgb / max(r.a, 1e-3), keep), 1.0);
    }
    return c * a + bg * (1.0 - a);
}
