//! Pixels of still sources (ADR-0074): colours and gradients, PNG files and the
//! built-in sample pictures. Everything here runs on the control thread, never in a
//! node's `process`.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

/// Largest side of a picture, in pixels.
pub const MAX_SIDE: u32 = 8192;
/// Largest decoded picture, in bytes.
pub const MAX_BYTES: u64 = 256 * 1024 * 1024;
/// Size of a colour source and of the built-in samples when nothing else is asked.
pub const DEFAULT_SIZE: (u32, u32) = (1920, 1080);

/// Straight-alpha sRGB pixels: width, height and `width * height * 4` bytes.
pub type Picture = (u32, u32, Vec<u8>);

/// Parses `#rrggbb` or `#rrggbbaa` (the `#` is optional).
pub fn parse_color(text: &str) -> Option<[u8; 4]> {
    let hex = text.trim().trim_start_matches('#');
    if !hex.is_ascii() || !(hex.len() == 6 || hex.len() == 8) {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    Some([
        byte(0)?,
        byte(2)?,
        byte(4)?,
        if hex.len() == 8 { byte(6)? } else { 255 },
    ])
}

fn lerp(a: [u8; 4], b: [u8; 4], t: f32) -> [u8; 4] {
    let t = t.clamp(0.0, 1.0);
    let mut out = [0u8; 4];
    for i in 0..4 {
        out[i] = (f32::from(a[i]) + (f32::from(b[i]) - f32::from(a[i])) * t).round() as u8;
    }
    out
}

fn fill(w: u32, h: u32, mut pixel: impl FnMut(u32, u32) -> [u8; 4]) -> Vec<u8> {
    let mut out = Vec::with_capacity(w as usize * h as usize * 4);
    for y in 0..h {
        for x in 0..w {
            out.extend_from_slice(&pixel(x, y));
        }
    }
    out
}

/// A position along a gradient line through the picture at `angle` degrees (0 = top to
/// bottom, 90 = left to right), 0.0 at the start and 1.0 at the end.
fn gradient_t(x: u32, y: u32, w: u32, h: u32, angle: f32) -> f32 {
    let (sin, cos) = angle.to_radians().sin_cos();
    let (dx, dy) = (sin, cos);
    let half = (w as f32 * dx.abs() + h as f32 * dy.abs()) / 2.0;
    if half <= f32::EPSILON {
        return 0.0;
    }
    let px = x as f32 + 0.5 - w as f32 / 2.0;
    let py = y as f32 + 0.5 - h as f32 / 2.0;
    (px * dx + py * dy) / (2.0 * half) + 0.5
}

/// A solid colour (`second` is `None`) or a linear gradient from `first` to `second`.
pub fn color_picture(
    w: u32,
    h: u32,
    first: [u8; 4],
    second: Option<[u8; 4]>,
    angle: f32,
) -> Vec<u8> {
    match second {
        None => fill(w, h, |_, _| first),
        Some(second) => fill(w, h, |x, y| {
            lerp(first, second, gradient_t(x, y, w, h, angle))
        }),
    }
}

/// Decodes a PNG file into straight-alpha RGBA8.
pub fn decode_png(path: &Path) -> Result<Picture, String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    let mut decoder = png::Decoder::new(BufReader::new(file));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
    let info = reader.info();
    let (w, h) = (info.width, info.height);
    if w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE {
        return Err(format!("a picture of {w} x {h} pixels is not supported"));
    }
    if u64::from(w) * u64::from(h) * 4 > MAX_BYTES {
        return Err("the picture is too large".to_owned());
    }
    let mut buffer = vec![
        0u8;
        reader
            .output_buffer_size()
            .ok_or("the picture is too large")?
    ];
    let frame = reader.next_frame(&mut buffer).map_err(|e| e.to_string())?;
    let data = &buffer[..frame.buffer_size()];
    let rgba = match frame.color_type {
        png::ColorType::Rgba => data.to_vec(),
        png::ColorType::Rgb => data
            .chunks_exact(3)
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        png::ColorType::Grayscale => data.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::GrayscaleAlpha => data
            .chunks_exact(2)
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
        png::ColorType::Indexed => return Err("indexed colour was not expanded".to_owned()),
    };
    Ok((frame.width, frame.height, rgba))
}

/// What a built-in sample picture is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleKind {
    /// Fills the canvas behind everything.
    Background,
    /// A border or plate drawn over the picture; transparent in the middle.
    Frame,
}

/// One built-in sample picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    /// The `sample` setting.
    pub id: &'static str,
    /// Name for lists.
    pub name: &'static str,
    /// What it is for.
    pub kind: SampleKind,
}

