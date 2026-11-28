//! The Media Foundation live H.264 decoder: the port's decoder suite for system-memory
//! and native output, the native pool (exhaustion, release, dropping the decoder while
//! holding pictures), an in-band size change, and hostile bytes. Clips are encoded at
//! test time with Microsoft's H.264 encoder. A machine without the decoder or encoder
//! skips with a reason, unless `RBS_REQUIRE_MF_H264=1`.
#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use rbs_codec::conformance::{self, DecodeClip};
use rbs_codec::{
    CreateError, DecodeError, DecodeOutput, VideoDecoder, VideoDecoderConfig, VideoDecoderFactory,
    caps,
};
use rbs_codec_mf::MfH264DecoderFactory;
use rbs_codec_mf::decode::MfH264Decoder;
use rbs_codec_mf::testing::h264;
use rbs_core::capability::CapabilitySet;
use rbs_core::media::{
    BitstreamFormat, Bytes, CodecId, EncodedPacket, FramePayload, MemoryKind, Size, SurfaceSync,
    VideoFrame,
};
use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};
use rbs_core::time::Timestamp;

const W: u32 = 320;
const H: u32 = 240;
/// Not a multiple of 100 ns, so rounding to Media Foundation time would show.
const STEP_NS: u64 = 33_333_337;

fn required() -> bool {
    std::env::var("RBS_REQUIRE_MF_H264").is_ok_and(|v| v == "1")
}

/// The clip, or `None` (skip) on a machine without the encoder or decoder.
fn clip(frames: usize) -> Option<DecodeClip> {
    let probe = MfH264DecoderFactory::new().probe().unwrap();
    if probe.is_empty() {
        assert!(
            !required(),
            "RBS_REQUIRE_MF_H264=1 but no H.264 decoder MFT"
        );
        eprintln!("no Media Foundation H.264 decoder here: skipped");
        return None;
    }
    match h264::clip(W, H, frames, Timestamp::from_nanos(1_000_000_007), STEP_NS) {
        Ok(packets) => Some(DecodeClip {
            codec: CodecId::H264,
            packets,
            size: Size::new(W, H),
            lookahead: 0,
        }),
        Err(e) => {
            assert!(!required(), "RBS_REQUIRE_MF_H264=1 but no encoder: {e}");
            eprintln!("no H.264 encoder MFT to make a clip ({e}): skipped");
            None
        }
    }
}

fn native_target() -> Option<DecodeOutput> {
    h264::default_adapter_luid().map(|luid| {
        DecodeOutput::system()
            .accepting(MemoryKind::D3d11)
            .on_adapter(luid)
    })
}

/// The real factory, with every session forced to `target` (the suite always asks
/// for system memory).
struct WithTarget(MfH264DecoderFactory, DecodeOutput);

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

fn session(target: DecodeOutput, min_surfaces: usize) -> MfH264Decoder {
    let config = VideoDecoderConfig::h264_annex_b(Size::new(1920, 1088))
        .with_target(target)
        .with_min_surfaces(min_surfaces);
    MfH264DecoderFactory::new().create_session(&config).unwrap()
}

#[test]
fn probe_declares_h264_and_memory_kinds() {
    let caps = MfH264DecoderFactory::new().probe().unwrap();
    if caps.is_empty() {
        assert!(!required());
        return;
    }
    let set = |k: rbs_core::capability::CapabilityKey| rbs_core::conformance::set_values(&caps, &k);
    assert_eq!(set(caps::VIDEO_DECODE), ["h264"]);
    assert_eq!(set(caps::DECODE_OUTPUT_MEMORY), ["system", "d3d11"]);
}

#[test]
fn passes_the_decoder_suite_in_system_memory() {
    let Some(clip) = clip(10) else { return };
    let report = conformance::video_decoder(&MfH264DecoderFactory::new(), &clip);
    eprintln!("{report}");
    report.assert_passed();
}

