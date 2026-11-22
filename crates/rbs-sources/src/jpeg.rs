//! JPEG files for image and slide show sources (ADR-0171 decision 3): decoding with
//! `zune-jpeg` and the EXIF orientation tag. Everything here runs on the control side or
//! the slide show decode thread, never in a node's `process`.

use std::path::Path;

use zune_core::bytestream::ZCursor;
use zune_core::colorspace::ColorSpace;
use zune_core::options::DecoderOptions;
use zune_jpeg::JpegDecoder;

use crate::still::{MAX_BYTES, MAX_SIDE, Picture};

/// Most EXIF bytes the orientation reader looks at.
pub const EXIF_MAX_BYTES: usize = 64 * 1024;
/// Most entries the orientation reader walks in the first directory.
pub const EXIF_MAX_ENTRIES: usize = 256;

/// The EXIF orientation tag.
const ORIENTATION_TAG: u16 = 0x0112;

/// Decodes a JPEG file into straight-alpha RGBA8, turned upright by its EXIF
/// orientation tag when it has one.
pub fn decode_jpeg(path: &Path) -> Result<Picture, String> {
    let len = std::fs::metadata(path).map_err(|e| e.to_string())?.len();
    if len > MAX_BYTES {
        return Err("the picture is too large".to_owned());
    }
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    decode_jpeg_bytes(&bytes)
}

/// [`decode_jpeg`] for bytes already in memory.
pub fn decode_jpeg_bytes(bytes: &[u8]) -> Result<Picture, String> {
    let side = MAX_SIDE as usize;
    let options = DecoderOptions::default()
        .set_max_width(side)
        .set_max_height(side)
        .jpeg_set_out_colorspace(ColorSpace::RGBA);
    let mut decoder = JpegDecoder::new_with_options(ZCursor::new(bytes), options);
    decoder.decode_headers().map_err(|e| e.to_string())?;
    let info = decoder.info().ok_or("the picture has no header")?;
    let (w, h) = (u32::from(info.width), u32::from(info.height));
    if w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE {
        return Err(format!("a picture of {w} x {h} pixels is not supported"));
    }
    let size = decoder
        .output_buffer_size()
        .ok_or("the picture is too large")?;
    if size as u64 > MAX_BYTES || size != w as usize * h as usize * 4 {
        return Err("the picture is too large".to_owned());
    }
    let orientation = decoder
        .exif()
        .map_or(Orientation::Normal, |exif| exif_orientation(exif));
    let mut rgba = vec![0u8; size];
    decoder.decode_into(&mut rgba).map_err(|e| e.to_string())?;
    Ok(orient((w, h, rgba), orientation))
}

/// The eight EXIF orientations (tag values 1 to 8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Orientation {
    /// 1: as stored.
    Normal,
    /// 2: mirrored left to right.
    FlipH,
    /// 3: turned 180 degrees.
    Turn180,
    /// 4: mirrored top to bottom.
    FlipV,
    /// 5: mirrored along the top-left to bottom-right diagonal.
    Transpose,
    /// 6: shown after turning 90 degrees clockwise.
    Turn90,
    /// 7: mirrored along the other diagonal.
    Transverse,
    /// 8: shown after turning 90 degrees counter-clockwise.
    Turn270,
}

impl Orientation {
    /// The orientation of an EXIF tag value; values outside 1 to 8 are [`Self::Normal`].
    pub fn from_tag(value: u16) -> Self {
        match value {
            2 => Self::FlipH,
            3 => Self::Turn180,
            4 => Self::FlipV,
            5 => Self::Transpose,
            6 => Self::Turn90,
            7 => Self::Transverse,
            8 => Self::Turn270,
            _ => Self::Normal,
        }
    }

    /// True when the upright picture swaps width and height.
    pub fn swaps_sides(self) -> bool {
        matches!(
            self,
            Self::Transpose | Self::Turn90 | Self::Transverse | Self::Turn270
        )
    }
}

/// Reads the orientation tag from EXIF bytes that start at the TIFF header. Anything
/// malformed, truncated or past the limits gives [`Orientation::Normal`]; it never
/// panics and reads at most [`EXIF_MAX_BYTES`] bytes and [`EXIF_MAX_ENTRIES`] entries.
pub fn exif_orientation(exif: &[u8]) -> Orientation {
    let data = &exif[..exif.len().min(EXIF_MAX_BYTES)];
    // Some writers keep the APP1 identifier in front of the TIFF header.
    let data = data.strip_prefix(b"Exif\0\0").unwrap_or(data);
    read_orientation(data).map_or(Orientation::Normal, Orientation::from_tag)
}

