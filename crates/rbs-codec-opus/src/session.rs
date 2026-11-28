//! Encoder and decoder sessions over the libopus states of `ffi`.

use rbs_codec::{
    AudioDecoder, AudioDecoderConfig, AudioEncoder, AudioEncoderConfig, CreateError, DecodeError,
    EncodeError, EncoderChange, RateControl,
};
use rbs_core::media::{
    AudioBlock, AudioFormat, AudioSamples, BitstreamFormat, Bytes, CodecId, EncodedPacket,
    StreamDescriptor, StreamParams,
};
use rbs_core::time::Timestamp;

use crate::options::EncoderOptions;
use crate::{MAX_PACKET_FRAMES, SAMPLE_RATE_HZ, ffi};

/// Largest packet libopus produces for one frame (RFC 6716: 1275 bytes per frame, up to
/// 3 frames; the encoder here emits one frame per packet), with room to spare.
const MAX_PACKET_BYTES: usize = 1500;
const MIN_BITRATE: u64 = 500;
const MAX_BITRATE_PER_CHANNEL: u64 = 512_000;

fn channel_count(format: AudioFormat) -> Result<usize, CreateError> {
    if format.sample_rate.hz() != SAMPLE_RATE_HZ {
        return Err(CreateError::Unsupported {
            what: format!(
                "{} Hz (opus streams here are 48000 Hz)",
                format.sample_rate.hz()
            ),
        });
    }
    match format.channels.channels() {
        n @ (1 | 2) => Ok(n),
        n => Err(CreateError::Unsupported {
            what: format!("{n} channels (opus takes 1 or 2)"),
        }),
    }
}

fn code_error(what: &str, code: ffi::OpusCode) -> CreateError {
    CreateError::Device(format!("{what}: libopus error {code}"))
}

/// The `OpusHead` identification header (RFC 7845 section 5.1), channel mapping family 0.
fn opus_head(channels: usize, pre_skip: u16) -> Bytes {
    let mut head = Vec::with_capacity(19);
    head.extend_from_slice(b"OpusHead");
    head.push(1);
    head.push(channels as u8);
    head.extend_from_slice(&pre_skip.to_le_bytes());
    head.extend_from_slice(&SAMPLE_RATE_HZ.to_le_bytes());
    head.extend_from_slice(&0_i16.to_le_bytes());
    head.push(0);
    Bytes::from(head)
}

pub struct EncoderSession {
    stream: StreamDescriptor,
    format: AudioFormat,
    channels: usize,
    frame: usize,
    encoder: ffi::Encoder,
    /// One frame of interleaved input, filled before each encode.
    stage: Vec<f32>,
    /// Sample frames in `stage`.
    filled: usize,
    packet: Vec<u8>,
    ready: Vec<EncodedPacket>,
    /// Input time of the first sample of the current timeline: the continuity of later
    /// blocks is judged against it (output-ports invariant 8).
    in_start: Option<Timestamp>,
    /// Output time of the first packet of the current timeline. Equals `in_start`
    /// except after a backward jump, where it stays at the end of the previous timeline.
    out_start: Option<Timestamp>,
    /// Packets emitted since the timeline started.
    packets: u64,
}

