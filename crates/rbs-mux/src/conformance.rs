//! Conformance suite for muxer adapters.
//!
//! ```ignore
//! rbs_mux::conformance::muxer(&MyMuxerFactory::default()).assert_passed();
//! ```
//!
//! For every declared container the suite muxes three seconds of synthetic H.264 and
//! AAC packets (for the codecs the adapter declares) and checks the invariants of
//! `docs/specs/output-ports.md`, including a structural check of the live byte stream
//! with a minimal reader per container. It is test support only.

use std::time::Duration;

use rbs_core::capability::CapabilitySet;
pub use rbs_core::conformance::{Case, Outcome, PROBE_BUDGET, Report};
use rbs_core::conformance::{flag, guarded, probe_case, set_values};
use rbs_core::media::{
    AudioFormat, BitstreamFormat, Bytes, ChunkKind, CodecId, ColorSpace, ContainerId,
    EncodedPacket, MuxChunk, PixelFormat, Size, StreamDescriptor, StreamIndex, StreamParams,
    VideoFormat,
};
use rbs_core::time::Timestamp;
use rbs_core::timing::{Cadence, FrameRate, SampleRate};

use crate::caps;
use crate::error::{CreateError, MuxError};
use crate::muxer::{MuxerConfig, MuxerFactory};

const ORIGIN: Timestamp = Timestamp::from_nanos(2_000_000_000);
const SECONDS: u64 = 3;
const FPS: FrameRate = FrameRate::FPS_30;
const GOP: u64 = 30;
const AAC_FRAME: u32 = 1024;
/// One video frame interval plus one AAC frame, the most one interleaving step adds.
const PACKET_SLACK: Duration = Duration::from_millis(56);

/// A synthetic `AVCDecoderConfigurationRecord` (High profile, level 3.1) with one SPS
/// and one PPS, enough for container readers that parse the record.
pub const SYNTHETIC_AVCC: &[u8] = &[
    0x01, 0x64, 0x00, 0x1f, 0xff, 0xe1, 0x00, 0x09, 0x67, 0x64, 0x00, 0x1f, 0xac, 0xd9, 0x40, 0x50,
    0x05, 0x01, 0x00, 0x04, 0x68, 0xeb, 0xe3, 0xcb,
];

/// `AudioSpecificConfig` for AAC-LC, 48 kHz, stereo.
pub const SYNTHETIC_ASC: &[u8] = &[0x11, 0x90];

/// A synthetic H.264 stream descriptor: 320x240 NV12 at 30 fps, length-prefixed NAL
/// units with 4-byte lengths.
pub fn h264_stream() -> StreamDescriptor {
    StreamDescriptor::new(
        CodecId::H264,
        BitstreamFormat::LengthPrefixed { length_size: 4 },
        Bytes::from_static(SYNTHETIC_AVCC),
        StreamParams::video(
            VideoFormat::new(
                Size::new(320, 240),
                PixelFormat::Nv12,
                ColorSpace::BT709_LIMITED,
            ),
            FPS,
        ),
    )
}

/// A synthetic AAC-LC stream descriptor: 48 kHz stereo, raw frames.
pub fn aac_stream() -> StreamDescriptor {
    StreamDescriptor::new(
        CodecId::Aac,
        BitstreamFormat::Raw,
        Bytes::from_static(SYNTHETIC_ASC),
        StreamParams::audio(AudioFormat::STEREO_48K, AAC_FRAME),
    )
}

fn h264_packet(index: u64) -> EncodedPacket {
    let keyframe = index.is_multiple_of(GOP);
    // One length-prefixed NAL: IDR (type 5) or non-IDR slice (type 1).
    let nal_type = if keyframe { 0x65 } else { 0x41 };
    let mut data = vec![0, 0, 0, 16, nal_type];
    data.extend((0..15).map(|i| (i as u8).wrapping_add(index as u8)));
    let pts = Cadence::frames(ORIGIN, FPS).time_of(index);
    EncodedPacket::new(
        CodecId::H264,
        BitstreamFormat::LengthPrefixed { length_size: 4 },
        pts,
        pts,
        keyframe,
        Bytes::from(data),
    )
    .unwrap_or_else(|_| unreachable!("synthetic packets have dts == pts"))
    .with_duration(FPS.frame_duration())
}

