//! One live H.264 decoder session over the Media Foundation MFT.
//!
//! Low-level module (principle P11): every `unsafe` block states its invariant.

use std::collections::VecDeque;
use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use rbs_codec::h264::{self, NAL_IDR, NAL_PPS, NAL_SPS};
use rbs_codec::{CreateError, DecodeError, DecoderStats, VideoDecoder, VideoDecoderConfig};
use rbs_core::log::Area;
use rbs_core::media::{
    BitstreamFormat, Bytes, CodecId, ColorSpace, EncodedPacket, MemoryKind, PixelFormat,
    PlaneLayout, Size, SystemFrame, VideoFormat, VideoFrame,
};
use rbs_core::time::Timestamp;
use windows::Win32::Media::MediaFoundation::{
    IMFSample, IMFTransform, MF_E_NOTACCEPTING, MF_E_TRANSFORM_NEED_MORE_INPUT,
    MF_E_TRANSFORM_STREAM_CHANGE, MF_E_TRANSFORM_TYPE_NOT_SET, MFT_MESSAGE_COMMAND_DRAIN,
    MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_END_STREAMING, MFT_OUTPUT_DATA_BUFFER,
};

use super::mft::{self, Layout, failed};
use super::pool::{Gpu, GpuPicture};
use crate::win::Runtime;

/// Packets in the MFT whose pictures have not come out yet. A full queue is `Busy`.
const QUEUE: usize = 16;

/// Media Foundation time of one submission step. Sample times carry the submission
/// index only; the stamps themselves stay in [`Pending`], bit-exact.
const TICK: i64 = 10_000;

/// Output calls in one pull before the session gives up (a misbehaving MFT never
/// keeps the worker in a loop).
const MAX_OUTPUTS_PER_PULL: usize = 64;

/// Largest slot count (the port's limit, backend-ports.md invariant 9).
pub(crate) const MAX_SLOTS: usize = 8;

/// A submitted packet whose picture has not come out yet.
#[derive(Debug, Clone, Copy)]
struct Pending {
    index: u64,
    pts: Timestamp,
    size: Size,
}

#[derive(Default)]
struct Counters {
    pictures: AtomicU64,
    dropped_no_surface: AtomicU64,
    cpu_copies: AtomicU64,
    need_keyframe: AtomicBool,
}

/// One H.264 decoder session over Media Foundation. Field order is drop order: the
/// pictures and the MFT go before the device, the runtime shuts down last.
pub struct MfH264Decoder {
    ready: Vec<VideoFrame>,
    mft: IMFTransform,
    /// The DXVA device and native pool; `None` for system-memory output.
    gpu: Option<Gpu>,
    /// Our output sample when the MFT does not provide its own.
    out_sample: Option<IMFSample>,
    provides: bool,
    layout: Option<Layout>,
    max_size: Size,
    format: Option<VideoFormat>,
    gated: bool,
    next_index: u64,
    pending: VecDeque<Pending>,
    stats: Counters,
    _runtime: Runtime,
}

// SAFETY: the H.264 decoder MFT, Media Foundation samples and the D3D11 objects (made
// multithread protected) are free-threaded in-process objects without thread affinity;
// the session is used by one thread at a time (port rule) and the leases it hands out
// touch only the pool's atomics.
unsafe impl Send for MfH264Decoder {}

fn invalid(what: &str) -> DecodeError {
    DecodeError::InvalidInput(what.into())
}

