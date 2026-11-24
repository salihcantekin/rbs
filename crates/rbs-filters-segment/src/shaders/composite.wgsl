// Background removal (org.rbs.filter.background-removal): the picture over a new
// background, cut by the refined matte. Group 2 is the matte, one channel, any size,
// sampled bilinearly. Output: linear light, premultiplied.
// u[0]: low edge, high edge (matte values), 0, 0.
// u[1]: background colour, linear light, premultiplied (mode 2).
// u[2]: mode (0 transparent, 1 blur, 2 colour), blur radius in uv along x and along y.
@group(2) @binding(0) var matte: texture_2d<f32>;
@group(2) @binding(1) var matte_sampler: sampler;

const TAPS = 32;
const GOLDEN = 2.399963;

// The room around the person, from the picture being drawn (the one the matte was made
// from): taps on a disc, each weighted by how much of it is background so the person's
// colours do not bleed into the blur.
// A per-pixel angle (interleaved gradient noise) turns the disc's fixed tap pattern into
// fine grain instead of visible blocks.
fn spin(pos: vec2<f32>) -> f32 {
    return 6.2831853 * fract(52.9829189 * fract(dot(pos, vec2<f32>(0.06711056, 0.00583715))));
}

fn blurred(uv: vec2<f32>, pos: vec2<f32>) -> vec3<f32> {
    let turn = spin(pos);
    var sum = vec3<f32>(0.0);
    var weight = 0.0;
    var plain = vec3<f32>(0.0);
    for (var i = 0; i < TAPS; i = i + 1) {
        let t = (f32(i) + 0.5) / f32(TAPS);
        let angle = f32(i) * GOLDEN + turn;
        let p = uv + vec2<f32>(cos(angle), sin(angle)) * sqrt(t) * u[2].yz;
        let s = sample_linear(p);
        let c = s.rgb / max(s.a, 1e-4);
        let m = textureSampleLevel(matte, matte_sampler, p, 0.0).r;
        let w = 1.0 - smoothstep(u[0].x, u[0].y, m);
        sum = sum + c * w;
        weight = weight + w;
        plain = plain + c;
    }
    let room = sum / max(weight, 1e-4);
    return mix(plain / f32(TAPS), room, clamp(weight * 0.25, 0.0, 1.0));
}

@fragment
fn fs_main(in: FilterIn) -> @location(0) vec4<f32> {
    let c = sample_linear(in.uv);
    let m = textureSampleLevel(matte, matte_sampler, in.uv, 0.0).r;
    let a = smoothstep(u[0].x, u[0].y, m);
    if (u[2].x < 0.5) {
        return c * a;
    }
    var bg = u[1];
    if (u[2].x < 1.5) {
        bg = vec4<f32>(blurred(in.uv, in.position.xy), 1.0);
    }
    return c * a + bg * (1.0 - a);
}
