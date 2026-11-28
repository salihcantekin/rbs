//! One live H.264 decoder session over VideoToolbox.
//!
//! Low-level module (principle P11): every `unsafe` block states its invariant.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use objc2_core_foundation::CFRetained;
use objc2_core_video::CVPixelBuffer;
use rbs_codec::h264::Sps;
use rbs_codec::{CreateError, DecoderStats, VideoDecodeError, VideoDecoder, VideoDecoderConfig};
use rbs_core::log::Area;
use rbs_core::media::VideoFrame;
use rbs_core::media::{BitstreamFormat, CodecId, EncodedPacket, MemoryKind, Size, VideoFormat};

use super::input::{self, MAX_PPS, Placement, Seen, invalid};
use super::picture::{self, Native};
use super::slots::Slots;
use super::vt::{FOURCC_420F, FOURCC_420V, Failure, Output, Session};

/// Decoded pictures not drained yet. A full queue is `Busy`.
const QUEUE: usize = 16;

/// Largest slot count (the port's limit, backend-ports.md invariant 9).
pub(crate) const MAX_SLOTS: usize = 8;

/// A submitted packet with a slice whose picture has not come out yet.
#[derive(Debug, Clone, Copy)]
struct Pending {
    index: u64,
    at: Placement,
}

/// Parameter sets: the latest ones accepted, and the ones the session decodes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Params {
    sps: Vec<u8>,
    info: Option<Sps>,
    pps: Vec<Vec<u8>>,
}

impl Params {
    fn complete(&self) -> bool {
        self.info.is_some() && !self.pps.is_empty()
    }

    fn pps_refs(&self) -> [Option<&[u8]>; MAX_PPS] {
        let mut refs = [None; MAX_PPS];
        for (slot, pps) in refs.iter_mut().zip(&self.pps) {
            *slot = Some(pps.as_slice());
        }
        refs
    }

    /// Takes the parameter sets of a packet; allocates only when they changed.
    fn stage(&mut self, seen: &Seen<'_>) {
        if let Some((nal, info)) = seen.sps
            && (self.sps != nal || self.info != Some(info))
        {
            self.sps = nal.to_vec();
            self.info = Some(info);
        }
        if seen.has_pps() {
            let new = || seen.pps.iter().flatten().copied();
            if self.pps.iter().map(Vec::as_slice).ne(new()) {
                self.pps = new().map(<[u8]>::to_vec).collect();
            }
        }
    }

    fn fourcc(&self) -> u32 {
        match self.info.and_then(|s| s.color) {
            Some(c) if c.full_range => FOURCC_420F,
            _ => FOURCC_420V,
        }
    }
}

#[derive(Default)]
struct Counters {
    pictures: AtomicU64,
    dropped_no_surface: AtomicU64,
    cpu_copies: AtomicU64,
    need_keyframe: AtomicBool,
}

/// One H.264 decoder session over VideoToolbox. Field order is drop order: the
/// pictures and the session go before the pool.
pub struct VtH264Decoder {
    ready: Vec<VideoFrame>,
    outputs: VecDeque<Output>,
    session: Option<Session>,
    /// The native pool; `None` for system-memory output.
    slots: Option<Slots>,
    /// A picture could not be handed out natively: every picture is copied to system
    /// memory from then on (and `max_surfaces` is 0).
    native_failed: bool,
    /// What the session decodes.
    active: Params,
    /// The latest parameter sets accepted (used at the next IDR).
    staged: Params,
    max_size: Size,
    gated: bool,
    next_index: u64,
    pending: VecDeque<Pending>,
    warned_software: bool,
    stats: Counters,
}