impl EncoderSession {
    pub fn new(config: &AudioEncoderConfig) -> Result<Self, CreateError> {
        if config.codec != CodecId::Opus {
            return Err(CreateError::Unsupported {
                what: format!("{} (this adapter writes opus)", config.codec),
            });
        }
        let channels = channel_count(config.format)?;
        let options = EncoderOptions::parse(&config.adapter_options)?;
        let bps = config.bitrate_bps;
        if !(MIN_BITRATE..=MAX_BITRATE_PER_CHANNEL * channels as u64).contains(&bps) {
            return Err(CreateError::InvalidConfig(format!(
                "opus bitrate {bps} bit/s is outside {MIN_BITRATE} to {}",
                MAX_BITRATE_PER_CHANNEL * channels as u64
            )));
        }
        let mut encoder = ffi::Encoder::new(SAMPLE_RATE_HZ as i32, channels, options.application)
            .map_err(|c| code_error("create encoder", c))?;
        let setup = |r: Result<(), ffi::OpusCode>, what: &str| r.map_err(|c| code_error(what, c));
        setup(encoder.set_bitrate(bps as i32), "bitrate")?;
        setup(encoder.set_complexity(options.complexity), "complexity")?;
        setup(encoder.set_vbr(options.vbr), "vbr")?;
        setup(encoder.set_inband_fec(options.fec), "fec")?;
        setup(
            encoder.set_packet_loss_percent(options.loss_percent),
            "loss percent",
        )?;
        let lookahead = encoder
            .lookahead()
            .map_err(|c| code_error("lookahead", c))?;
        let pre_skip = u16::try_from(lookahead).unwrap_or(0);
        let frame = options.frame;
        Ok(Self {
            stream: StreamDescriptor::new(
                CodecId::Opus,
                BitstreamFormat::Raw,
                opus_head(channels, pre_skip),
                StreamParams::audio(config.format, frame as u32),
            ),
            format: config.format,
            channels,
            frame,
            encoder,
            stage: vec![0.0; frame * channels],
            filled: 0,
            packet: vec![0; MAX_PACKET_BYTES],
            ready: Vec::with_capacity(8),
            in_start: None,
            out_start: None,
            packets: 0,
        })
    }

    fn frame_time(&self, frames: u64) -> std::time::Duration {
        self.format.sample_rate.duration_of(frames)
    }

    /// Input time at which the next sample of the current timeline belongs.
    fn expected(&self, in_start: Timestamp) -> Timestamp {
        in_start + self.frame_time(self.packets * self.frame as u64 + self.filled as u64)
    }

    /// Pads the partial frame with silence and encodes it.
    fn finish_partial(&mut self) -> Result<(), EncodeError> {
        if self.filled > 0 {
            let used = self.filled * self.channels;
            self.stage[used..].fill(0.0);
            self.filled = self.frame;
            self.encode_stage()?;
        }
        Ok(())
    }

    fn encode_stage(&mut self) -> Result<(), EncodeError> {
        // The frame is consumed whether or not libopus accepts it, so a failure never
        // leaves the staging buffer over-full.
        self.filled = 0;
        let start = self.out_start.ok_or_else(|| EncodeError::Failed {
            reason: "opus frame without a start time".into(),
            recoverable: false,
        })?;
        let pts = start + self.frame_time(self.packets * self.frame as u64);
        let len = self
            .encoder
            .encode(&self.stage, self.frame, &mut self.packet)
            .map_err(|code| EncodeError::Failed {
                reason: format!("libopus error {code}"),
                recoverable: false,
            })?;
        self.packets += 1;
        let packet = EncodedPacket::new(
            CodecId::Opus,
            BitstreamFormat::Raw,
            pts,
            pts,
            true,
            Bytes::copy_from_slice(&self.packet[..len]),
        )
        .map_err(|_| EncodeError::Failed {
            reason: "opus timestamp".into(),
            recoverable: false,
        })?
        .with_duration(self.frame_time(self.frame as u64));
        self.ready.push(packet);
        Ok(())
    }

    /// Continues the timeline or, for a gap or overlap of a codec frame or more,
    /// restarts it (output-ports invariant 8).
    fn align(&mut self, ts: Timestamp) -> Result<(), EncodeError> {
        let (Some(in_start), Some(out_start)) = (self.in_start, self.out_start) else {
            self.in_start = Some(ts);
            self.out_start = Some(ts);
            return Ok(());
        };
        let expected = self.expected(in_start);
        let error = ts.as_nanos().abs_diff(expected.as_nanos());
        if error < self.frame_time(self.frame as u64).as_nanos() as u64 {
            return Ok(());
        }
        self.finish_partial()?;
        let end = out_start + self.frame_time(self.packets * self.frame as u64);
        self.in_start = Some(ts);
        self.out_start = Some(ts.max(end));
        self.packets = 0;
        Ok(())
    }