fn aac_packet(index: u64) -> EncodedPacket {
    let rate = SampleRate::HZ_48000;
    let pts = ORIGIN + rate.duration_of(index * u64::from(AAC_FRAME));
    EncodedPacket::new(
        CodecId::Aac,
        BitstreamFormat::Raw,
        pts,
        pts,
        true,
        Bytes::from(vec![0x21, 0x10, 0x04, 0x60, 0x8c, 0x1c]),
    )
    .unwrap_or_else(|_| unreachable!("synthetic packets have dts == pts"))
    .with_duration(rate.duration_of(u64::from(AAC_FRAME)))
}

/// All packets of the synthetic sequence for the given streams, in dts order.
fn sequence(streams: &[StreamDescriptor]) -> Vec<(StreamIndex, EncodedPacket)> {
    let mut out = Vec::new();
    for (i, s) in streams.iter().enumerate() {
        let index = StreamIndex(i as u16);
        match s.codec {
            CodecId::H264 => {
                out.extend((0..SECONDS * 30).map(|n| (index, h264_packet(n))));
            }
            CodecId::Aac => {
                let count = SECONDS * 48_000 / u64::from(AAC_FRAME);
                out.extend((0..count).map(|n| (index, aac_packet(n))));
            }
            _ => {}
        }
    }
    out.sort_by_key(|(s, p)| (p.dts(), *s));
    out
}

/// Runs the muxer suite against `factory`.
pub fn muxer(factory: &dyn MuxerFactory) -> Report {
    let mut report = Report::default();
    let Some(caps) = probe_case(&mut report, factory) else {
        return report;
    };
    let containers = set_values(&caps, &caps::CONTAINER);
    if containers.is_empty() {
        report.record(
            "mux.container",
            Outcome::Failed("no container declared".into()),
        );
    }
    let codecs = set_values(&caps, &caps::CODECS);
    let mut streams = Vec::new();
    if codecs.iter().any(|c| c == "h264") {
        streams.push(h264_stream());
    }
    if codecs.iter().any(|c| c == "aac") {
        streams.push(aac_stream());
    }
    for name in &containers {
        let Some(container) = ContainerId::from_name(name) else {
            report.record(
                name.clone(),
                Outcome::Skipped("container unknown to this suite".into()),
            );
            continue;
        };
        if streams.is_empty() {
            report.record(
                name.clone(),
                Outcome::Skipped("declares neither h264 nor aac".into()),
            );
            continue;
        }
        let config = MuxerConfig::new(container, streams.clone())
            .with_fragment_duration(Duration::from_millis(500));
        mux_sequence(&mut report, name, factory, &config, &caps);
        non_monotonic_case(&mut report, name, factory, &config);
        unsupported_codec_case(&mut report, name, factory, container, &codecs);
    }
    report
}

