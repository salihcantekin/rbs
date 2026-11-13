//! One WAV session. The header layout is fixed so every size patch lands at the
//! same offset for the whole recording.

use rbs_core::media::{BitstreamFormat, Bytes, ChunkKind, CodecId, MuxChunk, StreamParams};
use rbs_core::time::Timestamp;
use rbs_mux::{CreateError, MuxError, Muxer, MuxerConfig};

/// Header bytes. `JUNK` occupies the `ds64` slot (28 bytes of payload plus its size).
const HEADER_LEN: usize = 80;
const OFF_FORM: u64 = 0;
const OFF_RIFF_SIZE: u64 = 4;
const OFF_DS64_ID: u64 = 12;
const OFF_DS64_RIFF: u64 = 20;
const OFF_DS64_DATA: u64 = 28;
const OFF_DS64_SAMPLES: u64 = 36;
const OFF_DATA_SIZE: u64 = 76;
/// `riff_size` field when there is no PCM yet: header without the 8-byte RIFF prefix.
const EMPTY_RIFF_SIZE: u32 = (HEADER_LEN as u32) - 8;
/// How many payload bytes between size patches.
const PATCH_EVERY: u64 = 1 << 20;

pub(crate) struct WavMuxer {
    bits: u16,
    channels: u16,
    rate: u32,
    block_align: u16,
    begun: bool,
    finished: bool,
    data_bytes: u64,
    last_dts: Option<Timestamp>,
    rf64: bool,
    since_patch: u64,
}

impl WavMuxer {
    pub(crate) fn new(config: &MuxerConfig) -> Result<Self, CreateError> {
        if config.streams.len() != 1 {
            return Err(CreateError::Unsupported {
                what: format!(
                    "{} streams (wav carries one PCM stream)",
                    config.streams.len()
                ),
            });
        }
        let stream = &config.streams[0];
        if stream.codec != CodecId::Pcm || stream.bitstream != BitstreamFormat::Raw {
            return Err(CreateError::Unsupported {
                what: format!("{} ({:?})", stream.codec, stream.bitstream),
            });
        }
        let (channels, rate) = match stream.params {
            StreamParams::Audio { format, .. } => {
                (format.channels.channels(), format.sample_rate.hz())
            }
            StreamParams::Video { .. } => {
                return Err(CreateError::InvalidConfig(
                    "wav carries audio, and this stream is video".into(),
                ));
            }
            _ => {
                return Err(CreateError::Unsupported {
                    what: "stream kind".into(),
                });
            }
        };
        if channels == 0 || channels > u16::MAX as usize || rate == 0 {
            return Err(CreateError::InvalidConfig(
                "wav needs a sample rate and at least one channel".into(),
            ));
        }
        let bits = bit_depth(&stream.extradata)?;
        let width = match bits {
            16 => 2,
            24 => 3,
            32 => 4,
            _ => {
                return Err(CreateError::InvalidConfig(
                    "pcm bit depth must be 16, 24 or 32".into(),
                ));
            }
        };
        let block_align = (channels * width) as u16;
        Ok(Self {
            bits,
            channels: channels as u16,
            rate,
            block_align,
            begun: false,
            finished: false,
            data_bytes: 0,
            last_dts: None,
            rf64: false,
            since_patch: PATCH_EVERY,
        })
    }

    fn emit_sizes(&mut self, out: &mut Vec<MuxChunk>) {
        let samples = self.data_bytes / u64::from(self.block_align);
        let riff = EMPTY_RIFF_SIZE as u64 + self.data_bytes;
        if !self.rf64 && riff > u64::from(u32::MAX) {
            self.rf64 = true;
            patch(out, OFF_FORM, b"RF64");
            patch(out, OFF_DS64_ID, b"ds64");
            patch_u32(out, OFF_RIFF_SIZE, u32::MAX);
            patch_u32(out, OFF_DATA_SIZE, u32::MAX);
        }
        patch_u64(out, OFF_DS64_RIFF, riff);
        patch_u64(out, OFF_DS64_DATA, self.data_bytes);
        patch_u64(out, OFF_DS64_SAMPLES, samples);
        if !self.rf64 {
            patch_u32(out, OFF_RIFF_SIZE, riff as u32);
            patch_u32(out, OFF_DATA_SIZE, self.data_bytes as u32);
        }
    }
}

