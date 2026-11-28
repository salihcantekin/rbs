//! The VideoToolbox live H.264 decoder: the port's decoder suite for system-memory and
//! native output, the native pool (exhaustion, release, dropping the decoder while
//! holding pictures), an in-band size change, a cropped 1080p stream, and hostile bytes.
//! Clips are encoded at test time with this crate's VideoToolbox encoder. A machine
//! without an H.264 encoder skips with a reason, unless `RBS_REQUIRE_VIDEOTOOLBOX=1`.

#![cfg(target_os = "macos")]
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod decode_support;

use decode_support::{FIRST, STEP_NS, clip, decode_clip, two_size_clip};
use rbs_codec::conformance::{self, DecodeClip};
use rbs_codec::{
    CreateError, DecodeOutput, VideoDecodeError, VideoDecoder, VideoDecoderConfig,
    VideoDecoderFactory, caps,
};
use rbs_codec_videotoolbox::{VideoToolboxDecoderFactory, VtH264Decoder};
use rbs_core::capability::CapabilitySet;
use rbs_core::media::{
    BitstreamFormat, Bytes, CodecId, EncodedPacket, FramePayload, MemoryKind, NativeSurface, Size,
    SurfaceSync, VideoFrame,
};
use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};
use rbs_core::time::Timestamp;

const W: u32 = 320;
const H: u32 = 240;

fn required() -> bool {
    std::env::var("RBS_REQUIRE_VIDEOTOOLBOX").is_ok_and(|v| v == "1")
}

/// The suite's clip, or `None` (skip) on a machine without the encoder.
fn suite_clip(frames: usize) -> Option<DecodeClip> {
    match decode_clip(W, H, frames) {
        Ok(clip) => Some(clip),
        Err(e) => {
            assert!(
                !required(),
                "RBS_REQUIRE_VIDEOTOOLBOX=1 but no encoder: {e}"
            );
            eprintln!("no VideoToolbox H.264 encoder to make a clip ({e}): skipped");
            None
        }
    }
}

fn native() -> DecodeOutput {
    DecodeOutput::system().accepting(MemoryKind::Metal)
}

/// The real factory, with every session forced to `target` (the suite always asks
/// for system memory).
struct WithTarget(VideoToolboxDecoderFactory, DecodeOutput);

impl AdapterFactory for WithTarget {
    fn id(&self) -> AdapterId {
        self.0.id()
    }
    fn display_name(&self) -> &str {
        self.0.display_name()
    }
    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        self.0.probe()
    }
}

impl VideoDecoderFactory for WithTarget {
    fn create(&self, config: &VideoDecoderConfig) -> Result<Box<dyn VideoDecoder>, CreateError> {
        self.0.create(&config.clone().with_target(self.1))
    }
}

fn session(target: DecodeOutput, min_surfaces: usize) -> VtH264Decoder {
    let config = VideoDecoderConfig::h264_annex_b(Size::new(1920, 1088))
        .with_target(target)
        .with_min_surfaces(min_surfaces);
    VideoToolboxDecoderFactory::new()
        .create_session(&config)
        .unwrap()
}

fn decode(d: &mut VtH264Decoder, packets: &[EncodedPacket], out: &mut Vec<VideoFrame>) {
    for p in packets {
        d.submit(p).unwrap();
        d.drain(out).unwrap();
    }
}

fn surface_of(frame: &VideoFrame) -> usize {
    match frame.payload() {
        FramePayload::Native {
            surface: NativeSurface::IoSurface { surface, .. },
            ..
        } => *surface,
        other => panic!("not an IOSurface: {other:?}"),
    }
}

#[test]
fn probe_declares_h264_and_memory_kinds() {
    let caps = VideoToolboxDecoderFactory::new().probe().unwrap();
    let set = |k: rbs_core::capability::CapabilityKey| rbs_core::conformance::set_values(&caps, &k);
    assert_eq!(set(caps::VIDEO_DECODE), ["h264"]);
    assert_eq!(set(caps::DECODE_OUTPUT_MEMORY), ["system", "metal"]);
    assert!(caps.get(&caps::HARDWARE).is_some());
}

#[test]
fn passes_the_decoder_suite_in_system_memory() {
    let Some(clip) = suite_clip(10) else { return };
    let report = conformance::video_decoder(&VideoToolboxDecoderFactory::new(), &clip);
    eprintln!("{report}");
    report.assert_passed();
}