fn mux_sequence(
    report: &mut Report,
    name: &str,
    factory: &dyn MuxerFactory,
    config: &MuxerConfig,
    caps: &CapabilitySet,
) {
    let mut chunks: Vec<MuxChunk> = Vec::new();
    let mut lag_error = None;
    let bound = config.max_buffered();
    let run = guarded(|| {
        let mut muxer = factory.create(config).map_err(|e| format!("create: {e}"))?;
        muxer
            .begin(&mut chunks)
            .map_err(|e| format!("begin: {e}"))?;
        if chunks.is_empty() || chunks.iter().any(|c| c.kind != ChunkKind::Header) {
            return Err("begin must emit only Header chunks, at least one".into());
        }
        let mut newest_media: Option<Timestamp> = None;
        for (stream, packet) in sequence(&config.streams) {
            let before = chunks.len();
            muxer
                .write(stream, &packet, &mut chunks)
                .map_err(|e| format!("write {stream:?} at {:?}: {e}", packet.dts()))?;
            for c in &chunks[before..] {
                if let ChunkKind::Media { timestamp, .. } = c.kind {
                    newest_media = Some(newest_media.map_or(timestamp, |t| t.max(timestamp)));
                }
            }
            // Interleaving bound (spec invariant 2): media is out within
            // `max_buffered` of its dts, so the newest Media chunk starts at most two
            // bounds plus one packet interval before the current packet.
            let held_since = newest_media.unwrap_or(ORIGIN);
            let lag = packet.dts().saturating_since(held_since);
            if lag > bound * 2 + PACKET_SLACK && lag_error.is_none() {
                lag_error = Some(format!(
                    "at {:?} the newest media chunk starts at {held_since:?}: {lag:?} held back, bound {bound:?}",
                    packet.dts()
                ));
            }
        }
        let before = chunks.len();
        muxer
            .finish(&mut chunks)
            .map_err(|e| format!("finish: {e}"))?;
        if chunks[before..]
            .iter()
            .any(|c| matches!(c.kind, ChunkKind::Header))
        {
            return Err("finish emitted a Header chunk".into());
        }
        Ok(())
    });
    if let Err(e) = run {
        report.record(format!("{name}/mux"), Outcome::Failed(e));
        return;
    }
    report.check(format!("{name}/chunk-order"), check_chunk_order(&chunks));
    report.check(
        format!("{name}/keyframe-entry-points"),
        check_entry_points(config.container, &chunks),
    );
    report.check(
        format!("{name}/interleaving"),
        lag_error.map_or(Ok(()), Err),
    );
    report.check(format!("{name}/patches"), {
        let has_patch = chunks
            .iter()
            .any(|c| matches!(c.kind, ChunkKind::Patch { .. }));
        if has_patch && !flag(caps, &caps::PATCHES) {
            Err("emits Patch chunks without declaring mux.patches".into())
        } else {
            Ok(())
        }
    });

    let live: Vec<u8> = chunks
        .iter()
        .filter(|c| matches!(c.kind, ChunkKind::Header | ChunkKind::Media { .. }))
        .flat_map(|c| c.data.iter().copied())
        .collect();
    let case = format!("{name}/live-stream-structure");
    if flag(caps, &caps::LIVE) {
        report.check(case, read_structure(config.container, &live));
    } else {
        report.record(case, Outcome::Skipped("mux.live not declared".into()));
    }
}

fn check_chunk_order(chunks: &[MuxChunk]) -> Result<(), String> {
    let mut seen_media = false;
    let mut seen_end = false;
    let mut first_media_keyframe = None;
    let mut last_ts = None;
    for (i, c) in chunks.iter().enumerate() {
        match c.kind {
            ChunkKind::Header if seen_media || seen_end => {
                return Err(format!("chunk {i}: Header after media"));
            }
            ChunkKind::Header => {}
            ChunkKind::Media {
                keyframe,
                timestamp,
            } => {
                if seen_end {
                    return Err(format!("chunk {i}: Media after Trailer or Patch"));
                }
                seen_media = true;
                first_media_keyframe.get_or_insert(keyframe);
                if last_ts.is_some_and(|t| timestamp < t) {
                    return Err(format!("chunk {i}: Media timestamp goes backwards"));
                }
                last_ts = Some(timestamp);
            }
            ChunkKind::Trailer | ChunkKind::Patch { .. } => seen_end = true,
            _ => {}
        }
    }
    match first_media_keyframe {
        None => Err("no Media chunk".into()),
        Some(false) => Err("first Media chunk is not marked keyframe".into()),
        Some(true) => Ok(()),
    }
}

fn check_entry_points(container: ContainerId, chunks: &[MuxChunk]) -> Result<(), String> {
    let mut checked = 0;
    for (i, c) in chunks.iter().enumerate() {
        if let ChunkKind::Media { keyframe: true, .. } = c.kind {
            is_entry_point(container, &c.data)
                .map_err(|e| format!("chunk {i} is marked keyframe but {e}"))?;
            checked += 1;
        }
    }
    if checked < 2 {
        return Err(format!(
            "{checked} keyframe chunks for a 3 s sequence with a keyframe every second"
        ));
    }
    Ok(())
}

