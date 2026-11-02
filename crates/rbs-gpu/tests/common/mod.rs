//! Shared helpers for GPU integration tests.

#![allow(dead_code, clippy::expect_used)] // Not every test binary uses every helper; tests fail by panicking.

use rbs_gpu::{GpuContext, GpuError, GpuRequest, TextureDesc, TextureId, TexturePool, readback};

/// A context on any adapter, software included. `None` (with a message) when no
/// adapter exists, so tests skip instead of failing on machines without a GPU.
pub fn gpu() -> Option<GpuContext> {
    match GpuContext::new(&GpuRequest {
        allow_software: true,
        ..GpuRequest::default()
    }) {
        Ok(ctx) => Some(ctx),
        Err(GpuError::NoAdapter) => {
            eprintln!("SKIPPED: no GPU adapter (install lavapipe or WARP to run GPU tests)");
            None
        }
        Err(e) => panic!("GPU context failed: {e}"),
    }
}

/// Encodes an `f32` as IEEE half (exact for the simple values tests use).
pub fn f32_to_f16(v: f32) -> u16 {
    let bits = v.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp = ((bits >> 23) & 0xff) as i32 - 127 + 15;
    let mant = ((bits >> 13) & 0x3ff) as u16;
    if v == 0.0 || exp <= 0 {
        sign
    } else if exp >= 31 {
        sign | 0x7c00
    } else {
        sign | ((exp as u16) << 10) | mant
    }
}

/// A `w x h` `Rgba16Float` source filled with one premultiplied linear colour.
pub fn solid(
    ctx: &GpuContext,
    pool: &mut TexturePool,
    w: u32,
    h: u32,
    rgba: [f32; 4],
) -> TextureId {
    let id = pool
        .acquire(
            ctx,
            &TextureDesc::source(w, h, wgpu::TextureFormat::Rgba16Float),
        )
        .expect("acquire");
    let texel: Vec<u8> = rgba
        .iter()
        .flat_map(|c| f32_to_f16(*c).to_le_bytes())
        .collect();
    let data: Vec<u8> = (0..w * h).flat_map(|_| texel.iter().copied()).collect();
    readback::upload(ctx, pool, id, &data).expect("upload");
    id
}

/// The texel at `(x, y)` of an `Rgba16Float` texture of width `w`.
pub fn at(pixels: &[[f32; 4]], w: u32, x: u32, y: u32) -> [f32; 4] {
    pixels[(y * w + x) as usize]
}

/// Asserts two colours are equal within `tol` per channel.
pub fn assert_close(got: [f32; 4], want: [f32; 4], tol: f32) {
    for i in 0..4 {
        assert!(
            (got[i] - want[i]).abs() <= tol,
            "channel {i}: got {got:?}, want {want:?} (tolerance {tol})"
        );
    }
}
