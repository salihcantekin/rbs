//! The stinger's file, looked at once off the live paths (ADR-0140 decisions 2 and 3):
//! its length, whether its transparency can be kept here, and the cut point found from
//! its alpha.
//!
//! [`look`] runs on a low-priority worker the host owns (never the control, render or
//! audio thread). It opens the file with `keep_alpha`, decodes one picture at a time
//! and drops it, for at most [`SEARCH_SPAN`] of media, and stops at the first picture
//! that covers the canvas.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use rbs_media_decode::{
    DecodeError, DecodeTarget, MediaDecoder, MediaDecoderFactory, PictureData, PictureLayout,
};

/// Most media searched from the start of the play range.
pub const SEARCH_SPAN: Duration = Duration::from_secs(10);
/// Columns and rows of canvas samples.
pub const GRID: (u32, u32) = (64, 36);
/// Alpha at or above this covers a sample (of 255).
pub const COVERED_ALPHA: u8 = 252;
/// Share of covered samples, in per mille, that makes a picture the cut.
pub const COVERED_PER_MILLE: u32 = 990;

/// What the file's transparency is here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StingerAlpha {
    /// The picture stream has no alpha: the stinger is a full-frame picture.
    None,
    /// It has alpha, and an adapter keeps it.
    Kept,
    /// It has alpha, and no adapter here keeps it: every Stinger change cuts.
    Unsupported,
}

/// What [`look`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StingerLook {
    /// The container's stated length, when it states one.
    pub duration: Option<Duration>,
    /// The file's transparency here.
    pub alpha: StingerAlpha,
    /// The first picture that covers the canvas, from the start of the play range;
    /// `None` without alpha kept, or with no such picture in the span.
    pub cut: Option<Duration>,
    /// The size of the range's first picture, which the stinger is fitted by;
    /// `None` when no picture was decoded.
    pub size: Option<(u32, u32)>,
}

/// Why [`look`] found nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LookError {
    /// The search was cancelled.
    Cancelled,
    /// No adapter opened the file; the text of the most specific refusal.
    Failed(String),
}

/// Opens `path` with each of `factories` in order, keeping alpha, and searches the
/// play range `range` (start, optional end) for the cut on a `canvas` of that size.
/// `cancel` is checked before each picture.
pub fn look(
    factories: &[Arc<dyn MediaDecoderFactory>],
    path: &Path,
    range: (Duration, Option<Duration>),
    canvas: (u32, u32),
    cancel: &AtomicBool,
) -> Result<StingerLook, LookError> {
    let target = DecodeTarget::none().with_keep_alpha();
    let mut refused: Option<DecodeError> = None;
    let mut alpha_refused = false;
    for factory in factories {
        if cancel.load(Ordering::Acquire) {
            return Err(LookError::Cancelled);
        }
        match factory.open_for(path, &target) {
            Ok(mut decoder) => {
                let duration = decoder.duration();
                if !decoder.has_alpha() {
                    return Ok(StingerLook {
                        duration,
                        alpha: StingerAlpha::None,
                        cut: None,
                        size: first_size(&mut *decoder, range.0),
                    });
                }
                let (cut, size) = search(&mut *decoder, range, canvas, cancel)?;
                return Ok(StingerLook {
                    duration,
                    alpha: StingerAlpha::Kept,
                    cut,
                    size,
                });
            }
            Err(DecodeError::AlphaUnsupported) => alpha_refused = true,
            Err(e) => {
                if refused.is_none() {
                    refused = Some(e);
                }
            }
        }
    }
    if alpha_refused {
        // Some adapter opens it, but none keeps its alpha: read the length plainly.
        let duration = factories
            .iter()
            .find_map(|f| f.open(path).ok())
            .and_then(|d| d.duration());
        return Ok(StingerLook {
            duration,
            alpha: StingerAlpha::Unsupported,
            cut: None,
            size: None,
        });
    }
    Err(LookError::Failed(refused.map_or_else(
        || "no adapter can open media files".to_owned(),
        |e| e.to_string(),
    )))
}

/// Whether the file's transparency is kept here, from opens only (no picture is
/// decoded): what `collection.set_transition` answers at once (ADR-0140 decision 9).
/// `None` when no adapter opens it.
pub fn alpha_here(factories: &[Arc<dyn MediaDecoderFactory>], path: &Path) -> Option<StingerAlpha> {
    let target = DecodeTarget::none().with_keep_alpha();
    let mut refused = false;
    for factory in factories {
        match factory.open_for(path, &target) {
            Ok(decoder) if decoder.has_alpha() => return Some(StingerAlpha::Kept),
            Ok(_) => return Some(StingerAlpha::None),
            Err(DecodeError::AlphaUnsupported) => refused = true,
            Err(_) => {}
        }
    }
    refused.then_some(StingerAlpha::Unsupported)
}

/// A picture's width and height.
type PictureSize = (u32, u32);

/// The size of the first picture at or after `start`; `None` when none decodes.
fn first_size(decoder: &mut dyn MediaDecoder, start: Duration) -> Option<(u32, u32)> {
    decoder.seek(start).ok()?;
    let sample = decoder.next_picture().ok()??;
    Some((sample.picture.width, sample.picture.height))
}