/// Checks that `chunk` starts where a reader can begin decoding (spec invariant 3).
pub fn is_entry_point(container: ContainerId, chunk: &[u8]) -> Result<(), String> {
    match container {
        ContainerId::Flv => {
            if chunk.first().map(|b| b & 0x1f) != Some(9)
                || chunk.get(11).map(|b| b >> 4) != Some(1)
            {
                return Err("does not start with an FLV video keyframe tag".into());
            }
            Ok(())
        }
        ContainerId::Mp4Fragmented => match chunk.get(4..8) {
            Some(b"moof") | Some(b"styp") => Ok(()),
            _ => Err("does not start with a moof or styp box".into()),
        },
        ContainerId::Matroska => {
            if chunk.starts_with(&CLUSTER) {
                Ok(())
            } else {
                Err("does not start with a Matroska Cluster".into())
            }
        }
        ContainerId::MpegTs => {
            let rai = chunk.chunks(188).take(8).any(|p| {
                p.len() == 188 && p[0] == 0x47 && p[3] & 0x20 != 0 && p[4] > 0 && p[5] & 0x40 != 0
            });
            if rai {
                Ok(())
            } else {
                Err("has no TS packet with random_access_indicator in its first 8 packets".into())
            }
        }
        other => Err(format!("no entry point check for {other}")),
    }
}

fn non_monotonic_case(
    report: &mut Report,
    name: &str,
    factory: &dyn MuxerFactory,
    config: &MuxerConfig,
) {
    let result = guarded(|| {
        let mut muxer = factory.create(config).map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        muxer.begin(&mut out).map_err(|e| e.to_string())?;
        let stream = StreamIndex(0);
        let first = match config.streams[0].codec {
            CodecId::H264 => h264_packet(30),
            _ => aac_packet(30),
        };
        let earlier = first.dts() - Duration::from_millis(100);
        let late = EncodedPacket::new(
            first.codec(),
            first.bitstream(),
            earlier,
            earlier,
            true,
            first.data().clone(),
        )
        .map_err(|e| e.to_string())?;
        muxer
            .write(stream, &first, &mut out)
            .map_err(|e| e.to_string())?;
        match muxer.write(stream, &late, &mut out) {
            Err(e @ MuxError::NonMonotonic { .. }) if e.is_recoverable() => Ok(()),
            Err(e) => Err(format!("expected recoverable NonMonotonic, got {e}")),
            Ok(()) => Err("a packet with a decreasing dts was accepted".into()),
        }
    });
    report.check(format!("{name}/non-monotonic-dts"), result);
}

fn unsupported_codec_case(
    report: &mut Report,
    name: &str,
    factory: &dyn MuxerFactory,
    container: ContainerId,
    declared: &[String],
) {
    let case = format!("{name}/undeclared-codec");
    let Some(codec) = [CodecId::Vp9, CodecId::Opus, CodecId::Av1, CodecId::Hevc]
        .into_iter()
        .find(|c| !declared.iter().any(|d| d == c.as_str()))
    else {
        report.record(case, Outcome::Skipped("every codec is declared".into()));
        return;
    };
    let params = if codec == CodecId::Opus {
        aac_stream().params
    } else {
        h264_stream().params
    };
    let config = MuxerConfig::new(
        container,
        vec![StreamDescriptor::new(
            codec,
            BitstreamFormat::Raw,
            Bytes::new(),
            params,
        )],
    );
    let result = guarded(|| match factory.create(&config) {
        Err(CreateError::Unsupported { .. }) => Ok(()),
        Err(e) => Err(format!("expected Unsupported for {codec}, got {e}")),
        Ok(_) => Err(format!("{codec} is not declared but was accepted")),
    });
    report.check(case, result);
}

