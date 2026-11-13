//! FLAC encoder for the RBS audio encoder port (`rbs-codec`).
//!
//! Pure Rust, every platform. Turns the graph's `f32` blocks into native FLAC
//! frames (RFC 9639, the streamable subset): fixed predictors of order 0 to 4,
//! one Rice partition, constant and verbatim subframes. A frame is 4096 samples
//! except the last, which may be shorter. 16-bit is the default. `adapter_options`
//! key `bit_depth` (`16` or `24`) selects the integer width; FLAC has no float
//! samples, so `32` is refused. NaN becomes silence. `1.0` is the largest finite
//! integer of that width.
//!
//! The frame bytes are the one allocation per frame (`Bytes::copy_from_slice`).
//! Residual scratch and the bit buffer are reserved at creation. No lock and no
//! I/O on `submit`.

mod frame;

use frame::{BLOCK, BitWriter, FrameIn, encode_planar, rate_error, rate_fields};

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
pub const ADAPTER_ID: &str = "org.rbs.codec.flac";

/// Factory for FLAC sessions. Probing is free.
#[derive(Debug, Default, Clone, Copy)]
pub struct FlacEncoderFactory;

impl AdapterFactory for FlacEncoderFactory {
    fn id(&self) -> AdapterId {
        AdapterId::new(ADAPTER_ID)
    }