    fn push(
        &mut self,
        frames: usize,
        mut sample: impl FnMut(usize, usize) -> f32,
    ) -> Result<(), EncodeError> {
        for index in 0..frames {
            let base = self.filled * self.channels;
            for channel in 0..self.channels {
                let value = sample(channel, index);
                self.stage[base + channel] = if value.is_finite() { value } else { 0.0 };
            }
            self.filled += 1;
            if self.filled == self.frame {
                self.encode_stage()?;
            }
        }
        Ok(())
    }
}

impl AudioEncoder for EncoderSession {
    fn stream(&self) -> &StreamDescriptor {
        &self.stream
    }

    fn frame_size(&self) -> u32 {
        self.frame as u32
    }

    fn submit(&mut self, block: &AudioBlock<'_>) -> Result<(), EncodeError> {
        if block.format() != self.format {
            return Err(EncodeError::InvalidInput("format".into()));
        }
        let frames = block.frames();
        if frames == 0 {
            return Ok(());
        }
        match block.samples() {
            AudioSamples::Planar(channels) => {
                if channels.len() != self.channels || channels.iter().any(|c| c.len() != frames) {
                    return Err(EncodeError::InvalidInput("channels".into()));
                }
                self.align(block.timestamp())?;
                let mut slices = [&[][..]; 2];
                for (slot, channel) in slices.iter_mut().zip(channels.iter()) {
                    *slot = channel;
                }
                self.push(frames, |channel, index| slices[channel][index])
            }
            AudioSamples::Interleaved(samples) => {
                if samples.len() != frames * self.channels {
                    return Err(EncodeError::InvalidInput("interleaved length".into()));
                }
                self.align(block.timestamp())?;
                let channels = self.channels;
                self.push(frames, |channel, index| samples[index * channels + channel])
            }
            _ => Err(EncodeError::InvalidInput("sample layout".into())),
        }
    }

    fn drain(&mut self, out: &mut Vec<EncodedPacket>) -> Result<(), EncodeError> {
        out.append(&mut self.ready);
        Ok(())
    }

    fn reconfigure(&mut self, change: &EncoderChange) -> Result<(), EncodeError> {
        let (target, vbr) = match change {
            EncoderChange::Bitrate(RateControl::Cbr { bitrate_bps }) => (*bitrate_bps, false),
            EncoderChange::Bitrate(RateControl::Vbr { target_bps, .. }) => (*target_bps, true),
            _ => return Err(EncodeError::Unsupported),
        };
        let max = MAX_BITRATE_PER_CHANNEL * self.channels as u64;
        if !(MIN_BITRATE..=max).contains(&target) {
            return Err(EncodeError::InvalidInput(format!(
                "bitrate {target} outside {MIN_BITRATE} to {max}"
            )));
        }
        let failed = |code| EncodeError::Failed {
            reason: format!("libopus error {code}"),
            recoverable: true,
        };
        self.encoder.set_vbr(vbr).map_err(failed)?;
        self.encoder
            .set_bitrate(target as i32)
            .map_err(|code| EncodeError::Failed {
                reason: format!("libopus error {code}"),
                recoverable: true,
            })
    }

    fn flush(&mut self, out: &mut Vec<EncodedPacket>) -> Result<(), EncodeError> {
        self.finish_partial()?;
        // A block after a flush starts a new timeline exactly at its timestamp (as the
        // AAC adapters do).
        self.in_start = None;
        self.out_start = None;
        self.packets = 0;
        self.drain(out)
    }
}

pub struct DecoderSession {
    format: AudioFormat,
    channels: usize,
    decoder: ffi::Decoder,
}