impl VtH264Decoder {
    /// Opens a session. Nothing is created on the device before the first keyframe.
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
        let native = config.target.accepts(MemoryKind::Metal);
        if native && config.min_surfaces > MAX_SLOTS {
            return Err(CreateError::Unsupported {
                what: format!("{} surfaces (at most {MAX_SLOTS})", config.min_surfaces),
            });
        }
        let stats = Counters::default();
        stats.need_keyframe.store(true, Ordering::Relaxed);
        Ok(Self {
            ready: Vec::with_capacity(QUEUE),
            outputs: VecDeque::with_capacity(QUEUE),
            session: None,
            slots: native.then(|| Slots::new(config.min_surfaces.max(1))),
            native_failed: false,
            active: Params::default(),
            staged: Params::default(),
            max_size: config.max_size,
            gated: true,
            next_index: 0,
            pending: VecDeque::with_capacity(QUEUE),
            warned_software: false,
            stats,
        })
    }

    /// True when pictures are handed out as IOSurfaces.
    pub fn is_native(&self) -> bool {
        self.slots.is_some() && !self.native_failed
    }

    /// True when the current VideoToolbox session decodes in hardware.
    pub fn is_hardware(&self) -> bool {
        self.session.as_ref().is_some_and(Session::is_hardware)
    }

    fn gate(&mut self) {
        self.gated = true;
        self.stats.need_keyframe.store(true, Ordering::Relaxed);
    }

    /// Native-pool lease releases the pool refused (a protocol violation; always 0).
    pub fn lease_misuses(&self) -> u64 {
        self.slots.as_ref().map_or(0, Slots::misuses)
    }

    /// After a platform error: gates on the next keyframe, and drops the VideoToolbox
    /// session when the failure asks for a new one (the next IDR makes it). Pictures
    /// already decoded stay drainable.
    fn fail_stream(&mut self, failure: &Failure) {
        self.gate();
        self.pending.clear();
        if failure.drop_session {
            self.session = None;
        }
    }

    /// A refused packet that carried an SPS or a slice: the decoder has no valid
    /// reference and only freshly accepted parameter sets may lift the gate.
    fn refuse_stream(&mut self) {
        self.gate();
        self.staged = Params::default();
    }

    /// Makes the session decode the staged parameter sets, recreating it at this IDR
    /// when they changed (an in-band SPS change is followed, invariant 7).
    fn follow_parameter_sets(&mut self) -> Result<(), VideoDecodeError> {
        if self.session.is_some() && self.active == self.staged {
            return Ok(());
        }
        let changed = self.session.is_some();
        self.session = None;
        let created = Session::new(
            &self.staged.sps,
            &self.staged.pps_refs(),
            self.staged.fourcc(),
        );
        let session = match created {
            Ok(session) => session,
            Err(e @ VideoDecodeError::Failed { .. }) if changed => {
                rbs_core::debug!(Area::Encode, "H.264 live decoder cannot follow: {e}");
                return Err(VideoDecodeError::FormatChanged);
            }
            Err(e) => return Err(e),
        };
        if !session.is_hardware() && !self.warned_software {
            self.warned_software = true;
            rbs_core::warn!(Area::Encode, "H.264 live decode in software (VideoToolbox)");
        }
        self.session = Some(session);
        self.active = self.staged.clone();
        // A new session may give native pictures again (invariant 9).
        self.native_failed = false;
        Ok(())
    }

    /// Turns the callback's outputs into pictures. Returns the first decode failure a
    /// callback reported; every picture decoded is kept.
    fn collect(&mut self) -> Result<(), Failure> {
        if let Some(session) = &self.session {
            session.take(&mut self.outputs);
        }
        let mut first_error = None;
        while let Some(output) = self.outputs.pop_front() {
            // Entries before this one produced no picture (dropped by the decoder).
            while self.pending.front().is_some_and(|p| p.index < output.index) {
                self.pending.pop_front();
            }
            let Some(entry) = self
                .pending
                .front()
                .filter(|p| p.index == output.index)
                .copied()
            else {
                continue; // a picture of a packet dropped by reset or an error
            };
            self.pending.pop_front();
            let buffer = match output.buffer {
                Some(buffer) if output.status == 0 && !output.dropped => buffer,
                _ => {
                    if output.status != 0 && first_error.is_none() {
                        first_error = Some(Failure::status("decode", output.status));
                    }
                    continue;
                }
            };
            if let Err(e) = self.picture(buffer, entry.at) {
                first_error.get_or_insert(Failure::from(e));
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    fn picture(
        &mut self,
        buffer: CFRetained<CVPixelBuffer>,
        at: Placement,
    ) -> Result<(), VideoDecodeError> {
        let frame = match self.slots.as_mut().filter(|_| !self.native_failed) {
            Some(slots) => match picture::native(slots, buffer, at) {
                Native::Frame(frame) => frame,
                Native::NoSlot => {
                    self.stats
                        .dropped_no_surface
                        .fetch_add(1, Ordering::Relaxed);
                    return Ok(());
                }
                Native::Copy(buffer) => {
                    self.native_failed = true;
                    rbs_core::warn!(
                        Area::Encode,
                        "H.264 live decoder gave a picture that is not a display-size IOSurface; copying to system memory"
                    );
                    self.copy(&buffer, at)?
                }
            },
            None => self.copy(&buffer, at)?,
        };
        self.ready.push(frame);
        self.stats.pictures.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn copy(&self, buffer: &CVPixelBuffer, at: Placement) -> Result<VideoFrame, VideoDecodeError> {
        let frame = picture::system_copy(buffer, at)?;
        self.stats.cpu_copies.fetch_add(1, Ordering::Relaxed);
        Ok(frame)
    }

    /// Decodes one packet with a slice; gating was checked.
    fn decode(&mut self, packet: &EncodedPacket, seen: &Seen<'_>) -> Result<(), Failure> {
        if seen.idr {
            self.follow_parameter_sets()?;
        }
        let (Some(session), Some(sps)) = (&self.session, self.active.info) else {
            return Err(Failure::from(VideoDecodeError::NeedKeyframe));
        };
        let index = self.next_index;
        self.next_index += 1;
        self.pending.push_back(Pending {
            index,
            at: Placement::of(&sps, packet.pts()),
        });
        let decoded = session.decode(packet.data(), seen.sample_len, index);
        let collected = self.collect();
        // Synchronous decode: nothing of this packet is still in VideoToolbox, so an
        // entry left over produced no picture.
        self.pending.retain(|p| p.index > index);
        decoded.and(collected)
    }
}

impl VideoDecoder for VtH264Decoder {
    fn output_format(&self) -> Option<VideoFormat> {
        // The latest accepted SPS; after a refused one, what the session still decodes.
        let sps = self.staged.info.or(self.active.info)?;
        Some(Placement::of(&sps, rbs_core::time::Timestamp::ZERO).format())
    }

    fn submit(&mut self, packet: &EncodedPacket) -> Result<(), VideoDecodeError> {
        if packet.codec() != CodecId::H264 || packet.bitstream() != BitstreamFormat::AnnexB {
            return Err(invalid("not an H.264 Annex B packet"));
        }
        let data = packet.data();
        let seen = match input::inspect(data, self.max_size) {
            Ok(seen) => seen,
            Err(e) => {
                // A refused SPS or slice leaves the decoder without a valid reference.
                if input::carries_slice_or_sps(data)
                    || matches!(e, VideoDecodeError::Unsupported(_))
                {
                    self.refuse_stream();
                }
                return Err(e);
            }
        };
        // Parameter sets are kept also while gated, so a later IDR without them decodes.
        self.staged.stage(&seen);
        if !seen.vcl {
            return Ok(()); // no picture: parameter sets, SEI, AUD and the like
        }
        if self.ready.len() >= QUEUE {
            return Err(VideoDecodeError::Busy);
        }
        if self.gated && !(seen.idr && self.staged.complete()) {
            return Err(VideoDecodeError::NeedKeyframe);
        }
        match self.decode(packet, &seen) {
            Ok(()) => {
                if seen.idr {
                    self.gated = false;
                    self.stats.need_keyframe.store(false, Ordering::Relaxed);
                }
                Ok(())
            }
            Err(failure) => {
                self.fail_stream(&failure);
                Err(failure.error)
            }
        }
    }

    fn drain(&mut self, out: &mut Vec<VideoFrame>) -> Result<(), VideoDecodeError> {
        let collected = self.collect();
        out.append(&mut self.ready);
        collected.map_err(|failure| {
            self.fail_stream(&failure);
            failure.error
        })
    }

    fn reset(&mut self) -> Result<(), VideoDecodeError> {
        self.gate();
        if let Some(session) = &self.session {
            session.finish();
            session.take(&mut self.outputs);
        }
        self.outputs.clear();
        self.pending.clear();
        self.ready.clear();
        Ok(())
    }

    fn flush(&mut self, out: &mut Vec<VideoFrame>) -> Result<(), VideoDecodeError> {
        if let Some(session) = &self.session {
            session.finish();
        }
        let collected = self.collect();
        out.append(&mut self.ready);
        self.gate();
        self.pending.clear();
        collected.map_err(|failure| {
            self.fail_stream(&failure);
            failure.error
        })
    }

    fn max_surfaces(&self) -> usize {
        if self.native_failed {
            return 0;
        }
        self.slots.as_ref().map_or(0, Slots::count)
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

impl Drop for VtH264Decoder {
    fn drop(&mut self) {
        // Pictures first, then the session (invalidated in its own `Drop`).
        self.ready.clear();
        self.outputs.clear();
        self.session = None;
    }
}