/// The built-in sample pictures, generated in code (ADR-0074 decision 6).
pub const SAMPLES: [Sample; 8] = [
    Sample {
        id: "aurora",
        name: "Aurora",
        kind: SampleKind::Background,
    },
    Sample {
        id: "sunset",
        name: "Sunset",
        kind: SampleKind::Background,
    },
    Sample {
        id: "slate",
        name: "Slate",
        kind: SampleKind::Background,
    },
    Sample {
        id: "dots",
        name: "Dots",
        kind: SampleKind::Background,
    },
    Sample {
        id: "frame-thin",
        name: "Thin frame",
        kind: SampleKind::Frame,
    },
    Sample {
        id: "frame-thick",
        name: "Rounded frame",
        kind: SampleKind::Frame,
    },
    Sample {
        id: "frame-neon",
        name: "Neon frame",
        kind: SampleKind::Frame,
    },
    Sample {
        id: "lower-third",
        name: "Lower third plate",
        kind: SampleKind::Frame,
    },
];

/// Signed distance from `(px, py)` to a rounded rectangle centred in `w` x `h`
/// inset by `inset`, negative inside.
fn rounded_rect_distance(px: f32, py: f32, w: f32, h: f32, inset: f32, radius: f32) -> f32 {
    let (hx, hy) = (w / 2.0 - inset, h / 2.0 - inset);
    let qx = (px - w / 2.0).abs() - (hx - radius);
    let qy = (py - h / 2.0).abs() - (hy - radius);
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    outside + qx.max(qy).min(0.0) - radius
}

/// Coverage 0..1 of a pixel from a signed distance (one pixel wide edge).
fn coverage(distance: f32) -> f32 {
    (0.5 - distance).clamp(0.0, 1.0)
}

fn with_alpha(c: [u8; 4], coverage: f32) -> [u8; 4] {
    [c[0], c[1], c[2], (f32::from(c[3]) * coverage).round() as u8]
}

fn ring(w: u32, h: u32, inset: f32, width: f32, radius: f32, color: [u8; 4]) -> Vec<u8> {
    fill(w, h, |x, y| {
        let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
        let outer = rounded_rect_distance(px, py, w as f32, h as f32, inset, radius);
        let inner = rounded_rect_distance(
            px,
            py,
            w as f32,
            h as f32,
            inset + width,
            (radius - width).max(0.0),
        );
        with_alpha(color, coverage(outer) * (1.0 - coverage(inner)))
    })
}