/// Checks that `bytes` is structurally a valid live stream of `container`, using a
/// minimal reader. Public so adapter tests can check their own outputs too.
pub fn read_structure(container: ContainerId, bytes: &[u8]) -> Result<(), String> {
    match container {
        ContainerId::MpegTs => read_mpegts(bytes),
        ContainerId::Flv => read_flv(bytes),
        ContainerId::Mp4Fragmented => read_fmp4(bytes),
        ContainerId::Matroska => read_matroska(bytes),
        other => Err(format!("no reader for {other}")),
    }
}

fn read_mpegts(bytes: &[u8]) -> Result<(), String> {
    if bytes.is_empty() || !bytes.len().is_multiple_of(188) {
        return Err(format!("length {} is not a multiple of 188", bytes.len()));
    }
    let mut continuity = [None::<u8>; 8192];
    let mut pmt_pid = None;
    let mut pmt_seen = false;
    for (i, p) in bytes.chunks(188).enumerate() {
        if p[0] != 0x47 {
            return Err(format!("TS packet {i} lacks the 0x47 sync byte"));
        }
        let pid = (usize::from(p[1] & 0x1f) << 8) | usize::from(p[2]);
        let start = p[1] & 0x40 != 0;
        let control = (p[3] >> 4) & 0x3;
        let cc = p[3] & 0x0f;
        if i == 0 && pid != 0 {
            return Err(format!(
                "first TS packet has PID {pid}, expected the PAT (0)"
            ));
        }
        let mut payload = 4;
        if control & 0x2 != 0 {
            let len = usize::from(p[4]);
            if 5 + len > 188 {
                return Err(format!("TS packet {i}: adaptation field too long"));
            }
            let discontinuity = len > 0 && p[5] & 0x80 != 0;
            if discontinuity {
                continuity[pid] = None;
            }
            payload = 5 + len;
        }
        if control & 0x1 != 0 && pid != 0x1fff {
            if let Some(prev) = continuity[pid] {
                if cc != (prev + 1) % 16 {
                    return Err(format!(
                        "TS packet {i}: continuity counter of PID {pid} jumps {prev} -> {cc}"
                    ));
                }
            }
            continuity[pid] = Some(cc);
        }
        if pid == 0 && start && pmt_pid.is_none() {
            // PAT: pointer field, then the section; first program entry at offset 8.
            let section = payload + 1 + usize::from(*p.get(payload).unwrap_or(&0));
            let entry = p
                .get(section + 8..section + 12)
                .ok_or_else(|| format!("TS packet {i}: truncated PAT"))?;
            pmt_pid = Some((usize::from(entry[2] & 0x1f) << 8) | usize::from(entry[3]));
        } else if Some(pid) == pmt_pid {
            pmt_seen = true;
        }
    }
    match (pmt_pid, pmt_seen) {
        (None, _) => Err("no PAT with a program".into()),
        (Some(pid), false) => Err(format!("PAT names PMT PID {pid}, but no such packet")),
        _ => Ok(()),
    }
}

fn be(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0, |acc, b| (acc << 8) | u64::from(*b))
}

fn read_flv(bytes: &[u8]) -> Result<(), String> {
    if bytes.len() < 13 || &bytes[..3] != b"FLV" {
        return Err("missing FLV signature".into());
    }
    let mut pos = be(&bytes[5..9]) as usize;
    if bytes.get(pos..pos + 4) != Some(&[0, 0, 0, 0]) {
        return Err("PreviousTagSize0 is not zero".into());
    }
    pos += 4;
    let mut tags = 0;
    while pos < bytes.len() {
        let header = bytes
            .get(pos..pos + 11)
            .ok_or_else(|| format!("truncated tag header at {pos}"))?;
        let kind = header[0] & 0x1f;
        if !matches!(kind, 8 | 9 | 18) {
            return Err(format!("tag at {pos} has unknown type {kind}"));
        }
        let size = be(&header[1..4]) as usize;
        let end = pos + 11 + size;
        let previous = bytes
            .get(end..end + 4)
            .ok_or_else(|| format!("truncated tag at {pos}"))?;
        if be(previous) as usize != 11 + size {
            return Err(format!("PreviousTagSize mismatch after tag at {pos}"));
        }
        pos = end + 4;
        tags += 1;
    }
    if tags == 0 {
        return Err("no tags".into());
    }
    Ok(())
}

