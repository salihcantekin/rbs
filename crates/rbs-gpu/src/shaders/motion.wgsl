// Motion of a picture for a sync offset measurement (ADR-0126 decision 3).
// Spec: docs/specs/gpu-compositor.md, "Motion probe".
//
// The picture is averaged over a 64 x 36 grid (4 x 4 samples per cell). Each cell's
// luma is compared with the same cell of the previous call, the absolute differences
// are summed in 16.16 fixed point into `sum`, and the cell's luma is kept for the next
// call. One dispatch of 8 x 9 workgroups of 8 x 4.

@group(0) @binding(0) var picture: texture_2d<f32>;
@group(0) @binding(1) var linear_sampler: sampler;
@group(1) @binding(0) var<storage, read_write> previous: array<f32>;
@group(1) @binding(1) var<storage, read_write> sum: atomic<u32>;

const GRID_W: u32 = 64u;
const GRID_H: u32 = 36u;
const SUB: u32 = 4u;

@compute @workgroup_size(8, 4)
fn cs_motion(@builtin(global_invocation_id) id: vec3<u32>) {
    if (id.x >= GRID_W || id.y >= GRID_H) {
        return;
    }
    let grid = vec2<f32>(f32(GRID_W), f32(GRID_H));
    var luma = 0.0;
    for (var sy = 0u; sy < SUB; sy = sy + 1u) {
        for (var sx = 0u; sx < SUB; sx = sx + 1u) {
            let at = vec2<f32>(id.xy) + (vec2<f32>(f32(sx), f32(sy)) + 0.5) / f32(SUB);
            let c = textureSampleLevel(picture, linear_sampler, at / grid, 0.0);
            luma = luma + dot(clamp(c.rgb, vec3<f32>(0.0), vec3<f32>(1.0)), vec3<f32>(0.2126, 0.7152, 0.0722));
        }
    }
    luma = luma / f32(SUB * SUB);
    let i = id.y * GRID_W + id.x;
    let d = abs(luma - previous[i]);
    previous[i] = luma;
    atomicAdd(&sum, u32(d * 65536.0));
}
