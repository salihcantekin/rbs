//! The platform-neutral part of a session: configuration checks, planar `f32` staging,
//! the sample timeline with its discontinuity rule, and packet timestamps. An [`Engine`]
//! turns staged frames into AAC access units (libavcodec, or the fake engine of the
//! `testing` feature).

use std::time::Duration;

use rbs_codec::{AudioEncoderConfig, CreateError, EncodeError, EncoderChange, caps};
use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::media::{
    AudioBlock, AudioFormat, AudioSamples, BitstreamFormat, Bytes, ChannelLayout, CodecId,
    EncodedPacket, StreamDescriptor, StreamParams,
};
use rbs_core::time::Timestamp;
use rbs_core::timing::SampleRate;
use rbs_media_format::aac::AudioSpecificConfig;

use crate::{FRAME_SIZE, MAX_BITRATE_PER_CHANNEL, MIN_BITRATE, SAMPLE_RATES};

pub(crate) const FRAME: usize = FRAME_SIZE as usize;
pub(crate) const MAX_CHANNELS: usize = 2;

/// Packets a session holds between `drain` calls before its queue grows.
const PENDING: usize = 32;

/// Turns frames of planar `f32` samples into AAC-LC access units.
pub(crate) trait Engine: Send {
    /// Encodes one frame (`FRAME` samples per channel, channel after channel) and
    /// pushes every access unit it produces, in order.
    fn encode(&mut self, planar: &[f32], out: &mut Output) -> Result<(), EncodeError>;

    /// Pushes every access unit still buffered (the encoder delay) and gets ready for a
    /// new timeline that starts from silence.
    fn finish(&mut self, out: &mut Output) -> Result<(), EncodeError>;
}

/// What the probe declares, for the real and the fake factory.
pub(crate) fn capabilities() -> CapabilitySet {
    CapabilitySet::new()
        .with(caps::HARDWARE, CapabilityValue::Flag(false))
        .with(
            caps::AUDIO_ENCODE,
            CapabilityValue::Set(vec![CodecId::Aac.as_str().to_owned()]),
        )
        .with(
            caps::AUDIO_SAMPLE_RATES,
            CapabilityValue::Set(SAMPLE_RATES.iter().map(u32::to_string).collect()),
        )
        .with(
            caps::AUDIO_CHANNELS_MAX,
            CapabilityValue::Int(MAX_CHANNELS as i64),
        )
        .with(
            caps::RECONFIGURE,
            // libavcodec's AAC encoder reads its bitrate once, when it opens.
            CapabilityValue::Set(vec![caps::RECONFIGURE_KEYFRAME.to_owned()]),
        )
}

/// A configuration the session can encode.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Checked {
    pub format: AudioFormat,
    pub channels: usize,
    #[cfg_attr(
        not(all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        )),
        allow(dead_code)
    )]
    pub bitrate_bps: u64,
    pub asc: AudioSpecificConfig,
}

/// Checks a configuration against what the adapter declares.
pub(crate) fn check(config: &AudioEncoderConfig) -> Result<Checked, CreateError> {
    if config.codec != CodecId::Aac {
        return Err(CreateError::Unsupported {
            what: format!("codec {}", config.codec),
        });
    }
    let format = config.format;
    let rate = format.sample_rate.hz();
    if !SAMPLE_RATES.contains(&rate) {
        return Err(CreateError::Unsupported {
            what: format!("sample rate {rate}"),
        });
    }
    let channels = match format.channels {
        ChannelLayout::Mono => 1,
        ChannelLayout::Stereo => 2,
        other => {
            return Err(CreateError::Unsupported {
                what: format!("channel layout {other:?}"),
            });
        }
    };
    let max = MAX_BITRATE_PER_CHANNEL * channels as u64;
    if !(MIN_BITRATE..=max).contains(&config.bitrate_bps) {
        return Err(CreateError::InvalidConfig(format!(
            "AAC bitrate {} is outside {MIN_BITRATE}..={max} for {channels} channel(s)",
            config.bitrate_bps
        )));
    }
    let asc = AudioSpecificConfig::lc(rate, channels as u8)
        .map_err(|e| CreateError::InvalidConfig(format!("{e:?}")))?;
    Ok(Checked {
        format,
        channels,
        bitrate_bps: config.bitrate_bps,
        asc,
    })
}

