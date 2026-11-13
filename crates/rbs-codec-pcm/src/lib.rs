//! PCM encoder for the RBS audio encoder port (`rbs-codec`).
//!
//! Pure Rust, every platform. Turns the graph's `f32` blocks into interleaved
//! little-endian PCM (ADR-0085): 16-bit or 24-bit integer, or 32-bit float. The
//! default is 16-bit. `adapter_options` key `bit_depth` (`16`, `24` or `32`) selects
//! the others; any other value is refused at open.
//!
//! One packet is 10 ms (`sample rate / 100` frames, at least one). `submit` copies
//! into a buffer reserved at creation. The packet payload is the one allocation per
//! packet. NaN becomes silence. A full-scale sample is the largest finite integer
//! of that width, so `1.0` does not overflow.

use rbs_codec::{
    AudioEncoder, AudioEncoderConfig, AudioEncoderFactory, CreateError, EncodeError, EncoderChange,
    caps,
};
use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::media::{
    AudioBlock, AudioFormat, AudioSamples, BitstreamFormat, Bytes, CodecId, EncodedPacket,
    StreamDescriptor, StreamParams,
};
use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};
use rbs_core::time::Timestamp;

/// Stable adapter id.
pub const ADAPTER_ID: &str = "org.rbs.codec.pcm";

/// Factory for PCM sessions. Probing is free.
#[derive(Debug, Default, Clone, Copy)]
pub struct PcmEncoderFactory;

impl AdapterFactory for PcmEncoderFactory {
    fn id(&self) -> AdapterId {
        AdapterId::new(ADAPTER_ID)
    }

    fn display_name(&self) -> &str {
        "PCM"
    }

    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        Ok(CapabilitySet::new()
            .with(caps::HARDWARE, CapabilityValue::Flag(false))
            .with(
                caps::AUDIO_ENCODE,
                CapabilityValue::Set(vec![CodecId::Pcm.as_str().to_owned()]),
            )
            .with(
                caps::AUDIO_SAMPLE_RATES,
                CapabilityValue::Set(vec![
                    "44100".to_owned(),
                    "48000".to_owned(),
                    "96000".to_owned(),
                ]),
            )
            .with(caps::AUDIO_CHANNELS_MAX, CapabilityValue::Int(8))
            .with(
                caps::RECONFIGURE,
                CapabilityValue::Set(vec![caps::RECONFIGURE_KEYFRAME.to_owned()]),
            ))
    }
}

impl AudioEncoderFactory for PcmEncoderFactory {
    fn create(&self, config: &AudioEncoderConfig) -> Result<Box<dyn AudioEncoder>, CreateError> {
        Ok(Box::new(PcmSession::new(config)?))
    }
}

struct PcmSession {
    stream: StreamDescriptor,
    format: AudioFormat,
    frame_size: usize,
    channels: usize,
    bits: u16,
    pending: Vec<f32>,
    origin: Option<Timestamp>,
    emitted: u64,
    ready: Vec<EncodedPacket>,
    packet: Vec<u8>,
}

impl PcmSession {
    fn new(config: &AudioEncoderConfig) -> Result<Self, CreateError> {
        if config.codec != CodecId::Pcm {
            return Err(CreateError::Unsupported {
                what: format!("{} (this adapter writes pcm)", config.codec),
            });
        }
        let channels = config.format.channels.channels();
        let hz = config.format.sample_rate.hz();
        if channels == 0 || hz == 0 {
            return Err(CreateError::InvalidConfig(
                "pcm needs a sample rate and at least one channel".into(),
            ));
        }
        let bits = bit_depth(config)?;
        let frame_size = (hz / 100).max(1) as usize;
        let bytes_per = match bits {
            16 => 2,
            24 => 3,
            _ => 4,
        };
        Ok(Self {
            stream: StreamDescriptor::new(
                CodecId::Pcm,
                BitstreamFormat::Raw,
                Bytes::from(vec![bits as u8]),
                StreamParams::audio(config.format, frame_size as u32),
            ),
            format: config.format,
            frame_size,
            channels,
            bits,
            pending: Vec::with_capacity(frame_size * channels * 2),
            origin: None,
            emitted: 0,
            ready: Vec::with_capacity(4),
            packet: Vec::with_capacity(frame_size * channels * bytes_per),
        })
    }

    fn take_packet(&mut self) -> Result<(), EncodeError> {
        let frames = self.frame_size;
        let channels = self.channels;
        self.packet.clear();
        for frame in 0..frames {
            for channel in 0..channels {
                let sample = self.pending[frame * channels + channel];
                match self.bits {
                    16 => self.packet.extend_from_slice(&to_i16(sample).to_le_bytes()),
                    24 => {
                        let value = to_i24(sample);
                        self.packet.extend_from_slice(&value.to_le_bytes()[..3]);
                    }
                    _ => self.packet.extend_from_slice(&to_f32(sample).to_le_bytes()),
                }
            }
        }
        let rest = self.pending.split_off(frames * channels);
        self.pending = rest;
        let origin = self.origin.ok_or(EncodeError::Failed {
            reason: "pcm packet without a start time".into(),
            recoverable: false,
        })?;
        let pts = origin
            + self
                .format
                .sample_rate
                .duration_of(self.emitted * frames as u64);
        let data = Bytes::copy_from_slice(&self.packet);
        self.packet.clear();
        let packet = EncodedPacket::new(CodecId::Pcm, BitstreamFormat::Raw, pts, pts, true, data)
            .map_err(|_| EncodeError::Failed {
                reason: "pcm timestamp".into(),
                recoverable: false,
            })?
            .with_duration(self.format.sample_rate.duration_of(frames as u64));
        self.ready.push(packet);
        self.emitted += 1;
        if self.pending.is_empty() {
            self.origin = None;
            self.emitted = 0;
        }
        Ok(())
    }
}

