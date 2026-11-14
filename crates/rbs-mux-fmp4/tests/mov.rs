//! Checks the QuickTime (MOV) form of the muxer: header boxes, the `lpcm` sound
//! description and PCM fragments, and what it refuses (ADR-0085).

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use rbs_core::capability::CapabilityValue;
use rbs_core::media::{
    AudioFormat, BitstreamFormat, Bytes, ChannelLayout, ChunkKind, CodecId, ContainerId,
    EncodedPacket, MuxChunk, StreamDescriptor, StreamIndex, StreamParams,
};
use rbs_core::registry::AdapterFactory;
use rbs_core::time::Timestamp;
use rbs_core::timing::SampleRate;
use rbs_mux::conformance::{aac_stream, h264_stream};
use rbs_mux::{CreateError, MuxError, MuxerConfig, MuxerFactory, caps};
use rbs_mux_fmp4::Fmp4MuxerFactory;

const MOV: ContainerId = ContainerId::Mov;

/// Boxes whose payload is more boxes.
const CONTAINERS: [&[u8; 4]; 11] = [
    b"moov", b"trak", b"mdia", b"minf", b"dinf", b"stbl", b"mvex", b"moof", b"traf", b"stsd",
    b"dref",
];

struct Box_ {
    kind: [u8; 4],
    payload: Vec<u8>,
    children: Vec<Box_>,
}

impl Box_ {
    fn child(&self, kind: &[u8; 4]) -> &Box_ {
        self.children
            .iter()
            .find(|b| &b.kind == kind)
            .unwrap_or_else(|| panic!("no {}", String::from_utf8_lossy(kind)))
    }

    fn path(&self, kinds: &[&[u8; 4]]) -> &Box_ {
        kinds.iter().fold(self, |b, k| b.child(k))
    }

    fn u32_at(&self, at: usize) -> u32 {
        u32::from_be_bytes(self.payload[at..at + 4].try_into().unwrap())
    }
}

fn parse(bytes: &[u8]) -> Vec<Box_> {
    let mut pos = 0;
    let mut out = Vec::new();
    while pos < bytes.len() {
        let size = u32::from_be_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
        assert!(size >= 8 && pos + size <= bytes.len(), "box size {size}");
        let kind: [u8; 4] = bytes[pos + 4..pos + 8].try_into().unwrap();
        let payload = bytes[pos + 8..pos + size].to_vec();
        // `stsd` and `dref` are full boxes with an entry count before their children.
        let children = match &kind {
            b"stsd" | b"dref" => parse(&payload[8..]),
            k if CONTAINERS.contains(&k) => parse(&payload),
            _ => Vec::new(),
        };
        out.push(Box_ {
            kind,
            payload,
            children,
        });
        pos += size;
    }
    out
}

fn pcm_stream(bits: u8, channels: AudioFormat) -> StreamDescriptor {
    StreamDescriptor::new(
        CodecId::Pcm,
        BitstreamFormat::Raw,
        Bytes::copy_from_slice(&[bits]),
        StreamParams::audio(channels, 480),
    )
}

fn at(ms: u64) -> Timestamp {
    Timestamp::from_nanos(3_000_000_000 + ms * 1_000_000)
}

fn pcm(ms: u64, bytes: Vec<u8>) -> EncodedPacket {
    EncodedPacket::new(
        CodecId::Pcm,
        BitstreamFormat::Raw,
        at(ms),
        at(ms),
        true,
        Bytes::from(bytes),
    )
    .unwrap()
}

fn header(config: &MuxerConfig) -> Vec<Box_> {
    let mut muxer = Fmp4MuxerFactory.create(config).unwrap();
    let mut chunks = Vec::new();
    muxer.begin(&mut chunks).unwrap();
    assert_eq!(chunks.len(), 1);
    parse(&chunks[0].data)
}

#[test]
fn the_probe_lists_mov_and_its_codecs() {
    let probed = Fmp4MuxerFactory.probe().unwrap();
    let Some(CapabilityValue::Set(containers)) = probed.get(&caps::CONTAINER) else {
        panic!("no container set");
    };
    assert!(containers.iter().any(|c| c == "mov"));
    let Some(CapabilityValue::Set(codecs)) = probed.get(&caps::CODECS) else {
        panic!("no codec set");
    };
    for pair in ["mov.h264", "mov.hevc", "mov.aac", "mov.pcm"] {
        assert!(codecs.iter().any(|c| c == pair), "{pair}");
    }
    assert!(!codecs.iter().any(|c| c == "mov.av1"));
    assert!(!codecs.iter().any(|c| c == "mp4.fragmented.pcm"));
}

