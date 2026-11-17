//! Animated pictures for the media-file source: GIF, APNG and animated WebP, decoded in
//! pure Rust on every platform (ADR-0108 decision 4, port spec `media-decode-port.md`).
//!
//! - Wraps the `gif`, `png` and `image-webp` crates (MIT or Apache-2.0). No FFmpeg, no
//!   platform API, no `unsafe`.
//! - Pixel capability `cpu-upload`: every picture is a full straight-alpha sRGB `Rgba8`
//!   frame in system memory (decision 6), composed on the decoder's canvas from the
//!   file's sub-frames with their disposal and blend rules.
//! - Timestamps are the sums of the stated frame delays. The frame-duration rule of
//!   decision 4 applies: a delay of 10 ms or less reads as 100 ms.
//! - A picture file that does not animate (a plain PNG, GIF or WebP) is one picture
//!   with no duration: the source shows it and holds it.
//! - `seek` opens the file again and composes forward to the frame that covers the
//!   cursor; animations have no keyframes.
//! - There is no sound: `has_audio` is false and the source's sound node is silent.
//!
//! Limitations: the loop count in the file is ignored (the source's `loop` decides).
//! A GIF or APNG duration is read from the frame headers at open; an animated WebP
//! states it with `loop_duration`, without the 100 ms rule.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};
use rbs_media_decode::caps::PIXEL;
use rbs_media_decode::pixel::CPU_UPLOAD;
use rbs_media_decode::{
    DecodeError, MediaDecoder, MediaDecoderFactory, Picture, PictureLayout, PictureSample,
    SampleColor, SoundSample, frame_duration_rule, refuse_picture,
};

/// The adapter id.
pub const ADAPTER_ID: &str = "org.rbs.media-decode.anim";

/// The factory: probes to `cpu-upload` on every platform.
#[derive(Debug, Default)]
pub struct AnimFactory;

impl AnimFactory {
    /// A factory; it holds no state.
    pub fn new() -> Self {
        Self
    }
}

impl AdapterFactory for AnimFactory {
    fn id(&self) -> AdapterId {
        AdapterId::new(ADAPTER_ID)
    }

    fn display_name(&self) -> &str {
        "Animated pictures (GIF, APNG, WebP)"
    }

    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        Ok(CapabilitySet::new().with(PIXEL, CapabilityValue::Set(vec![CPU_UPLOAD.to_owned()])))
    }
}

impl MediaDecoderFactory for AnimFactory {
    fn open(&self, path: &Path) -> Result<Box<dyn MediaDecoder>, DecodeError> {
        Ok(Box::new(AnimDecoder::open(path)?))
    }
}

/// The three containers, told apart by their first bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Gif,
    Png,
    Webp,
}

fn sniff(head: &[u8]) -> Option<Kind> {
    if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        Some(Kind::Gif)
    } else if head.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some(Kind::Png)
    } else if head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"WEBP" {
        Some(Kind::Webp)
    } else {
        None
    }
}

fn failed(e: impl std::fmt::Display) -> DecodeError {
    DecodeError::Failed(e.to_string())
}

/// One composed frame: the full canvas and how long it shows.
struct Composed {
    rgba: Vec<u8>,
    delay: Duration,
}

/// The decoder of one file.
pub struct AnimDecoder {
    path: PathBuf,
    kind: Kind,
    width: u32,
    height: u32,
    duration: Option<Duration>,
    frames: Frames,
    /// Presentation time of the next frame.
    next_pts: Duration,
    /// A frame read ahead by `seek` that covers the cursor.
    pending: Option<(Duration, Composed)>,
}

impl AnimDecoder {
    /// Opens `path` (already accepted as a local regular file by the host).
    pub fn open(path: &Path) -> Result<Self, DecodeError> {
        let mut head = [0u8; 12];
        let mut file = File::open(path).map_err(failed)?;
        let read = read_head(&mut file, &mut head)?;
        let kind = sniff(&head[..read])
            .ok_or_else(|| DecodeError::Unsupported("not a GIF, PNG or WebP file".into()))?;
        let frames = Frames::open(path, kind)?;
        let (width, height) = frames.size();
        if let Some(refused) = refuse_picture(width, height, rgba_len(width, height)) {
            return Err(DecodeError::Failed(format!(
                "the picture must be {}",
                refused.expected()
            )));
        }
        let duration = match kind {
            Kind::Gif => gif_duration(path)?,
            Kind::Png => png_duration(path)?,
            Kind::Webp if frames.is_animated_webp() => webp_duration(path)?,
            Kind::Webp => None,
        };
        Ok(Self {
            path: path.to_owned(),
            kind,
            width,
            height,
            duration,
            frames,
            next_pts: Duration::ZERO,
            pending: None,
        })
    }

