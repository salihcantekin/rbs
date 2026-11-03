//! macOS implementation over `AudioConverter`.
//!
//! This is the adapter's low-level module (principle P11): every `unsafe` block states
//! its invariant.

use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::NonNull;
use std::time::Duration;

use objc2_audio_toolbox::{
    AudioConverterDispose, AudioConverterFillComplexBuffer, AudioConverterGetProperty,
    AudioConverterNew, AudioConverterPrimeInfo, AudioConverterRef, AudioConverterReset,
    AudioConverterSetProperty, kAudioConverterEncodeBitRate, kAudioConverterPrimeInfo,
    kAudioConverterPropertyMaximumOutputPacketSize,
};
use objc2_core_audio_types::{
    AudioBuffer, AudioBufferList, AudioStreamBasicDescription, AudioStreamPacketDescription,
    kAudioFormatFlagIsFloat, kAudioFormatFlagIsNonInterleaved, kAudioFormatFlagIsPacked,
    kAudioFormatLinearPCM, kAudioFormatMPEG4AAC,
};
use rbs_codec::{
    AudioEncoder, AudioEncoderConfig, AudioEncoderFactory, CreateError, EncodeError, EncoderChange,
    RateControl, caps,
};
use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::media::{
    AudioBlock, AudioFormat, AudioSamples, BitstreamFormat, Bytes, ChannelLayout, CodecId,
    EncodedPacket, StreamDescriptor, StreamParams,
};
use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};
use rbs_core::time::Timestamp;
use rbs_media_format::aac::AudioSpecificConfig;

use crate::{FRAME_SIZE, SAMPLE_RATES};

const FRAME: usize = FRAME_SIZE as usize;
const MAX_CHANNELS: usize = 2;

/// Status the input callback returns when it has nothing more right now (not end of
/// stream). Any non-zero value works; the converter returns it from the fill call and
/// keeps its state.
const NEED_MORE: i32 = i32::from_be_bytes(*b"more");

/// Status the input callback returns when the converter asks for a buffer layout the
/// session did not set up (or the callback panicked). `fill` turns it into a failure.
const BAD_INPUT: i32 = i32::from_be_bytes(*b"bad!");

/// Bitrates AudioToolbox's AAC-LC encoder accepts, per channel.
const MIN_BPS_PER_CHANNEL: u64 = 16_000;
const MAX_BPS_PER_CHANNEL: u64 = 160_000;

/// Packets a session holds between `drain` calls before its queue grows.
const PENDING: usize = 32;

/// Factory of the AudioToolbox AAC adapter.
#[derive(Debug, Default)]
pub struct AacFactory;

impl AacFactory {
    /// Creates the factory. Does nothing until used.
    pub fn new() -> Self {
        Self
    }

    /// Creates a session with its concrete type.
    pub fn create_session(&self, config: &AudioEncoderConfig) -> Result<AacSession, CreateError> {
        AacSession::new(config)
    }
}

impl AdapterFactory for AacFactory {
    fn id(&self) -> AdapterId {
        AdapterId::new(crate::ADAPTER_ID)
    }

    fn display_name(&self) -> &str {
        "AudioToolbox AAC"
    }

    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        // The AAC encoder is part of every supported macOS; nothing to open.
        Ok(CapabilitySet::new()
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
                CapabilityValue::Set(vec![
                    caps::RECONFIGURE_BITRATE.to_owned(),
                    caps::RECONFIGURE_KEYFRAME.to_owned(),
                ]),
            ))
    }
}

impl AudioEncoderFactory for AacFactory {
    fn create(&self, config: &AudioEncoderConfig) -> Result<Box<dyn AudioEncoder>, CreateError> {
        Ok(Box::new(self.create_session(config)?))
    }
}

/// What the input callback hands to the converter during one fill call.
struct Input {
    /// Planar staging: channel `c` at `c * FRAME`.
    data: *const f32,
    channels: usize,
    /// Frames to hand over; 0 after they were taken.
    frames: usize,
    /// No more input will come: report end of stream instead of [`NEED_MORE`].
    end: bool,
}