#[test]
fn mov_header_is_quicktime() {
    let config = MuxerConfig::new(
        MOV,
        vec![h264_stream(), pcm_stream(24, AudioFormat::STEREO_48K)],
    );
    let boxes = header(&config);
    let ftyp = &boxes[0];
    assert_eq!(&ftyp.kind, b"ftyp");
    assert_eq!(&ftyp.payload[..4], b"qt  ");
    assert_eq!(&ftyp.payload[8..], b"qt  ", "only compatible brand");
    let moov = &boxes[1];
    let traks: Vec<&Box_> = moov
        .children
        .iter()
        .filter(|b| &b.kind == b"trak")
        .collect();
    assert_eq!(traks.len(), 2);
    for (trak, handler) in traks.iter().zip([b"vide", b"soun"]) {
        let hdlr = trak.path(&[b"mdia", b"hdlr"]);
        assert_eq!(&hdlr.payload[4..8], b"mhlr");
        assert_eq!(&hdlr.payload[8..12], handler);
        assert_eq!(&hdlr.payload[24..], b"\x03RBS", "Pascal-string name");
        let dref = trak.path(&[b"mdia", b"minf", b"dinf", b"dref"]);
        assert_eq!(&dref.children[0].kind, b"alis");
        assert_eq!(dref.children[0].payload, [0, 0, 0, 1], "self reference");
    }
    let video = traks[0].path(&[b"mdia", b"minf", b"stbl", b"stsd"]);
    assert_eq!(&video.children[0].kind, b"avc1");
    assert!(moov.child(b"mvex").children.len() == 2);
}

#[test]
fn lpcm_sound_description_matches_each_width() {
    // (bits, channels, flags, bytes per frame)
    for (bits, format, flags, frame) in [
        (16u8, AudioFormat::STEREO_48K, 0xc, 4u32),
        (24, AudioFormat::STEREO_48K, 0xc, 6),
        (32, AudioFormat::STEREO_48K, 0x9, 8),
    ] {
        let config = MuxerConfig::new(MOV, vec![pcm_stream(bits, format)]);
        let boxes = header(&config);
        let stsd = boxes[1].path(&[b"trak", b"mdia", b"minf", b"stbl", b"stsd"]);
        let entry = &stsd.children[0];
        assert_eq!(&entry.kind, b"lpcm");
        assert_eq!(entry.payload.len(), 64, "72-byte v2 description");
        let p = &entry;
        assert_eq!(
            u16::from_be_bytes([p.payload[8], p.payload[9]]),
            2,
            "version"
        );
        assert_eq!(p.u32_at(24), 0x0001_0000);
        assert_eq!(p.u32_at(28), 72);
        let rate = f64::from_bits(u64::from_be_bytes(p.payload[32..40].try_into().unwrap()));
        assert_eq!(rate, 48_000.0);
        assert_eq!(p.u32_at(40), 2, "channels");
        assert_eq!(p.u32_at(44), 0x7f00_0000);
        assert_eq!(p.u32_at(48), u32::from(bits));
        assert_eq!(p.u32_at(52), flags, "{bits}-bit flags");
        assert_eq!(p.u32_at(56), frame);
        assert_eq!(p.u32_at(60), 1);
        let mdhd = boxes[1].path(&[b"trak", b"mdia", b"mdhd"]);
        assert_eq!(mdhd.u32_at(12), 48_000, "timescale is the sample rate");
    }
}