#[test]
fn passes_the_decoder_suite_with_native_output() {
    let Some(clip) = suite_clip(10) else { return };
    let factory = WithTarget(VideoToolboxDecoderFactory::new(), native());
    let report = conformance::video_decoder(&factory, &clip);
    eprintln!("{report}");
    report.assert_passed();
}

#[test]
fn native_pictures_are_nv12_io_surfaces_with_their_stamps() {
    let Some(clip) = suite_clip(4) else { return };
    let mut d = session(native(), 3);
    let mut out = Vec::new();
    decode(&mut d, &clip.packets[..1], &mut out);
    assert_eq!(out.len(), 1);
    let FramePayload::Native { surface, sync, .. } = out[0].payload() else {
        panic!("not native: {:?}", out[0].memory_kind());
    };
    assert_eq!(*sync, SurfaceSync::Ready);
    assert_eq!(surface.memory_kind(), MemoryKind::Metal);
    assert_eq!(out[0].format().size, Size::new(W, H));
    assert_eq!(out[0].timestamp(), clip.packets[0].pts());
    assert!(d.is_native());
    assert_eq!(d.max_surfaces(), 3);
    assert_eq!(d.stats().cpu_copies, 0);
}

#[test]
fn system_memory_output_has_no_surfaces_and_counts_copies() {
    let Some(clip) = suite_clip(3) else { return };
    let mut d = session(DecodeOutput::system(), 3);
    let mut out = Vec::new();
    decode(&mut d, &clip.packets, &mut out);
    assert_eq!(out.len(), 3);
    assert!(out.iter().all(|f| f.memory_kind() == MemoryKind::System));
    assert_eq!(d.max_surfaces(), 0);
    assert_eq!(d.stats().cpu_copies, 3);
}

#[test]
fn a_full_pool_drops_pictures_and_recovers_after_release() {
    let Some(clip) = suite_clip(8) else { return };
    let mut d = session(native(), 3);
    assert_eq!(d.max_surfaces(), 3);
    let mut held = Vec::new();
    decode(&mut d, &clip.packets[..6], &mut held);
    assert_eq!(held.len(), 3, "never more pictures than max_surfaces");
    assert_eq!(d.stats().dropped_no_surface, 3);
    let leased: Vec<usize> = held.iter().map(surface_of).collect();
    held.clear(); // releases every lease
    let mut more = Vec::new();
    decode(&mut d, &clip.packets[6..], &mut more);
    assert_eq!(more.len(), 2, "decoding continues after release");
    assert_eq!(more[0].timestamp(), clip.packets[6].pts());
    assert_eq!(more[1].timestamp(), clip.packets[7].pts());
    drop(leased);
}

#[test]
fn a_leased_surface_is_not_handed_out_again() {
    let Some(clip) = suite_clip(6) else { return };
    let mut d = session(native(), 3);
    let mut first = Vec::new();
    decode(&mut d, &clip.packets[..1], &mut first);
    let kept = surface_of(&first[0]);
    let mut others = Vec::new();
    for p in &clip.packets[1..] {
        d.submit(p).unwrap();
        d.drain(&mut others).unwrap();
        // Keep one more lease at most, so slots keep coming free.
        if others.len() > 1 {
            others.remove(0);
        }
        assert!(others.iter().all(|f| surface_of(f) != kept));
    }
    assert_eq!(d.stats().dropped_no_surface, 0);
}

#[test]
fn dropping_the_decoder_while_holding_pictures_keeps_them_valid() {
    let Some(clip) = suite_clip(3) else { return };
    let mut d = session(native(), 3);
    let mut held = Vec::new();
    decode(&mut d, &clip.packets, &mut held);
    drop(d);
    assert_eq!(held.len(), 3);
    for f in &held {
        assert_eq!(f.format().size, Size::new(W, H));
        assert_ne!(surface_of(f), 0);
    }
    drop(held); // the last leases release the buried pool on this thread, freeing nothing
}