/// Packets on their way to `drain`, with the timeline that stamps them.
pub(crate) struct Output {
    pending: Vec<EncodedPacket>,
    rate: SampleRate,
    /// Timeline start of the packets: the input's origin, or the end of the previous
    /// timeline's last packet if that is later (invariant 4 after a discontinuity).
    packet_origin: Timestamp,
    packets_out: u64,
    /// End (`pts + duration`) of the last packet emitted, on any timeline.
    last_end: Option<Timestamp>,
}

impl Output {
    /// Queues one access unit: one exact-size `Vec`, the only allocation per packet.
    pub(crate) fn push(&mut self, data: &[u8]) -> Result<(), EncodeError> {
        if data.is_empty() {
            // A packet without bytes would shift every later timestamp.
            return Err(EncodeError::Failed {
                reason: "the AAC encoder returned an empty packet".into(),
                recoverable: false,
            });
        }
        let frame = u64::from(FRAME_SIZE);
        let pts = self.packet_origin + self.rate.duration_of(self.packets_out * frame);
        let duration = self.rate.duration_of(frame);
        let packet = EncodedPacket::new(
            CodecId::Aac,
            BitstreamFormat::Raw,
            pts,
            pts,
            true,
            Bytes::from(data.to_vec()),
        )
        .map_err(|e| EncodeError::Failed {
            reason: e.to_string(),
            recoverable: false,
        })?
        .with_duration(duration);
        self.pending.push(packet);
        self.packets_out += 1;
        self.last_end = Some(pts + duration);
        Ok(())
    }
}

/// One AAC session over an engine.
pub(crate) struct Session<E> {
    engine: E,
    stream: StreamDescriptor,
    format: AudioFormat,
    channels: usize,
    /// Planar staging frame: `FRAME` samples of channel 0, then of channel 1.
    staging: Box<[f32]>,
    staged: usize,
    out: Output,
    /// Timeline start as the input sees it (the first block's timestamp), for the
    /// continuity check.
    origin: Option<Timestamp>,
    samples_in: u64,
}

impl<E: Engine> Session<E> {
    pub(crate) fn new(engine: E, checked: &Checked) -> Self {
        let format = checked.format;
        Self {
            engine,
            stream: StreamDescriptor::new(
                CodecId::Aac,
                BitstreamFormat::Raw,
                Bytes::copy_from_slice(&checked.asc.to_bytes()),
                StreamParams::audio(format, FRAME_SIZE),
            ),
            format,
            channels: checked.channels,
            staging: vec![0.0; checked.channels * FRAME].into_boxed_slice(),
            staged: 0,
            out: Output {
                pending: Vec::with_capacity(PENDING),
                rate: format.sample_rate,
                packet_origin: Timestamp::from_nanos(0),
                packets_out: 0,
                last_end: None,
            },
            origin: None,
            samples_in: 0,
        }
    }

    pub(crate) fn engine(&self) -> &E {
        &self.engine
    }

    /// Pads the staged frame with silence (if any), drains the engine, and forgets the
    /// timeline.
    fn finish_timeline(&mut self) -> Result<(), EncodeError> {
        if self.staged > 0 {
            for plane in self.staging.chunks_exact_mut(FRAME) {
                plane[self.staged..].fill(0.0);
            }
            self.staged = 0;
            self.engine.encode(&self.staging, &mut self.out)?;
        }
        if self.origin.is_some() {
            self.engine.finish(&mut self.out)?;
        }
        self.origin = None;
        self.samples_in = 0;
        self.out.packets_out = 0;
        Ok(())
    }

    pub(crate) fn stream(&self) -> &StreamDescriptor {
        &self.stream
    }