impl Muxer for WavMuxer {
    fn begin(&mut self, out: &mut Vec<MuxChunk>) -> Result<(), MuxError> {
        if self.begun {
            return Err(MuxError::InvalidState("begin was already called"));
        }
        self.begun = true;
        out.push(MuxChunk::new(
            ChunkKind::Header,
            Bytes::copy_from_slice(&header(
                self.channels,
                self.rate,
                self.bits,
                self.block_align,
            )),
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
        if packet.codec() != CodecId::Pcm || packet.bitstream() != BitstreamFormat::Raw {
            return Err(MuxError::InvalidPacket("not raw pcm".into()));
        }
        if let Some(prev) = self.last_dts
            && packet.dts() < prev
        {
            return Err(MuxError::NonMonotonic { stream: stream.0 });
        }
        let payload = packet.data();
        if !payload.len().is_multiple_of(usize::from(self.block_align)) {
            return Err(MuxError::InvalidPacket(
                "payload is not a whole number of frames".into(),
            ));
        }
        self.last_dts = Some(packet.dts());
        if payload.is_empty() {
            return Ok(());
        }
        self.data_bytes += payload.len() as u64;
        self.since_patch += payload.len() as u64;
        out.push(MuxChunk::new(
            ChunkKind::Media {
                keyframe: true,
                timestamp: packet.dts(),
            },
            payload.clone(),
        ));
        if self.since_patch >= PATCH_EVERY {
            self.emit_sizes(out);
            self.since_patch = 0;
        }
        Ok(())
    }

    fn finish(&mut self, out: &mut Vec<MuxChunk>) -> Result<(), MuxError> {
        if !self.begun || self.finished {
            return Err(MuxError::InvalidState("finish before begin or twice"));
        }
        self.finished = true;
        self.emit_sizes(out);
        out.push(MuxChunk::new(ChunkKind::Trailer, Bytes::new()));
        Ok(())
    }
}

fn bit_depth(extradata: &Bytes) -> Result<u16, CreateError> {
    match extradata.as_ref() {
        [] => Ok(16),
        [16] => Ok(16),
        [24] => Ok(24),
        [32] => Ok(32),
        _ => Err(CreateError::InvalidConfig(
            "pcm extradata must be one byte: 16, 24 or 32".into(),
        )),
    }
}

fn header(channels: u16, rate: u32, bits: u16, block_align: u16) -> [u8; HEADER_LEN] {
    let byte_rate = rate * u32::from(block_align);
    let format: u16 = if bits == 32 { 3 } else { 1 };
    let mut h = [0_u8; HEADER_LEN];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&EMPTY_RIFF_SIZE.to_le_bytes());
    h[8..12].copy_from_slice(b"WAVE");
    h[12..16].copy_from_slice(b"JUNK");
    h[16..20].copy_from_slice(&28_u32.to_le_bytes());
    h[44..48].copy_from_slice(&0_u32.to_le_bytes());
    h[48..52].copy_from_slice(b"fmt ");
    h[52..56].copy_from_slice(&16_u32.to_le_bytes());
    h[56..58].copy_from_slice(&format.to_le_bytes());
    h[58..60].copy_from_slice(&channels.to_le_bytes());
    h[60..64].copy_from_slice(&rate.to_le_bytes());
    h[64..68].copy_from_slice(&byte_rate.to_le_bytes());
    h[68..70].copy_from_slice(&block_align.to_le_bytes());
    h[70..72].copy_from_slice(&bits.to_le_bytes());
    h[72..76].copy_from_slice(b"data");
    h
}

fn patch(out: &mut Vec<MuxChunk>, offset: u64, bytes: &[u8]) {
    out.push(MuxChunk::new(
        ChunkKind::Patch { offset },
        Bytes::copy_from_slice(bytes),
    ));
}

fn patch_u32(out: &mut Vec<MuxChunk>, offset: u64, value: u32) {
    patch(out, offset, &value.to_le_bytes());
}

fn patch_u64(out: &mut Vec<MuxChunk>, offset: u64, value: u64) {
    patch(out, offset, &value.to_le_bytes());
}

/// Applies header and media in order, then patches, the way a seekable file does.
#[cfg(test)]
fn assemble(chunks: &[MuxChunk]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut patches = Vec::new();
    for chunk in chunks {
        match chunk.kind {
            ChunkKind::Patch { offset } => patches.push((offset, chunk.data.clone())),
            _ => out.extend_from_slice(&chunk.data),
        }
    }
    for (offset, data) in patches {
        let start = offset as usize;
        let end = start + data.len();
        if end > out.len() {
            out.resize(end, 0);
        }
        out[start..end].copy_from_slice(&data);
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use rbs_core::media::{AudioFormat, EncodedPacket, StreamDescriptor, StreamIndex};

    #[test]
    fn a_short_pcm_file_is_riff_and_the_sizes_match_the_payload() {
        let payload = [0_u8, 1, 2, 3, 4, 5, 6, 7];
        let file = record(&payload, &[16]);
        assert_eq!(&file[0..4], b"RIFF");
        assert_eq!(&file[8..12], b"WAVE");
        assert_eq!(&file[12..16], b"JUNK");
        assert_eq!(&file[48..52], b"fmt ");
        assert_eq!(u16::from_le_bytes([file[56], file[57]]), 1);
        assert_eq!(u16::from_le_bytes([file[70], file[71]]), 16);
        let data_size = u32::from_le_bytes(file[76..80].try_into().unwrap());
        assert_eq!(data_size as usize, payload.len());
        assert_eq!(&file[80..], &payload);
        let riff = u32::from_le_bytes(file[4..8].try_into().unwrap()) as usize;
        assert_eq!(riff, file.len() - 8);
    }

    #[test]
    fn twenty_four_bit_sets_the_fmt_width_and_a_decreasing_dts_is_dropped() {
        let file = record(&[1, 2, 3, 4, 5, 6], &[24]);
        assert_eq!(u16::from_le_bytes([file[70], file[71]]), 24);
        assert_eq!(u16::from_le_bytes([file[68], file[69]]), 6);

        let mut muxer = WavMuxer::new(&config(&[16])).unwrap();
        let mut out = Vec::new();
        muxer.begin(&mut out).unwrap();
        let first = packet(Timestamp::from_nanos(20), &[0, 0, 0, 0]);
        let late = packet(Timestamp::from_nanos(10), &[1, 1, 1, 1]);
        muxer.write(StreamIndex(0), &first, &mut out).unwrap();
        let err = muxer.write(StreamIndex(0), &late, &mut out).unwrap_err();
        assert!(matches!(err, MuxError::NonMonotonic { .. }));
        assert!(err.is_recoverable());
    }

    #[test]
    fn the_riff_form_switches_when_the_32_bit_size_would_overflow() {
        let mut muxer = WavMuxer::new(&config(&[16])).unwrap();
        muxer.data_bytes = u64::from(u32::MAX);
        let mut out = Vec::new();
        muxer.emit_sizes(&mut out);
        assert!(muxer.rf64);
        assert!(out.iter().any(|c| {
            matches!(c.kind, ChunkKind::Patch { offset: 0 }) && c.data.as_ref() == b"RF64"
        }));
    }

    fn record(payload: &[u8], depth: &[u8]) -> Vec<u8> {
        let mut muxer = WavMuxer::new(&config(depth)).unwrap();
        let mut out = Vec::new();
        muxer.begin(&mut out).unwrap();
        muxer
            .write(
                StreamIndex(0),
                &packet(Timestamp::from_nanos(1), payload),
                &mut out,
            )
            .unwrap();
        muxer.finish(&mut out).unwrap();
        assemble(&out)
    }

    fn config(depth: &[u8]) -> MuxerConfig {
        MuxerConfig::new(
            rbs_core::media::ContainerId::Wav,
            vec![StreamDescriptor::new(
                CodecId::Pcm,
                BitstreamFormat::Raw,
                Bytes::copy_from_slice(depth),
                StreamParams::audio(AudioFormat::STEREO_48K, 480),
            )],
        )
    }

    fn packet(ts: Timestamp, payload: &[u8]) -> EncodedPacket {
        EncodedPacket::new(
            CodecId::Pcm,
            BitstreamFormat::Raw,
            ts,
            ts,
            true,
            Bytes::copy_from_slice(payload),
        )
        .unwrap()
    }
}
