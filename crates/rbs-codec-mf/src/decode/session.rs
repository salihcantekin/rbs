//! One live H.264 decoder session over the Media Foundation MFT.
//!
//! Low-level module (principle P11): every `unsafe` block states its invariant.

use std::collections::VecDeque;
use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use rbs_codec::h264::{self, NAL_IDR, NAL_PPS, NAL_SPS, Sps, SpsError};
use rbs_codec::{CreateError, DecoderStats, VideoDecodeError, VideoDecoder, VideoDecoderConfig};
use rbs_core::log::Area;
use rbs_core::media::{
    BitstreamFormat, Bytes, CodecId, EncodedPacket, MemoryKind, PixelFormat, PlaneLayout, Size,
    SystemFrame, VideoFormat, VideoFrame,
};
use windows::Win32::Media::MediaFoundation::{
    IMFSample, IMFTransform, MF_E_NOTACCEPTING, MF_E_TRANSFORM_NEED_MORE_INPUT,
    MF_E_TRANSFORM_STREAM_CHANGE, MF_E_TRANSFORM_TYPE_NOT_SET, MFT_MESSAGE_COMMAND_DRAIN,
    MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_END_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER,
};

use super::mft::{self, Layout, failed};
use super::pool::{Gpu, GpuPicture};
use super::{MAX_DECODE_SIZE, Placement};
use crate::win::Runtime;

/// Packets in the MFT whose pictures have not come out yet. A full queue is `Busy`.
const QUEUE: usize = 16;

/// Pending packets kept once the MFT asks for more input: the port allows a lookahead
/// of one picture. Older entries belong to pictures the MFT dropped (corrupt input).
const KEPT_AFTER_PULL: usize = 1;

/// Media Foundation time of one submission step. Sample times carry the submission
/// index only; the stamps themselves stay in [`Pending`], bit-exact.
const TICK: i64 = 10_000;

/// Output calls in one pull before the session gives up (a misbehaving MFT never
/// keeps the worker in a loop).
const MAX_OUTPUTS_PER_PULL: usize = 64;

/// Largest slot count (the port's limit, backend-ports.md invariant 9).
pub(crate) const MAX_SLOTS: usize = 8;

/// NAL types refused outright: unspecified (0), SPS extension (13), prefix (14),
/// subset SPS (15) and coded slice extension (20), i.e. SVC and MVC streams.
const REFUSED_NAL_TYPES: [u8; 5] = [0, 13, 14, 15, 20];

/// NAL type of a non-IDR slice.
const NAL_SLICE: u8 = 1;

/// A submitted packet with a slice whose picture has not come out yet.
#[derive(Debug, Clone, Copy)]
struct Pending {
    index: u64,
    at: Placement,
}