/// The first picture of the range that covers the canvas, from the range's start, and
/// the size of the range's first picture.
fn search(
    decoder: &mut dyn MediaDecoder,
    (start, end): (Duration, Option<Duration>),
    canvas: (u32, u32),
    cancel: &AtomicBool,
) -> Result<(Option<Duration>, Option<PictureSize>), LookError> {
    let failed = |e: DecodeError| LookError::Failed(e.to_string());
    decoder.seek(start).map_err(failed)?;
    let limit = start + SEARCH_SPAN;
    let limit = end.map_or(limit, |end| end.min(limit));
    let mut last: Option<Duration> = None;
    let mut size = None;
    loop {
        if cancel.load(Ordering::Acquire) {
            return Err(LookError::Cancelled);
        }
        let Some(sample) = decoder.next_picture().map_err(failed)? else {
            return Ok((None, size));
        };
        // A picture without a stamp continues from the last by its duration.
        let pts = sample
            .pts
            .or_else(|| last.zip(sample.frame_duration).map(|(l, d)| l + d))
            .unwrap_or(start);
        last = Some(pts);
        if pts < start {
            continue;
        }
        let picture = &sample.picture;
        size.get_or_insert((picture.width, picture.height));
        if pts >= limit {
            return Ok((None, size));
        }
        if let PictureData::System {
            layout: PictureLayout::Rgba8,
            bytes,
            ..
        } = picture.data()
            && covers(bytes, (picture.width, picture.height), canvas)
        {
            return Ok((Some(pts - start), size));
        }
    }
}

/// Whether a straight-alpha `Rgba8` picture of `size`, fitted to `canvas` (contain,
/// centred, as the stinger is drawn), covers it: at least [`COVERED_PER_MILLE`] of the
/// [`GRID`] samples over the canvas have alpha at or above [`COVERED_ALPHA`]. A sample
/// outside the fitted picture is not covered.
pub fn covers(bytes: &[u8], size: (u32, u32), canvas: (u32, u32)) -> bool {
    let (w, h) = (f64::from(size.0), f64::from(size.1));
    let (cw, ch) = (f64::from(canvas.0), f64::from(canvas.1));
    if w <= 0.0 || h <= 0.0 || cw <= 0.0 || ch <= 0.0 {
        return false;
    }
    let scale = (cw / w).min(ch / h);
    let (fw, fh) = (w * scale, h * scale);
    let (ox, oy) = ((cw - fw) / 2.0, (ch - fh) / 2.0);
    let (cols, rows) = GRID;
    let mut covered = 0u32;
    for row in 0..rows {
        for col in 0..cols {
            let x = (f64::from(col) + 0.5) * cw / f64::from(cols);
            let y = (f64::from(row) + 0.5) * ch / f64::from(rows);
            let (px, py) = ((x - ox) / scale, (y - oy) / scale);
            if px < 0.0 || py < 0.0 || px >= w || py >= h {
                continue;
            }
            // In range: px < w and py < h, both at most 8192 (`refuse_picture`).
            let index = (py as usize * size.0 as usize + px as usize) * 4 + 3;
            if bytes.get(index).is_some_and(|&a| a >= COVERED_ALPHA) {
                covered += 1;
            }
        }
    }
    covered * 1000 >= COVERED_PER_MILLE * cols * rows
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn picture(w: u32, h: u32, alpha: impl Fn(u32, u32) -> u8) -> Vec<u8> {
        let mut bytes = Vec::new();
        for y in 0..h {
            for x in 0..w {
                bytes.extend_from_slice(&[0, 0, 0, alpha(x, y)]);
            }
        }
        bytes
    }

    #[test]
    fn an_opaque_picture_of_the_canvas_shape_covers_it() {
        let opaque = picture(16, 9, |_, _| 255);
        assert!(covers(&opaque, (16, 9), (1920, 1080)));
        let clear = picture(16, 9, |_, _| 0);
        assert!(!covers(&clear, (16, 9), (1920, 1080)));
        // Almost opaque is not enough.
        let soft = picture(16, 9, |_, _| 251);
        assert!(!covers(&soft, (16, 9), (1920, 1080)));
    }

    #[test]
    fn a_letterboxed_picture_never_covers_the_canvas() {
        // A square picture on a 16:9 canvas leaves bars at the sides.
        let opaque = picture(9, 9, |_, _| 255);
        assert!(!covers(&opaque, (9, 9), (1920, 1080)));
    }

    #[test]
    fn a_small_gap_is_allowed_and_a_wide_one_is_not() {
        // One column of 64 clear (1.6 %) fails; a single clear pixel passes.
        let column = picture(64, 36, |x, _| if x == 0 { 0 } else { 255 });
        assert!(!covers(&column, (64, 36), (64, 36)));
        let speck = picture(64, 36, |x, y| if x == 3 && y == 3 { 0 } else { 255 });
        assert!(covers(&speck, (64, 36), (64, 36)));
    }
}
