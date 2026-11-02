//! Checks the bytes the FLV muxer writes with a small tag reader.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use rbs_core::media::{
    BitstreamFormat, Bytes, ChunkKind, CodecId, ContainerId, EncodedPacket, MuxChunk, StreamIndex,
};
use rbs_core::time::Timestamp;
use rbs_mux::conformance::{SYNTHETIC_ASC, SYNTHETIC_AVCC, aac_stream, h264_stream};
use rbs_mux::{CreateError, MuxError, MuxerConfig, MuxerFactory};
use rbs_mux_flv::FlvMuxerFactory;

const LP4: BitstreamFormat = BitstreamFormat::LengthPrefixed { length_size: 4 };

struct Tag {
    kind: u8,
    ms: u32,
    body: Vec<u8>,
}

fn tags(bytes: &[u8]) -> Vec<Tag> {
    assert_eq!(&bytes[..3], b"FLV");
    let mut pos = 13;
    let mut out = Vec::new();
    while pos < bytes.len() {
        let h = &bytes[pos..pos + 11];
        let size = (usize::from(h[1]) << 16) | (usize::from(h[2]) << 8) | usize::from(h[3]);
        let ms = u32::from_be_bytes([h[7], h[4], h[5], h[6]]);
        let body = bytes[pos + 11..pos + 11 + size].to_vec();
        let prev = &bytes[pos + 11 + size..pos + 15 + size];
        assert_eq!(
            u32::from_be_bytes(prev.try_into().unwrap()) as usize,
            11 + size
        );
        out.push(Tag {
            kind: h[0],
            ms,
            body,
        });
        pos += 15 + size;
    }
    out
}

fn at(ms: u64) -> Timestamp {
    Timestamp::from_nanos(7_000_000_000 + ms * 1_000_000)
}

fn video(
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

fn audio(ms: u64) -> EncodedPacket {
    EncodedPacket::new(
        CodecId::Aac,
        BitstreamFormat::Raw,
        at(ms),
        at(ms),
        true,
        Bytes::from_static(&[1, 2, 3]),
    )
    .unwrap()
}

fn concat(chunks: &[MuxChunk]) -> Vec<u8> {
    chunks.iter().flat_map(|c| c.data.iter().copied()).collect()
}

#[test]
fn header_has_metadata_and_sequence_headers() {
    let config = MuxerConfig::new(ContainerId::Flv, vec![h264_stream(), aac_stream()]);
    let mut muxer = FlvMuxerFactory.create(&config).unwrap();
    let mut chunks = Vec::new();
    muxer.begin(&mut chunks).unwrap();
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].kind, ChunkKind::Header);
    let bytes = concat(&chunks);
    assert_eq!(&bytes[..9], &[b'F', b'L', b'V', 1, 0x05, 0, 0, 0, 9]);
    let tags = tags(&bytes);
    assert_eq!(tags.iter().map(|t| t.kind).collect::<Vec<_>>(), [18, 9, 8]);
    assert_eq!(&tags[0].body[..14], b"\x02\x00\x0aonMetaData\x08");
    let text = String::from_utf8_lossy(&tags[0].body);
    for key in [
        "width",
        "height",
        "framerate",
        "videocodecid",
        "audiocodecid",
    ] {
        assert!(text.contains(key), "{key}");
    }
    assert_eq!(&tags[1].body[..5], &[0x17, 0, 0, 0, 0]);
    assert_eq!(&tags[1].body[5..], SYNTHETIC_AVCC);
    assert_eq!(&tags[2].body[..2], &[0xaf, 0]);
    assert_eq!(&tags[2].body[2..], SYNTHETIC_ASC);
}

#[test]
fn media_tags_carry_times_composition_offsets_and_passed_through_payloads() {
    let config = MuxerConfig::new(ContainerId::Flv, vec![h264_stream(), aac_stream()]);
    let mut muxer = FlvMuxerFactory.create(&config).unwrap();
    let mut chunks = Vec::new();
    muxer.begin(&mut chunks).unwrap();
    let key = video(100, 0, true, vec![0, 0, 0, 2, 0x65, 9], LP4);
    muxer.write(StreamIndex(0), &key, &mut chunks).unwrap();
    muxer
        .write(StreamIndex(1), &audio(21), &mut chunks)
        .unwrap();
    let p = video(33, 33, false, vec![0, 0, 0, 2, 0x41, 8], LP4);
    muxer.write(StreamIndex(0), &p, &mut chunks).unwrap();
    muxer.finish(&mut chunks).unwrap();
    assert_eq!(
        chunks.len(),
        1 + 3 * 3,
        "three chunks per packet, no trailer"
    );

    // Zero copy: the payload chunk is the packet's own buffer.
    assert_eq!(chunks[2].data.as_ptr(), key.data().as_ptr());
    let flags: Vec<bool> = chunks[1..]
        .iter()
        .map(|c| matches!(c.kind, ChunkKind::Media { keyframe: true, .. }))
        .collect();
    assert_eq!(
        flags,
        [true, false, false, false, false, false, false, false, false]
    );

    let tags = tags(&concat(&chunks));
    let media = &tags[3..];
    assert_eq!(media[0].kind, 9);
    assert_eq!(media[0].ms, 0);
    assert_eq!(
        &media[0].body[..5],
        &[0x17, 1, 0, 0, 100],
        "composition time 100 ms"
    );
    assert_eq!(&media[0].body[5..], &[0, 0, 0, 2, 0x65, 9]);
    assert_eq!(media[1].kind, 8);
    assert_eq!(media[1].ms, 21);
    assert_eq!(media[1].body, [0xaf, 1, 1, 2, 3]);
    assert_eq!(media[2].ms, 33);
    assert_eq!(&media[2].body[..2], &[0x27, 1]);
}

