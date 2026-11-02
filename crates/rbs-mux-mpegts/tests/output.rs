//! Checks the bytes the MPEG-TS muxer writes with a small demuxer.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::collections::HashMap;

use rbs_core::media::{
    BitstreamFormat, Bytes, ChunkKind, CodecId, ContainerId, EncodedPacket, MuxChunk,
    StreamDescriptor, StreamIndex,
};
use rbs_core::time::Timestamp;
use rbs_mux::conformance::{aac_stream, h264_stream};
use rbs_mux::{CreateError, MuxError, MuxerConfig, MuxerFactory};
use rbs_mux_mpegts::MpegTsMuxerFactory;

const SPS: [u8; 9] = [0x67, 0x64, 0x00, 0x1f, 0xac, 0xd9, 0x40, 0x50, 0x05];
const PPS: [u8; 4] = [0x68, 0xeb, 0xe3, 0xcb];

/// Payload units (PES packets) per PID, plus every PCR seen.
#[derive(Default)]
struct Demuxed {
    units: HashMap<u16, Vec<Vec<u8>>>,
    pcrs: Vec<u64>,
}

fn demux(bytes: &[u8]) -> Demuxed {
    assert_eq!(bytes.len() % 188, 0);
    let mut out = Demuxed::default();
    for p in bytes.chunks(188) {
        assert_eq!(p[0], 0x47);
        let pid = (u16::from(p[1] & 0x1f) << 8) | u16::from(p[2]);
        let mut at = 4;
        if p[3] & 0x20 != 0 {
            let len = usize::from(p[4]);
            if len > 0 && p[5] & 0x10 != 0 {
                let b = &p[6..11];
                out.pcrs.push(
                    (u64::from(b[0]) << 25)
                        | (u64::from(b[1]) << 17)
                        | (u64::from(b[2]) << 9)
                        | (u64::from(b[3]) << 1)
                        | u64::from(b[4] >> 7),
                );
            }
            at += 1 + len;
        }
        let units = out.units.entry(pid).or_default();
        if p[1] & 0x40 != 0 {
            units.push(Vec::new());
        }
        if let Some(unit) = units.last_mut() {
            unit.extend_from_slice(&p[at..]);
        }
    }
    out
}

fn read_ts(b: &[u8]) -> u64 {
    (u64::from(b[0] & 0x0e) << 29)
        | (u64::from(b[1]) << 22)
        | (u64::from(b[2] >> 1) << 15)
        | (u64::from(b[3]) << 7)
        | u64::from(b[4] >> 1)
}

/// (pts, dts, elementary stream bytes) of a PES packet.
fn parse_pes(pes: &[u8]) -> (u64, u64, &[u8]) {
    assert_eq!(&pes[..3], &[0, 0, 1]);
    let len = usize::from(u16::from_be_bytes([pes[4], pes[5]]));
    if len != 0 {
        assert_eq!(len + 6, pes.len(), "PES_packet_length");
    }
    let flags = pes[7];
    let header_len = usize::from(pes[8]);
    let pts = read_ts(&pes[9..14]);
    let dts = if flags & 0x40 != 0 {
        read_ts(&pes[14..19])
    } else {
        pts
    };
    (pts, dts, &pes[9 + header_len..])
}

fn at(ms: u64) -> Timestamp {
    Timestamp::from_nanos(1_000_000_000 + ms * 1_000_000)
}

fn h264(
    pts: u64,
    dts: u64,
    keyframe: bool,
    data: Vec<u8>,
    format: BitstreamFormat,
) -> EncodedPacket {
    EncodedPacket::new(
        CodecId::H264,
        format,
        at(pts),
        at(dts),
        keyframe,
        Bytes::from(data),
    )
    .unwrap()
}

fn length_prefixed(units: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for u in units {
        out.extend_from_slice(&(u.len() as u32).to_be_bytes());
        out.extend_from_slice(u);
    }
    out
}

fn concat(chunks: &[MuxChunk]) -> Vec<u8> {
    chunks.iter().flat_map(|c| c.data.iter().copied()).collect()
}

const LP4: BitstreamFormat = BitstreamFormat::LengthPrefixed { length_size: 4 };