    fn sample(&self, pts: Duration, frame: Composed) -> PictureSample {
        // A still has no duration: the source holds it.
        let frame_duration = self.duration.map(|_| frame.delay);
        PictureSample {
            pts: Some(pts),
            frame_duration,
            picture: Picture::system(
                self.width,
                self.height,
                PictureLayout::Rgba8,
                SampleColor::Srgb,
                frame.rgba,
            ),
        }
    }
}

fn read_head(file: &mut File, head: &mut [u8]) -> Result<usize, DecodeError> {
    let mut read = 0;
    while read < head.len() {
        match file.read(&mut head[read..]).map_err(failed)? {
            0 => break,
            n => read += n,
        }
    }
    Ok(read)
}

fn rgba_len(width: u32, height: u32) -> usize {
    (width as usize)
        .saturating_mul(height as usize)
        .saturating_mul(4)
}

/// A stated delay after the 100 ms rule (ADR-0108 decision 4).
fn delay_rule(stated: Duration) -> Duration {
    frame_duration_rule(Some(stated), true, None).unwrap_or(stated)
}

impl MediaDecoder for AnimDecoder {
    fn seek(&mut self, cursor: Duration) -> Result<(), DecodeError> {
        self.frames = Frames::open(&self.path, self.kind)?;
        self.next_pts = Duration::ZERO;
        self.pending = None;
        if cursor.is_zero() || self.duration.is_none() {
            return Ok(());
        }
        // No keyframes: compose forward to the frame that covers the cursor.
        while let Some(frame) = self.frames.next(self.width, self.height)? {
            let pts = self.next_pts;
            self.next_pts = pts + frame.delay;
            if self.next_pts > cursor {
                self.pending = Some((pts, frame));
                break;
            }
        }
        Ok(())
    }

    fn next_picture(&mut self) -> Result<Option<PictureSample>, DecodeError> {
        if let Some((pts, frame)) = self.pending.take() {
            return Ok(Some(self.sample(pts, frame)));
        }
        let Some(frame) = self.frames.next(self.width, self.height)? else {
            return Ok(None);
        };
        let pts = self.next_pts;
        self.next_pts = pts + frame.delay;
        Ok(Some(self.sample(pts, frame)))
    }

    fn next_sound(&mut self) -> Result<Option<SoundSample>, DecodeError> {
        Ok(None)
    }

    fn has_audio(&self) -> bool {
        false
    }

    fn duration(&self) -> Option<Duration> {
        self.duration
    }
}

/// A format's decoder and the canvas it composes on.
enum Frames {
    Gif(Box<GifFrames>),
    Png(Box<PngFrames>),
    Webp(Box<WebpFrames>),
}

impl Frames {
    fn open(path: &Path, kind: Kind) -> Result<Self, DecodeError> {
        let reader = BufReader::new(File::open(path).map_err(failed)?);
        Ok(match kind {
            Kind::Gif => Frames::Gif(Box::new(GifFrames::new(reader)?)),
            Kind::Png => Frames::Png(Box::new(PngFrames::new(reader)?)),
            Kind::Webp => Frames::Webp(Box::new(WebpFrames::new(reader)?)),
        })
    }

    fn size(&self) -> (u32, u32) {
        match self {
            Frames::Gif(f) => (f.width, f.height),
            Frames::Png(f) => (f.width, f.height),
            Frames::Webp(f) => f.decoder.dimensions(),
        }
    }

    fn is_animated_webp(&self) -> bool {
        matches!(self, Frames::Webp(f) if f.decoder.is_animated())
    }

    fn next(&mut self, width: u32, height: u32) -> Result<Option<Composed>, DecodeError> {
        match self {
            Frames::Gif(f) => f.next(),
            Frames::Png(f) => f.next(),
            Frames::Webp(f) => f.next(width, height),
        }
    }
}

// ---- GIF ----

struct GifFrames {
    decoder: gif::Decoder<BufReader<File>>,
    width: u32,
    height: u32,
    canvas: Vec<u8>,
    /// What the previous frame asks to be done before the next one is drawn.
    dispose: Option<(gif::DisposalMethod, Rect, Option<Vec<u8>>)>,
}