fn read_fmp4(bytes: &[u8]) -> Result<(), String> {
    let mut pos = 0;
    let mut kinds = Vec::new();
    while pos < bytes.len() {
        let header = bytes
            .get(pos..pos + 8)
            .ok_or_else(|| format!("truncated box header at {pos}"))?;
        let mut size = be(&header[..4]);
        let kind = String::from_utf8_lossy(&header[4..8]).into_owned();
        if size == 1 {
            size = be(bytes
                .get(pos + 8..pos + 16)
                .ok_or_else(|| format!("truncated largesize at {pos}"))?);
        } else if size == 0 {
            size = (bytes.len() - pos) as u64;
        }
        if size < 8 || pos as u64 + size > bytes.len() as u64 {
            return Err(format!("box {kind} at {pos} has bad size {size}"));
        }
        kinds.push(kind);
        pos += size as usize;
    }
    if kinds.first().map(String::as_str) != Some("ftyp") {
        return Err(format!("first box is {:?}, expected ftyp", kinds.first()));
    }
    if !kinds.iter().any(|k| k == "moov") {
        return Err("no moov box".into());
    }
    let moof = kinds
        .iter()
        .position(|k| k == "moof")
        .ok_or("no moof box (not fragmented)")?;
    if !kinds[moof..].iter().any(|k| k == "mdat") {
        return Err("moof without a following mdat".into());
    }
    Ok(())
}

const EBML: [u8; 4] = [0x1a, 0x45, 0xdf, 0xa3];
const SEGMENT: [u8; 4] = [0x18, 0x53, 0x80, 0x67];
const CLUSTER: [u8; 4] = [0x1f, 0x43, 0xb6, 0x75];

/// Reads an EBML variable-length integer at `pos`: (length, value with the marker
/// bit kept for ids or removed for sizes, all-ones flag).
fn vint(bytes: &[u8], pos: usize, keep_marker: bool) -> Option<(usize, u64, bool)> {
    let first = *bytes.get(pos)?;
    let len = first.leading_zeros() as usize + 1;
    if len > 8 {
        return None;
    }
    let raw = be(bytes.get(pos..pos + len)?);
    let marker = 1_u64 << (7 * len);
    let value = if keep_marker { raw } else { raw & (marker - 1) };
    Some((len, value, raw & (marker - 1) == marker - 1))
}