    fn display_name(&self) -> &str {
        "FLAC"
    }

    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        Ok(CapabilitySet::new()
            .with(caps::HARDWARE, CapabilityValue::Flag(false))
            .with(
                caps::AUDIO_ENCODE,
                CapabilityValue::Set(vec![CodecId::Flac.as_str().to_owned()]),
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

impl AudioEncoderFactory for FlacEncoderFactory {
    fn create(&self, config: &AudioEncoderConfig) -> Result<Box<dyn AudioEncoder>, CreateError> {
        Ok(Box::new(FlacSession::new(config)?))
    }
}

struct FlacSession {
    stream: StreamDescriptor,
    format: AudioFormat,
    channels: usize,
    bits: u16,
    bps: u8,
    rate: u32,
    planar: Vec<i32>,
    filled: usize,
    origin: Option<Timestamp>,
    frame_number: u32,
    ready: Vec<EncodedPacket>,
    residuals: Vec<i32>,
    frame: BitWriter,
}

impl FlacSession {
    fn new(config: &AudioEncoderConfig) -> Result<Self, CreateError> {
        if config.codec != CodecId::Flac {
            return Err(CreateError::Unsupported {
                what: format!("{} (this adapter writes flac)", config.codec),
            });
        }
        let channels = config.format.channels.channels();
        let hz = config.format.sample_rate.hz();
        if channels == 0 || channels > 8 || hz == 0 {
            return Err(CreateError::InvalidConfig(
                "flac needs 1 to 8 channels and a sample rate".into(),
            ));
        }
        if rate_fields(hz).is_err() {
            return Err(CreateError::Unsupported {
                what: rate_error(hz),
            });
        }
        let bits = bit_depth(config)?;
        Ok(Self {
            stream: StreamDescriptor::new(
                CodecId::Flac,
                BitstreamFormat::Raw,
                Bytes::from(vec![bits as u8]),
                StreamParams::audio(config.format, BLOCK as u32),
            ),
            format: config.format,
            channels,
            bits,
            bps: bits as u8,
            rate: hz,
            planar: vec![0; channels * BLOCK],
            filled: 0,
            origin: None,
            frame_number: 0,
            ready: Vec::with_capacity(4),
            residuals: Vec::with_capacity(BLOCK),
            frame: BitWriter::with_capacity(channels * BLOCK * 3 + 64),
        })
    }

    fn emit(&mut self, n: usize) -> Result<(), EncodeError> {
        let origin = self.origin.take().ok_or(EncodeError::Failed {
            reason: "flac frame without a start time".into(),
            recoverable: false,
        })?;
        self.frame.clear();
        encode_planar(
            &FrameIn {
                planar: &self.planar,
                channels: self.channels,
                n,
                stride: BLOCK,
                bps: self.bps,
                rate_hz: self.rate,
                frame_number: self.frame_number,
            },
            &mut self.residuals,
            &mut self.frame,
        )
        .map_err(|_| EncodeError::Failed {
            reason: "flac frame".into(),
            recoverable: false,
        })?;
        let data = Bytes::copy_from_slice(self.frame.as_slice());
        self.frame.clear();
        let packet = EncodedPacket::new(
            CodecId::Flac,
            BitstreamFormat::Raw,
            origin,
            origin,
            true,
            data,
        )
        .map_err(|_| EncodeError::Failed {
            reason: "flac timestamp".into(),
            recoverable: false,
        })?
        .with_duration(self.format.sample_rate.duration_of(n as u64));
        self.ready.push(packet);
        self.frame_number = self
            .frame_number
            .checked_add(1)
            .ok_or(EncodeError::Failed {
                reason: "flac frame number".into(),
                recoverable: false,
            })?;
        self.filled = 0;
        Ok(())
    }

    fn push_sample(&mut self, channel: usize, sample: f32) {
        self.planar[channel * BLOCK + self.filled] = to_int(sample, self.bits);
    }
}

impl AudioEncoder for FlacSession {
    fn stream(&self) -> &StreamDescriptor {
        &self.stream
    }

    fn frame_size(&self) -> u32 {
        BLOCK as u32
    }

    fn submit(&mut self, block: &AudioBlock<'_>) -> Result<(), EncodeError> {
        if block.format() != self.format {
            return Err(EncodeError::InvalidInput("format".into()));
        }
        let frames = block.frames();
        match block.samples() {
            AudioSamples::Planar(channels) => {
                for channel in 0..self.channels {
                    let samples = channels
                        .get(channel)
                        .ok_or_else(|| EncodeError::InvalidInput("channel".into()))?;
                    if samples.len() != frames {
                        return Err(EncodeError::InvalidInput("channel length".into()));
                    }
                }
                for frame in 0..frames {
                    if self.filled == 0 {
                        self.origin = Some(
                            block.timestamp() + self.format.sample_rate.duration_of(frame as u64),
                        );
                    }
                    for channel in 0..self.channels {
                        let samples = channels
                            .get(channel)
                            .ok_or_else(|| EncodeError::InvalidInput("channel".into()))?;
                        self.push_sample(channel, samples[frame]);
                    }
                    self.filled += 1;
                    if self.filled == BLOCK {
                        self.emit(BLOCK)?;
                    }
                }
            }
            AudioSamples::Interleaved(samples) => {
                if samples.len() != frames * self.channels {
                    return Err(EncodeError::InvalidInput("interleaved length".into()));
                }
                for frame in 0..frames {
                    if self.filled == 0 {
                        self.origin = Some(
                            block.timestamp() + self.format.sample_rate.duration_of(frame as u64),
                        );
                    }
                    for channel in 0..self.channels {
                        self.push_sample(channel, samples[frame * self.channels + channel]);
                    }
                    self.filled += 1;
                    if self.filled == BLOCK {
                        self.emit(BLOCK)?;
                    }
                }
            }
            _ => return Err(EncodeError::InvalidInput("sample layout".into())),
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
        if self.filled > 0 {
            self.emit(self.filled)?;
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
        other => Err(CreateError::Unsupported {
            what: format!("bit_depth {other}"),
        }),
    }
}

fn to_int(sample: f32, bits: u16) -> i32 {
    let sample = if sample.is_nan() {
        0.0
    } else {
        sample.clamp(-1.0, 1.0)
    };
    if bits == 16 {
        (sample * 32768.0).clamp(-32768.0, 32767.0) as i32
    } else {
        (sample * 8_388_608.0).clamp(-8_388_608.0, 8_388_607.0) as i32
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn full_scale_and_nan_survive_a_frame() {
        let mut session = FlacSession::new(&AudioEncoderConfig::new(
            CodecId::Flac,
            AudioFormat::STEREO_48K,
            0,
        ))
        .unwrap();
        assert_eq!(session.frame_size(), 4096);
        assert_eq!(session.stream().extradata.as_ref(), &[16]);
        let mut left = vec![0.0; 4096];
        let mut right = vec![0.0; 4096];
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
        assert_eq!(packets[0].codec(), CodecId::Flac);
        let decoded = frame::tests_decode(packets[0].data()).unwrap();
        assert_eq!(decoded[0][0], 32767);
        assert_eq!(decoded[1][0], 0);
        assert_eq!(decoded[0][1], -32768);
    }

    #[test]
    fn an_unknown_bit_depth_is_refused() {
        let mut config = AudioEncoderConfig::new(CodecId::Flac, AudioFormat::STEREO_48K, 0);
        config
            .adapter_options
            .push(("bit_depth".into(), "32".into()));
        assert!(matches!(
            FlacSession::new(&config),
            Err(CreateError::Unsupported { .. })
        ));
    }

    #[test]
    fn a_wrong_format_is_refused_and_a_short_flush_still_emits() {
        let mut session = FlacSession::new(&AudioEncoderConfig::new(
            CodecId::Flac,
            AudioFormat::STEREO_48K,
            0,
        ))
        .unwrap();
        let mono = AudioFormat::new(
            AudioFormat::STEREO_48K.sample_rate,
            rbs_core::media::ChannelLayout::Mono,
        )
        .unwrap();
        let silence = vec![0.0; 16];
        let block = AudioBlock::planar(mono, Timestamp::from_nanos(0), &[&silence]).unwrap();
        assert!(session.submit(&block).is_err());
        let left = vec![0.1; 480];
        let right = vec![-0.1; 480];
        let block = AudioBlock::planar(
            AudioFormat::STEREO_48K,
            Timestamp::from_nanos(5_000_000_000),
            &[&left, &right],
        )
        .unwrap();
        session.submit(&block).unwrap();
        let mut packets = Vec::new();
        session.flush(&mut packets).unwrap();
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].pts(), Timestamp::from_nanos(5_000_000_000));
        let decoded = frame::tests_decode(packets[0].data()).unwrap();
        assert_eq!(decoded[0].len(), 480);
    }
}