#[test]
fn length_prefixed_h264_becomes_annexb_with_aud_and_parameter_sets() {
    let config = MuxerConfig::new(ContainerId::MpegTs, vec![h264_stream(), aac_stream()]);
    let mut muxer = MpegTsMuxerFactory.create(&config).unwrap();
    let mut chunks = Vec::new();
    muxer.begin(&mut chunks).unwrap();
    let idr: Vec<u8> = (0..500).map(|i| i as u8).collect();
    let mut idr_nal = vec![0x65];
    idr_nal.extend_from_slice(&idr);
    muxer
        .write(
            StreamIndex(0),
            &h264(66, 0, true, length_prefixed(&[&idr_nal]), LP4),
            &mut chunks,
        )
        .unwrap();
    let audio = EncodedPacket::new(
        CodecId::Aac,
        BitstreamFormat::Raw,
        at(10),
        at(10),
        true,
        Bytes::from_static(&[1, 2, 3, 4]),
    )
    .unwrap();
    muxer.write(StreamIndex(1), &audio, &mut chunks).unwrap();
    muxer
        .write(
            StreamIndex(0),
            &h264(33, 33, false, length_prefixed(&[&[0x41, 9, 9]]), LP4),
            &mut chunks,
        )
        .unwrap();
    muxer.finish(&mut chunks).unwrap();

    let kinds: Vec<_> = chunks.iter().map(|c| c.kind).collect();
    assert_eq!(kinds[0], ChunkKind::Header);
    assert!(matches!(kinds[1], ChunkKind::Media { keyframe: true, .. }));
    assert!(
        matches!(
            kinds[2],
            ChunkKind::Media {
                keyframe: false,
                ..
            }
        ),
        "audio in an A/V output"
    );
    assert!(matches!(
        kinds[3],
        ChunkKind::Media {
            keyframe: false,
            ..
        }
    ));
    assert_eq!(chunks.len(), 4, "no trailer");

    let demuxed = demux(&concat(&chunks));
    let video = &demuxed.units[&0x100];
    assert_eq!(video.len(), 2);
    let (pts, dts, es) = parse_pes(&video[0]);
    assert_eq!(
        pts - dts,
        66 * 90,
        "B-frame reorder delay on the 90 kHz clock"
    );
    let mut expected = vec![0, 0, 0, 1, 0x09, 0xf0, 0, 0, 0, 1];
    expected.extend_from_slice(&SPS);
    expected.extend_from_slice(&[0, 0, 0, 1]);
    expected.extend_from_slice(&PPS);
    expected.extend_from_slice(&[0, 0, 0, 1]);
    expected.extend_from_slice(&idr_nal);
    assert_eq!(es, expected.as_slice());
    let (_, _, es) = parse_pes(&video[1]);
    assert_eq!(
        es,
        &[0, 0, 0, 1, 0x09, 0xf0, 0, 0, 0, 1, 0x41, 9, 9],
        "no parameter sets on P frames"
    );

    let audio = &demuxed.units[&0x101];
    let (_, _, adts) = parse_pes(&audio[0]);
    assert_eq!(&adts[..2], &[0xff, 0xf1]);
    assert_eq!(&adts[7..], &[1, 2, 3, 4]);

    // PCR rides on video and trails the DTS by the mux delay.
    assert_eq!(demuxed.pcrs.len(), 2);
    assert_eq!(dts - demuxed.pcrs[0], 63_000);
    // PAT and PMT before both keyframe and the header, nowhere else within 100 ms.
    assert_eq!(demuxed.units[&0].len(), 2);
    assert_eq!(demuxed.units[&0x1000].len(), 2);
}

#[test]
fn annexb_input_keeps_its_own_parameter_sets() {
    let mut stream = h264_stream();
    stream.bitstream = BitstreamFormat::AnnexB;
    stream.extradata = Bytes::new();
    let config = MuxerConfig::new(ContainerId::MpegTs, vec![stream]);
    let mut muxer = MpegTsMuxerFactory.create(&config).unwrap();
    let mut chunks = Vec::new();
    muxer.begin(&mut chunks).unwrap();
    let mut au = vec![0, 0, 0, 1, 0x09, 0xf0, 0, 0, 1];
    au.extend_from_slice(&SPS);
    au.extend_from_slice(&[0, 0, 1]);
    au.extend_from_slice(&PPS);
    au.extend_from_slice(&[0, 0, 1, 0x65, 1, 2, 3]);
    muxer
        .write(
            StreamIndex(0),
            &h264(0, 0, true, au.clone(), BitstreamFormat::AnnexB),
            &mut chunks,
        )
        .unwrap();
    let demuxed = demux(&concat(&chunks));
    let (_, _, es) = parse_pes(&demuxed.units[&0x100][0]);
    assert_eq!(es, au.as_slice(), "passed through unchanged");
}

#[test]
fn audio_only_output_marks_every_chunk_as_entry_point() {
    let config = MuxerConfig::new(ContainerId::MpegTs, vec![aac_stream()]);
    let mut muxer = MpegTsMuxerFactory.create(&config).unwrap();
    let mut chunks = Vec::new();
    muxer.begin(&mut chunks).unwrap();
    for i in 0..10u64 {
        let p = EncodedPacket::new(
            CodecId::Aac,
            BitstreamFormat::Raw,
            at(i * 21),
            at(i * 21),
            true,
            Bytes::from_static(&[7; 20]),
        )
        .unwrap();
        muxer.write(StreamIndex(0), &p, &mut chunks).unwrap();
    }
    assert!(
        chunks[1..]
            .iter()
            .all(|c| matches!(c.kind, ChunkKind::Media { keyframe: true, .. }))
    );
    let demuxed = demux(&concat(&chunks));
    assert_eq!(demuxed.pcrs.len(), 10, "PCR on the audio stream");
}

