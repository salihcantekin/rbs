//! H.264 clips for the decoder tests, made at test time with this crate's own
//! VideoToolbox encoder, so no video is committed. The live settings of the guest
//! source: High profile without B-frames, one keyframe at the start, Annex B, one access
//! unit per packet, SPS and PPS in band in the keyframe, no extradata.
//!
//! The encoder writes length-prefixed packets and an `avcC` record; this module turns
//! them into Annex B and puts the record's parameter sets in front of every keyframe.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)] // Test support.

use rbs_codec::conformance::DecodeClip;
use rbs_codec::{KeyframeInterval, RateControl, VideoEncoder, VideoEncoderConfig};
use rbs_codec_videotoolbox::VideoToolboxFactory;
use rbs_core::media::{
    BitstreamFormat, Bytes, CodecId, ColorSpace, EncodedPacket, MemoryKind, PixelFormat,
    PlaneLayout, Size, SystemFrame, VideoFormat, VideoFrame,
};
use rbs_core::time::Timestamp;
use rbs_core::timing::FrameRate;

/// Not a multiple of 100 ns or 1 µs, so any rounding of stamps shows.
pub const STEP_NS: u64 = 33_333_337;

/// First stamp of a clip (odd nanoseconds).
pub const FIRST: Timestamp = Timestamp::from_nanos(1_000_000_007);

/// Draws one NV12 input frame: `(frame, width, height, index)`.
pub type Painter<'a> = dyn FnMut(&mut [u8], usize, usize, usize) + 'a;

/// A moving gradient in luma, grey chroma.
fn gradient(nv12: &mut [u8], width: usize, height: usize, index: usize) {
    let (luma, chroma) = nv12.split_at_mut(width * height);
    for (i, px) in luma.iter_mut().enumerate() {
        let (x, y) = (i % width, i / width);
        *px = (x * 2 + y + index * 8) as u8;
    }
    chroma.fill(128);
}

/// Parameter sets of an `avcC` record, as Annex B (with start codes).
fn parameter_sets(avcc: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut at = 5;
    let sps_count = avcc[at] & 0x1f;
    at += 1;
    let read = |at: &mut usize, out: &mut Vec<u8>| {
        let len = usize::from(u16::from_be_bytes([avcc[*at], avcc[*at + 1]]));
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&avcc[*at + 2..*at + 2 + len]);
        *at += 2 + len;
    };
    for _ in 0..sps_count {
        read(&mut at, &mut out);
    }
    let pps_count = avcc[at];
    at += 1;
    for _ in 0..pps_count {
        read(&mut at, &mut out);
    }
    out
}

/// A length-prefixed (4-byte) access unit as Annex B.
fn annex_b(data: &[u8], out: &mut Vec<u8>) {
    let mut at = 0;
    while at + 4 <= data.len() {
        let len = u32::from_be_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]) as usize;
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&data[at + 4..at + 4 + len]);
        at += 4 + len;
    }
}