#[test]
fn an_in_band_size_change_is_followed() {
    let packets = match two_size_clip((W, H), (256, 144), 3) {
        Ok(p) => p,
        Err(e) => {
            assert!(!required(), "{e}");
            return;
        }
    };
    for target in [DecodeOutput::system(), native()] {
        let mut d = session(target, 3);
        let mut out = Vec::new();
        let mut seen = Vec::new(); // (size, stamp); pictures are released at once
        for p in &packets {
            d.submit(p).unwrap();
            d.drain(&mut out).unwrap();
            seen.extend(out.drain(..).map(|f| (f.format().size, f.timestamp())));
        }
        d.flush(&mut out).unwrap();
        seen.extend(out.drain(..).map(|f| (f.format().size, f.timestamp())));
        let expected: Vec<(Size, Timestamp)> = packets
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let size = if i < 3 {
                    Size::new(W, H)
                } else {
                    Size::new(256, 144)
                };
                (size, p.pts())
            })
            .collect();
        assert_eq!(seen, expected);
        assert_eq!(d.output_format().map(|f| f.size), Some(Size::new(256, 144)));
    }
}

/// xorshift64*: deterministic bytes.
fn noise(seed: &mut u64, len: usize) -> Vec<u8> {
    (0..len)
        .map(|_| {
            *seed ^= *seed >> 12;
            *seed ^= *seed << 25;
            *seed ^= *seed >> 27;
            (seed.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 56) as u8
        })
        .collect()
}

fn packet(bytes: &[u8], pts: Timestamp, keyframe: bool) -> EncodedPacket {
    EncodedPacket::new(
        CodecId::H264,
        BitstreamFormat::AnnexB,
        pts,
        pts,
        keyframe,
        Bytes::from(bytes.to_vec()),
    )
    .unwrap()
}

#[test]
fn hostile_bytes_give_typed_errors_and_decoding_continues() {
    let Some(clip) = suite_clip(4) else { return };
    let mut d = session(DecodeOutput::system(), 3);
    let key = &clip.packets[0];
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let mut cases: Vec<Vec<u8>> = Vec::new();
    for len in [1usize, 3, 4, 5, 17, 300, 5000] {
        cases.push(noise(&mut seed, len));
        let mut framed = vec![0, 0, 0, 1];
        framed.extend(noise(&mut seed, len));
        cases.push(framed);
    }
    for cut in [5usize, 9, 12, 20, 40] {
        cases.push(key.data()[..cut.min(key.data().len())].to_vec());
    }
    for t in [0u8, 13, 14, 15, 20] {
        cases.push(vec![0, 0, 0, 1, t, 0x42, 0, 0x1e]);
    }
    // Floods of SEI and access unit delimiters, alone and around a slice.
    let mut flood = Vec::new();
    for _ in 0..2000 {
        flood.extend_from_slice(&[
            0, 0, 0, 1, 0x09, 0xf0, 0, 0, 1, 0x06, 0x05, 0x01, 0x00, 0x80,
        ]);
    }
    cases.push(flood.clone());
    let mut flooded_delta = flood.clone();
    flooded_delta.extend_from_slice(clip.packets[1].data());
    cases.push(flooded_delta);
    // Only bytes that pass every check (valid parameter sets, a truncated or corrupted
    // slice) reach VideoToolbox, which may refuse them (`kVTVideoDecoderBadDataErr`):
    // the cases where a platform `Failed` is allowed.
    let platform_from = cases.len();
    for cut in [key.data().len() / 2, key.data().len() - 1] {
        cases.push(key.data()[..cut].to_vec());
    }
    let mut corrupted = key.data().to_vec();
    for b in corrupted.iter_mut().skip(40).step_by(7) {
        *b ^= 0x5a;
    }
    cases.push(corrupted);
    let mut out = Vec::new();
    for (round, bytes) in cases.iter().enumerate() {
        // Alternate between a gated session and one mid-stream.
        if round % 2 == 0 {
            let _ = d.submit(key);
        }
        for keyframe in [true, false] {
            match d.submit(&packet(bytes, key.pts(), keyframe)) {
                Ok(())
                | Err(
                    VideoDecodeError::InvalidInput(_)
                    | VideoDecodeError::Unsupported(_)
                    | VideoDecodeError::NeedKeyframe,
                ) => {}
                Err(VideoDecodeError::Failed { .. }) if round >= platform_from => {}
                Err(e) => panic!("case {round}: unexpected {e:?}"),
            }
            let _ = d.drain(&mut out);
            out.clear();
        }
    }
    d.reset().unwrap();
    let mut frames = Vec::new();
    decode(&mut d, &clip.packets, &mut frames);
    d.flush(&mut frames).unwrap();
    assert_eq!(frames.len(), clip.packets.len());
    for (f, p) in frames.iter().zip(&clip.packets) {
        assert_eq!(f.timestamp(), p.pts());
    }
}