#[test]
fn pcm_fragments_have_one_sample_per_frame_and_no_rows() {
    // 16-bit stereo: 4 bytes a frame. Two 10 ms packets of 480 frames each.
    let config = MuxerConfig::new(MOV, vec![pcm_stream(16, AudioFormat::STEREO_48K)]);
    let mut muxer = Fmp4MuxerFactory.create(&config).unwrap();
    let mut chunks = Vec::new();
    muxer.begin(&mut chunks).unwrap();
    let a: Vec<u8> = (0..1920u32).map(|i| i as u8).collect();
    let b: Vec<u8> = (0..1920u32).map(|i| (i * 7) as u8).collect();
    muxer
        .write(StreamIndex(0), &pcm(0, a.clone()), &mut chunks)
        .unwrap();
    muxer
        .write(StreamIndex(0), &pcm(10, b.clone()), &mut chunks)
        .unwrap();
    muxer.finish(&mut chunks).unwrap();
    let media: Vec<&MuxChunk> = chunks
        .iter()
        .filter(|c| matches!(c.kind, ChunkKind::Media { .. }))
        .collect();
    assert_eq!(media.len(), 1);
    assert!(matches!(
        media[0].kind,
        ChunkKind::Media { keyframe: true, .. }
    ));
    let boxes = parse(&media[0].data);
    let traf = boxes[0].child(b"traf");
    let tfhd = traf.child(b"tfhd");
    assert_eq!(
        tfhd.u32_at(0),
        0x0000_0038,
        "three defaults, no base-is-moof"
    );
    assert_eq!(tfhd.u32_at(4), 1, "track id");
    assert_eq!(tfhd.u32_at(8), 1, "duration 1 tick");
    assert_eq!(tfhd.u32_at(12), 4, "bytes per frame");
    assert_eq!(tfhd.u32_at(16), 0x0200_0000, "sync");
    let trun = traf.child(b"trun");
    assert_eq!(trun.u32_at(0), 0x0000_0001, "version 0, data offset only");
    assert_eq!(trun.u32_at(4), 960, "frames");
    assert_eq!(trun.payload.len(), 12, "no per-sample rows");
    let data_offset = trun.u32_at(8) as usize;
    let mut want = a;
    want.extend_from_slice(&b);
    assert_eq!(&media[0].data[data_offset..], &want[..]);
    let mdat = &boxes[1];
    assert_eq!(mdat.payload, want);
    // The duration patches: 20 ms of audio. The first fragment spans less than the
    // interleaving bound (1 s), so media time 0 is one bound before the newest packet
    // and the audio starts 990 ms in (crate docs): 1010 ms.
    let patches: Vec<&MuxChunk> = chunks
        .iter()
        .filter(|c| matches!(c.kind, ChunkKind::Patch { .. }))
        .collect();
    assert_eq!(patches.len(), 2);
    assert!(
        patches
            .iter()
            .all(|p| p.data.as_ref() == 1010u32.to_be_bytes())
    );
}

/// Two GOPs of one second with 24-bit stereo PCM in 20 ms packets, audio written up
/// to each video packet; `pcm_first` puts the PCM stream first.
fn video_and_pcm(pcm_first: bool) -> Vec<MuxChunk> {
    let (video, audio) = if pcm_first {
        (StreamIndex(1), StreamIndex(0))
    } else {
        (StreamIndex(0), StreamIndex(1))
    };
    let pcm_desc = pcm_stream(24, AudioFormat::STEREO_48K);
    let streams = if pcm_first {
        vec![pcm_desc, h264_stream()]
    } else {
        vec![h264_stream(), pcm_desc]
    };
    let mut muxer = Fmp4MuxerFactory
        .create(&MuxerConfig::new(MOV, streams))
        .unwrap();
    let mut chunks = Vec::new();
    muxer.begin(&mut chunks).unwrap();
    let mut next_audio = 0u64;
    for i in 0..60u64 {
        let ms = i * 1000 / 30;
        while next_audio <= ms {
            let p = pcm(next_audio, vec![0x11; 960 * 6]);
            muxer.write(audio, &p, &mut chunks).unwrap();
            next_audio += 20;
        }
        let key = i.is_multiple_of(30);
        let p = EncodedPacket::new(
            CodecId::H264,
            BitstreamFormat::LengthPrefixed { length_size: 4 },
            at(ms),
            at(ms),
            key,
            Bytes::from(vec![0, 0, 0, 4, if key { 0x65 } else { 0x41 }, 1, 2, 3]),
        )
        .unwrap();
        muxer.write(video, &p, &mut chunks).unwrap();
    }
    muxer.finish(&mut chunks).unwrap();
    chunks
}