impl AudioEncoder for PcmSession {
    fn stream(&self) -> &StreamDescriptor {
        &self.stream
    }

    fn frame_size(&self) -> u32 {
        self.frame_size as u32
    }

    fn submit(&mut self, block: &AudioBlock<'_>) -> Result<(), EncodeError> {
        if block.format() != self.format {
            return Err(EncodeError::InvalidInput("format".into()));
        }
        if self.pending.is_empty() {
            self.origin = Some(block.timestamp());
        }
        match block.samples() {
            AudioSamples::Planar(channels) => {
                for frame in 0..block.frames() {
                    for channel in 0..self.channels {
                        let samples = channels
                            .get(channel)
                            .ok_or_else(|| EncodeError::InvalidInput("channel".into()))?;
                        self.pending.push(samples[frame]);
                    }
                }
            }
            AudioSamples::Interleaved(samples) => self.pending.extend_from_slice(samples),
            _ => return Err(EncodeError::InvalidInput("sample layout".into())),
        }
        while self.pending.len() >= self.frame_size * self.channels {
            self.take_packet()?;
        }
        Ok(())
    }

    fn drain(&mut self, out: &mut Vec<EncodedPacket>) -> Result<(), EncodeError> {
        out.append(&mut self.ready);
        Ok(())
    }

    fn reconfigure(&mut self, change: &EncoderChange) -> Result<(), EncodeError> {
        match change {
            EncoderChange::RequestKeyframe => Ok(()),
            EncoderChange::Bitrate(_) => Err(EncodeError::Unsupported),
            _ => Err(EncodeError::Unsupported),
        }
    }

    fn flush(&mut self, out: &mut Vec<EncodedPacket>) -> Result<(), EncodeError> {
        if !self.pending.is_empty() {
            let need = self.frame_size * self.channels;
            self.pending.resize(need, 0.0);
            self.take_packet()?;
        }
        self.drain(out)
    }
}

fn bit_depth(config: &AudioEncoderConfig) -> Result<u16, CreateError> {
    let Some((_, value)) = config
        .adapter_options
        .iter()
        .find(|(key, _)| key == "bit_depth")
    else {
        return Ok(16);
    };
    match value.as_str() {
        "16" => Ok(16),
        "24" => Ok(24),
        "32" => Ok(32),
        other => Err(CreateError::Unsupported {
            what: format!("bit_depth {other}"),
        }),
    }
}

fn to_f32(sample: f32) -> f32 {
    if sample.is_nan() { 0.0 } else { sample }
}

fn to_i16(sample: f32) -> i16 {
    let scaled = to_f32(sample).clamp(-1.0, 1.0) * 32768.0;
    scaled.clamp(-32768.0, 32767.0) as i16
}

fn to_i24(sample: f32) -> i32 {
    let scaled = to_f32(sample).clamp(-1.0, 1.0) * 8_388_608.0;
    scaled.clamp(-8_388_608.0, 8_388_607.0) as i32
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn full_scale_and_nan_become_the_integer_extremes_and_silence() {
        let mut session = PcmSession::new(&AudioEncoderConfig::new(
            CodecId::Pcm,
            AudioFormat::STEREO_48K,
            0,
        ))
        .unwrap();
        assert_eq!(session.frame_size(), 480);
        assert_eq!(session.stream().extradata.as_ref(), &[16]);
        let mut left = vec![0.0; 480];
        let mut right = vec![0.0; 480];
        left[0] = 1.0;
        right[0] = f32::NAN;
        left[1] = -1.0;
        let block = AudioBlock::planar(
            AudioFormat::STEREO_48K,
            Timestamp::from_nanos(0),
            &[&left, &right],
        )
        .unwrap();
        session.submit(&block).unwrap();
        let mut packets = Vec::new();
        session.drain(&mut packets).unwrap();
        assert_eq!(packets.len(), 1);
        let data = packets[0].data();
        assert_eq!(i16::from_le_bytes([data[0], data[1]]), 32767);
        assert_eq!(i16::from_le_bytes([data[2], data[3]]), 0);
        assert_eq!(i16::from_le_bytes([data[4], data[5]]), -32768);
    }

    #[test]
    fn an_unknown_bit_depth_is_refused() {
        let mut config = AudioEncoderConfig::new(CodecId::Pcm, AudioFormat::STEREO_48K, 0);
        config
            .adapter_options
            .push(("bit_depth".into(), "12".into()));
        assert!(matches!(
            PcmSession::new(&config),
            Err(CreateError::Unsupported { .. })
        ));
    }
}