/// What one packet carries, after its SPS passed the checks.
#[derive(Debug, Clone, Copy, Default)]
struct Seen {
    vcl: bool,
    idr: bool,
    pps: bool,
    sps: Option<Sps>,
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
    /// The MFT gave a picture that is not a texture of our device: every picture is
    /// copied to system memory from then on (and `max_surfaces` is 0).
    native_failed: bool,
    /// Our output sample when the MFT does not provide its own.
    out_sample: Option<IMFSample>,
    provides: bool,
    layout: Option<Layout>,
    max_size: Size,
    /// The last SPS submitted (the stream's current parameters).
    sps: Option<Sps>,
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

fn invalid(what: &str) -> VideoDecodeError {
    VideoDecodeError::InvalidInput(what.into())
}

fn unsupported(what: &str) -> VideoDecodeError {
    VideoDecodeError::Unsupported(what.into())
}

/// Not decoded, but the session continues (keyframe gating follows).
fn skipped(what: &str) -> VideoDecodeError {
    VideoDecodeError::Failed {
        reason: what.into(),
        recoverable: true,
    }
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
                    rbs_core::warn!(Area::Encode, "H.264 live decode in software: {e}");
                    None
                }
            },
            None => None,
        };
        let hardware = mft::configure(&mft, gpu.as_ref().map(Gpu::manager))
            .map_err(|e| CreateError::Device(format!("H.264 MFT setup: {e}")))?;
        if gpu.is_some() && !hardware {
            rbs_core::warn!(
                Area::Encode,
                "H.264 live decoder refused the D3D11 device; decoding to system memory"
            );
            gpu = None;
        }
        let stats = Counters::default();
        stats.need_keyframe.store(true, Ordering::Relaxed);
        let mut session = Self {
            ready: Vec::with_capacity(QUEUE),
            mft,
            gpu,
            native_failed: false,
            out_sample: None,
            provides: false,
            layout: None,
            max_size: config.max_size,
            sps: None,
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
        self.gpu.is_some() && !self.native_failed
    }

    fn gate(&mut self) {
        self.gated = true;
        self.stats.need_keyframe.store(true, Ordering::Relaxed);
    }

    /// After a platform error: drops what the MFT holds and gates on the next
    /// keyframe. Pictures already decoded stay drainable.
    fn fail_stream(&mut self) {
        self.gate();
        let _ = mft::flush(&self.mft);
        self.pending.clear();
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
    fn pull(&mut self) -> Result<(), VideoDecodeError> {
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
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                    while self.pending.len() > KEPT_AFTER_PULL {
                        self.pending.pop_front();
                    }
                    return Ok(());
                }
                Err(e)
                    if e.code() == MF_E_TRANSFORM_STREAM_CHANGE
                        || e.code() == MF_E_TRANSFORM_TYPE_NOT_SET =>
                {
                    mft::set_nv12_output(&self.mft).map_err(|e| mft::decode_failed(&e))?;
                    self.adopt_output().map_err(|e| failed("output type", &e))?;
                }
                Err(e) => return Err(mft::decode_failed(&e)),
            }
        }
        Err(skipped("the MFT kept producing output"))
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

    fn picture(&mut self, sample: &IMFSample) -> Result<(), VideoDecodeError> {
        // SAFETY: a plain read on a live sample.
        let time = unsafe { sample.GetSampleTime() }.ok();
        let Some(entry) = self.take_pending(time) else {
            return Ok(()); // a picture of a packet dropped by reset or an error
        };
        let at = entry.at;
        let gpu = self.gpu.as_mut().filter(|_| !self.native_failed);
        let frame = match gpu {
            Some(gpu) => match gpu.picture(sample, at)? {
                GpuPicture::Native(frame) => frame,
                GpuPicture::NoSlot => {
                    self.stats
                        .dropped_no_surface
                        .fetch_add(1, Ordering::Relaxed);
                    return Ok(());
                }
                GpuPicture::NotGpu => {
                    self.native_failed = true;
                    rbs_core::warn!(
                        Area::Encode,
                        "H.264 live decoder gave a picture outside the D3D11 pool; copying to system memory"
                    );
                    self.system_copy(sample, at)?
                }
            },
            None => self.system_copy(sample, at)?,
        };
        self.ready.push(frame);
        self.stats.pictures.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Copies the cropped NV12 picture of `sample` to system memory (P4, counted). A
    /// new buffer per picture: this is the fallback path.
    fn system_copy(
        &self,
        sample: &IMFSample,
        at: Placement,
    ) -> Result<VideoFrame, VideoDecodeError> {
        let layout = self.layout.ok_or_else(|| skipped("no output type"))?;
        if !at.fits(layout.width, layout.height) || layout.stride < layout.width {
            return Err(skipped("picture outside the output type"));
        }
        let (w, h) = (at.size.width as usize, at.size.height as usize);
        let (left, top) = (at.left as usize, at.top as usize);
        let stride = layout.stride as usize;
        let uv_base = stride * layout.height as usize;
        let mut bytes = vec![0u8; w * h * 3 / 2];
        // SAFETY: plain calls on a live sample; `Lock` gives `len` readable bytes until
        // `Unlock`, and the slice made of them is dropped before `Unlock`. Rows are
        // read with checked `get`.
        let copied = unsafe {
            let buffer = sample
                .ConvertToContiguousBuffer()
                .map_err(|e| failed("output buffer", &e))?;
            let mut ptr = std::ptr::null_mut();
            let mut len = 0u32;
            buffer
                .Lock(&mut ptr, None, Some(&mut len))
                .map_err(|e| failed("output buffer", &e))?;
            let copied = if ptr.is_null() {
                false
            } else {
                let src = std::slice::from_raw_parts(ptr, len as usize);
                let rows = (0..h).map(|r| ((top + r) * stride + left, r * w)).chain(
                    (0..h / 2).map(|r| (uv_base + (top / 2 + r) * stride + left, w * h + r * w)),
                );
                let mut ok = true;
                for (from, to) in rows {
                    match (src.get(from..from + w), bytes.get_mut(to..to + w)) {
                        (Some(s), Some(d)) => d.copy_from_slice(s),
                        _ => {
                            ok = false;
                            break;
                        }
                    }
                }
                ok
            };
            let _ = buffer.Unlock();
            copied
        };
        if !copied {
            return Err(skipped("output buffer smaller than its type"));
        }
        let format = VideoFormat::new(at.size, PixelFormat::Nv12, at.color);
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
            .map_err(|e| skipped(&format!("picture layout: {e}")))?;
        self.stats.cpu_copies.fetch_add(1, Ordering::Relaxed);
        Ok(VideoFrame::system(at.pts, frame, None))
    }

    /// Checks the packet's NAL units and its SPS (backend-ports.md invariant 2).
    fn inspect(&self, data: &[u8]) -> Result<Seen, VideoDecodeError> {
        let mut seen = Seen::default();
        let mut any = false;
        for nal in h264::nal_units(data) {
            any = true;
            let header = nal.first().copied().unwrap_or(0x80);
            if header & 0x80 != 0 {
                return Err(invalid("forbidden_zero_bit set"));
            }
            match header & 0x1f {
                t if REFUSED_NAL_TYPES.contains(&t) => {
                    return Err(invalid(&format!("NAL type {t}")));
                }
                NAL_SPS => seen.sps = Some(self.check_sps(nal)?),
                NAL_PPS => seen.pps = true,
                NAL_IDR => {
                    seen.idr = true;
                    seen.vcl = true;
                }
                NAL_SLICE => seen.vcl = true,
                _ => {}
            }
        }
        if !any {
            return Err(invalid("no Annex B start code"));
        }
        Ok(seen)
    }

    fn check_sps(&self, nal: &[u8]) -> Result<Sps, VideoDecodeError> {
        let sps = match h264::parse_sps(nal) {
            Ok(sps) => sps,
            Err(SpsError::Unsupported(what)) => return Err(unsupported(what)),
            Err(e) => return Err(invalid(&format!("SPS: {e:?}"))),
        };
        if sps.size.width > self.max_size.width || sps.size.height > self.max_size.height {
            return Err(invalid("picture above max_size"));
        }
        if !sps.is_allowed_profile() {
            return Err(unsupported("profile, level, chroma format or bit depth"));
        }
        if sps.size.width > MAX_DECODE_SIZE.width || sps.size.height > MAX_DECODE_SIZE.height {
            return Err(unsupported("picture above 4096 x 2304"));
        }
        Ok(sps)
    }

    fn feed(&mut self, packet: &EncodedPacket, keyframe: bool) -> Result<u64, VideoDecodeError> {
        let index = self.next_index;
        let time = i64::try_from(index)
            .unwrap_or(i64::MAX)
            .saturating_mul(TICK);
        let sample =
            mft::input_sample(packet.data(), time, keyframe).map_err(|e| failed("input", &e))?;
        for attempt in 0..2 {
            // SAFETY: a live MFT and sample.
            match unsafe { self.mft.ProcessInput(0, &sample, 0) } {
                Ok(()) => {
                    self.next_index += 1;
                    return Ok(index);
                }
                Err(e) if e.code() == MF_E_NOTACCEPTING && attempt == 0 => self.pull()?,
                Err(e) if e.code() == MF_E_NOTACCEPTING => return Err(VideoDecodeError::Busy),
                Err(e) => return Err(mft::decode_failed(&e)),
            }
        }
        Err(VideoDecodeError::Busy)
    }

    /// Feeds a packet; an error other than `Busy` gates on the next keyframe.
    fn feed_or_gate(
        &mut self,
        packet: &EncodedPacket,
        keyframe: bool,
    ) -> Result<u64, VideoDecodeError> {
        let index = match self.feed(packet, keyframe) {
            Ok(index) => index,
            Err(VideoDecodeError::Busy) => return Err(VideoDecodeError::Busy),
            Err(e) => {
                self.fail_stream();
                return Err(e);
            }
        };
        Ok(index)
    }
}