/// Reads every run of a MOV fragment the way a reader without `default-base-is-moof`
/// does: the first `traf`'s data from the `moof` start, each later one's from the end
/// of the data before it. Returns (track id, bytes of each sample).
fn mov_runs(chunk: &[u8]) -> Vec<(u32, Vec<Vec<u8>>)> {
    let boxes = parse(chunk);
    let moof = &boxes[0];
    let mut implicit = 0usize;
    let mut runs = Vec::new();
    for traf in moof.children.iter().filter(|b| &b.kind == b"traf") {
        let tfhd = traf.child(b"tfhd");
        assert_eq!(
            tfhd.u32_at(0) & 0x0002_0000,
            0,
            "no default-base-is-moof in MOV"
        );
        let trun = traf.child(b"trun");
        let count = trun.u32_at(4) as usize;
        let mut at = implicit + trun.u32_at(8) as usize;
        let sizes: Vec<usize> = if tfhd.u32_at(0) & 0x10 != 0 {
            vec![tfhd.u32_at(12) as usize; count]
        } else {
            (0..count)
                .map(|i| trun.u32_at(12 + 16 * i + 4) as usize)
                .collect()
        };
        let mut samples = Vec::new();
        for size in sizes {
            samples.push(chunk[at..at + size].to_vec());
            at += size;
        }
        implicit = at;
        runs.push((tfhd.u32_at(4), samples));
    }
    assert_eq!(
        implicit,
        chunk.len(),
        "the last run ends at the end of mdat"
    );
    runs
}

#[test]
fn pcm_beside_video_shares_fragments_in_either_order() {
    for pcm_first in [false, true] {
        let chunks = video_and_pcm(pcm_first);
        let media: Vec<&MuxChunk> = chunks
            .iter()
            .filter(|c| matches!(c.kind, ChunkKind::Media { .. }))
            .collect();
        assert_eq!(media.len(), 2, "one fragment per GOP");
        let (video_id, pcm_id) = if pcm_first { (2, 1) } else { (1, 2) };
        for chunk in media {
            let runs = mov_runs(&chunk.data);
            assert_eq!(runs.len(), 2);
            for (id, samples) in runs {
                if id == video_id {
                    assert_eq!(samples.len(), 30);
                    assert!(
                        samples
                            .iter()
                            .all(|s| s.len() == 8 && s[..4] == [0, 0, 0, 4])
                    );
                } else {
                    assert_eq!(id, pcm_id);
                    assert!(samples.iter().all(|s| s == &[0x11; 6]));
                }
            }
        }
        let mut file = Vec::new();
        for c in chunks.iter().filter(|c| c.is_sequential()) {
            file.extend_from_slice(&c.data);
        }
        rbs_mux::conformance::read_structure(MOV, &file).unwrap();
    }
}

/// (tfdt, frames) of the PCM run of every fragment.
fn pcm_timeline(packets: &[(u64, usize)]) -> Vec<(u64, u32)> {
    let config = MuxerConfig::new(MOV, vec![pcm_stream(16, AudioFormat::STEREO_48K)]);
    let mut muxer = Fmp4MuxerFactory.create(&config).unwrap();
    let mut chunks = Vec::new();
    muxer.begin(&mut chunks).unwrap();
    for &(us, frames) in packets {
        let t = Timestamp::from_nanos(3_000_000_000 + us * 1000);
        let p = EncodedPacket::new(
            CodecId::Pcm,
            BitstreamFormat::Raw,
            t,
            t,
            true,
            Bytes::from(vec![0; frames * 4]),
        )
        .unwrap();
        muxer.write(StreamIndex(0), &p, &mut chunks).unwrap();
    }
    muxer.finish(&mut chunks).unwrap();
    chunks
        .iter()
        .filter(|c| matches!(c.kind, ChunkKind::Media { .. }))
        .map(|c| {
            let boxes = parse(&c.data);
            let traf = boxes[0].child(b"traf");
            let tfdt = traf.child(b"tfdt");
            let base = u64::from_be_bytes(tfdt.payload[4..12].try_into().unwrap());
            (base, traf.child(b"trun").u32_at(4))
        })
        .collect()
}

#[test]
fn a_pcm_gap_starts_a_fragment_at_its_own_time() {
    // 10 ms packets (480 frames); a 100 ms gap after the fifth.
    let packets: Vec<(u64, usize)> = (0..10u64)
        .map(|i| (i * 10_000 + if i >= 5 { 100_000 } else { 0 }, 480))
        .collect();
    let runs = pcm_timeline(&packets);
    assert_eq!(runs.len(), 2, "{runs:?}");
    assert_eq!(runs[0].1, 2400);
    assert_eq!(runs[1].1, 2400);
    // The second run starts 150 ms (7200 ticks) after the first: the gap is kept.
    assert_eq!(runs[1].0 - runs[0].0, 7200);
}