#[test]
fn packets_without_a_slice_wait_for_no_picture() {
    let Some(clip) = suite_clip(6) else { return };
    let mut d = session(DecodeOutput::system(), 3);
    let mut frames = Vec::new();
    decode(&mut d, &clip.packets[..2], &mut frames);
    let aud = [0, 0, 0, 1, 0x09, 0xf0];
    let sei = [0, 0, 0, 1, 0x06, 0x05, 0x01, 0x00, 0x80];
    for i in 0..20u64 {
        let bytes: &[u8] = if i % 2 == 0 { &aud } else { &sei };
        d.submit(&packet(bytes, Timestamp::from_nanos(5 + i), false))
            .unwrap();
        d.drain(&mut frames).unwrap();
    }
    decode(&mut d, &clip.packets[2..], &mut frames);
    d.flush(&mut frames).unwrap();
    let stamps: Vec<_> = frames.iter().map(VideoFrame::timestamp).collect();
    let expected: Vec<_> = clip.packets.iter().map(EncodedPacket::pts).collect();
    assert_eq!(
        stamps, expected,
        "no stamp of an SEI or AUD packet, none shifted"
    );
}

#[test]
fn a_mid_stream_sps_above_max_size_is_refused_and_gates() {
    let Some(clip) = suite_clip(4) else { return };
    let big = clip_or_skip(640, 480, 1);
    let config = VideoDecoderConfig::h264_annex_b(Size::new(W, H));
    let mut d = VideoToolboxDecoderFactory::new()
        .create_session(&config)
        .unwrap();
    let mut frames = Vec::new();
    decode(&mut d, &clip.packets[..2], &mut frames);
    assert!(matches!(
        d.submit(&big[0]),
        Err(VideoDecodeError::InvalidInput(_))
    ));
    assert!(d.stats().need_keyframe);
    assert_eq!(
        d.submit(&clip.packets[2]),
        Err(VideoDecodeError::NeedKeyframe)
    );
    // The refused packet left the format alone, and a keyframe resumes decoding.
    assert_eq!(d.output_format().map(|f| f.size), Some(Size::new(W, H)));
    d.drain(&mut frames).unwrap();
    let before = frames.len();
    decode(&mut d, &clip.packets[..1], &mut frames);
    d.flush(&mut frames).unwrap();
    assert_eq!(frames.len(), before + 1);
}

fn clip_or_skip(w: u32, h: u32, frames: usize) -> Vec<EncodedPacket> {
    clip(w, h, frames, FIRST, STEP_NS).unwrap()
}

#[test]
fn length_prefixed_input_and_too_many_surfaces_are_refused_at_create() {
    let config = VideoDecoderConfig::h264_annex_b(Size::new(640, 480))
        .length_prefixed(4, Bytes::from_static(&[1, 66, 0, 30]));
    assert!(matches!(
        VideoToolboxDecoderFactory::new().create(&config),
        Err(CreateError::Unsupported { .. })
    ));
    let config = VideoDecoderConfig::h264_annex_b(Size::new(640, 480))
        .with_target(native())
        .with_min_surfaces(9);
    assert!(matches!(
        VideoToolboxDecoderFactory::new().create(&config),
        Err(CreateError::Unsupported { .. })
    ));
}

#[test]
fn a_cropped_1080p_stream_has_its_display_size() {
    let Some(_) = suite_clip(1) else { return };
    // Coded at 1088 lines, cropped to 1080 by the SPS.
    let packets = clip(1920, 1080, 2, Timestamp::from_nanos(11), STEP_NS).unwrap();
    for target in [DecodeOutput::system(), native()] {
        let mut d = session(target, 3);
        let mut frames = Vec::new();
        decode(&mut d, &packets, &mut frames);
        d.flush(&mut frames).unwrap();
        assert_eq!(frames.len(), 2);
        assert!(
            frames
                .iter()
                .all(|f| f.format().size == Size::new(1920, 1080))
        );
        if target == native() {
            assert!(d.is_native(), "a 1080p picture is handed out natively");
        }
        if let Some(FramePayload::System(s)) = frames.first().map(VideoFrame::payload) {
            // The moving gradient's first luma row survives the copy (no row shift).
            let row = &s.data()[..16];
            assert!(
                row.windows(2).filter(|w| w[1] > w[0]).count() >= 8,
                "{row:?}"
            );
        }
    }
}