fn read_orientation(data: &[u8]) -> Option<u16> {
    let little = match data.get(0..2)? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let u16_at = |at: usize| -> Option<u16> {
        let b: [u8; 2] = data.get(at..at.checked_add(2)?)?.try_into().ok()?;
        Some(if little {
            u16::from_le_bytes(b)
        } else {
            u16::from_be_bytes(b)
        })
    };
    let u32_at = |at: usize| -> Option<u32> {
        let b: [u8; 4] = data.get(at..at.checked_add(4)?)?.try_into().ok()?;
        Some(if little {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        })
    };
    if u16_at(2)? != 42 {
        return None;
    }
    let ifd = usize::try_from(u32_at(4)?).ok()?;
    let count = usize::from(u16_at(ifd)?).min(EXIF_MAX_ENTRIES);
    for i in 0..count {
        let entry = ifd.checked_add(2)?.checked_add(i.checked_mul(12)?)?;
        if u16_at(entry)? != ORIENTATION_TAG {
            continue;
        }
        // Type 3 (SHORT), count 1: the value sits in the first two bytes of the field.
        if u16_at(entry + 2)? != 3 || u32_at(entry + 4)? != 1 {
            return None;
        }
        return u16_at(entry + 8);
    }
    None
}

/// Turns straight-alpha RGBA8 pixels upright.
pub fn orient(picture: Picture, orientation: Orientation) -> Picture {
    let (w, h, src) = picture;
    if orientation == Orientation::Normal {
        return (w, h, src);
    }
    let (ow, oh) = if orientation.swaps_sides() {
        (h, w)
    } else {
        (w, h)
    };
    let (wu, hu, owu) = (w as usize, h as usize, ow as usize);
    let mut out = vec![0u8; src.len()];
    for y in 0..hu {
        for x in 0..wu {
            // Where the stored pixel (x, y) lands in the upright picture.
            let (dx, dy) = match orientation {
                Orientation::Normal => (x, y),
                Orientation::FlipH => (wu - 1 - x, y),
                Orientation::Turn180 => (wu - 1 - x, hu - 1 - y),
                Orientation::FlipV => (x, hu - 1 - y),
                Orientation::Transpose => (y, x),
                Orientation::Turn90 => (hu - 1 - y, x),
                Orientation::Transverse => (hu - 1 - y, wu - 1 - x),
                Orientation::Turn270 => (y, wu - 1 - x),
            };
            let s = (y * wu + x) * 4;
            let d = (dy * owu + dx) * 4;
            out[d..d + 4].copy_from_slice(&src[s..s + 4]);
        }
    }
    (ow, oh, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A TIFF header and one directory holding only the orientation tag.
    fn exif(little: bool, value: u16) -> Vec<u8> {
        let mut v = Vec::new();
        let u16b = |n: u16| {
            if little {
                n.to_le_bytes()
            } else {
                n.to_be_bytes()
            }
        };
        let u32b = |n: u32| {
            if little {
                n.to_le_bytes()
            } else {
                n.to_be_bytes()
            }
        };
        v.extend_from_slice(if little { b"II" } else { b"MM" });
        v.extend_from_slice(&u16b(42));
        v.extend_from_slice(&u32b(8));
        v.extend_from_slice(&u16b(1));
        v.extend_from_slice(&u16b(ORIENTATION_TAG));
        v.extend_from_slice(&u16b(3));
        v.extend_from_slice(&u32b(1));
        v.extend_from_slice(&u16b(value));
        v.extend_from_slice(&[0, 0]);
        v.extend_from_slice(&u32b(0));
        v
    }

    #[test]
    fn reads_the_orientation_in_both_byte_orders() {
        for little in [true, false] {
            for value in 1..=8 {
                assert_eq!(
                    exif_orientation(&exif(little, value)),
                    Orientation::from_tag(value)
                );
            }
        }
    }

    #[test]
    fn accepts_the_app1_identifier_in_front() {
        let mut data = b"Exif\0\0".to_vec();
        data.extend(exif(true, 6));
        assert_eq!(exif_orientation(&data), Orientation::Turn90);
    }

    #[test]
    fn malformed_exif_is_normal() {
        assert_eq!(exif_orientation(&[]), Orientation::Normal);
        assert_eq!(exif_orientation(b"XX*\0"), Orientation::Normal);
        let full = exif(true, 6);
        // The value's two bytes end at offset 20 (header 8, count 2, entry fields 10).
        for cut in 0..20 {
            // Every truncation before the end of the value fails cleanly.
            let o = exif_orientation(&full[..cut]);
            assert_eq!(o, Orientation::Normal, "cut at {cut}");
        }
        // A directory offset far past the end.
        let mut far = exif(true, 6);
        far[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(exif_orientation(&far), Orientation::Normal);
        // Wrong type for the tag.
        let mut wrong = exif(true, 6);
        wrong[12..14].copy_from_slice(&4u16.to_le_bytes());
        assert_eq!(exif_orientation(&wrong), Orientation::Normal);
    }

    #[test]
    fn random_bytes_never_panic() {
        // A small deterministic generator (no new dependency): xorshift.
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for round in 0..2000 {
            let len = (next() % 96) as usize;
            let mut data: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            if round % 2 == 0 && data.len() >= 4 {
                // Keep a valid header half the time so the walker runs.
                data[0..4].copy_from_slice(b"II*\0");
            }
            let _ = exif_orientation(&data);
        }
    }

    #[test]
    fn the_entry_count_is_capped() {
        let mut data = exif(true, 6);
        // Claim 65535 entries; the walker stops at the cap or the end of the data.
        data[8..10].copy_from_slice(&u16::MAX.to_le_bytes());
        assert_eq!(exif_orientation(&data), Orientation::Turn90);
    }

    /// A 3 x 2 picture whose pixel value is its index.
    fn sample() -> Picture {
        (3, 2, (0u8..6).flat_map(|i| [i, 0, 0, 255]).collect())
    }

    fn reds(p: &Picture) -> Vec<u8> {
        p.2.chunks_exact(4).map(|c| c[0]).collect()
    }

    #[test]
    fn orientations_turn_the_pixels() {
        // Stored:  0 1 2
        //          3 4 5
        let cases: [(u16, (u32, u32), [u8; 6]); 8] = [
            (1, (3, 2), [0, 1, 2, 3, 4, 5]),
            (2, (3, 2), [2, 1, 0, 5, 4, 3]),
            (3, (3, 2), [5, 4, 3, 2, 1, 0]),
            (4, (3, 2), [3, 4, 5, 0, 1, 2]),
            (5, (2, 3), [0, 3, 1, 4, 2, 5]),
            (6, (2, 3), [3, 0, 4, 1, 5, 2]),
            (7, (2, 3), [5, 2, 4, 1, 3, 0]),
            (8, (2, 3), [2, 5, 1, 4, 0, 3]),
        ];
        for (tag, size, want) in cases {
            let out = orient(sample(), Orientation::from_tag(tag));
            assert_eq!((out.0, out.1), size, "tag {tag}");
            assert_eq!(reds(&out), want, "tag {tag}");
        }
    }

    #[test]
    fn garbage_is_not_a_jpeg() {
        assert!(decode_jpeg_bytes(b"not a jpeg").is_err());
        assert!(decode_jpeg_bytes(&[]).is_err());
    }
}

#[cfg(test)]
mod file_tests {
    use super::*;

    fn data(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data")
            .join(name)
    }

    #[test]
    fn decodes_a_jpeg_file() {
        let (w, h, rgba) = decode_jpeg(&data("plain-4x2.jpg")).unwrap();
        assert_eq!((w, h), (4, 2));
        assert_eq!(rgba.len(), 4 * 2 * 4);
        for p in rgba.chunks_exact(4) {
            // Quality 100 without subsampling: close to the written (200, 10, 10).
            assert!(p[0].abs_diff(200) <= 3 && p[1] <= 14 && p[2] <= 14, "{p:?}");
            assert_eq!(p[3], 255);
        }
    }

    #[test]
    fn the_exif_orientation_turns_the_file_upright() {
        // Stored 4 x 2 with tag 6: shown 2 x 4, the stored first row becomes the right
        // column, top to bottom.
        let (w, h, rgba) = decode_jpeg(&data("turn90-4x2.jpg")).unwrap();
        assert_eq!((w, h), (2, 4));
        let at = |x: usize, y: usize| &rgba[(y * 2 + x) * 4..(y * 2 + x) * 4 + 3];
        // Stored (0,0) is red and lands at the top right.
        let red = at(1, 0);
        assert!(red[0] > 200 && red[1] < 60 && red[2] < 60, "{red:?}");
        // Stored (3,0) is white and lands at the bottom right.
        assert!(at(1, 3).iter().all(|&c| c > 200));
    }

    #[test]
    fn decode_picture_chooses_by_content() {
        let (w, h, _) = crate::still::decode_picture(&data("plain-4x2.jpg")).unwrap();
        assert_eq!((w, h), (4, 2));
        assert!(crate::still::decode_picture(&data("missing.jpg")).is_err());
    }
}