#[test]
fn annexb_input_is_converted_to_length_prefixed() {
    let mut stream = h264_stream();
    stream.bitstream = BitstreamFormat::AnnexB;
    let config = MuxerConfig::new(ContainerId::Flv, vec![stream]);
    let mut muxer = FlvMuxerFactory.create(&config).unwrap();
    let mut chunks = Vec::new();
    muxer.begin(&mut chunks).unwrap();
    let au = vec![0, 0, 1, 0x09, 0xf0, 0, 0, 0, 1, 0x65, 1, 2];
    muxer
        .write(
            StreamIndex(0),
            &video(0, 0, true, au, BitstreamFormat::AnnexB),
            &mut chunks,
        )
        .unwrap();
    let tags = tags(&concat(&chunks));
    assert_eq!(
        &tags[2].body[5..],
        &[0, 0, 0, 2, 0x09, 0xf0, 0, 0, 0, 3, 0x65, 1, 2]
    );
}

#[test]
fn audio_only_output_marks_every_tag_as_entry_point() {
    let config = MuxerConfig::new(ContainerId::Flv, vec![aac_stream()]);
    let mut muxer = FlvMuxerFactory.create(&config).unwrap();
    let mut chunks = Vec::new();
    muxer.begin(&mut chunks).unwrap();
    assert_eq!(chunks[0].data[4], 0x04, "audio flag only");
    for i in 0..3 {
        muxer
            .write(StreamIndex(0), &audio(i * 21), &mut chunks)
            .unwrap();
    }
    let keys = chunks
        .iter()
        .filter(|c| matches!(c.kind, ChunkKind::Media { keyframe: true, .. }))
        .count();
    assert_eq!(keys, 3);
}

#[test]
fn rejects_bad_configs_and_packets() {
    let f = FlvMuxerFactory;
    let config = |streams| MuxerConfig::new(ContainerId::Flv, streams);
    assert!(matches!(
        f.create(&MuxerConfig::new(ContainerId::MpegTs, vec![h264_stream()])),
        Err(CreateError::Unsupported { .. })
    ));
    assert!(matches!(
        f.create(&config(vec![])),
        Err(CreateError::InvalidConfig(_))
    ));
    assert!(matches!(
        f.create(&config(vec![h264_stream(), h264_stream()])),
        Err(CreateError::InvalidConfig(_))
    ));
    let mut wrong_length = h264_stream();
    wrong_length.bitstream = BitstreamFormat::LengthPrefixed { length_size: 2 };
    assert!(matches!(
        f.create(&config(vec![wrong_length])),
        Err(CreateError::InvalidConfig(_))
    ));
    let mut annexb_without_sets = h264_stream();
    annexb_without_sets.bitstream = BitstreamFormat::AnnexB;
    annexb_without_sets.extradata = Bytes::new();
    assert!(matches!(
        f.create(&config(vec![annexb_without_sets])),
        Err(CreateError::InvalidConfig(_))
    ));

    let mut muxer = f
        .create(&config(vec![h264_stream(), aac_stream()]))
        .unwrap();
    let mut out = Vec::new();
    let packet = video(0, 0, true, vec![0, 0, 0, 1, 0x65], LP4);
    assert!(matches!(
        muxer.write(StreamIndex(0), &packet, &mut out),
        Err(MuxError::InvalidState(_))
    ));
    muxer.begin(&mut out).unwrap();
    assert!(matches!(
        muxer.write(StreamIndex(2), &packet, &mut out),
        Err(MuxError::UnknownStream(2))
    ));
    assert!(matches!(
        muxer.write(StreamIndex(1), &packet, &mut out),
        Err(MuxError::InvalidPacket(_))
    ));
    let annexb = video(0, 0, true, vec![0, 0, 1, 0x65], BitstreamFormat::AnnexB);
    assert!(matches!(
        muxer.write(StreamIndex(0), &annexb, &mut out),
        Err(MuxError::InvalidPacket(_))
    ));
    muxer.write(StreamIndex(0), &packet, &mut out).unwrap();
    let earlier = EncodedPacket::new(
        CodecId::H264,
        LP4,
        at(0) - std::time::Duration::from_millis(1),
        at(0) - std::time::Duration::from_millis(1),
        false,
        Bytes::from_static(&[0, 0, 0, 1, 0x41]),
    )
    .unwrap();
    let err = muxer.write(StreamIndex(0), &earlier, &mut out).unwrap_err();
    assert!(matches!(err, MuxError::NonMonotonic { stream: 0 }) && err.is_recoverable());
    let empty = video(5, 5, false, vec![], LP4);
    assert!(matches!(
        muxer.write(StreamIndex(0), &empty, &mut out),
        Err(MuxError::InvalidPacket(_))
    ));
    muxer.finish(&mut out).unwrap();
    assert!(muxer.finish(&mut out).is_err());
}

#[test]
fn he_aac_config_is_carried_whole() {
    let mut he = aac_stream();
    he.extradata = Bytes::from_static(&[0x2b, 0x12, 0x08, 0x00]);
    let mut muxer = FlvMuxerFactory
        .create(&MuxerConfig::new(ContainerId::Flv, vec![he]))
        .unwrap();
    let mut chunks = Vec::new();
    muxer.begin(&mut chunks).unwrap();
    let tags = tags(&concat(&chunks));
    assert_eq!(tags[1].body, [0xaf, 0, 0x2b, 0x12, 0x08, 0x00]);
}
