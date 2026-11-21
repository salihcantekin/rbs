// Pointer prelude (spec docs/specs/video-filters.md, "Host service: pointer").
// Appended after the filter prelude for types that declare Needs::POINTER.

// fx[0].x: the number of live effects; effect i is fx[1 + 2i] and fx[2 + 2i].
@group(2) @binding(0) var<uniform> fx: array<vec4<f32>, 33>;

fn effect_count() -> u32 {
    return min(u32(fx[0].x), 16u);
}

// The first (half 0) or second (half 1) vector of effect `i`.
fn effect(i: u32, half: u32) -> vec4<f32> {
    return fx[1u + 2u * min(i, 15u) + min(half, 1u)];
}
