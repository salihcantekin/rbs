//! CPU upload and readback. Never on the default frame path (P4).
//!
//! Readback blocks, allocates and increments [`crate::GpuMetrics::frames_cpu_copied`];
//! scenario tests assert that counter stays 0. Upload is for still images, test
//! fixtures and the CPU fallback of sources that cannot share GPU memory.

use crate::context::GpuContext;
use crate::error::GpuError;
use crate::pool::{TextureId, TexturePool};

/// Writes tightly packed texel rows into the whole texture `id` (needs `COPY_DST`).
pub fn upload(
    ctx: &GpuContext,
    pool: &TexturePool,
    id: TextureId,
    data: &[u8],
) -> Result<(), GpuError> {
    let tex = pool.get(id).ok_or(GpuError::StaleTexture(id))?;
    let d = *tex.desc();
    let texel = d
        .format
        .block_copy_size(None)
        .ok_or(GpuError::TargetMismatch(id))?;
    let row = d.width * texel;
    if !d.usage.contains(wgpu::TextureUsages::COPY_DST) || data.len() != (row * d.height) as usize {
        return Err(GpuError::TargetMismatch(id));
    }
    ctx.queue().write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: tex.texture(),
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        data,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(row),
            rows_per_image: Some(d.height),
        },
        wgpu::Extent3d {
            width: d.width,
            height: d.height,
            depth_or_array_layers: 1,
        },
    );
    Ok(())
}

/// Copies the whole texture `id` to the CPU as tightly packed rows (needs `COPY_SRC`).
///
/// Blocks until the GPU finished. Counted in `frames_cpu_copied`.
pub fn read_bytes(
    ctx: &GpuContext,
    pool: &TexturePool,
    id: TextureId,
) -> Result<Vec<u8>, GpuError> {
    let tex = pool.get(id).ok_or(GpuError::StaleTexture(id))?;
    let d = *tex.desc();
    let texel = d
        .format
        .block_copy_size(None)
        .ok_or(GpuError::TargetMismatch(id))?;
    if !d.usage.contains(wgpu::TextureUsages::COPY_SRC) {
        return Err(GpuError::TargetMismatch(id));
    }
    let row = d.width * texel;
    let padded =
        row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buffer = ctx.device().create_buffer(&wgpu::BufferDescriptor {
        label: Some("rbs-gpu readback"),
        size: u64::from(padded) * u64::from(d.height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = ctx
        .device()
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("rbs-gpu readback"),
        });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: tex.texture(),
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(d.height),
            },
        },
        wgpu::Extent3d {
            width: d.width,
            height: d.height,
            depth_or_array_layers: 1,
        },
    );
    ctx.queue().submit(Some(encoder.finish()));

    let slice = buffer.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    ctx.wait_idle()?;
    rx.recv()
        .map_err(|e| GpuError::Readback(e.to_string()))?
        .map_err(|e| GpuError::Readback(e.to_string()))?;
    let mapped = slice
        .get_mapped_range()
        .map_err(|e| GpuError::Readback(e.to_string()))?;
    let mut out = Vec::with_capacity((row * d.height) as usize);
    for chunk in mapped.chunks(padded as usize) {
        out.extend_from_slice(&chunk[..row as usize]);
    }
    drop(mapped);
    buffer.unmap();
    ctx.metrics().count_cpu_copy();
    Ok(out)
}

/// Reads an `Rgba16Float` texture as `f32` texels.
pub fn read_rgba16f(
    ctx: &GpuContext,
    pool: &TexturePool,
    id: TextureId,
) -> Result<Vec<[f32; 4]>, GpuError> {
    if pool.get(id).map(|t| t.desc().format) != Some(wgpu::TextureFormat::Rgba16Float) {
        return Err(GpuError::TargetMismatch(id));
    }
    let bytes = read_bytes(ctx, pool, id)?;
    Ok(bytes
        .chunks_exact(8)
        .map(|t| {
            let h = |i: usize| f16_to_f32(u16::from_le_bytes([t[i], t[i + 1]]));
            [h(0), h(2), h(4), h(6)]
        })
        .collect())
}

/// Converts an IEEE 754 half-precision value to `f32`.
pub fn f16_to_f32(h: u16) -> f32 {
    let sign = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exp = i32::from((h >> 10) & 0x1f);
    let frac = f32::from(h & 0x3ff);
    match exp {
        0 => sign * frac * 2f32.powi(-24),
        31 if frac == 0.0 => sign * f32::INFINITY,
        31 => f32::NAN,
        _ => sign * (1.0 + frac / 1024.0) * 2f32.powi(exp - 15),
    }
}

#[cfg(test)]
mod tests {
    use super::f16_to_f32;

    #[test]
    fn half_values() {
        assert_eq!(f16_to_f32(0x3c00), 1.0);
        assert_eq!(f16_to_f32(0x3800), 0.5);
        assert_eq!(f16_to_f32(0xc000), -2.0);
        assert_eq!(f16_to_f32(0x0000), 0.0);
        assert!(f16_to_f32(0x7e00).is_nan());
    }
}