#[derive(Debug, Clone, Copy)]
struct Rect {
    left: u32,
    top: u32,
    width: u32,
    height: u32,
}

impl GifFrames {
    fn new(reader: BufReader<File>) -> Result<Self, DecodeError> {
        let mut options = gif::DecodeOptions::new();
        options.set_color_output(gif::ColorOutput::RGBA);
        let decoder = options.read_info(reader).map_err(failed)?;
        let (width, height) = (u32::from(decoder.width()), u32::from(decoder.height()));
        if refuse_picture(width, height, rgba_len(width, height)).is_some() {
            return Err(DecodeError::Failed(
                "the picture must be at most 8192 pixels on a side".into(),
            ));
        }
        Ok(Self {
            decoder,
            width,
            height,
            canvas: vec![0; rgba_len(width, height)],
            dispose: None,
        })
    }

    fn next(&mut self) -> Result<Option<Composed>, DecodeError> {
        if let Some((method, rect, saved)) = self.dispose.take() {
            match method {
                gif::DisposalMethod::Background => {
                    clear_rect(&mut self.canvas, self.width, self.height, rect);
                }
                gif::DisposalMethod::Previous => {
                    if let Some(saved) = saved {
                        put_rect(&mut self.canvas, self.width, self.height, rect, &saved);
                    }
                }
                _ => {}
            }
        }
        let Some(frame) = self.decoder.read_next_frame().map_err(failed)? else {
            return Ok(None);
        };
        let rect = Rect {
            left: u32::from(frame.left),
            top: u32::from(frame.top),
            width: u32::from(frame.width),
            height: u32::from(frame.height),
        };
        let saved = (frame.dispose == gif::DisposalMethod::Previous)
            .then(|| get_rect(&self.canvas, self.width, self.height, rect));
        // A transparent GIF pixel leaves the canvas as it is.
        blend_rect(
            &mut self.canvas,
            self.width,
            self.height,
            rect,
            &frame.buffer,
            true,
        );
        self.dispose = Some((frame.dispose, rect, saved));
        let delay = delay_rule(Duration::from_millis(u64::from(frame.delay) * 10));
        Ok(Some(Composed {
            rgba: self.canvas.clone(),
            delay,
        }))
    }
}

fn gif_duration(path: &Path) -> Result<Option<Duration>, DecodeError> {
    let reader = BufReader::new(File::open(path).map_err(failed)?);
    let mut options = gif::DecodeOptions::new();
    options.skip_frame_decoding(true);
    let mut decoder = options.read_info(reader).map_err(failed)?;
    let mut total = Duration::ZERO;
    let mut frames = 0u32;
    while let Some(frame) = decoder.read_next_frame().map_err(failed)? {
        total += delay_rule(Duration::from_millis(u64::from(frame.delay) * 10));
        frames += 1;
    }
    Ok((frames > 1).then_some(total))
}

// ---- APNG ----

struct PngFrames {
    reader: png::Reader<BufReader<File>>,
    width: u32,
    height: u32,
    canvas: Vec<u8>,
    /// The decoded sub-frame, in the output colour type.
    buffer: Vec<u8>,
    /// The sub-frame converted to `Rgba8`.
    rgba: Vec<u8>,
    animated: bool,
    /// The default image is not part of the animation and is skipped once.
    skip_default: bool,
    done: bool,
    dispose: Option<(png::DisposeOp, Rect, Option<Vec<u8>>)>,
}

impl PngFrames {
    fn new(reader: BufReader<File>) -> Result<Self, DecodeError> {
        let mut decoder = png::Decoder::new(reader);
        decoder.set_transformations(png::Transformations::normalize_to_color8());
        let reader = decoder.read_info().map_err(failed)?;
        let info = reader.info();
        let (width, height) = (info.width, info.height);
        if refuse_picture(width, height, rgba_len(width, height)).is_some() {
            return Err(DecodeError::Failed(
                "the picture must be at most 8192 pixels on a side".into(),
            ));
        }
        let animated = info.animation_control.is_some_and(|a| a.num_frames > 0);
        let skip_default = animated && info.frame_control.is_none();
        let size = reader
            .output_buffer_size()
            .ok_or_else(|| DecodeError::Failed("the PNG is too large".into()))?;
        Ok(Self {
            reader,
            width,
            height,
            canvas: vec![0; rgba_len(width, height)],
            buffer: vec![0; size],
            rgba: Vec::new(),
            animated,
            skip_default,
            done: false,
            dispose: None,
        })
    }

