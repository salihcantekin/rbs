//! The still picture the camera shows while RBS sends nothing (ADR-0066 decision 7):
//! "RBS VIRTUAL CAMERA IS OFF" in light grey on black, NV12, drawn once when a stream
//! starts.

use crate::announced::Announced;

/// The text drawn.
pub const TEXT: &str = "RBS VIRTUAL CAMERA IS OFF";

/// Limited-range luma of the background (black) and of the text (light grey).
const BLACK: u8 = 16;
const TEXT_LUMA: u8 = 180;
/// Neutral chroma.
const NEUTRAL: u8 = 128;

/// 5 x 7 glyphs, one byte per row, the low five bits from left (bit 4) to right.
fn glyph(c: char) -> [u8; 7] {
    match c {
        'A' => [0x0E, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'B' => [0x1E, 0x11, 0x11, 0x1E, 0x11, 0x11, 0x1E],
        'C' => [0x0E, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0E],
        'E' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x1F],
        'F' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x10],
        'I' => [0x0E, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0E],
        'L' => [0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1F],
        'M' => [0x11, 0x1B, 0x15, 0x15, 0x11, 0x11, 0x11],
        'O' => [0x0E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'R' => [0x1E, 0x11, 0x11, 0x1E, 0x14, 0x12, 0x11],
        'S' => [0x0F, 0x10, 0x10, 0x0E, 0x01, 0x01, 0x1E],
        'T' => [0x1F, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
        'U' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'V' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x0A, 0x04],
        _ => [0; 7],
    }
}

/// The still picture for `format`, tightly packed NV12 (stride = width). Not real-time:
/// allocates the frame once.
pub fn picture(format: &Announced) -> Vec<u8> {
    let (w, h) = (format.width() as usize, format.height() as usize);
    let mut frame = vec![BLACK; w * h];
    frame.resize(format.frame_len(), NEUTRAL);
    // Each glyph cell is 6 x 8 units (one column and one row of spacing); the text
    // spans at most 80 % of the width.
    let chars = TEXT.chars().count();
    let unit = ((w * 8 / 10) / (chars * 6)).max(1);
    let text_w = chars * 6 * unit;
    let text_h = 7 * unit;
    if text_w > w || text_h > h {
        return frame;
    }
    let (x0, y0) = ((w - text_w) / 2, (h - text_h) / 2);
    for (i, c) in TEXT.chars().enumerate() {
        let rows = glyph(c);
        for (gy, bits) in rows.iter().enumerate() {
            for gx in 0..5 {
                if bits & (0x10 >> gx) == 0 {
                    continue;
                }
                let px = x0 + (i * 6 + gx) * unit;
                let py = y0 + gy * unit;
                for y in py..py + unit {
                    frame[y * w + px..y * w + px + unit].fill(TEXT_LUMA);
                }
            }
        }
    }
    frame
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    #[test]
    fn the_picture_is_black_with_light_text_and_neutral_chroma() {
        let f = Announced::default();
        let p = picture(&f);
        assert_eq!(p.len(), f.frame_len());
        let luma = &p[..1280 * 720];
        let lit = luma.iter().filter(|&&y| y == TEXT_LUMA).count();
        assert!(lit > 1000, "{lit} text pixels");
        assert!(luma.iter().all(|&y| y == BLACK || y == TEXT_LUMA));
        assert!(p[1280 * 720..].iter().all(|&c| c == NEUTRAL));
        // Corners stay black.
        assert_eq!(luma[0], BLACK);
        assert_eq!(luma[1280 * 720 - 1], BLACK);
    }

    #[test]
    fn every_size_the_source_announces_gets_a_picture() {
        for (w, h) in crate::announced::SIZES {
            let f = Announced::new(w, h, 30, 1).unwrap();
            assert_eq!(picture(&f).len(), f.frame_len());
        }
    }
}