/// The converter's input callback.
unsafe extern "C-unwind" fn input_proc(
    _converter: AudioConverterRef,
    packets: NonNull<u32>,
    data: NonNull<AudioBufferList>,
    _descriptions: *mut *mut AudioStreamPacketDescription,
    user: *mut c_void,
) -> i32 {
    let run = || {
        // SAFETY: `user` is the `Input` that `AacSession::fill` passed for this call;
        // it is not otherwise accessed while the converter runs.
        let input = unsafe { &mut *user.cast::<Input>() };
        if input.frames == 0 {
            // SAFETY: `packets` is the converter's valid in/out count.
            unsafe { packets.write(0) };
            return if input.end { 0 } else { NEED_MORE };
        }
        let list = data.as_ptr();
        // SAFETY: the converter passes a list with one buffer per channel for the
        // non-interleaved source format; `mBuffers` is a flexible array member.
        let buffers = unsafe { std::ptr::addr_of_mut!((*list).mBuffers).cast::<AudioBuffer>() };
        // SAFETY: as above.
        let count = unsafe { (*list).mNumberBuffers } as usize;
        if count != input.channels {
            // SAFETY: valid in/out count.
            unsafe { packets.write(0) };
            return BAD_INPUT;
        }
        for c in 0..count {
            // SAFETY: `c < mNumberBuffers`; the staging area outlives the fill call and
            // holds `frames` samples per channel at `c * FRAME`.
            unsafe {
                let b = &mut *buffers.add(c);
                b.mNumberChannels = 1;
                b.mDataByteSize = (input.frames * 4) as u32;
                b.mData = input.data.add(c * FRAME).cast_mut().cast();
            }
        }
        // SAFETY: valid in/out count.
        unsafe { packets.write(input.frames as u32) };
        input.frames = 0;
        0
    };
    // Never unwind into AudioToolbox.
    catch_unwind(AssertUnwindSafe(run)).unwrap_or_else(|_| {
        // SAFETY: valid in/out count.
        unsafe { packets.write(0) };
        BAD_INPUT
    })
}

/// One AAC encoder session.
pub struct AacSession {
    converter: AudioConverterRef,
    stream: StreamDescriptor,
    format: AudioFormat,
    channels: usize,
    staging: Box<[f32]>,
    staged: usize,
    output: Box<[u8]>,
    pending: Vec<EncodedPacket>,
    /// Timeline start as the input sees it (the first block's timestamp), for the
    /// continuity check.
    origin: Option<Timestamp>,
    /// Timeline start of the packets: `origin`, or the end of the previous timeline's
    /// last packet if that is later (invariant 4 after a discontinuity).
    packet_origin: Timestamp,
    /// End (`pts + duration`) of the last packet emitted, on any timeline.
    last_end: Option<Timestamp>,
    samples_in: u64,
    packets_out: u64,
    priming: u32,
}

// SAFETY: an AudioConverter has no thread affinity; the session (and with it the
// converter) is used by one thread at a time (port rule).
unsafe impl Send for AacSession {}

impl Drop for AacSession {
    fn drop(&mut self) {
        // SAFETY: the converter was created by `AudioConverterNew` and is disposed once.
        unsafe { AudioConverterDispose(self.converter) };
    }
}

fn device(what: &str, status: i32) -> CreateError {
    CreateError::Device(format!("{what} failed with OSStatus {status}"))
}

fn check_bitrate(bps: u64, channels: usize) -> Result<(), String> {
    let ch = channels as u64;
    if (MIN_BPS_PER_CHANNEL * ch..=MAX_BPS_PER_CHANNEL * ch).contains(&bps) {
        Ok(())
    } else {
        Err(format!(
            "AAC bitrate {bps} outside {}..={} for {channels} channel(s)",
            MIN_BPS_PER_CHANNEL * ch,
            MAX_BPS_PER_CHANNEL * ch
        ))
    }
}

fn set_bitrate(converter: AudioConverterRef, bps: u64) -> i32 {
    let value = u32::try_from(bps).unwrap_or(u32::MAX);
    // SAFETY: the converter is valid and the property is a `u32` read during the call.
    unsafe {
        AudioConverterSetProperty(
            converter,
            kAudioConverterEncodeBitRate,
            4,
            NonNull::from(&value).cast(),
        )
    }
}

