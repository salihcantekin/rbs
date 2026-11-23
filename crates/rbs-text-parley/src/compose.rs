//! Composes the coverage masks into one straight-alpha picture (ADR-0129 decision 4):
//! background, shadow, outline, fill, then colour glyphs, blended "over" in sRGB as
//! text renderers do.

use rbs_text::{Rgba, TextBlock};

/// One coverage channel the size of the box, 0.0 to 1.0.
pub(crate) struct Mask {
    w: u32,
    h: u32,
    a: Vec<f32>,
}

impl Mask {
    pub(crate) fn new(w: u32, h: u32) -> Self {
        Self {
            w,
            h,
            a: vec![0.0; w as usize * h as usize],
        }
    }

    /// Takes the larger coverage of this mask and an 8-bit glyph mask placed at `at`.
    pub(crate) fn max(&mut self, at: [i32; 2], width: u32, height: u32, data: &[u8]) {
        for y in 0..height {
            let cy = at[1] + y as i32;
            if cy < 0 || cy >= self.h as i32 {
                continue;
            }
            for x in 0..width {
                let cx = at[0] + x as i32;
                if cx < 0 || cx >= self.w as i32 {
                    continue;
                }
                let Some(&v) = data.get((y * width + x) as usize) else {
                    continue;
                };
                let i = cy as usize * self.w as usize + cx as usize;
                if let Some(p) = self.a.get_mut(i) {
                    *p = p.max(f32::from(v) / 255.0);
                }
            }
        }
    }

    fn get(&self, x: i32, y: i32) -> f32 {
        if x < 0 || y < 0 || x >= self.w as i32 || y >= self.h as i32 {
            return 0.0;
        }
        self.a
            .get(y as usize * self.w as usize + x as usize)
            .copied()
            .unwrap_or(0.0)
    }
}

/// A colour glyph (emoji): straight-alpha RGBA placed at `at`.
pub(crate) struct Layer {
    pub(crate) at: [i32; 2],
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) rgba: Vec<u8>,
}

/// Three box blurs of radius `r` along both axes, close to a Gaussian.
fn blur(src: &Mask, radius: f32) -> Vec<f32> {
    let mut a = src.a.clone();
    let r = (radius / 1.5).round() as usize;
    if r == 0 {
        return a;
    }
    let (w, h) = (src.w as usize, src.h as usize);
    let span = (2 * r + 1) as f32;
    let mut tmp = vec![0.0f32; w * h];
    for _ in 0..3 {
        for y in 0..h {
            let row = &a[y * w..(y + 1) * w];
            let mut sum = 0.0;
            for x in 0..w + r {
                if x < w {
                    sum += row[x];
                }
                if x > 2 * r {
                    sum -= row[x - 2 * r - 1];
                }
                if x >= r {
                    tmp[y * w + x - r] = sum / span;
                }
            }
        }
        for x in 0..w {
            let mut sum = 0.0;
            for y in 0..h + r {
                if y < h {
                    sum += tmp[y * w + x];
                }
                if y > 2 * r {
                    sum -= tmp[(y - 2 * r - 1) * w + x];
                }
                if y >= r {
                    a[(y - r) * w + x] = sum / span;
                }
            }
        }
    }
    a
}

/// Premultiplied colour, 0.0 to 1.0.
type Px = [f32; 4];

fn over(d: &mut Px, c: Rgba, coverage: f32) {
    let a = coverage * f32::from(c[3]) / 255.0;
    if a <= 0.0 {
        return;
    }
    for k in 0..3 {
        d[k] = f32::from(c[k]) / 255.0 * a + d[k] * (1.0 - a);
    }
    d[3] = a + d[3] * (1.0 - a);
}

