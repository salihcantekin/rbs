//! One native FLAC session. The header is 42 bytes and is never rewritten.

use rbs_core::media::{BitstreamFormat, Bytes, ChunkKind, CodecId, MuxChunk, StreamParams};
use rbs_core::time::Timestamp;
use rbs_mux::{CreateError, MuxError, Muxer, MuxerConfig};

/// `fLaC` + metadata block header + 34-byte STREAMINFO.
const HEADER_LEN: usize = 42;

pub(crate) struct FlacMuxer {
    header: [u8; HEADER_LEN],
    begun: bool,
    finished: bool,
    last_dts: Option<Timestamp>,
}

impl FlacMuxer {
    pub(crate) fn new(config: &MuxerConfig) -> Result<Self, CreateError> {
        if config.streams.len() != 1 {
            return Err(CreateError::Unsupported {
                what: format!(
                    "{} streams (flac carries one audio stream)",
                    config.streams.len()
                ),
            });
        }
        let stream = &config.streams[0];
        if stream.codec != CodecId::Flac || stream.bitstream != BitstreamFormat::Raw {
            return Err(CreateError::Unsupported {
                what: format!("{} ({:?})", stream.codec, stream.bitstream),
            });
        }
        let (channels, rate, block) = match stream.params {
            StreamParams::Audio {
                format, frame_size, ..
            } => (
                format.channels.channels(),
                format.sample_rate.hz(),
                frame_size,
            ),
            StreamParams::Video { .. } => {
                return Err(CreateError::InvalidConfig(
                    "flac carries audio, and this stream is video".into(),
                ));
            }
            _ => {
                return Err(CreateError::Unsupported {
                    what: "stream kind".into(),
                });
            }
        };
        if !(1..=8).contains(&channels) || rate == 0 || rate > 0x000F_FFFF {
            return Err(CreateError::InvalidConfig(
                "flac needs 1 to 8 channels and a sample rate that fits STREAMINFO".into(),
            ));
        }
        if !(16..=65_535).contains(&block) {
            return Err(CreateError::InvalidConfig(
                "flac block size must be 16 to 65535".into(),
            ));
        }
        let bits = bit_depth(&stream.extradata)?;
        Ok(Self {
            header: streaminfo(rate, channels as u8, bits as u8, block as u16),
            begun: false,
            finished: false,
            last_dts: None,
        })
    }
}

impl Muxer for FlacMuxer {
    fn begin(&mut self, out: &mut Vec<MuxChunk>) -> Result<(), MuxError> {
        if self.begun {
            return Err(MuxError::InvalidState("begin was already called"));
        }
        self.begun = true;
        out.push(MuxChunk::new(
            ChunkKind::Header,
            Bytes::copy_from_slice(&self.header),
        ));
        Ok(())
    }

    fn write(
        &mut self,
        stream: rbs_core::media::StreamIndex,
        packet: &rbs_core::media::EncodedPacket,
        out: &mut Vec<MuxChunk>,
    ) -> Result<(), MuxError> {
        if !self.begun || self.finished {
            return Err(MuxError::InvalidState("write before begin or after finish"));
        }
        if stream.0 != 0 {
            return Err(MuxError::UnknownStream(stream.0));
        }
        if packet.codec() != CodecId::Flac || packet.bitstream() != BitstreamFormat::Raw {
            return Err(MuxError::InvalidPacket("not a flac frame".into()));
        }
        if let Some(prev) = self.last_dts
            && packet.dts() < prev
        {
            return Err(MuxError::NonMonotonic { stream: stream.0 });
        }
        self.last_dts = Some(packet.dts());
        let payload = packet.data();
        if payload.is_empty() {
            return Ok(());
        }
        out.push(MuxChunk::new(
            ChunkKind::Media {
                keyframe: true,
                timestamp: packet.dts(),
            },
            payload.clone(),
        ));
        Ok(())
    }