impl AacSession {
    fn new(config: &AudioEncoderConfig) -> Result<Self, CreateError> {
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
        check_bitrate(config.bitrate_bps, channels).map_err(CreateError::InvalidConfig)?;
        let asc = AudioSpecificConfig::lc(rate, channels as u8)
            .map_err(|e| CreateError::InvalidConfig(format!("{e:?}")))?;

        let source = AudioStreamBasicDescription {
            mSampleRate: f64::from(rate),
            mFormatID: kAudioFormatLinearPCM,
            mFormatFlags: kAudioFormatFlagIsFloat
                | kAudioFormatFlagIsPacked
                | kAudioFormatFlagIsNonInterleaved,
            mBytesPerPacket: 4,
            mFramesPerPacket: 1,
            mBytesPerFrame: 4,
            mChannelsPerFrame: channels as u32,
            mBitsPerChannel: 32,
            mReserved: 0,
        };
        let destination = AudioStreamBasicDescription {
            mSampleRate: f64::from(rate),
            mFormatID: kAudioFormatMPEG4AAC,
            mFormatFlags: 0, // kMPEG4Object_AAC_LC is implied by 0 for 'aac '.
            mBytesPerPacket: 0,
            mFramesPerPacket: FRAME_SIZE,
            mBytesPerFrame: 0,
            mChannelsPerFrame: channels as u32,
            mBitsPerChannel: 0,
            mReserved: 0,
        };
        let mut converter: AudioConverterRef = std::ptr::null_mut();
        // SAFETY: valid descriptions and out pointer; the converter is disposed on drop.
        let status = unsafe {
            AudioConverterNew(
                NonNull::from(&source),
                NonNull::from(&destination),
                NonNull::from(&mut converter),
            )
        };
        if status != 0 || converter.is_null() {
            return Err(device("AudioConverterNew", status));
        }
        // From here on, dropping `session` disposes the converter on every error path.
        let mut session = Self {
            converter,
            stream: StreamDescriptor::new(
                CodecId::Aac,
                BitstreamFormat::Raw,
                Bytes::copy_from_slice(&asc.to_bytes()),
                StreamParams::audio(format, FRAME_SIZE),
            ),
            format,
            channels,
            staging: vec![0.0; channels * FRAME].into_boxed_slice(),
            staged: 0,
            output: Box::new([]),
            pending: Vec::with_capacity(PENDING),
            origin: None,
            packet_origin: Timestamp::from_nanos(0),
            last_end: None,
            samples_in: 0,
            packets_out: 0,
            priming: 0,
        };
        let status = set_bitrate(converter, config.bitrate_bps);
        if status != 0 {
            return Err(device("kAudioConverterEncodeBitRate", status));
        }
        let mut max_packet: u32 = 0;
        let mut size = 4u32;
        // SAFETY: valid converter; the property is a `u32` written into `max_packet`.
        let status = unsafe {
            AudioConverterGetProperty(
                converter,
                kAudioConverterPropertyMaximumOutputPacketSize,
                NonNull::from(&mut size),
                NonNull::from(&mut max_packet).cast(),
            )
        };
        if status != 0 || max_packet == 0 {
            return Err(device("MaximumOutputPacketSize", status));
        }
        session.output = vec![0u8; max_packet as usize].into_boxed_slice();
        let mut prime = AudioConverterPrimeInfo {
            leadingFrames: 0,
            trailingFrames: 0,
        };
        let mut size = std::mem::size_of::<AudioConverterPrimeInfo>() as u32;
        // SAFETY: valid converter; the property is an `AudioConverterPrimeInfo`.
        let status = unsafe {
            AudioConverterGetProperty(
                converter,
                kAudioConverterPrimeInfo,
                NonNull::from(&mut size),
                NonNull::from(&mut prime).cast(),
            )
        };
        if status == 0 {
            session.priming = prime.leadingFrames;
        }
        Ok(session)
    }

    /// Priming samples the encoder puts before the first input sample
    /// (`leadingFrames`), 0 if AudioToolbox does not report it.
    pub fn priming(&self) -> u32 {
        self.priming
    }

    fn timestamp_of(&self, packet: u64) -> Timestamp {
        self.packet_origin
            + self
                .format
                .sample_rate
                .duration_of(packet * u64::from(FRAME_SIZE))
    }

    /// Hands `frames` staged frames to the converter (or none, with `end`) and moves
    /// every packet it produces to `pending`.
    fn fill(&mut self, frames: usize, end: bool) -> Result<(), EncodeError> {
        let mut input = Input {
            data: self.staging.as_ptr(),
            channels: self.channels,
            frames,
            end,
        };
        loop {
            let mut list = AudioBufferList {
                mNumberBuffers: 1,
                mBuffers: [AudioBuffer {
                    mNumberChannels: self.channels as u32,
                    mDataByteSize: self.output.len() as u32,
                    mData: self.output.as_mut_ptr().cast(),
                }],
            };
            let mut packets: u32 = 1;
            let mut description = AudioStreamPacketDescription {
                mStartOffset: 0,
                mVariableFramesInPacket: 0,
                mDataByteSize: 0,
            };
            // SAFETY: the converter is valid; `input` and the output buffer outlive the
            // call; the callback only reads `staging` through `input`.
            let status = unsafe {
                AudioConverterFillComplexBuffer(
                    self.converter,
                    Some(input_proc),
                    (&raw mut input).cast(),
                    NonNull::from(&mut packets),
                    NonNull::from(&mut list),
                    &mut description,
                )
            };
            if status == BAD_INPUT {
                return Err(EncodeError::Failed {
                    reason: "AudioConverter asked for an unexpected input layout".into(),
                    recoverable: false,
                });
            }
            if status != 0 && status != NEED_MORE {
                return Err(EncodeError::Failed {
                    reason: format!("AudioConverterFillComplexBuffer failed with {status}"),
                    recoverable: false,
                });
            }
            if packets == 0 {
                return Ok(());
            }
            let len = (list.mBuffers[0].mDataByteSize as usize).min(self.output.len());
            if len == 0 {
                // A packet without bytes would shift every later timestamp.
                return Err(EncodeError::Failed {
                    reason: "AudioConverter returned an empty packet".into(),
                    recoverable: false,
                });
            }
            {
                let mut data = Vec::with_capacity(len);
                data.extend_from_slice(&self.output[..len]);
                let pts = self.timestamp_of(self.packets_out);
                let duration = self.format.sample_rate.duration_of(u64::from(FRAME_SIZE));
                let packet = EncodedPacket::new(
                    CodecId::Aac,
                    BitstreamFormat::Raw,
                    pts,
                    pts,
                    true,
                    Bytes::from(data),
                )
                .map_err(|e| EncodeError::Failed {
                    reason: e.to_string(),
                    recoverable: false,
                })?
                .with_duration(duration);
                self.pending.push(packet);
                self.packets_out += 1;
                self.last_end = Some(pts + duration);
            }
            if status == NEED_MORE && input.frames == 0 && !end {
                return Ok(());
            }
        }
    }