    /// Decodes the next sub-frame into `rgba`; `None` at the end.
    fn read(&mut self) -> Result<Option<png::OutputInfo>, DecodeError> {
        if self.done {
            return Ok(None);
        }
        match self.reader.next_frame(&mut self.buffer) {
            Ok(out) => {
                let (color, _) = self.reader.output_color_type();
                to_rgba(color, &self.buffer[..out.buffer_size()], &mut self.rgba)?;
                Ok(Some(out))
            }
            Err(png::DecodingError::Parameter(_)) => {
                // Polled after the last frame.
                self.done = true;
                Ok(None)
            }
            Err(e) => Err(failed(e)),
        }
    }

    fn next(&mut self) -> Result<Option<Composed>, DecodeError> {
        if !self.animated {
            // One picture, shown and held.
            let Some(out) = self.read()? else {
                return Ok(None);
            };
            self.done = true;
            let rect = Rect {
                left: 0,
                top: 0,
                width: out.width,
                height: out.height,
            };
            put_rect(&mut self.canvas, self.width, self.height, rect, &self.rgba);
            return Ok(Some(Composed {
                rgba: self.canvas.clone(),
                delay: Duration::ZERO,
            }));
        }
        if std::mem::take(&mut self.skip_default) && self.read()?.is_none() {
            return Ok(None);
        }
        if let Some((op, rect, saved)) = self.dispose.take() {
            match op {
                png::DisposeOp::Background => {
                    clear_rect(&mut self.canvas, self.width, self.height, rect)
                }
                png::DisposeOp::Previous => {
                    if let Some(saved) = saved {
                        put_rect(&mut self.canvas, self.width, self.height, rect, &saved);
                    }
                }
                png::DisposeOp::None => {}
            }
        }
        if self.read()?.is_none() {
            return Ok(None);
        }
        let Some(fc) = self.reader.info().frame_control else {
            return Err(DecodeError::Failed(
                "an APNG frame has no frame control".into(),
            ));
        };
        let rect = Rect {
            left: fc.x_offset,
            top: fc.y_offset,
            width: fc.width,
            height: fc.height,
        };
        let saved = (fc.dispose_op == png::DisposeOp::Previous)
            .then(|| get_rect(&self.canvas, self.width, self.height, rect));
        match fc.blend_op {
            png::BlendOp::Source => {
                put_rect(&mut self.canvas, self.width, self.height, rect, &self.rgba)
            }
            png::BlendOp::Over => {
                over_rect(&mut self.canvas, self.width, self.height, rect, &self.rgba)
            }
        }
        self.dispose = Some((fc.dispose_op, rect, saved));
        Ok(Some(Composed {
            rgba: self.canvas.clone(),
            delay: delay_rule(apng_delay(&fc)),
        }))
    }
}

fn apng_delay(fc: &png::FrameControl) -> Duration {
    // A denominator of 0 means 100 (the APNG specification).
    let den = if fc.delay_den == 0 {
        100
    } else {
        u64::from(fc.delay_den)
    };
    Duration::from_nanos(u64::from(fc.delay_num).saturating_mul(1_000_000_000) / den)
}

fn png_duration(path: &Path) -> Result<Option<Duration>, DecodeError> {
    let reader = BufReader::new(File::open(path).map_err(failed)?);
    let mut reader = png::Decoder::new(reader).read_info().map_err(failed)?;
    let Some(actl) = reader.info().animation_control else {
        return Ok(None);
    };
    if actl.num_frames == 0 {
        return Ok(None);
    }
    let mut total = Duration::ZERO;
    if let Some(fc) = reader.info().frame_control {
        total += delay_rule(apng_delay(&fc));
    }
    // The headers of the other frames, without decoding them.
    while let Ok(fc) = reader.next_frame_info() {
        total += delay_rule(apng_delay(fc));
    }
    Ok(Some(total))
}

// ---- WebP ----

struct WebpFrames {
    decoder: image_webp::WebPDecoder<BufReader<File>>,
    buffer: Vec<u8>,
    done: bool,
}