    pub(crate) fn submit(&mut self, block: &AudioBlock<'_>) -> Result<(), EncodeError> {
        if block.format() != self.format {
            return Err(EncodeError::InvalidInput(
                "block format differs from the session".into(),
            ));
        }
        let samples = block.samples();
        // Checked before any state changes.
        match &samples {
            AudioSamples::Planar(planes) if planes.len() < self.channels => {
                return Err(EncodeError::InvalidInput("missing channel".into()));
            }
            AudioSamples::Planar(_) | AudioSamples::Interleaved(_) => {}
            _ => return Err(EncodeError::InvalidInput("unknown sample layout".into())),
        }
        if let Some(origin) = self.origin {
            let expected = origin + self.format.sample_rate.duration_of(self.samples_in);
            let gap = block.timestamp().as_nanos().abs_diff(expected.as_nanos());
            let frame = self.format.sample_rate.duration_of(u64::from(FRAME_SIZE));
            if Duration::from_nanos(gap) >= frame {
                self.finish_timeline()?;
            }
        }
        if self.origin.is_none() {
            let start = block.timestamp();
            self.origin = Some(start);
            // The encoder delay pushes a drained timeline's tail past its last sample;
            // the next timeline never starts before that tail ends (invariant 4).
            self.out.packet_origin = self.out.last_end.map_or(start, |end| end.max(start));
        }
        let frames = block.frames();
        let ch = self.channels;
        let mut done = 0;
        while done < frames {
            let take = (FRAME - self.staged).min(frames - done);
            for c in 0..ch {
                let dst =
                    &mut self.staging[c * FRAME + self.staged..c * FRAME + self.staged + take];
                match &samples {
                    AudioSamples::Interleaved(data) => {
                        let src = &data[done * ch..(done + take) * ch];
                        for (d, s) in dst.iter_mut().zip(src.iter().skip(c).step_by(ch)) {
                            *d = *s;
                        }
                    }
                    AudioSamples::Planar(planes) => {
                        let src = planes
                            .get(c)
                            .ok_or_else(|| EncodeError::InvalidInput("missing channel".into()))?;
                        dst.copy_from_slice(&src[done..done + take]);
                    }
                    _ => return Err(EncodeError::InvalidInput("unknown sample layout".into())),
                }
            }
            self.staged += take;
            done += take;
            if self.staged == FRAME {
                self.staged = 0;
                self.engine.encode(&self.staging, &mut self.out)?;
            }
        }
        self.samples_in += frames as u64;
        Ok(())
    }

    pub(crate) fn drain(&mut self, out: &mut Vec<EncodedPacket>) -> Result<(), EncodeError> {
        out.append(&mut self.out.pending);
        Ok(())
    }

    pub(crate) fn reconfigure(&mut self, change: &EncoderChange) -> Result<(), EncodeError> {
        match change {
            // Every AAC packet is a keyframe.
            EncoderChange::RequestKeyframe => Ok(()),
            _ => Err(EncodeError::Unsupported),
        }
    }

    pub(crate) fn flush(&mut self, out: &mut Vec<EncodedPacket>) -> Result<(), EncodeError> {
        self.finish_timeline()?;
        // A flush ends the stream: what follows is a new stream on its own time base.
        self.out.last_end = None;
        self.drain(out)
    }
}

/// Implements `AudioEncoder` for a newtype over `Session<E>` in field `0`.
macro_rules! delegate_audio_encoder {
    ($ty:ty) => {
        impl rbs_codec::AudioEncoder for $ty {
            fn stream(&self) -> &rbs_core::media::StreamDescriptor {
                self.0.stream()
            }

            fn frame_size(&self) -> u32 {
                $crate::FRAME_SIZE
            }

            fn submit(
                &mut self,
                block: &rbs_core::media::AudioBlock<'_>,
            ) -> Result<(), rbs_codec::EncodeError> {
                self.0.submit(block)
            }

            fn drain(
                &mut self,
                out: &mut Vec<rbs_core::media::EncodedPacket>,
            ) -> Result<(), rbs_codec::EncodeError> {
                self.0.drain(out)
            }

            fn reconfigure(
                &mut self,
                change: &rbs_codec::EncoderChange,
            ) -> Result<(), rbs_codec::EncodeError> {
                self.0.reconfigure(change)
            }

            fn flush(
                &mut self,
                out: &mut Vec<rbs_core::media::EncodedPacket>,
            ) -> Result<(), rbs_codec::EncodeError> {
                self.0.flush(out)
            }
        }
    };
}
pub(crate) use delegate_audio_encoder;