#[test]
fn passes_the_decoder_suite_with_native_output() {
    let Some(clip) = clip(10) else { return };
    let Some(target) = native_target() else {
        eprintln!("no DXGI adapter: skipped");
        return;
    };
    let factory = WithTarget(MfH264DecoderFactory::new(), target);
    let report = conformance::video_decoder(&factory, &clip);
    eprintln!("{report}");
    report.assert_passed();
}

fn decode(d: &mut MfH264Decoder, packets: &[EncodedPacket], out: &mut Vec<VideoFrame>) {
    for p in packets {
        d.submit(p).unwrap();
        d.drain(out).unwrap();
    }
}

#[test]
fn native_pictures_are_shared_nv12_on_the_target_adapter() {
    let (Some(clip), Some(target)) = (clip(4), native_target()) else {
        return;
    };
    let mut d = session(target, 3);
    if !d.is_native() {
        eprintln!("the decoder MFT refused DXVA here: native checks skipped");
        return;
    }
    let mut out = Vec::new();
    decode(&mut d, &clip.packets[..1], &mut out);
    assert_eq!(out.len(), 1);
    let FramePayload::Native { surface, sync, .. } = out[0].payload() else {
        panic!("not native: {:?}", out[0].memory_kind());
    };
    assert_eq!(*sync, SurfaceSync::Ready);
    assert_eq!(surface.memory_kind(), MemoryKind::D3d11);
    assert_eq!(out[0].format().size, Size::new(W, H));
    assert_eq!(out[0].timestamp(), clip.packets[0].pts());
    assert_eq!(d.stats().cpu_copies, 0);
}

#[test]
fn a_full_pool_drops_pictures_and_recovers_after_release() {
    let (Some(clip), Some(target)) = (clip(8), native_target()) else {
        return;
    };
    let mut d = session(target, 3);
    if !d.is_native() {
        eprintln!("the decoder MFT refused DXVA here: pool checks skipped");
        return;
    }
    assert_eq!(d.max_surfaces(), 3);
    let mut held = Vec::new();
    decode(&mut d, &clip.packets[..6], &mut held);
    assert_eq!(held.len(), 3, "never more pictures than max_surfaces");
    assert_eq!(d.stats().dropped_no_surface, 3);
    held.clear(); // releases every lease
    let mut more = Vec::new();
    decode(&mut d, &clip.packets[6..], &mut more);
    assert_eq!(more.len(), 2, "decoding continues after release");
    assert_eq!(more[0].timestamp(), clip.packets[6].pts());
}

#[test]
fn dropping_the_decoder_while_holding_pictures_keeps_them_valid() {
    let (Some(clip), Some(target)) = (clip(3), native_target()) else {
        return;
    };
    let mut d = session(target, 3);
    let mut held = Vec::new();
    decode(&mut d, &clip.packets, &mut held);
    drop(d);
    assert_eq!(held.len(), 3);
    for f in &held {
        assert_eq!(f.format().size, Size::new(W, H));
    }
    drop(held); // the last leases release the buried pool on this thread, freeing nothing
}