#[test]
fn pcm_decode_time_never_goes_back() {
    // Packets stamped 9.5 ms apart but holding 10 ms of frames: the frames run ahead
    // of the timestamps until the overlap passes the tolerance and a fragment starts.
    let packets: Vec<(u64, usize)> = (0..40u64).map(|i| (i * 9_500, 480)).collect();
    let runs = pcm_timeline(&packets);
    assert!(runs.len() > 1, "{runs:?}");
    let mut end = 0;
    let mut frames = 0;
    for (base, count) in &runs {
        assert!(
            *base >= end,
            "fragment at {base} starts before the end {end}"
        );
        end = base + u64::from(*count);
        frames += count;
    }
    assert_eq!(frames, 40 * 480, "every frame is written");
}

#[test]
fn pcm_with_small_jitter_stays_in_one_fragment() {
    // +-0.5 ms of timestamp jitter around exact 10 ms steps.
    let packets: Vec<(u64, usize)> = (0..50u64)
        .map(|i| (i * 10_000 + if i % 2 == 1 { 500 } else { 0 }, 480))
        .collect();
    assert_eq!(pcm_timeline(&packets).len(), 1);
}

#[test]
fn refusals() {
    let unsupported = |config: MuxerConfig| match Fmp4MuxerFactory.create(&config) {
        Err(CreateError::Unsupported { .. }) => {}
        Err(e) => panic!("expected Unsupported, got {e}"),
        Ok(_) => panic!("accepted"),
    };
    // PCM only in MOV.
    unsupported(MuxerConfig::new(
        ContainerId::Mp4Fragmented,
        vec![pcm_stream(16, AudioFormat::STEREO_48K)],
    ));
    // AV1 not in MOV (ADR-0085 step c: H.264, HEVC, AAC, PCM).
    let mut av1 = h264_stream();
    av1.codec = CodecId::Av1;
    av1.bitstream = BitstreamFormat::Obu;
    unsupported(MuxerConfig::new(MOV, vec![av1]));
    // PCM beyond stereo waits for a `chan` layout atom.
    unsupported(MuxerConfig::new(
        MOV,
        vec![pcm_stream(
            24,
            AudioFormat::new(SampleRate::HZ_48000, ChannelLayout::Surround5_1).unwrap(),
        )],
    ));
    // Other containers still go elsewhere.
    unsupported(MuxerConfig::new(ContainerId::Matroska, vec![aac_stream()]));

    // A bad width is a config error.
    match Fmp4MuxerFactory.create(&MuxerConfig::new(
        MOV,
        vec![pcm_stream(20, AudioFormat::STEREO_48K)],
    )) {
        Err(CreateError::InvalidConfig(_)) => {}
        other => panic!("expected InvalidConfig, got {:?}", other.err()),
    }

    // A packet that is not whole frames is refused and leaves nothing behind.
    let config = MuxerConfig::new(MOV, vec![pcm_stream(24, AudioFormat::STEREO_48K)]);
    let mut muxer = Fmp4MuxerFactory.create(&config).unwrap();
    let mut chunks = Vec::new();
    muxer.begin(&mut chunks).unwrap();
    let before = chunks.len();
    match muxer.write(StreamIndex(0), &pcm(0, vec![0; 7]), &mut chunks) {
        Err(MuxError::InvalidPacket(_)) => {}
        other => panic!("expected InvalidPacket, got {other:?}"),
    }
    assert_eq!(chunks.len(), before);
    muxer
        .write(StreamIndex(0), &pcm(0, vec![0; 12]), &mut chunks)
        .unwrap();
    muxer.finish(&mut chunks).unwrap();
    let media = chunks
        .iter()
        .find(|c| matches!(c.kind, ChunkKind::Media { .. }))
        .unwrap();
    let boxes = parse(&media.data);
    assert_eq!(boxes[0].child(b"traf").child(b"trun").u32_at(4), 2);
}

#[test]
fn mp4_header_is_unchanged() {
    let config = MuxerConfig::new(
        ContainerId::Mp4Fragmented,
        vec![h264_stream(), aac_stream()],
    );
    let boxes = header(&config);
    assert_eq!(&boxes[0].payload[..4], b"iso6");
    let hdlr = boxes[1].path(&[b"trak", b"mdia", b"hdlr"]);
    assert_eq!(&hdlr.payload[4..8], &[0; 4]);
    assert_eq!(&hdlr.payload[24..], b"RBS\0");
    let dref = boxes[1].path(&[b"trak", b"mdia", b"minf", b"dinf", b"dref"]);
    assert_eq!(&dref.children[0].kind, b"url ");
}