impl MfH264Decoder {
    /// Opens a session. With a native target on a known adapter, decodes with DXVA
    /// there and hands out pool textures; otherwise decodes in software to system
    /// memory.
    pub(crate) fn open(config: &VideoDecoderConfig) -> Result<Self, CreateError> {
        if config.codec != CodecId::H264 {
            return Err(CreateError::Unsupported {
                what: format!("codec {}", config.codec),
            });
        }
        if config.bitstream != BitstreamFormat::AnnexB {
            return Err(CreateError::Unsupported {
                what: "only Annex B input (the live case)".into(),
            });
        }
        if config.max_size.is_empty() {
            return Err(CreateError::InvalidConfig("max_size is empty".into()));
        }
        let native = config
            .target
            .adapter_luid
            .filter(|_| config.target.accepts(MemoryKind::D3d11));
        if native.is_some() && config.min_surfaces > MAX_SLOTS {
            return Err(CreateError::Unsupported {
                what: format!("{} surfaces (at most {MAX_SLOTS})", config.min_surfaces),
            });
        }
        let runtime = Runtime::start()?;
        let mft = mft::create().map_err(|e| CreateError::Device(format!("H.264 MFT: {e}")))?;
        let mut gpu = match native {
            Some(luid) => match Gpu::new(luid, config.min_surfaces.max(1)) {
                Ok(gpu) => Some(gpu),
                Err(e) => {
                    rbs_core::info!(Area::Encode, "H.264 decode in software: {e}");
                    None
                }
            },
            None => None,
        };
        let hardware = mft::configure(&mft, gpu.as_ref().map(Gpu::manager))
            .map_err(|e| CreateError::Device(format!("H.264 MFT setup: {e}")))?;
        if gpu.is_some() && !hardware {
            rbs_core::info!(
                Area::Encode,
                "H.264 decoder refused the D3D11 device; decoding to system memory"
            );
            gpu = None;
        }
        let stats = Counters::default();
        stats.need_keyframe.store(true, Ordering::Relaxed);
        let mut session = Self {
            ready: Vec::with_capacity(QUEUE),
            mft,
            gpu,
            out_sample: None,
            provides: false,
            layout: None,
            max_size: config.max_size,
            format: None,
            gated: true,
            next_index: 0,
            pending: VecDeque::with_capacity(QUEUE),
            stats,
            _runtime: runtime,
        };
        // An output type set during setup is used as it is; otherwise the first
        // stream change sets it.
        let _ = session.adopt_output();
        Ok(session)
    }

    /// True when pictures are decoded with DXVA into the native pool.
    pub fn is_native(&self) -> bool {
        self.gpu.is_some()
    }

    fn gate(&mut self) {
        self.gated = true;
        self.stats.need_keyframe.store(true, Ordering::Relaxed);
    }

    /// Reads the current output type and makes the output sample for it.
    fn adopt_output(&mut self) -> windows::core::Result<()> {
        let (layout, provides, bytes) = mft::output_layout(&self.mft)?;
        self.layout = Some(layout);
        self.provides = provides;
        self.out_sample = if provides {
            None
        } else {
            Some(mft::empty_sample(bytes)?)
        };
        Ok(())
    }