/// The fill colour at a pixel: the colour, or the gradient as the colour source draws
/// it (0 degrees = top to bottom).
fn fill_at(block: &TextBlock, x: u32, y: u32) -> Rgba {
    let Some((second, angle)) = block.fill.gradient else {
        return block.fill.color;
    };
    let [w, h] = block.box_size.map(|v| v as f32);
    let (dx, dy) = angle.to_radians().sin_cos();
    let half = (w * dx.abs() + h * dy.abs()) / 2.0;
    let t = if half <= f32::EPSILON {
        0.0
    } else {
        let px = x as f32 + 0.5 - w / 2.0;
        let py = y as f32 + 0.5 - h / 2.0;
        ((px * dx + py * dy) / (2.0 * half) + 0.5).clamp(0.0, 1.0)
    };
    let first = block.fill.color;
    let mut out = [0u8; 4];
    for i in 0..4 {
        out[i] =
            (f32::from(first[i]) + (f32::from(second[i]) - f32::from(first[i])) * t).round() as u8;
    }
    out
}

/// The finished picture, straight-alpha `Rgba8`.
pub(crate) fn compose(
    block: &TextBlock,
    fill: &Mask,
    stroke: Option<&Mask>,
    color: &[Layer],
) -> Vec<u8> {
    let (w, h) = (fill.w, fill.h);
    let mut out: Vec<Px> = vec![[0.0; 4]; w as usize * h as usize];
    if let Some(bg) = block.background {
        for d in &mut out {
            over(d, bg, 1.0);
        }
    }
    if let Some(shadow) = block.shadow {
        let mut silhouette = Mask::new(w, h);
        for (i, s) in silhouette.a.iter_mut().enumerate() {
            let stroke_a = stroke.map_or(0.0, |m| m.a.get(i).copied().unwrap_or(0.0));
            *s = fill.a.get(i).copied().unwrap_or(0.0).max(stroke_a);
        }
        let soft = Mask {
            w,
            h,
            a: blur(&silhouette, shadow.blur),
        };
        let (ox, oy) = (
            shadow.offset[0].round() as i32,
            shadow.offset[1].round() as i32,
        );
        for y in 0..h as i32 {
            for x in 0..w as i32 {
                let a = soft.get(x - ox, y - oy);
                if let Some(d) = out.get_mut(y as usize * w as usize + x as usize) {
                    over(d, shadow.color, a);
                }
            }
        }
    }
    if let (Some(outline), Some(stroke)) = (block.outline, stroke) {
        for (d, &a) in out.iter_mut().zip(&stroke.a) {
            over(d, outline.color, a);
        }
    }
    for y in 0..h {
        for x in 0..w {
            let i = y as usize * w as usize + x as usize;
            let a = fill.a.get(i).copied().unwrap_or(0.0);
            if a > 0.0
                && let Some(d) = out.get_mut(i)
            {
                over(d, fill_at(block, x, y), a);
            }
        }
    }
    for layer in color {
        for y in 0..layer.height {
            for x in 0..layer.width {
                let (cx, cy) = (layer.at[0] + x as i32, layer.at[1] + y as i32);
                if cx < 0 || cy < 0 || cx >= w as i32 || cy >= h as i32 {
                    continue;
                }
                let i = ((y * layer.width + x) * 4) as usize;
                let Some(p) = layer.rgba.get(i..i + 4) else {
                    continue;
                };
                if let Some(d) = out.get_mut(cy as usize * w as usize + cx as usize) {
                    over(d, [p[0], p[1], p[2], p[3]], 1.0);
                }
            }
        }
    }
    let mut rgba = Vec::with_capacity(out.len() * 4);
    for (i, d) in out.into_iter().enumerate() {
        let a = d[3];
        if a <= 0.0 {
            // Clear pixels keep the colour of the ink beside them, with no coverage, so
            // a picture sampled between pixels (a fractional position or a stretch)
            // blends the straight colours without a dark fringe.
            let edge = match block.outline {
                Some(outline) => outline.color,
                None => fill_at(block, i as u32 % w, i as u32 / w),
            };
            rgba.extend_from_slice(&[edge[0], edge[1], edge[2], 0]);
            continue;
        }
        for c in &d[..3] {
            rgba.push(((c / a).clamp(0.0, 1.0) * 255.0).round() as u8);
        }
        rgba.push((a.clamp(0.0, 1.0) * 255.0).round() as u8);
    }
    rgba
}