    fn finish(&mut self, out: &mut Vec<MuxChunk>) -> Result<(), MuxError> {
        if !self.begun || self.finished {
            return Err(MuxError::InvalidState("finish before begin or twice"));
        }
        self.finished = true;
        out.push(MuxChunk::new(ChunkKind::Trailer, Bytes::new()));
        Ok(())
    }
}

fn bit_depth(extradata: &Bytes) -> Result<u16, CreateError> {
    match extradata.as_ref() {
        [] | [16] => Ok(16),
        [24] => Ok(24),
        _ => Err(CreateError::InvalidConfig(
            "flac extradata must be one byte: 16 or 24".into(),
        )),
    }
}

/// `fLaC`, a last-block STREAMINFO header, then min = max = `block`, unknown
/// frame sizes, the rate and width, unknown total samples and an unknown MD5.
fn streaminfo(rate: u32, channels: u8, bps: u8, block: u16) -> [u8; HEADER_LEN] {
    let mut header = [0_u8; HEADER_LEN];
    header[0..4].copy_from_slice(b"fLaC");
    header[4] = 0x80;
    header[7] = 34;
    header[8..10].copy_from_slice(&block.to_be_bytes());
    header[10..12].copy_from_slice(&block.to_be_bytes());
    let packed =
        (u64::from(rate) << 44) | (u64::from(channels - 1) << 41) | (u64::from(bps - 1) << 36);
    header[18..26].copy_from_slice(&packed.to_be_bytes());
    header
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use rbs_core::media::{AudioFormat, ContainerId, StreamDescriptor, StreamIndex};
    use rbs_core::time::Timestamp;
    use rbs_mux::MuxerConfig;

    fn session() -> FlacMuxer {
        let stream = StreamDescriptor::new(
            CodecId::Flac,
            BitstreamFormat::Raw,
            Bytes::from(vec![16_u8]),
            StreamParams::audio(AudioFormat::STEREO_48K, 4096),
        );
        FlacMuxer::new(&MuxerConfig::new(ContainerId::Flac, vec![stream])).unwrap()
    }

    #[test]
    fn the_header_is_a_streamable_streaminfo_with_nothing_to_patch() {
        let mut muxer = session();
        let mut out = Vec::new();
        muxer.begin(&mut out).unwrap();
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0].kind, ChunkKind::Header));
        let header = out[0].data.as_ref();
        assert_eq!(&header[..4], b"fLaC");
        assert_eq!(header[4], 0x80);
        assert_eq!(header[7], 34);
        assert_eq!(&header[8..12], &[0x10, 0x00, 0x10, 0x00]);
        // 48 kHz, stereo, 16-bit, total samples unknown.
        assert_eq!(
            &header[18..26],
            &[0x0B, 0xB8, 0x02, 0xF0, 0x00, 0x00, 0x00, 0x00]
        );
        assert!(header[26..].iter().all(|b| *b == 0));

        let t = Timestamp::from_nanos(0);
        let frame = rbs_core::media::EncodedPacket::new(
            CodecId::Flac,
            BitstreamFormat::Raw,
            t,
            t,
            true,
            Bytes::from(vec![0xFF, 0xF8, 0xCA, 0x18]),
        )
        .unwrap();
        out.clear();
        muxer.write(StreamIndex(0), &frame, &mut out).unwrap();
        assert!(matches!(
            out[0].kind,
            ChunkKind::Media { keyframe: true, .. }
        ));
        assert_eq!(out[0].data.as_ref(), &[0xFF, 0xF8, 0xCA, 0x18]);
        assert!(
            out.iter()
                .all(|c| !matches!(c.kind, ChunkKind::Patch { .. }))
        );
        out.clear();
        muxer.finish(&mut out).unwrap();
        assert!(matches!(out[0].kind, ChunkKind::Trailer));
        assert!(
            out.iter()
                .all(|c| !matches!(c.kind, ChunkKind::Patch { .. }))
        );
    }
}