    /// Pads the staged frame with silence (if any) and drains the converter to the end,
    /// then resets it for a new timeline.
    fn finish_timeline(&mut self) -> Result<(), EncodeError> {
        if self.staged > 0 {
            for c in 0..self.channels {
                self.staging[c * FRAME + self.staged..(c + 1) * FRAME].fill(0.0);
            }
            self.staged = 0;
            self.fill(FRAME, false)?;
        }
        if self.origin.is_some() {
            self.fill(0, true)?;
            // SAFETY: valid converter.
            let status = unsafe { AudioConverterReset(self.converter) };
            if status != 0 {
                return Err(EncodeError::Failed {
                    reason: format!("AudioConverterReset failed with {status}"),
                    recoverable: false,
                });
            }
        }
        self.origin = None;
        self.samples_in = 0;
        self.packets_out = 0;
        Ok(())
    }
}

impl AudioEncoder for AacSession {
    fn stream(&self) -> &StreamDescriptor {
        &self.stream
    }

    fn frame_size(&self) -> u32 {
        FRAME_SIZE
    }

    fn submit(&mut self, block: &AudioBlock<'_>) -> Result<(), EncodeError> {
        if block.format() != self.format {
            return Err(EncodeError::InvalidInput(
                "block format differs from the session".into(),
            ));
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
            self.packet_origin = self.last_end.map_or(start, |end| end.max(start));
        }
        let frames = block.frames();
        let samples = block.samples();
        let mut done = 0;
        while done < frames {
            let take = (FRAME - self.staged).min(frames - done);
            for c in 0..self.channels {
                let dst =
                    &mut self.staging[c * FRAME + self.staged..c * FRAME + self.staged + take];
                match &samples {
                    AudioSamples::Planar(planes) => {
                        let src = planes
                            .get(c)
                            .ok_or_else(|| EncodeError::InvalidInput("missing channel".into()))?;
                        dst.copy_from_slice(&src[done..done + take]);
                    }
                    AudioSamples::Interleaved(data) => {
                        for (i, d) in dst.iter_mut().enumerate() {
                            *d = data[(done + i) * self.channels + c];
                        }
                    }
                    _ => {
                        return Err(EncodeError::InvalidInput("unknown sample layout".into()));
                    }
                }
            }
            self.staged += take;
            done += take;
            if self.staged == FRAME {
                self.staged = 0;
                self.fill(FRAME, false)?;
            }
        }
        self.samples_in += frames as u64;
        Ok(())
    }

    fn drain(&mut self, out: &mut Vec<EncodedPacket>) -> Result<(), EncodeError> {
        out.append(&mut self.pending);
        Ok(())
    }

    fn reconfigure(&mut self, change: &EncoderChange) -> Result<(), EncodeError> {
        match change {
            // Every AAC packet is a keyframe.
            EncoderChange::RequestKeyframe => Ok(()),
            EncoderChange::Bitrate(RateControl::Cbr { bitrate_bps }) => {
                check_bitrate(*bitrate_bps, self.channels).map_err(EncodeError::InvalidInput)?;
                if set_bitrate(self.converter, *bitrate_bps) == 0 {
                    Ok(())
                } else {
                    Err(EncodeError::Unsupported)
                }
            }
            _ => Err(EncodeError::Unsupported),
        }
    }

    fn flush(&mut self, out: &mut Vec<EncodedPacket>) -> Result<(), EncodeError> {
        self.finish_timeline()?;
        // A flush ends the stream: what follows is a new stream on its own time base.
        self.last_end = None;
        self.drain(out)
    }
}
