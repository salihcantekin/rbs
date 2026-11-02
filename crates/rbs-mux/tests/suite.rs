//! Checks the muxer suite with a tiny FLV writer: correct behaviour passes, and each
//! broken variant fails the expected case.

use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::media::{
    Bytes, ChunkKind, CodecId, EncodedPacket, MuxChunk, StreamIndex, StreamParams,
};
use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};
use rbs_core::time::Timestamp;
use rbs_mux::caps;
use rbs_mux::conformance::{self, Report};
use rbs_mux::{CreateError, MuxError, Muxer, MuxerConfig, MuxerFactory};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Fault {
    None,
    HoldsEverything,
    AcceptsNonMonotonic,
    AcceptsAnyCodec,
    BadTagSize,
    KeyframeOnAudio,
}

struct FlvFactory(Fault);

impl AdapterFactory for FlvFactory {
    fn id(&self) -> AdapterId {
        AdapterId::new("test.mux.flv")
    }
    fn display_name(&self) -> &str {
        "test FLV"
    }
    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        let set = |v: &[&str]| CapabilityValue::Set(v.iter().map(|s| s.to_string()).collect());
        Ok(CapabilitySet::new()
            .with(caps::CONTAINER, set(&["flv"]))
            .with(caps::CODECS, set(&["h264", "aac"]))
            .with(caps::LIVE, CapabilityValue::Flag(true)))
    }
}

impl MuxerFactory for FlvFactory {
    fn create(&self, config: &MuxerConfig) -> Result<Box<dyn Muxer>, CreateError> {
        for s in &config.streams {
            if !matches!(s.codec, CodecId::H264 | CodecId::Aac) && self.0 != Fault::AcceptsAnyCodec
            {
                return Err(CreateError::Unsupported {
                    what: format!("{} in FLV", s.codec),
                });
            }
        }
        Ok(Box::new(Flv {
            fault: self.0,
            config: config.clone(),
            origin: None,
            last_dts: vec![None; config.streams.len()],
            held: Vec::new(),
        }))
    }
}

struct Flv {
    fault: Fault,
    config: MuxerConfig,
    origin: Option<Timestamp>,
    last_dts: Vec<Option<Timestamp>>,
    held: Vec<MuxChunk>,
}

fn tag(kind: u8, ms: u32, body: &[u8], fault: Fault) -> Vec<u8> {
    let size = body.len() as u32;
    let mut t = vec![
        kind,
        (size >> 16) as u8,
        (size >> 8) as u8,
        size as u8,
        (ms >> 16) as u8,
        (ms >> 8) as u8,
        ms as u8,
        (ms >> 24) as u8,
        0,
        0,
        0,
    ];
    t.extend_from_slice(body);
    let previous = if fault == Fault::BadTagSize {
        0
    } else {
        11 + size
    };
    t.extend_from_slice(&previous.to_be_bytes());
    t
}

impl Muxer for Flv {
    fn begin(&mut self, out: &mut Vec<MuxChunk>) -> Result<(), MuxError> {
        let mut header = b"FLV\x01\x05\x00\x00\x00\x09\x00\x00\x00\x00".to_vec();
        for s in &self.config.streams {
            let mut body = match s.params {
                StreamParams::Video { .. } => vec![0x17, 0, 0, 0, 0],
                _ => vec![0xaf, 0],
            };
            body.extend_from_slice(&s.extradata);
            let kind = if matches!(s.params, StreamParams::Video { .. }) {
                9
            } else {
                8
            };
            header.extend(tag(kind, 0, &body, self.fault));
        }
        out.push(MuxChunk::new(ChunkKind::Header, Bytes::from(header)));
        Ok(())
    }

    fn write(
        &mut self,
        stream: StreamIndex,
        packet: &EncodedPacket,
        out: &mut Vec<MuxChunk>,
    ) -> Result<(), MuxError> {
        let last = self
            .last_dts
            .get_mut(stream.get())
            .ok_or(MuxError::UnknownStream(stream.0))?;
        if last.is_some_and(|d| packet.dts() < d) && self.fault != Fault::AcceptsNonMonotonic {
            return Err(MuxError::NonMonotonic { stream: stream.0 });
        }
        *last = Some(packet.dts());
        let origin = *self.origin.get_or_insert(packet.dts());
        let ms = (packet.dts().saturating_since(origin).as_millis()) as u32;
        let (kind, mut body) = match packet.codec() {
            CodecId::H264 => (
                9,
                vec![if packet.keyframe() { 0x17 } else { 0x27 }, 1, 0, 0, 0],
            ),
            _ => (8, vec![0xaf, 1]),
        };
        body.extend_from_slice(packet.data());
        let chunk = MuxChunk::new(
            ChunkKind::Media {
                keyframe: packet.keyframe() && (kind == 9 || self.fault == Fault::KeyframeOnAudio),
                timestamp: packet.dts(),
            },
            Bytes::from(tag(kind, ms, &body, self.fault)),
        );
        if self.fault == Fault::HoldsEverything {
            self.held.push(chunk);
        } else {
            out.push(chunk);
        }
        Ok(())
    }

    fn finish(&mut self, out: &mut Vec<MuxChunk>) -> Result<(), MuxError> {
        out.append(&mut self.held);
        Ok(())
    }
}

fn failed(report: &Report) -> Vec<String> {
    report.failures().map(|c| c.name.clone()).collect()
}

#[test]
fn correct_muxer_passes() {
    let report = conformance::muxer(&FlvFactory(Fault::None));
    report.assert_ran(&["flv/live-stream-structure", "flv/keyframe-entry-points"]);
    assert!(report.skipped().next().is_none(), "{report}");
}

#[test]
fn each_mux_fault_is_caught() {
    let expectations = [
        (Fault::HoldsEverything, "flv/interleaving"),
        (Fault::AcceptsNonMonotonic, "flv/non-monotonic-dts"),
        (Fault::AcceptsAnyCodec, "flv/undeclared-codec"),
        (Fault::BadTagSize, "flv/live-stream-structure"),
        (Fault::KeyframeOnAudio, "flv/keyframe-entry-points"),
    ];
    for (fault, case) in expectations {
        let report = conformance::muxer(&FlvFactory(fault));
        let failed = failed(&report);
        assert!(
            failed.iter().any(|c| c == case),
            "{fault:?}: expected {case} to fail, got {failed:?}\n{report}"
        );
    }
}