#[test]
fn psi_repeats_at_least_every_100_ms_between_keyframes() {
    let config = MuxerConfig::new(ContainerId::MpegTs, vec![h264_stream()]);
    let mut muxer = MpegTsMuxerFactory.create(&config).unwrap();
    let mut chunks = Vec::new();
    muxer.begin(&mut chunks).unwrap();
    for i in 0..31u64 {
        let p = h264(i * 33, i * 33, i == 0, length_prefixed(&[&[0x41, 1]]), LP4);
        muxer.write(StreamIndex(0), &p, &mut chunks).unwrap();
    }
    let demuxed = demux(&concat(&chunks));
    // Header, keyframe at 0, then every fourth frame (132 ms > 100 ms): 0..990 ms.
    assert_eq!(demuxed.units[&0].len(), 2 + 990 / 132);
}

#[test]
fn chunk_timestamps_never_go_backwards() {
    let config = MuxerConfig::new(ContainerId::MpegTs, vec![h264_stream(), aac_stream()]);
    let mut muxer = MpegTsMuxerFactory.create(&config).unwrap();
    let mut chunks = Vec::new();
    muxer.begin(&mut chunks).unwrap();
    muxer
        .write(
            StreamIndex(0),
            &h264(40, 40, true, length_prefixed(&[&[0x65, 1]]), LP4),
            &mut chunks,
        )
        .unwrap();
    let late_audio = EncodedPacket::new(
        CodecId::Aac,
        BitstreamFormat::Raw,
        at(20),
        at(20),
        true,
        Bytes::from_static(&[1]),
    )
    .unwrap();
    muxer
        .write(StreamIndex(1), &late_audio, &mut chunks)
        .unwrap();
    let ChunkKind::Media { timestamp, .. } = chunks[2].kind else {
        panic!("expected media");
    };
    assert_eq!(timestamp, at(40));
}

#[test]
fn rejects_bad_configs_and_packets() {
    let factory = MpegTsMuxerFactory;
    let config = |streams| MuxerConfig::new(ContainerId::MpegTs, streams);
    assert!(matches!(
        factory.create(&MuxerConfig::new(ContainerId::Flv, vec![h264_stream()])),
        Err(CreateError::Unsupported { .. })
    ));
    assert!(matches!(
        factory.create(&config(vec![])),
        Err(CreateError::InvalidConfig(_))
    ));
    let mut no_avcc = h264_stream();
    no_avcc.extradata = Bytes::new();
    assert!(matches!(
        factory.create(&config(vec![no_avcc])),
        Err(CreateError::InvalidConfig(_))
    ));
    let mut no_asc = aac_stream();
    no_asc.extradata = Bytes::new();
    assert!(matches!(
        factory.create(&config(vec![no_asc])),
        Err(CreateError::InvalidConfig(_))
    ));
    let too_many: Vec<StreamDescriptor> = (0..33).map(|_| aac_stream()).collect();
    assert!(matches!(
        factory.create(&config(too_many)),
        Err(CreateError::InvalidConfig(_))
    ));

    let mut muxer = factory.create(&config(vec![h264_stream()])).unwrap();
    let packet = h264(0, 0, true, length_prefixed(&[&[0x65]]), LP4);
    let mut out = Vec::new();
    assert!(matches!(
        muxer.write(StreamIndex(0), &packet, &mut out),
        Err(MuxError::InvalidState(_))
    ));
    muxer.begin(&mut out).unwrap();
    assert!(matches!(
        muxer.write(StreamIndex(3), &packet, &mut out),
        Err(MuxError::UnknownStream(3))
    ));
    let annexb = h264(0, 0, true, vec![0, 0, 1, 0x65], BitstreamFormat::AnnexB);
    assert!(matches!(
        muxer.write(StreamIndex(0), &annexb, &mut out),
        Err(MuxError::InvalidPacket(_))
    ));
    let truncated = h264(0, 0, true, vec![0, 0, 0, 9, 0x65], LP4);
    let err = muxer
        .write(StreamIndex(0), &truncated, &mut out)
        .unwrap_err();
    assert!(matches!(err, MuxError::InvalidPacket(_)) && err.is_recoverable());
    muxer.write(StreamIndex(0), &packet, &mut out).unwrap();
    muxer.finish(&mut out).unwrap();
    assert!(muxer.write(StreamIndex(0), &packet, &mut out).is_err());
}