#[test]
fn an_in_band_size_change_is_followed() {
    if clip(1).is_none() {
        return;
    }
    let packets = h264::two_size_clip((W, H), (256, 144), 3, STEP_NS).unwrap();
    for target in [Some(DecodeOutput::system()), native_target()]
        .into_iter()
        .flatten()
    {
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

#[test]
fn hostile_bytes_give_typed_errors_and_decoding_continues() {
    let Some(clip) = clip(4) else { return };
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
    for cut in [
        5usize,
        9,
        12,
        20,
        40,
        key.data().len() / 2,
        key.data().len() - 1,
    ] {
        cases.push(key.data()[..cut.min(key.data().len())].to_vec());
    }
    let mut corrupted = key.data().to_vec();
    for b in corrupted.iter_mut().skip(40).step_by(7) {
        *b ^= 0x5a;
    }
    for t in [0u8, 13, 14, 15, 20] {
        cases.push(vec![0, 0, 0, 1, t, 0x42, 0, 0x1e]);
    }
    // Only bytes that pass every check (a valid SPS, a corrupted slice) reach the MFT,
    // which may refuse them: the one case where a platform `Failed` is allowed.
    let platform_case = cases.len();
    cases.push(corrupted);
    let mut out = Vec::new();
    for (round, bytes) in cases.iter().enumerate() {
        // Alternate between a gated session and one mid-stream.
        if round % 2 == 0 {
            let _ = d.submit(key);
        }
        for keyframe in [true, false] {
            let p = EncodedPacket::new(
                CodecId::H264,
                BitstreamFormat::AnnexB,
                key.pts(),
                key.pts(),
                keyframe,
                Bytes::from(bytes.clone()),
            )
            .unwrap();
            match d.submit(&p) {
                Ok(())
                | Err(
                    DecodeError::InvalidInput(_)
                    | DecodeError::Unsupported(_)
                    | DecodeError::NeedKeyframe,
                ) => {}
                Err(DecodeError::Failed { .. }) if round == platform_case => {}
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

fn packet(bytes: &[u8], pts: Timestamp) -> EncodedPacket {
    EncodedPacket::new(
        CodecId::H264,
        BitstreamFormat::AnnexB,
        pts,
        pts,
        false,
        Bytes::from(bytes.to_vec()),
    )
    .unwrap()
}

#[test]
fn packets_without_a_slice_wait_for_no_picture() {
    let Some(clip) = clip(6) else { return };
    let mut d = session(DecodeOutput::system(), 3);
    let mut frames = Vec::new();
    decode(&mut d, &clip.packets[..2], &mut frames);
    let aud = [0, 0, 0, 1, 0x09, 0xf0];
    let sei = [0, 0, 0, 1, 0x06, 0x05, 0x01, 0x00, 0x80];
    for i in 0..20u64 {
        let bytes: &[u8] = if i % 2 == 0 { &aud } else { &sei };
        d.submit(&packet(bytes, Timestamp::from_nanos(5 + i)))
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
    let Some(clip) = clip(4) else { return };
    let big = h264::clip(640, 480, 1, Timestamp::from_nanos(3), STEP_NS).unwrap();
    let config = VideoDecoderConfig::h264_annex_b(Size::new(W, H));
    let mut d = MfH264DecoderFactory::new().create_session(&config).unwrap();
    let mut frames = Vec::new();
    decode(&mut d, &clip.packets[..2], &mut frames);
    assert!(matches!(
        d.submit(&big[0]),
        Err(DecodeError::InvalidInput(_))
    ));
    assert!(d.stats().need_keyframe);
    assert_eq!(d.submit(&clip.packets[2]), Err(DecodeError::NeedKeyframe));
    // The refused packet left the format alone, and a keyframe resumes decoding.
    assert_eq!(d.output_format().map(|f| f.size), Some(Size::new(W, H)));
    d.drain(&mut frames).unwrap();
    let before = frames.len();
    decode(&mut d, &clip.packets[..1], &mut frames);
    d.flush(&mut frames).unwrap();
    assert_eq!(frames.len(), before + 1);
}

#[test]
fn length_prefixed_input_is_refused_at_create() {
    let config = VideoDecoderConfig::h264_annex_b(Size::new(640, 480))
        .length_prefixed(4, Bytes::from_static(&[1, 66, 0, 30]));
    assert!(matches!(
        MfH264DecoderFactory::new().create(&config),
        Err(CreateError::Unsupported { .. })
    ));
}

#[test]
fn a_cropped_1080p_stream_has_its_display_size() {
    if clip(1).is_none() {
        return;
    }
    // Coded at 1088 lines, cropped to 1080 by the SPS.
    let packets = h264::clip(1920, 1080, 2, Timestamp::from_nanos(11), STEP_NS).unwrap();
    for target in [Some(DecodeOutput::system()), native_target()]
        .into_iter()
        .flatten()
    {
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