impl VideoDecoder for MfH264Decoder {
    fn output_format(&self) -> Option<VideoFormat> {
        self.sps
            .map(|s| VideoFormat::new(s.size, PixelFormat::Nv12, super::color_of(s.color)))
    }

    fn submit(&mut self, packet: &EncodedPacket) -> Result<(), VideoDecodeError> {
        if packet.codec() != CodecId::H264 || packet.bitstream() != BitstreamFormat::AnnexB {
            return Err(invalid("not an H.264 Annex B packet"));
        }
        let data = packet.data();
        let seen = match self.inspect(data) {
            Ok(seen) => seen,
            Err(e) => {
                // A refused SPS or slice leaves the decoder without a valid reference.
                let carries = h264::nal_units(data)
                    .any(|n| matches!(h264::nal_type(n), Some(NAL_SLICE | NAL_IDR | NAL_SPS)));
                if carries || matches!(e, VideoDecodeError::Unsupported(_)) {
                    self.gate();
                }
                return Err(e);
            }
        };
        if !seen.vcl {
            // No picture: parameter sets are fed (also while gated, so a later IDR
            // without them decodes); SEI, AUD and the like are dropped.
            if seen.sps.is_none() && !seen.pps {
                return Ok(());
            }
            self.feed_or_gate(packet, false)?;
            if seen.sps.is_some() {
                self.sps = seen.sps;
            }
            return self.pull().inspect_err(|_| self.fail_stream());
        }
        let sps = seen.sps.or(self.sps);
        let Some(sps) =
            sps.filter(|_| !self.gated || (seen.idr && (seen.pps || self.sps.is_some())))
        else {
            return Err(VideoDecodeError::NeedKeyframe);
        };
        if self.pending.len() >= QUEUE {
            return Err(VideoDecodeError::Busy);
        }
        let index = self.feed_or_gate(packet, seen.idr)?;
        self.pending.push_back(Pending {
            index,
            at: Placement::of(&sps, packet.pts()),
        });
        if seen.sps.is_some() {
            self.sps = seen.sps;
        }
        if seen.idr {
            self.gated = false;
            self.stats.need_keyframe.store(false, Ordering::Relaxed);
        }
        self.pull().inspect_err(|_| self.fail_stream())
    }

    fn drain(&mut self, out: &mut Vec<VideoFrame>) -> Result<(), VideoDecodeError> {
        let pulled = self.pull();
        out.append(&mut self.ready);
        if pulled.is_err() {
            self.fail_stream();
        }
        pulled
    }

    fn reset(&mut self) -> Result<(), VideoDecodeError> {
        self.gate();
        self.pending.clear();
        self.ready.clear();
        mft::flush(&self.mft).map_err(|e| failed("flush", &e))
    }

    fn flush(&mut self, out: &mut Vec<VideoFrame>) -> Result<(), VideoDecodeError> {
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
            self.mft
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
        };
        pulled
    }

    fn max_surfaces(&self) -> usize {
        if self.native_failed {
            return 0;
        }
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
        // SAFETY: a plain message to the live MFT before it is released.
        let _ = unsafe { self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0) };
    }
}