    /// Takes pictures out of the MFT until it needs input. A stream change is handled
    /// here (new output type) and is not an error.
    fn pull(&mut self) -> Result<(), DecodeError> {
        for _ in 0..MAX_OUTPUTS_PER_PULL {
            let ours = if self.provides {
                None
            } else {
                self.out_sample.clone()
            };
            if let Some(sample) = &ours {
                mft::rewind(sample);
            }
            let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(ours),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            // SAFETY: a live MFT and one output buffer that lives across the call.
            let result = unsafe { self.mft.ProcessOutput(0, &mut buffers, &mut status) };
            let [buffer] = buffers;
            let sample = ManuallyDrop::into_inner(buffer.pSample);
            drop(ManuallyDrop::into_inner(buffer.pEvents));
            match result {
                Ok(()) => {
                    if let Some(sample) = sample {
                        self.picture(&sample)?;
                    }
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                Err(e)
                    if e.code() == MF_E_TRANSFORM_STREAM_CHANGE
                        || e.code() == MF_E_TRANSFORM_TYPE_NOT_SET =>
                {
                    mft::set_nv12_output(&self.mft).map_err(|e| failed("output type", &e))?;
                    self.adopt_output().map_err(|e| failed("output type", &e))?;
                }
                Err(e) => return Err(failed("decode", &e)),
            }
        }
        Err(DecodeError::Failed {
            reason: "the MFT kept producing output".into(),
            recoverable: true,
        })
    }

    /// The pending entry of a picture whose sample time is `time`. Entries before it
    /// were dropped by the MFT (corrupt input) and are forgotten.
    fn take_pending(&mut self, time: Option<i64>) -> Option<Pending> {
        let Some(index) = time.and_then(|t| u64::try_from(t / TICK).ok()) else {
            return self.pending.pop_front();
        };
        while let Some(front) = self.pending.front() {
            if front.index >= index {
                break;
            }
            self.pending.pop_front();
        }
        self.pending.pop_front()
    }

    fn picture(&mut self, sample: &IMFSample) -> Result<(), DecodeError> {
        // SAFETY: a plain read on a live sample.
        let time = unsafe { sample.GetSampleTime() }.ok();
        let Some(entry) = self.take_pending(time) else {
            return Ok(()); // a picture of a packet dropped by reset
        };
        let color = ColorSpace::BT709_LIMITED;
        let frame = match self.gpu.as_mut() {
            Some(gpu) => match gpu.picture(sample, entry.size, entry.pts, color)? {
                GpuPicture::Native(frame) => frame,
                GpuPicture::NoSlot => {
                    self.stats
                        .dropped_no_surface
                        .fetch_add(1, Ordering::Relaxed);
                    return Ok(());
                }
                GpuPicture::NotGpu => self.system_copy(sample, entry, color)?,
            },
            None => self.system_copy(sample, entry, color)?,
        };
        self.ready.push(frame);
        self.stats.pictures.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Copies the cropped NV12 picture of `sample` to system memory (P4, counted).
    fn system_copy(
        &self,
        sample: &IMFSample,
        entry: Pending,
        color: ColorSpace,
    ) -> Result<VideoFrame, DecodeError> {
        let layout = self.layout.ok_or_else(|| invalid("no output type"))?;
        let Size { width, height } = entry.size;
        let (w, h, stride) = (width as usize, height as usize, layout.stride as usize);
        let coded_h = layout.height as usize;
        if width > layout.width
            || height > layout.height
            || !width.is_multiple_of(2)
            || !height.is_multiple_of(2)
        {
            return Err(DecodeError::Failed {
                reason: "picture larger than the output type".into(),
                recoverable: true,
            });
        }
        let mut bytes = vec![0u8; w * h * 3 / 2];
        // SAFETY: plain calls on a live sample; `Lock` gives `len` readable bytes until
        // `Unlock`. Every row read lies inside them (checked against `len` first).
        unsafe {
            let buffer = sample
                .ConvertToContiguousBuffer()
                .map_err(|e| failed("output buffer", &e))?;
            let mut ptr = std::ptr::null_mut();
            let mut len = 0u32;
            buffer
                .Lock(&mut ptr, None, Some(&mut len))
                .map_err(|e| failed("output buffer", &e))?;
            let needed = stride * coded_h + stride * (h / 2);
            if ptr.is_null() || (len as usize) < needed || stride < w {
                let _ = buffer.Unlock();
                return Err(DecodeError::Failed {
                    reason: "output buffer smaller than its type".into(),
                    recoverable: true,
                });
            }
            let src = std::slice::from_raw_parts(ptr, len as usize);
            for row in 0..h {
                bytes[row * w..(row + 1) * w].copy_from_slice(&src[row * stride..row * stride + w]);
            }
            let uv = stride * coded_h;
            for row in 0..h / 2 {
                let at = w * h + row * w;
                let from = uv + row * stride;
                bytes[at..at + w].copy_from_slice(&src[from..from + w]);
            }
            let _ = buffer.Unlock();
        }
        let format = VideoFormat::new(entry.size, PixelFormat::Nv12, color);
        let planes = [
            PlaneLayout {
                offset: 0,
                stride: w,
            },
            PlaneLayout {
                offset: w * h,
                stride: w,
            },
        ];
        let frame = SystemFrame::new(format, Bytes::from(bytes), &planes)
            .map_err(|e| invalid(&format!("picture layout: {e}")))?;
        self.stats.cpu_copies.fetch_add(1, Ordering::Relaxed);
        Ok(VideoFrame::system(entry.pts, frame, None))
    }

    /// Checks the packet's NAL units. Returns (has IDR, the cropped size of its SPS).
    fn inspect(&self, data: &[u8]) -> Result<(bool, Option<Size>, bool), DecodeError> {
        let (mut any, mut idr, mut pps) = (false, false, false);
        let mut size = None;
        for nal in h264::nal_units(data) {
            any = true;
            let header = nal.first().copied().unwrap_or(0x80);
            if header & 0x80 != 0 {
                return Err(invalid("forbidden_zero_bit set"));
            }
            match header & 0x1f {
                NAL_SPS => {
                    let sps = h264::parse_sps(nal).map_err(|e| invalid(&format!("SPS: {e:?}")))?;
                    if !sps.is_allowed_profile() {
                        return Err(invalid("SPS profile, chroma format or bit depth"));
                    }
                    if sps.size.width > self.max_size.width
                        || sps.size.height > self.max_size.height
                    {
                        return Err(invalid("picture above max_size"));
                    }
                    size = Some(sps.size);
                }
                NAL_PPS => pps = true,
                NAL_IDR => idr = true,
                0 => return Err(invalid("NAL type 0")),
                _ => {}
            }
        }
        if !any {
            return Err(invalid("no Annex B start code"));
        }
        Ok((idr, size, pps))
    }

    fn feed(&mut self, packet: &EncodedPacket, keyframe: bool) -> Result<(), DecodeError> {
        let index = self.next_index;
        let time = i64::try_from(index).unwrap_or(i64::MAX / TICK) * TICK;
        let sample =
            mft::input_sample(packet.data(), time, keyframe).map_err(|e| failed("input", &e))?;
        for attempt in 0..2 {
            // SAFETY: a live MFT and sample.
            match unsafe { self.mft.ProcessInput(0, &sample, 0) } {
                Ok(()) => {
                    self.next_index += 1;
                    return Ok(());
                }
                Err(e) if e.code() == MF_E_NOTACCEPTING && attempt == 0 => self.pull()?,
                Err(e) if e.code() == MF_E_NOTACCEPTING => return Err(DecodeError::Busy),
                Err(e) => return Err(failed("decode", &e)),
            }
        }
        Err(DecodeError::Busy)
    }

    fn drop_queued(&mut self) -> Result<(), DecodeError> {
        mft::flush(&self.mft).map_err(|e| failed("flush", &e))?;
        self.pending.clear();
        self.ready.clear();
        Ok(())
    }
}

impl VideoDecoder for MfH264Decoder {
    fn output_format(&self) -> Option<VideoFormat> {
        self.format
    }

    fn submit(&mut self, packet: &EncodedPacket) -> Result<(), DecodeError> {
        if packet.codec() != CodecId::H264 || packet.bitstream() != BitstreamFormat::AnnexB {
            return Err(invalid("not an H.264 Annex B packet"));
        }
        let (idr, sps_size, pps) = self.inspect(packet.data())?;
        let size = sps_size.or(self.format.map(|f| f.size));
        if self.gated && !(idr && size.is_some() && (pps || self.format.is_some())) {
            return Err(DecodeError::NeedKeyframe);
        }
        let Some(size) = size else {
            return Err(DecodeError::NeedKeyframe);
        };
        if self.pending.len() >= QUEUE {
            return Err(DecodeError::Busy);
        }
        if let Err(e) = self.feed(packet, idr) {
            if e != DecodeError::Busy {
                self.gate();
            }
            return Err(e);
        }
        self.pending.push_back(Pending {
            index: self.next_index - 1,
            pts: packet.pts(),
            size,
        });
        if sps_size.is_some() {
            self.format = Some(VideoFormat::new(
                size,
                PixelFormat::Nv12,
                ColorSpace::BT709_LIMITED,
            ));
        }
        if idr {
            self.gated = false;
            self.stats.need_keyframe.store(false, Ordering::Relaxed);
        }
        if let Err(e) = self.pull() {
            self.gate();
            let _ = self.drop_queued();
            return Err(e);
        }
        Ok(())
    }

    fn drain(&mut self, out: &mut Vec<VideoFrame>) -> Result<(), DecodeError> {
        let pulled = self.pull();
        out.append(&mut self.ready);
        if let Err(e) = pulled {
            self.gate();
            let _ = self.drop_queued();
            return Err(e);
        }
        Ok(())
    }

    fn reset(&mut self) -> Result<(), DecodeError> {
        self.gate();
        self.drop_queued()
    }

    fn flush(&mut self, out: &mut Vec<VideoFrame>) -> Result<(), DecodeError> {
        // SAFETY: plain messages to the live MFT.
        let drained = unsafe {
            self.mft
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)
                .and_then(|()| self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0))
        };
        let pulled = drained
            .map_err(|e| failed("drain", &e))
            .and_then(|()| self.pull());
        out.append(&mut self.ready);
        self.gate();
        // The MFT starts a new stream after the drain; pictures still missing are gone.
        self.pending.clear();
        // SAFETY: a plain message to the live MFT.
        let _ = unsafe {
            self.mft.ProcessMessage(
                windows::Win32::Media::MediaFoundation::MFT_MESSAGE_NOTIFY_START_OF_STREAM,
                0,
            )
        };
        pulled
    }

    fn max_surfaces(&self) -> usize {
        self.gpu.as_ref().map_or(0, Gpu::slots)
    }

    fn stats(&self) -> DecoderStats {
        let mut s = DecoderStats::default();
        s.pictures = self.stats.pictures.load(Ordering::Relaxed);
        s.dropped_no_surface = self.stats.dropped_no_surface.load(Ordering::Relaxed);
        s.cpu_copies = self.stats.cpu_copies.load(Ordering::Relaxed);
        s.need_keyframe = self.stats.need_keyframe.load(Ordering::Relaxed);
        s
    }
}

impl Drop for MfH264Decoder {
    fn drop(&mut self) {
        self.ready.clear();
        // SAFETY: plain messages to the live MFT before it is released.
        let _ = unsafe { self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0) };
    }
}
