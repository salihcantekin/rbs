//! The three networks: their model names, input shapes, normalisation and the pure
//! preprocessing, all testable without ONNX Runtime.

use rbs_segment::LevelId;

/// Which network a level runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Net {
    /// MediaPipe selfie segmentation: fixed 256x256, values 0..1, output `alphas`.
    MediaPipe,
    /// MODNet webcam: sides multiples of 32, values -1..1, output `output`.
    Modnet,
    /// Robust Video Matting: any size, values 0..1, four recurrent states, output `pha`.
    Rvm,
}

/// What the backend needs to know about a level.
#[derive(Debug, Clone, Copy)]
pub struct Spec {
    /// The level.
    pub level: LevelId,
    /// The network.
    pub net: Net,
    /// The model's name for the [`crate::ModelSource`].
    pub model: &'static str,
    /// Milliseconds per frame (picture in to matte out, 512x288) on an RTX 4090 through DirectML (measured 2026-11-24).
    pub gpu_ms: f32,
    /// The cost text's note.
    pub note: &'static str,
}

/// The level's network.
pub fn spec_of(level: LevelId) -> Option<Spec> {
    Some(match level {
        LevelId::Low => Spec {
            level,
            net: Net::MediaPipe,
            model: "mediapipe-selfie-256",
            gpu_ms: 1.8,
            note: "fast, coarse edges",
        },
        LevelId::Medium => Spec {
            level,
            net: Net::Modnet,
            model: "modnet-webcam",
            gpu_ms: 6.3,
            note: "balanced",
        },
        LevelId::High => Spec {
            level,
            net: Net::Rvm,
            model: "rvm-mobilenetv3-fp32",
            gpu_ms: 7.3,
            note: "best edges and hair, remembers earlier pictures",
        },
        _ => return None,
    })
}

/// The picture size the network runs at for a small picture of `width` x `height`.
pub fn input_dims(net: Net, width: u32, height: u32) -> (u32, u32) {
    match net {
        Net::MediaPipe => (256, 256),
        Net::Modnet => (round32(width), round32(height)),
        Net::Rvm => (width.max(1), height.max(1)),
    }
}

fn round32(v: u32) -> u32 {
    (((v + 16) / 32) * 32).max(32)
}

/// The downsample ratio RVM runs its backbone at: the long side about 256 pixels.
pub fn rvm_ratio(width: u32, height: u32) -> f32 {
    (256.0 / width.max(height).max(1) as f32).min(1.0)
}

/// Fills `out` (NCHW, RGB) with `bgra` of `sw` x `sh` resized bilinearly to `dw` x `dh`
/// and normalised for `net`. Reuses `out`'s capacity.
pub fn fill_input(net: Net, bgra: &[u8], sw: u32, sh: u32, dw: u32, dh: u32, out: &mut Vec<f32>) {
    let (sw, sh, dw, dh) = (sw as usize, sh as usize, dw as usize, dh as usize);
    let plane = dw * dh;
    out.clear();
    out.resize(plane * 3, 0.0);
    if sw == 0 || sh == 0 || bgra.len() < sw * sh * 4 {
        return;
    }
    let (scale, bias) = match net {
        Net::Modnet => (1.0 / 127.5, -1.0),
        _ => (1.0 / 255.0, 0.0),
    };
    let fx = sw as f32 / dw as f32;
    let fy = sh as f32 / dh as f32;
    for y in 0..dh {
        let sy = ((y as f32 + 0.5) * fy - 0.5).clamp(0.0, (sh - 1) as f32);
        let y0 = sy as usize;
        let y1 = (y0 + 1).min(sh - 1);
        let wy = sy - y0 as f32;
        for x in 0..dw {
            let sx = ((x as f32 + 0.5) * fx - 0.5).clamp(0.0, (sw - 1) as f32);
            let x0 = sx as usize;
            let x1 = (x0 + 1).min(sw - 1);
            let wx = sx - x0 as f32;
            let at = |px: usize, py: usize, c: usize| f32::from(bgra[(py * sw + px) * 4 + c]);
            // BGRA in memory: channel 2 is red, 1 green, 0 blue.
            for (plane_index, c) in [2usize, 1, 0].into_iter().enumerate() {
                let top = at(x0, y0, c) * (1.0 - wx) + at(x1, y0, c) * wx;
                let bottom = at(x0, y1, c) * (1.0 - wx) + at(x1, y1, c) * wx;
                let v = top * (1.0 - wy) + bottom * wy;
                out[plane_index * plane + y * dw + x] = v * scale + bias;
            }
        }
    }
}

/// Converts a network's alpha plane (0..1) to one byte per pixel.
pub fn alpha_to_bytes(alpha: &[f32], out: &mut Vec<u8>) {
    out.clear();
    out.extend(
        alpha
            .iter()
            .map(|a| (a.clamp(0.0, 1.0) * 255.0 + 0.5) as u8),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_sizes_follow_the_network() {
        assert_eq!(input_dims(Net::MediaPipe, 512, 288), (256, 256));
        assert_eq!(input_dims(Net::Modnet, 384, 216), (384, 224));
        assert_eq!(input_dims(Net::Modnet, 5, 5), (32, 32));
        assert_eq!(input_dims(Net::Rvm, 512, 288), (512, 288));
        assert!((rvm_ratio(512, 288) - 0.5).abs() < 1e-6);
        assert!((rvm_ratio(200, 100) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn a_blue_green_red_picture_fills_the_planes_in_rgb_order() {
        // One pixel, BGRA (10, 20, 30, 255): red 30, green 20, blue 10.
        let mut out = Vec::new();
        fill_input(Net::Rvm, &[10, 20, 30, 255], 1, 1, 2, 2, &mut out);
        assert_eq!(out.len(), 12);
        assert!((out[0] - 30.0 / 255.0).abs() < 1e-6, "red plane");
        assert!((out[4] - 20.0 / 255.0).abs() < 1e-6, "green plane");
        assert!((out[8] - 10.0 / 255.0).abs() < 1e-6, "blue plane");
    }

    #[test]
    fn modnet_values_run_from_minus_one_to_one() {
        let mut out = Vec::new();
        fill_input(Net::Modnet, &[0, 255, 0, 255], 1, 1, 1, 1, &mut out);
        assert!((out[0] + 1.0).abs() < 1e-6 && (out[1] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn alpha_becomes_bytes() {
        let mut out = Vec::new();
        alpha_to_bytes(&[0.0, 0.5, 1.0, 2.0, -1.0], &mut out);
        assert_eq!(out, vec![0, 128, 255, 255, 0]);
    }
}