/// [`clip`] with each NV12 input frame drawn by `paint`. `Err` when this machine has no
/// H.264 encoder.
pub fn clip_with(
    width: u32,
    height: u32,
    frames: usize,
    first: Timestamp,
    step_ns: u64,
    paint: &mut Painter<'_>,
) -> Result<Vec<EncodedPacket>, String> {
    let format = VideoFormat::new(
        Size::new(width, height),
        PixelFormat::Nv12,
        ColorSpace::BT709_LIMITED,
    );
    let config = VideoEncoderConfig::new(
        CodecId::H264,
        format,
        FrameRate::FPS_30,
        MemoryKind::System,
        RateControl::Cbr {
            bitrate_bps: u64::from(width * height * 8).max(500_000),
        },
    )
    .with_keyframe_interval(KeyframeInterval::Frames(10_000))
    .with_max_b_frames(0)
    .with_profile("high");
    let mut encoder = VideoToolboxFactory::with_software()
        .create_session(&config)
        .map_err(|e| e.to_string())?;
    let header = parameter_sets(encoder.stream().extradata.as_ref());
    let (w, h) = (width as usize, height as usize);
    let planes = [
        PlaneLayout {
            offset: 0,
            stride: w,
        },
        PlaneLayout {
            offset: w * h,
            stride: w,
        },
    ];
    let mut encoded = Vec::new();
    for i in 0..frames {
        let mut picture = vec![0u8; w * h * 3 / 2];
        paint(&mut picture, w, h, i);
        let system = SystemFrame::new(format, Bytes::from(picture), &planes).unwrap();
        let ts = Timestamp::from_nanos(i as u64 * 33_333_333);
        let frame = VideoFrame::system(ts, system, None).with_sequence(i as u64);
        encoder.submit(&frame).map_err(|e| e.to_string())?;
        encoder.drain(&mut encoded).map_err(|e| e.to_string())?;
    }
    encoder.flush(&mut encoded).map_err(|e| e.to_string())?;
    if encoded.len() != frames {
        return Err(format!("{} packets for {frames} frames", encoded.len()));
    }
    encoded
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let mut data = Vec::with_capacity(p.data().len() + header.len() + 16);
            if p.keyframe() {
                data.extend_from_slice(&header);
            }
            annex_b(p.data(), &mut data);
            let pts = Timestamp::from_nanos(first.as_nanos() + i as u64 * step_ns);
            EncodedPacket::new(
                CodecId::H264,
                BitstreamFormat::AnnexB,
                pts,
                pts,
                p.keyframe(),
                Bytes::from(data),
            )
            .map_err(|e| format!("packet: {e}"))
        })
        .collect()
}

/// `frames` packets of a `width` x `height` clip with stamps `first + i * step_ns`.
pub fn clip(
    width: u32,
    height: u32,
    frames: usize,
    first: Timestamp,
    step_ns: u64,
) -> Result<Vec<EncodedPacket>, String> {
    clip_with(width, height, frames, first, step_ns, &mut gradient)
}

/// A clip whose frame `i` is one flat colour, `colors[i % colors.len()]`, given as
/// limited-range Y'CbCr bytes `(y, cb, cr)`.
pub fn flat_clip(
    width: u32,
    height: u32,
    colors: &[(u8, u8, u8)],
    frames: usize,
) -> Result<Vec<EncodedPacket>, String> {
    let mut flat = |nv12: &mut [u8], width: usize, height: usize, index: usize| {
        let (y, cb, cr) = colors[index % colors.len()];
        let (luma, chroma) = nv12.split_at_mut(width * height);
        luma.fill(y);
        for pair in chroma.chunks_exact_mut(2) {
            pair[0] = cb;
            pair[1] = cr;
        }
    };
    clip_with(width, height, frames, FIRST, STEP_NS, &mut flat)
}

/// Two clips one after the other, each starting with its own keyframe and SPS: the
/// second at another size (an in-band SPS change). Stamps continue.
pub fn two_size_clip(
    first_size: (u32, u32),
    second_size: (u32, u32),
    frames_each: usize,
) -> Result<Vec<EncodedPacket>, String> {
    let mut a = clip(first_size.0, first_size.1, frames_each, FIRST, STEP_NS)?;
    let next = Timestamp::from_nanos(FIRST.as_nanos() + frames_each as u64 * STEP_NS);
    let b = clip(second_size.0, second_size.1, frames_each, next, STEP_NS)?;
    a.extend(b);
    Ok(a)
}

/// The port suite's clip: the live case at `width` x `height`, lookahead 0.
pub fn decode_clip(width: u32, height: u32, frames: usize) -> Result<DecodeClip, String> {
    Ok(DecodeClip {
        codec: CodecId::H264,
        packets: clip(width, height, frames, FIRST, STEP_NS)?,
        size: Size::new(width, height),
        lookahead: 0,
    })
}