/// The pixels of the sample `id` at `w` x `h`, or `None` for an unknown id or an empty size.
pub fn sample_picture(id: &str, w: u32, h: u32) -> Option<Vec<u8>> {
    if w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE {
        return None;
    }
    let scale = h as f32 / 1080.0;
    Some(match id {
        "aurora" => {
            let (a, b, c) = ([14, 18, 52, 255], [92, 48, 150, 255], [20, 150, 160, 255]);
            fill(w, h, |x, y| {
                let t = gradient_t(x, y, w, h, 135.0);
                if t < 0.55 {
                    lerp(a, b, t / 0.55)
                } else {
                    lerp(b, c, (t - 0.55) / 0.45)
                }
            })
        }
        "sunset" => {
            let (a, b, c) = ([36, 20, 70, 255], [220, 80, 90, 255], [250, 190, 90, 255]);
            fill(w, h, |x, y| {
                let t = gradient_t(x, y, w, h, 0.0);
                if t < 0.6 {
                    lerp(a, b, t / 0.6)
                } else {
                    lerp(b, c, (t - 0.6) / 0.4)
                }
            })
        }
        "slate" => color_picture(w, h, [44, 48, 56, 255], Some([22, 24, 30, 255]), 0.0),
        "dots" => {
            let spacing = (48.0 * scale).max(8.0);
            let dot = spacing * 0.12;
            fill(w, h, |x, y| {
                let (fx, fy) = (
                    (x as f32 + 0.5) % spacing - spacing / 2.0,
                    (y as f32 + 0.5) % spacing - spacing / 2.0,
                );
                let d = (fx * fx + fy * fy).sqrt() - dot;
                lerp([24, 26, 34, 255], [70, 76, 96, 255], coverage(d))
            })
        }
        "frame-thin" => ring(
            w,
            h,
            14.0 * scale,
            (4.0 * scale).max(1.0),
            6.0 * scale,
            [240, 240, 240, 255],
        ),
        "frame-thick" => ring(
            w,
            h,
            10.0 * scale,
            28.0 * scale,
            40.0 * scale,
            [250, 250, 250, 255],
        ),
        "frame-neon" => {
            let core = ring(
                w,
                h,
                18.0 * scale,
                (5.0 * scale).max(1.0),
                14.0 * scale,
                [120, 240, 255, 255],
            );
            let glow = fill(w, h, |x, y| {
                let d = rounded_rect_distance(
                    x as f32 + 0.5,
                    y as f32 + 0.5,
                    w as f32,
                    h as f32,
                    18.0 * scale + 2.5 * scale,
                    14.0 * scale,
                );
                let falloff = (1.0 - (d.abs() / (40.0 * scale)).min(1.0)).powi(2) * 0.8;
                [60, 200, 255, (falloff * 255.0) as u8]
            });
            // Source-over of the bright core on the glow, both straight alpha.
            core.chunks_exact(4)
                .zip(glow.chunks_exact(4))
                .flat_map(|(c, g)| {
                    let (ca, ga) = (f32::from(c[3]) / 255.0, f32::from(g[3]) / 255.0);
                    let a = ca + ga * (1.0 - ca);
                    if a <= 0.0 {
                        return [0, 0, 0, 0];
                    }
                    let mix = |i: usize| {
                        ((f32::from(c[i]) * ca + f32::from(g[i]) * ga * (1.0 - ca)) / a).round()
                            as u8
                    };
                    [mix(0), mix(1), mix(2), (a * 255.0).round() as u8]
                })
                .collect()
        }
        "lower-third" => {
            let (left, right) = (80.0 * scale, w as f32 - 80.0 * scale);
            let (top, bottom) = (h as f32 - 220.0 * scale, h as f32 - 100.0 * scale);
            fill(w, h, |x, y| {
                let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
                if px < left || px > right * 0.6 || py < top || py > bottom {
                    return [0, 0, 0, 0];
                }
                if px < left + 10.0 * scale {
                    return [250, 190, 60, 255];
                }
                [20, 22, 30, 215]
            })
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

    use super::*;

    #[test]
    fn colours_parse_with_and_without_alpha() {
        assert_eq!(parse_color("#102030"), Some([16, 32, 48, 255]));
        assert_eq!(parse_color("10203040"), Some([16, 32, 48, 64]));
        for bad in ["", "#12", "#gg0000", "#1234567", "#é2030"] {
            assert_eq!(parse_color(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_solid_colour_fills_every_pixel() {
        let px = color_picture(3, 2, [1, 2, 3, 4], None, 0.0);
        assert_eq!(px.len(), 3 * 2 * 4);
        assert!(px.chunks_exact(4).all(|p| p == [1, 2, 3, 4]));
    }

    #[test]
    fn a_gradient_runs_from_the_first_colour_to_the_second() {
        let (w, h) = (4, 100);
        let px = color_picture(w, h, [0, 0, 0, 255], Some([255, 255, 255, 255]), 0.0);
        let at = |y: u32| px[((y * w) * 4) as usize];
        assert!(at(0) < 8 && at(h - 1) > 247, "{} {}", at(0), at(h - 1));
        assert!(at(10) < at(50) && at(50) < at(90));
        // 90 degrees runs left to right.
        let (w, h) = (100, 4);
        let px = color_picture(w, h, [0, 0, 0, 255], Some([255, 255, 255, 255]), 90.0);
        assert!(px[0] < 8 && px[(w as usize - 1) * 4] > 247);
    }

    #[test]
    fn every_sample_is_made_at_any_size_and_unknown_ones_are_not() {
        for s in SAMPLES {
            let px = sample_picture(s.id, 64, 36).unwrap();
            assert_eq!(px.len(), 64 * 36 * 4, "{}", s.id);
            let transparent = px.chunks_exact(4).filter(|p| p[3] == 0).count();
            match s.kind {
                SampleKind::Background => assert_eq!(transparent, 0, "{}", s.id),
                SampleKind::Frame => assert!(transparent > 64 * 36 / 2, "{}", s.id),
            }
        }
        assert!(sample_picture("nope", 64, 36).is_none());
        assert!(sample_picture("aurora", 0, 36).is_none());
        let ids: std::collections::HashSet<_> = SAMPLES.iter().map(|s| s.id).collect();
        assert_eq!(ids.len(), SAMPLES.len(), "sample ids are unique");
    }

    #[test]
    fn a_png_file_decodes_to_straight_alpha_rgba() {
        let dir = std::env::temp_dir().join(format!("rbs-still-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("p.png");
        let mut enc = png::Encoder::new(File::create(&path).unwrap(), 2, 1);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header()
            .unwrap()
            .write_image_data(&[10, 20, 30, 40, 50, 60])
            .unwrap();
        let (w, h, px) = decode_png(&path).unwrap();
        assert_eq!((w, h), (2, 1));
        assert_eq!(px, [10, 20, 30, 255, 40, 50, 60, 255]);
        assert!(decode_png(&dir.join("missing.png")).is_err());
        std::fs::write(dir.join("bad.png"), b"not a png").unwrap();
        assert!(decode_png(&dir.join("bad.png")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