impl DecoderSession {
    pub fn new(config: &AudioDecoderConfig) -> Result<Self, CreateError> {
        if config.codec != CodecId::Opus {
            return Err(CreateError::Unsupported {
                what: format!("{} (this adapter reads opus)", config.codec),
            });
        }
        let channels = channel_count(config.format)?;
        if let Some((key, _)) = config.adapter_options.first() {
            return Err(CreateError::Unsupported {
                what: format!("adapter option {key}"),
            });
        }
        if !config.extradata.is_empty() {
            // OpusHead: version 1, channel count, channel mapping family 0. Pre-skip and
            // output gain are ignored for live streams.
            let head = &config.extradata;
            let ok = head.len() >= 19
                && head.starts_with(b"OpusHead")
                && head[8] == 1
                && usize::from(head[9]) == channels
                && head[18] == 0;
            if !ok {
                return Err(CreateError::InvalidConfig(
                    "opus extradata is not an OpusHead of version 1, mapping family 0 and the session's channel count".into(),
                ));
            }
        }
        let decoder = ffi::Decoder::new(SAMPLE_RATE_HZ as i32, channels)
            .map_err(|c| code_error("create decoder", c))?;
        Ok(Self {
            format: config.format,
            channels,
            decoder,
        })
    }

    fn run(
        &mut self,
        packet: Option<&[u8]>,
        frames: usize,
        out: &mut Vec<f32>,
    ) -> Result<usize, DecodeError> {
        out.clear();
        out.resize(frames * self.channels, 0.0);
        match self.decoder.decode(packet, out, frames) {
            Ok(n) => {
                out.truncate(n * self.channels);
                Ok(n)
            }
            Err(code) => {
                out.clear();
                Err(match code {
                    -4 => DecodeError::Corrupt("not a valid opus packet".into()),
                    -1 | -2 => DecodeError::InvalidInput(format!("libopus error {code}")),
                    _ => DecodeError::Failed {
                        reason: format!("libopus error {code}"),
                        recoverable: false,
                    },
                })
            }
        }
    }
}

impl AudioDecoder for DecoderSession {
    fn format(&self) -> AudioFormat {
        self.format
    }

    fn max_frames(&self) -> usize {
        MAX_PACKET_FRAMES
    }

    fn decode(&mut self, packet: &EncodedPacket, out: &mut Vec<f32>) -> Result<usize, DecodeError> {
        if packet.codec() != CodecId::Opus {
            return Err(DecodeError::InvalidInput(format!(
                "{} packet in an opus decoder",
                packet.codec()
            )));
        }
        if packet.bitstream() != BitstreamFormat::Raw {
            return Err(DecodeError::InvalidInput(
                "opus packets must be raw (no container)".into(),
            ));
        }
        let data = packet.data();
        let frames = ffi::packet_samples(data, SAMPLE_RATE_HZ as i32).map_err(|code| {
            if data.is_empty() {
                DecodeError::InvalidInput("empty packet".into())
            } else {
                DecodeError::Corrupt(format!("libopus error {code}"))
            }
        })?;
        if frames == 0 || frames > MAX_PACKET_FRAMES {
            return Err(DecodeError::Corrupt(format!("{frames} sample frames")));
        }
        self.run(Some(data), frames, out)
    }

    fn conceal(&mut self, frames: usize, out: &mut Vec<f32>) -> Result<usize, DecodeError> {
        if frames == 0 || !frames.is_multiple_of(120) || frames > MAX_PACKET_FRAMES {
            return Err(DecodeError::InvalidInput(format!(
                "cannot conceal {frames} sample frames (multiples of 120 up to {MAX_PACKET_FRAMES})"
            )));
        }
        self.run(None, frames, out)
    }

    fn reset(&mut self) {
        // OPUS_RESET_STATE cannot fail on a live state; a failure would leave the old state,
        // which still decodes, so it is not worth an error path.
        let _ = self.decoder.reset();
    }
}