impl WebpFrames {
    fn new(reader: BufReader<File>) -> Result<Self, DecodeError> {
        let decoder = image_webp::WebPDecoder::new(reader).map_err(failed)?;
        let (width, height) = decoder.dimensions();
        if refuse_picture(width, height, rgba_len(width, height)).is_some() {
            return Err(DecodeError::Failed(
                "the picture must be at most 8192 pixels on a side".into(),
            ));
        }
        let size = decoder
            .output_buffer_size()
            .ok_or_else(|| DecodeError::Failed("the WebP is too large".into()))?;
        Ok(Self {
            decoder,
            buffer: vec![0; size],
            done: false,
        })
    }

    fn next(&mut self, width: u32, height: u32) -> Result<Option<Composed>, DecodeError> {
        if self.done {
            return Ok(None);
        }
        let delay = if self.decoder.is_animated() {
            match self.decoder.read_frame(&mut self.buffer) {
                Ok(ms) => delay_rule(Duration::from_millis(u64::from(ms))),
                Err(image_webp::DecodingError::NoMoreFrames) => {
                    self.done = true;
                    return Ok(None);
                }
                Err(e) => return Err(failed(e)),
            }
        } else {
            self.decoder.read_image(&mut self.buffer).map_err(failed)?;
            self.done = true;
            Duration::ZERO
        };
        let mut rgba = Vec::with_capacity(rgba_len(width, height));
        if self.decoder.has_alpha() {
            rgba.extend_from_slice(&self.buffer);
        } else {
            for px in self.buffer.chunks_exact(3) {
                rgba.extend_from_slice(&[px[0], px[1], px[2], 255]);
            }
        }
        Ok(Some(Composed { rgba, delay }))
    }
}

/// Sums the ANMF frame delays after the 100 ms rule, reading only chunk headers
/// (the decoder's `loop_duration` skips the rule).
fn webp_duration(path: &Path) -> Result<Option<Duration>, DecodeError> {
    let mut reader = BufReader::new(File::open(path).map_err(failed)?);
    let mut riff = [0u8; 12];
    reader.read_exact(&mut riff).map_err(failed)?;
    let mut total = Duration::ZERO;
    let mut frames = 0usize;
    loop {
        let mut head = [0u8; 8];
        match reader.read_exact(&mut head) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(failed(e)),
        }
        let size = u64::from(u32::from_le_bytes([head[4], head[5], head[6], head[7]]));
        let padded = size + (size & 1);
        let mut skip = padded;
        if &head[..4] == b"ANMF" && size >= 16 {
            let mut info = [0u8; 16];
            reader.read_exact(&mut info).map_err(failed)?;
            let ms = u32::from_le_bytes([info[12], info[13], info[14], 0]);
            total += delay_rule(Duration::from_millis(u64::from(ms)));
            frames += 1;
            skip -= 16;
        }
        std::io::copy(&mut (&mut reader).take(skip), &mut std::io::sink()).map_err(failed)?;
    }
    Ok((frames > 0).then_some(total))
}

// ---- Canvas helpers ----

/// Converts 8-bit PNG output of `color` to `Rgba8`.
fn to_rgba(color: png::ColorType, src: &[u8], out: &mut Vec<u8>) -> Result<(), DecodeError> {
    out.clear();
    match color {
        png::ColorType::Rgba => out.extend_from_slice(src),
        png::ColorType::Rgb => {
            for px in src.chunks_exact(3) {
                out.extend_from_slice(&[px[0], px[1], px[2], 255]);
            }
        }
        png::ColorType::GrayscaleAlpha => {
            for px in src.chunks_exact(2) {
                out.extend_from_slice(&[px[0], px[0], px[0], px[1]]);
            }
        }
        png::ColorType::Grayscale => {
            for &g in src {
                out.extend_from_slice(&[g, g, g, 255]);
            }
        }
        png::ColorType::Indexed => {
            return Err(DecodeError::Unsupported(
                "an indexed PNG that was not expanded".into(),
            ));
        }
    }
    Ok(())
}

/// The part of `rect` inside the canvas, as row ranges: (canvas start, source start,
/// bytes per row) for each row.
fn rows(width: u32, height: u32, rect: Rect) -> impl Iterator<Item = (usize, usize, usize)> {
    let x0 = rect.left.min(width);
    let x1 = rect.left.saturating_add(rect.width).min(width);
    let y0 = rect.top.min(height);
    let y1 = rect.top.saturating_add(rect.height).min(height);
    let cols = (x1 - x0) as usize * 4;
    (y0..y1).map(move |y| {
        let canvas = ((y as usize) * width as usize + x0 as usize) * 4;
        let source =
            (((y - rect.top) as usize) * rect.width as usize + (x0 - rect.left) as usize) * 4;
        (canvas, source, cols)
    })
}

