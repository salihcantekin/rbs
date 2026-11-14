//! Thumbnails made at the size they are shown.
//!
//! The renderer scales an image with linear filtering and no mipmaps, which reads
//! only the few source pixels nearest each screen pixel. Shrunk more than about
//! two times, a thin line can fall between those pixels and vanish. A picture
//! shown small is therefore made at its on-screen pixel size here, every target
//! pixel the average of all the source pixels it covers, so fine detail stays as
//! a fainter line instead of disappearing. This applies to any picture.

/// How many times larger than the target the source is drawn before it is
/// averaged down: enough that a detail one target pixel wide is several pixels.
pub const OVERSAMPLE: u32 = 4;

/// Averages `src` (`sw` x `sh`, straight RGBA) down to `dw` x `dh` by area.
/// Colour is averaged with alpha as weight, so transparent pixels do not darken
/// a coloured edge. Returns an empty vector when a size is zero or `src` is short.
pub fn shrink_rgba(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u8> {
    let (sw_us, sh_us) = (sw as usize, sh as usize);
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 || src.len() < sw_us * sh_us * 4 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(dw as usize * dh as usize * 4);
    let fx = f64::from(sw) / f64::from(dw);
    let fy = f64::from(sh) / f64::from(dh);
    for y in 0..dh {
        let (y0, y1) = (f64::from(y) * fy, f64::from(y + 1) * fy);
        for x in 0..dw {
            let (x0, x1) = (f64::from(x) * fx, f64::from(x + 1) * fx);
            let mut acc = [0.0f64; 4];
            let mut area = 0.0f64;
            let mut sy = y0.floor() as usize;
            while (sy as f64) < y1 && sy < sh_us {
                let wy = (y1.min(sy as f64 + 1.0) - y0.max(sy as f64)).max(0.0);
                let mut sx = x0.floor() as usize;
                while (sx as f64) < x1 && sx < sw_us {
                    let wx = (x1.min(sx as f64 + 1.0) - x0.max(sx as f64)).max(0.0);
                    let w = wx * wy;
                    let i = (sy * sw_us + sx) * 4;
                    let a = f64::from(src[i + 3]) / 255.0;
                    for (c, v) in acc.iter_mut().take(3).zip(&src[i..i + 3]) {
                        *c += f64::from(*v) * a * w;
                    }
                    acc[3] += a * w;
                    area += w;
                    sx += 1;
                }
                sy += 1;
            }
            if area <= 0.0 || acc[3] <= 0.0 {
                out.extend_from_slice(&[0, 0, 0, 0]);
                continue;
            }
            let alpha = acc[3] / area;
            let px = [
                (acc[0] / acc[3]).round().clamp(0.0, 255.0) as u8,
                (acc[1] / acc[3]).round().clamp(0.0, 255.0) as u8,
                (acc[2] / acc[3]).round().clamp(0.0, 255.0) as u8,
                (alpha * 255.0).round().clamp(0.0, 255.0) as u8,
            ];
            out.extend_from_slice(&px);
        }
    }
    out
}

/// The 16:9 pixel size of a picture shown `height_px` physical pixels high.
pub fn wide_size(height_px: u32) -> (u32, u32) {
    let h = height_px.max(1);
    ((h * 16).div_ceil(9), h)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(w: u32, h: u32, f: impl Fn(u32, u32) -> [u8; 4]) -> Vec<u8> {
        (0..h)
            .flat_map(|y| (0..w).map(move |x| (x, y)))
            .flat_map(|(x, y)| f(x, y))
            .collect()
    }

    #[test]
    fn a_one_pixel_line_survives_a_large_shrink() {
        // A white one-pixel column at x = 100 of 288, shrunk to 64: the column it
        // falls in keeps some alpha; nearest-pixel sampling would often lose it.
        let src = image(288, 162, |x, _| {
            if x == 100 {
                [255, 255, 255, 255]
            } else {
                [0, 0, 0, 0]
            }
        });
        let out = shrink_rgba(&src, 288, 162, 64, 36);
        let col = (100 * 64 / 288) as usize;
        let a = out[(10 * 64 + col) * 4 + 3];
        assert!(a > 0, "alpha {a}");
        assert_eq!(
            &out[(10 * 64 + col) * 4..(10 * 64 + col) * 4 + 3],
            &[255, 255, 255]
        );
    }

    #[test]
    fn a_flat_picture_stays_flat() {
        let src = image(30, 20, |_, _| [10, 200, 30, 255]);
        let out = shrink_rgba(&src, 30, 20, 7, 3);
        assert_eq!(out.len(), 7 * 3 * 4);
        assert!(out.chunks_exact(4).all(|p| p == [10, 200, 30, 255]));
    }

    #[test]
    fn transparent_pixels_do_not_darken_colour() {
        // Half red, half fully transparent black: the colour stays red, alpha halves.
        let src = image(2, 1, |x, _| {
            if x == 0 {
                [255, 0, 0, 255]
            } else {
                [0, 0, 0, 0]
            }
        });
        let out = shrink_rgba(&src, 2, 1, 1, 1);
        assert_eq!(&out[..3], &[255, 0, 0]);
        assert!((127..=128).contains(&out[3]));
    }

    #[test]
    fn empty_sizes_give_nothing() {
        assert!(shrink_rgba(&[], 0, 0, 4, 4).is_empty());
        assert!(shrink_rgba(&[0; 16], 2, 2, 0, 1).is_empty());
        assert!(shrink_rgba(&[0; 4], 2, 2, 1, 1).is_empty());
    }

    #[test]
    fn wide_size_is_sixteen_by_nine() {
        assert_eq!(wide_size(36), (64, 36));
        assert_eq!(wide_size(72), (128, 72));
        assert_eq!(wide_size(0), (2, 1));
    }
}