fn read_matroska(bytes: &[u8]) -> Result<(), String> {
    if !bytes.starts_with(&EBML) {
        return Err("missing EBML header".into());
    }
    let id_of = |v: [u8; 4]| u64::from(u32::from_be_bytes(v));
    let (segment, cluster) = (id_of(SEGMENT), id_of(CLUSTER));
    let mut pos = 0;
    let mut seen_segment = false;
    let mut clusters = 0;
    while pos < bytes.len() {
        let (id_len, id, _) = vint(bytes, pos, true).ok_or(format!("bad element id at {pos}"))?;
        let (size_len, size, unknown) =
            vint(bytes, pos + id_len, false).ok_or(format!("bad element size at {pos}"))?;
        let data = pos + id_len + size_len;
        if id == segment {
            seen_segment = true;
        }
        if id == cluster {
            clusters += 1;
        }
        if unknown || id == segment {
            // Unknown-size (live) Segment or Cluster, or a Segment: read its children.
            if id != segment && id != cluster {
                return Err(format!("unknown size on element {id:#x} at {pos}"));
            }
            pos = data;
            continue;
        }
        let end = data as u64 + size;
        if end > bytes.len() as u64 {
            return Err(format!("element {id:#x} at {pos} runs past the end"));
        }
        pos = end as usize;
    }
    if !seen_segment {
        return Err("no Segment element".into());
    }
    if clusters == 0 {
        return Err("no Cluster element".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_sequence_is_in_dts_order() {
        let seq = sequence(&[h264_stream(), aac_stream()]);
        assert_eq!(seq.len(), 90 + 140);
        assert!(seq.windows(2).all(|w| w[0].1.dts() <= w[1].1.dts()));
        assert!(seq[0].1.keyframe());
    }

    #[test]
    fn readers_reject_garbage() {
        for c in ContainerId::ALL {
            assert!(read_structure(c, b"garbage").is_err(), "{c}");
        }
    }

    #[test]
    fn matroska_reader_walks_live_elements() {
        let mut mkv = EBML.to_vec();
        mkv.extend_from_slice(&[0x80]); // empty EBML header
        mkv.extend_from_slice(&SEGMENT);
        mkv.extend_from_slice(&[0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
        mkv.extend_from_slice(&[0x15, 0x49, 0xa9, 0x66, 0x82, 0x2a, 0xd7]); // Info
        mkv.extend_from_slice(&CLUSTER);
        mkv.extend_from_slice(&[0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
        mkv.extend_from_slice(&[0xe7, 0x81, 0x00, 0xa3, 0x82, 0x81, 0x00]); // Timestamp, SimpleBlock
        assert_eq!(read_matroska(&mkv), Ok(()));
        // A Cluster id hidden in a payload is not a Cluster.
        let mut fake = EBML.to_vec();
        fake.extend_from_slice(&[0x80, 0xec, 0x84]);
        fake.extend_from_slice(&CLUSTER);
        assert!(read_matroska(&fake).is_err());
        assert!(read_matroska(&mkv[..mkv.len() - 1]).is_err());
    }

    #[test]
    fn ts_reader_checks_pat_pmt_and_continuity() {
        let packet = |pid: u16, cc: u8, start: bool, body: &[u8]| {
            let mut p = vec![
                0x47,
                (if start { 0x40 } else { 0 }) | (pid >> 8) as u8,
                pid as u8,
            ];
            p.push(0x10 | cc);
            p.extend_from_slice(body);
            p.resize(188, 0xff);
            p
        };
        // PAT: pointer 0, table 0, length, tsid, version, section numbers, program 1 -> PMT PID 0x1000.
        let pat = [0, 0, 0xb0, 13, 0, 1, 0xc1, 0, 0, 0, 1, 0xf0, 0x00];
        let mut ts = packet(0, 0, true, &pat);
        ts.extend(packet(0x1000, 0, true, &[0, 2]));
        ts.extend(packet(0x100, 0, true, &[]));
        ts.extend(packet(0x100, 1, false, &[]));
        assert_eq!(read_mpegts(&ts), Ok(()));
        let mut broken = ts.clone();
        broken[3 * 188 + 3] = 0x13; // cc 3 after 0
        assert!(read_mpegts(&broken).is_err());
        assert!(read_mpegts(&ts[188..]).is_err());
    }

    #[test]
    fn mp4_reader_accepts_minimal_fragmented_file() {
        let mut file = Vec::new();
        for kind in [b"ftyp", b"moov", b"moof", b"mdat"] {
            file.extend_from_slice(&[0, 0, 0, 8]);
            file.extend_from_slice(kind);
        }
        assert_eq!(read_fmp4(&file), Ok(()));
        assert!(read_fmp4(&file[8..]).is_err());
    }

    #[test]
    fn flv_reader_walks_tags() {
        let mut flv = b"FLV\x01\x05\x00\x00\x00\x09\x00\x00\x00\x00".to_vec();
        flv.extend_from_slice(&[9, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0x17, 0x00]);
        flv.extend_from_slice(&[0, 0, 0, 13]);
        assert_eq!(read_flv(&flv), Ok(()));
        let last = flv.len() - 1;
        flv[last] = 12;
        assert!(read_flv(&flv).is_err());
    }
}