fn clear_rect(canvas: &mut [u8], width: u32, height: u32, rect: Rect) {
    for (c, _, n) in rows(width, height, rect) {
        canvas[c..c + n].fill(0);
    }
}

fn get_rect(canvas: &[u8], width: u32, height: u32, rect: Rect) -> Vec<u8> {
    let mut out = vec![0; rgba_len(rect.width, rect.height)];
    for (c, s, n) in rows(width, height, rect) {
        out[s..s + n].copy_from_slice(&canvas[c..c + n]);
    }
    out
}

fn put_rect(canvas: &mut [u8], width: u32, height: u32, rect: Rect, src: &[u8]) {
    for (c, s, n) in rows(width, height, rect) {
        if let Some(from) = src.get(s..s + n) {
            canvas[c..c + n].copy_from_slice(from);
        }
    }
}

/// GIF: a pixel with alpha 0 keeps the canvas pixel; any other replaces it.
fn blend_rect(canvas: &mut [u8], width: u32, height: u32, rect: Rect, src: &[u8], keyed: bool) {
    for (c, s, n) in rows(width, height, rect) {
        let Some(from) = src.get(s..s + n) else {
            continue;
        };
        for (dst, px) in canvas[c..c + n]
            .chunks_exact_mut(4)
            .zip(from.chunks_exact(4))
        {
            if !keyed || px[3] != 0 {
                dst.copy_from_slice(px);
            }
        }
    }
}

/// APNG `OVER`: straight-alpha source over straight-alpha canvas.
fn over_rect(canvas: &mut [u8], width: u32, height: u32, rect: Rect, src: &[u8]) {
    for (c, s, n) in rows(width, height, rect) {
        let Some(from) = src.get(s..s + n) else {
            continue;
        };
        for (dst, px) in canvas[c..c + n]
            .chunks_exact_mut(4)
            .zip(from.chunks_exact(4))
        {
            let sa = u32::from(px[3]);
            if sa == 255 {
                dst.copy_from_slice(px);
                continue;
            }
            if sa == 0 {
                continue;
            }
            let da = u32::from(dst[3]);
            // out_a = sa + da (1 - sa), in 0..=255 units.
            let out_a = sa * 255 + da * (255 - sa);
            if out_a == 0 {
                dst.fill(0);
                continue;
            }
            for i in 0..3 {
                let v = (u32::from(px[i]) * sa * 255 + u32::from(dst[i]) * da * (255 - sa)) / out_a;
                dst[i] = v.min(255) as u8;
            }
            dst[3] = (out_a / 255).min(255) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_bytes_name_the_container() {
        assert_eq!(sniff(b"GIF89a......"), Some(Kind::Gif));
        assert_eq!(sniff(b"GIF87a"), Some(Kind::Gif));
        assert_eq!(
            sniff(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]),
            Some(Kind::Png)
        );
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8X"), Some(Kind::Webp));
        assert_eq!(sniff(b"\0\0\0\x18ftypmp42"), None);
    }

    #[test]
    fn an_apng_delay_with_a_zero_denominator_is_in_hundredths() {
        let fc = png::FrameControl {
            delay_num: 25,
            delay_den: 0,
            ..png::FrameControl::default()
        };
        assert_eq!(apng_delay(&fc), Duration::from_millis(250));
    }

    #[test]
    fn a_sub_frame_off_the_canvas_is_clipped() {
        let mut canvas = vec![0u8; 2 * 2 * 4];
        let rect = Rect {
            left: 1,
            top: 1,
            width: 2,
            height: 2,
        };
        put_rect(&mut canvas, 2, 2, rect, &[9u8; 16]);
        assert_eq!(&canvas[12..16], &[9, 9, 9, 9]);
        assert!(canvas[..12].iter().all(|b| *b == 0));
    }

    #[test]
    fn over_keeps_an_opaque_canvas_under_half_alpha() {
        let mut canvas = vec![0, 0, 255, 255];
        let rect = Rect {
            left: 0,
            top: 0,
            width: 1,
            height: 1,
        };
        over_rect(&mut canvas, 1, 1, rect, &[255, 0, 0, 128]);
        assert_eq!(canvas[3], 255);
        assert!(canvas[0] > 120 && canvas[0] < 135, "{canvas:?}");
        assert!(canvas[2] > 120 && canvas[2] < 135, "{canvas:?}");
    }
}
