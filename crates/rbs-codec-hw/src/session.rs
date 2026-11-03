//! The encoder session logic, over a small [`Engine`] trait that each hardware
//! adapter implements. Everything here is plain Rust, so the port contract
//! (ordering, keyframes, `Busy`, refusals, errors) is tested with a fake engine on
//! machines without the hardware.
//!
//! Each submitted frame takes the next of [`Engine::slots`] slots (for example one
//! input buffer and one bitstream buffer). Slots are used and drained strictly in
//! submission order, which is the output order while no frames are reordered (no
//! B-frames).

use rbs_codec::{EncodeError, EncoderChange, RateControl, VideoEncoder};
use rbs_core::media::{
    BitstreamFormat, Bytes, CodecId, EncodedPacket, FramePayload, StreamDescriptor, SystemFrame,
    VideoFormat, VideoFrame,
};
use rbs_core::time::Timestamp;

use crate::SessionStats;
use crate::plan::{self, Rc};

/// A failure inside the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineError {
    /// The encoder cannot take more work now. From `encode` the frame is not
    /// consumed; from `output` without waiting it means "not finished yet".
    Busy,
    /// The device is gone.
    DeviceLost,
    /// The device refused a parameter (for example a rate-control change).
    Unsupported,
    /// Any other failure: the API call and its status.
    Failed(&'static str, i64),
    /// The engine produced something the session cannot use (for example a reordered
    /// frame); the session cannot continue.
    Broken(&'static str),
}

impl EngineError {
    fn into_encode(self) -> EncodeError {
        match self {
            EngineError::Busy => EncodeError::Busy,
            EngineError::DeviceLost => EncodeError::DeviceLost,
            EngineError::Unsupported => EncodeError::Unsupported,
            EngineError::Failed(call, status) => EncodeError::Failed {
                reason: format!("{call} failed with status {status}"),
                recoverable: false,
            },
            EngineError::Broken(what) => EncodeError::Failed {
                reason: what.into(),
                recoverable: false,
            },
        }
    }
}

/// One finished picture.
#[derive(Debug)]
pub struct Output {
    /// Annex B access unit (exact size; the one payload allocation per packet).
    pub data: Vec<u8>,
    /// IDR picture.
    pub keyframe: bool,
    /// The token passed to [`Engine::encode`].
    pub token: u64,
}

/// What the session needs from an encoder device. Slots are `0..slots()`.
///
/// The session calls `upload` then `encode` for slot `n`, and `output` for slots in
/// the same order, never touching a slot again before its output was taken. An
/// engine whose driver holds its own queue (AMF `SubmitInput`/`QueryOutput`, QSV
/// sync points) may use the slot only for its input buffer and return the next
/// picture of its queue from `output`: the session checks the token, so a picture
/// out of order ends the session instead of being mislabelled. `encode` returning
/// [`EngineError::Busy`] while slots are free leaves the frame unconsumed; the
/// caller drains (which polls, it waits only when every slot is in flight) and
/// submits again.
pub trait Engine: Send {
    /// Frames in flight before `submit` returns `Busy`, at least 1. Read once when
    /// the session is created, so an engine may pick it at run time (for example
    /// the queue depth the driver accepted).
    fn slots(&self) -> usize;

    /// Copies `frame` into the input buffer of `slot`. Not called for a slot that is
    /// in flight.
    fn upload(&mut self, slot: usize, frame: &SystemFrame) -> Result<(), EngineError>;

    /// Queues `slot` for encoding; `token` comes back in its [`Output`]. `idr` forces
    /// an IDR picture with parameter sets.
    fn encode(&mut self, slot: usize, token: u64, idr: bool) -> Result<(), EngineError>;

    /// The output of `slot`, which is in flight. With `wait == false`, `Ok(None)` when
    /// the picture is not finished yet; with `wait == true`, blocks until it is.
    fn output(&mut self, slot: usize, wait: bool) -> Result<Option<Output>, EngineError>;

    /// Tells the encoder no more input follows for now, so every queued picture
    /// finishes.
    fn end_of_stream(&mut self) -> Result<(), EngineError>;

    /// Applies a new rate control of the same mode.
    fn set_rate_control(&mut self, rc: Rc) -> Result<(), EngineError>;
}

#[derive(Debug, Clone, Copy, Default)]
struct Slot {
    pts: Timestamp,
    token: u64,
}

/// A session over an engine. Implements the encoder port.
pub struct Session<E: Engine> {
    engine: E,
    stream: StreamDescriptor,
    codec: CodecId,
    format: VideoFormat,
    rate_control: RateControl,
    dynamic_bitrate: bool,
    slots: Box<[Slot]>,
    /// Oldest slot in flight.
    head: usize,
    /// Slots in flight (submitted, not yet drained).
    in_flight: usize,
    next_token: u64,
    last_pts: Option<Timestamp>,
    keyframe_pending: bool,
    /// Set once the session cannot continue; every later call returns it.
    broken: Option<EncodeError>,
    stats: SessionStats,
}

impl<E: Engine> Session<E> {
    /// A session over `engine`, which was opened for `format` and `rate_control`.
    pub fn new(
        engine: E,
        codec: CodecId,
        format: VideoFormat,
        rate_control: RateControl,
        dynamic_bitrate: bool,
        stream: StreamDescriptor,
    ) -> Self {
        let slots = vec![Slot::default(); engine.slots().max(1)].into_boxed_slice();
        Self {
            engine,
            stream,
            codec,
            format,
            rate_control,
            dynamic_bitrate,
            slots,
            head: 0,
            in_flight: 0,
            next_token: 0,
            last_pts: None,
            keyframe_pending: true,
            broken: None,
            stats: SessionStats::default(),
        }
    }

    /// Counters of this session.
    pub fn stats(&self) -> SessionStats {
        self.stats
    }

    /// The engine.
    pub fn engine(&self) -> &E {
        &self.engine
    }

    /// `index` (below twice the slot count) wrapped into the ring, without a division.
    fn wrap(&self, index: usize) -> usize {
        if index >= self.slots.len() {
            index - self.slots.len()
        } else {
            index
        }
    }

    fn check(&self) -> Result<(), EncodeError> {
        match &self.broken {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        }
    }

    /// Converts an engine error, remembering it when the session cannot continue.
    fn fail(&mut self, e: EngineError) -> EncodeError {
        let e = e.into_encode();
        if !e.is_recoverable() {
            self.broken = Some(e.clone());
        }
        e
    }

    /// Takes the oldest slot's output. `Ok(false)` when it is not ready.
    fn take_one(&mut self, out: &mut Vec<EncodedPacket>, wait: bool) -> Result<bool, EncodeError> {
        let slot = self.head;
        let output = match self.engine.output(slot, wait) {
            Ok(Some(o)) => o,
            Ok(None) if !wait => return Ok(false),
            Ok(None) => return Err(self.fail(EngineError::Broken("encoder returned no picture"))),
            Err(EngineError::Busy) if !wait => return Ok(false),
            // A lock that fails leaves the picture unread; retrying it would repeat
            // the failure forever, so the session ends (only device loss keeps its
            // own error).
            Err(EngineError::DeviceLost) => return Err(self.fail(EngineError::DeviceLost)),
            Err(EngineError::Failed(call, status)) => {
                return Err(self.fail(EngineError::Failed(call, status)));
            }
            Err(_) => return Err(self.fail(EngineError::Broken("reading a picture failed"))),
        };
        let expected = self.slots[slot];
        if output.token != expected.token {
            // A picture out of submission order: frames were reordered, which this
            // session does not support.
            return Err(self.fail(EngineError::Broken("encoder output out of order")));
        }
        self.head = self.wrap(self.head + 1);
        self.in_flight -= 1;
        let packet = EncodedPacket::new(
            self.codec,
            BitstreamFormat::AnnexB,
            expected.pts,
            expected.pts,
            output.keyframe,
            Bytes::from(output.data),
        )
        .map_err(|_| self.fail(EngineError::Broken("packet timestamps")))?;
        out.push(packet);
        self.stats.packets += 1;
        Ok(true)
    }
}

impl<E: Engine> VideoEncoder for Session<E> {
    fn stream(&self) -> &StreamDescriptor {
        &self.stream
    }

    fn submit(&mut self, frame: &VideoFrame) -> Result<(), EncodeError> {
        self.check()?;
        let FramePayload::System(system) = frame.payload() else {
            return Err(EncodeError::InvalidInput(
                "the session takes system-memory frames".into(),
            ));
        };
        if *frame.format() != self.format || *system.format() != self.format {
            return Err(EncodeError::InvalidInput(
                "frame format differs from the session format".into(),
            ));
        }
        let pts = frame.timestamp();
        if self.last_pts.is_some_and(|last| pts <= last) {
            return Err(EncodeError::InvalidInput(
                "frame timestamp is not after the previous frame".into(),
            ));
        }
        if self.in_flight == self.slots.len() {
            return Err(EncodeError::Busy);
        }
        let slot = self.wrap(self.head + self.in_flight);
        if let Err(e) = self.engine.upload(slot, system) {
            return Err(self.fail(e));
        }
        self.stats.copied += 1;
        let token = self.next_token;
        if let Err(e) = self.engine.encode(slot, token, self.keyframe_pending) {
            return Err(self.fail(e));
        }
        self.slots[slot] = Slot { pts, token };
        self.next_token += 1;
        self.in_flight += 1;
        self.last_pts = Some(pts);
        self.keyframe_pending = false;
        self.stats.submitted += 1;
        Ok(())
    }

    fn drain(&mut self, out: &mut Vec<EncodedPacket>) -> Result<(), EncodeError> {
        self.check()?;
        // With every slot in flight, wait for the oldest picture so that `submit` has
        // room again (the `Busy` contract). It was submitted `SLOTS` frames ago, so on
        // real hardware it is long finished and the wait is a formality.
        if self.in_flight == self.slots.len() {
            self.take_one(out, true)?;
        }
        while self.in_flight > 0 {
            if !self.take_one(out, false)? {
                break;
            }
        }
        Ok(())
    }

    fn reconfigure(&mut self, change: &EncoderChange) -> Result<(), EncodeError> {
        self.check()?;
        match change {
            EncoderChange::RequestKeyframe => {
                self.keyframe_pending = true;
                Ok(())
            }
            EncoderChange::Bitrate(rc) => {
                let same_mode = rc.name() == self.rate_control.name();
                if !self.dynamic_bitrate
                    || !same_mode
                    || matches!(rc, RateControl::ConstantQuality { .. })
                {
                    return Err(EncodeError::Unsupported);
                }
                let mapped = plan::rate_control(rc).map_err(EncodeError::InvalidInput)?;
                match self.engine.set_rate_control(mapped) {
                    Ok(()) => {
                        self.rate_control = *rc;
                        Ok(())
                    }
                    Err(EngineError::Unsupported) => Err(EncodeError::Unsupported),
                    Err(e) => Err(self.fail(e)),
                }
            }
            _ => Err(EncodeError::Unsupported),
        }
    }

    fn flush(&mut self, out: &mut Vec<EncodedPacket>) -> Result<(), EncodeError> {
        self.check()?;
        // The stream may go on after a flush; start it again with an IDR so it is
        // decodable from there.
        self.keyframe_pending = true;
        if self.in_flight == 0 {
            return Ok(());
        }
        if let Err(e) = self.engine.end_of_stream() {
            return Err(self.fail(e));
        }
        while self.in_flight > 0 {
            self.take_one(out, true)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod fake {
    //! A fake engine: pictures finish after a configurable number of `output` polls,
    //! keyframes follow the GOP and IDR requests like a hardware encoder with picture
    //! type decision.

    use super::*;

    /// Scripted behaviour of a fake engine.
    #[derive(Debug, Clone, Default)]
    pub(crate) struct Script {
        /// Polls with `wait == false` a picture needs before it is finished.
        pub latency: u32,
        /// `encode` calls (by token) that fail with this error once.
        pub encode_errors: Vec<(u64, EngineError)>,
        /// Fail every `output` call with this error.
        pub output_error: Option<EngineError>,
        /// Return outputs with swapped tokens (as if frames were reordered).
        pub reorder: bool,
        /// `set_rate_control` fails with this error.
        pub rate_error: Option<EngineError>,
    }

    #[derive(Debug, Clone, Copy, Default)]
    struct Pending {
        token: u64,
        keyframe: bool,
        polls: u32,
        busy: bool,
        uploaded: bool,
    }

    /// The fake engine.
    #[derive(Debug)]
    pub(crate) struct FakeEngine {
        script: Script,
        gop: u32,
        since_idr: u32,
        slots: [Pending; FAKE_SLOTS],
        /// Calls of `end_of_stream`.
        pub eos: u32,
        /// Last rate control applied.
        pub rc: Option<Rc>,
    }

    impl FakeEngine {
        pub fn new(gop: u32, script: Script) -> Self {
            Self {
                script,
                gop,
                since_idr: 0,
                slots: [Pending::default(); FAKE_SLOTS],
                eos: 0,
                rc: None,
            }
        }
    }

    /// Slots of the fake engine.
    pub(crate) const FAKE_SLOTS: usize = 8;

    impl Engine for FakeEngine {
        fn slots(&self) -> usize {
            FAKE_SLOTS
        }

        fn upload(&mut self, slot: usize, frame: &SystemFrame) -> Result<(), EngineError> {
            assert!(!self.slots[slot].busy, "upload into a slot in flight");
            assert!(frame.plane(0).is_some());
            self.slots[slot].uploaded = true;
            Ok(())
        }

        fn encode(&mut self, slot: usize, token: u64, idr: bool) -> Result<(), EngineError> {
            if let Some(i) = self
                .script
                .encode_errors
                .iter()
                .position(|(t, _)| *t == token)
            {
                return Err(self.script.encode_errors.remove(i).1);
            }
            let s = &mut self.slots[slot];
            assert!(s.uploaded && !s.busy);
            let keyframe = idr || self.since_idr + 1 >= self.gop;
            self.since_idr = if keyframe { 0 } else { self.since_idr + 1 };
            *s = Pending {
                token,
                keyframe,
                polls: 0,
                busy: true,
                uploaded: false,
            };
            Ok(())
        }

        fn output(&mut self, slot: usize, wait: bool) -> Result<Option<Output>, EngineError> {
            if let Some(e) = &self.script.output_error {
                return Err(e.clone());
            }
            let latency = self.script.latency;
            let s = &mut self.slots[slot];
            assert!(s.busy, "output of a slot not in flight");
            if !wait && s.polls < latency {
                s.polls += 1;
                return Ok(None);
            }
            s.busy = false;
            let token = if self.script.reorder {
                s.token + 1
            } else {
                s.token
            };
            Ok(Some(Output {
                data: vec![0, 0, 0, 1, if s.keyframe { 0x65 } else { 0x41 }, 0x88],
                keyframe: s.keyframe,
                token,
            }))
        }

        fn end_of_stream(&mut self) -> Result<(), EngineError> {
            self.eos += 1;
            Ok(())
        }

        fn set_rate_control(&mut self, rc: Rc) -> Result<(), EngineError> {
            if let Some(e) = self.script.rate_error.clone() {
                return Err(e);
            }
            self.rc = Some(rc);
            Ok(())
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
mod tests {
    use super::fake::FAKE_SLOTS as FRAMES_IN_FLIGHT;
    use super::fake::{FakeEngine, Script};
    use super::*;
    use crate::device::{DeviceCaps, capability_set};
    use crate::testing::discrete_gpu as geforce;
    use rbs_codec::conformance::{self, SystemFrameSource};
    use rbs_codec::{CreateError, VideoEncoderConfig, VideoEncoderFactory};
    use rbs_core::capability::CapabilitySet;
    use rbs_core::media::{ColorSpace, MemoryKind, PixelFormat, Size, StreamParams};
    use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};
    use rbs_core::timing::{Cadence, FrameRate};

    use rbs_codec::conformance::FrameSource;

    struct FakeFactory {
        device: DeviceCaps,
        script: Script,
    }

    impl AdapterFactory for FakeFactory {
        fn id(&self) -> AdapterId {
            AdapterId::new("org.rbs.codec.hw-fake")
        }
        fn display_name(&self) -> &str {
            "Hardware encoder session over a fake engine"
        }
        fn probe(&self) -> Result<CapabilitySet, ProbeError> {
            Ok(capability_set(&self.device))
        }
    }

    impl VideoEncoderFactory for FakeFactory {
        fn create(
            &self,
            config: &VideoEncoderConfig,
        ) -> Result<Box<dyn VideoEncoder>, CreateError> {
            Ok(Box::new(session(
                config,
                &self.device,
                self.script.clone(),
            )?))
        }
    }

    fn session(
        config: &VideoEncoderConfig,
        device: &DeviceCaps,
        script: Script,
    ) -> Result<Session<FakeEngine>, CreateError> {
        let plan = plan::plan(config, device)?;
        let stream = StreamDescriptor::new(
            config.codec,
            BitstreamFormat::AnnexB,
            Bytes::from(vec![0, 0, 0, 1, 0x67, 0, 0, 0, 1, 0x68]),
            StreamParams::video(config.format, config.frame_rate),
        );
        Ok(Session::new(
            FakeEngine::new(plan.gop, script),
            config.codec,
            config.format,
            config.rate_control,
            device
                .codec(config.codec)
                .is_some_and(|c| c.dynamic_bitrate),
            stream,
        ))
    }

    fn config() -> VideoEncoderConfig {
        VideoEncoderConfig::new(
            CodecId::H264,
            VideoFormat::new(
                Size::new(320, 240),
                PixelFormat::Nv12,
                ColorSpace::BT709_LIMITED,
            ),
            FrameRate::FPS_30,
            MemoryKind::System,
            RateControl::Cbr {
                bitrate_bps: 1_000_000,
            },
        )
    }

    #[test]
    fn conformance_with_immediate_output() {
        let factory = FakeFactory {
            device: geforce(),
            script: Script::default(),
        };
        let report = conformance::video_encoder(&factory, &mut SystemFrameSource::new());
        eprintln!("{report}");
        report.assert_passed();
    }

    #[test]
    fn conformance_with_late_output() {
        // Pictures finish only after several polls: drain returns partial results and
        // submit hits Busy once every slot is in flight.
        let factory = FakeFactory {
            device: geforce(),
            script: Script {
                latency: 20,
                ..Script::default()
            },
        };
        let report = conformance::video_encoder(&factory, &mut SystemFrameSource::new());
        eprintln!("{report}");
        report.assert_passed();
    }

    struct Run {
        session: Session<FakeEngine>,
        frames: SystemFrameSource,
        cadence: Cadence,
        index: u64,
        packets: Vec<EncodedPacket>,
    }

    impl Run {
        fn new(script: Script) -> Self {
            let config = config();
            Self {
                session: session(&config, &geforce(), script).unwrap(),
                frames: SystemFrameSource::new(),
                cadence: Cadence::frames(Timestamp::from_nanos(1_000_000_000), config.frame_rate),
                index: 0,
                packets: Vec::with_capacity(64),
            }
        }

        fn frame(&mut self) -> VideoFrame {
            let ts = self.cadence.time_of(self.index);
            let f = self.frames.frame(&config().format, self.index, ts).unwrap();
            self.index += 1;
            f
        }

        fn submit(&mut self) -> Result<(), EncodeError> {
            let f = self.frame();
            self.session.submit(&f)
        }
    }

    #[test]
    fn busy_when_every_slot_is_in_flight_then_recovers() {
        let mut run = Run::new(Script {
            latency: u32::MAX,
            ..Script::default()
        });
        for _ in 0..FRAMES_IN_FLIGHT {
            run.submit().unwrap();
        }
        let f = run.frame();
        assert_eq!(run.session.submit(&f), Err(EncodeError::Busy));
        // A full ring makes drain wait for the oldest picture, freeing one slot.
        run.session.drain(&mut run.packets).unwrap();
        assert_eq!(run.packets.len(), 1);
        run.session.submit(&f).unwrap();
        run.session.flush(&mut run.packets).unwrap();
        assert_eq!(run.packets.len(), FRAMES_IN_FLIGHT + 1);
        assert_eq!(run.session.engine().eos, 1);
        assert_eq!(run.session.stats().submitted, FRAMES_IN_FLIGHT as u64 + 1);
        // The session goes on after a flush.
        run.submit().unwrap();
    }

    #[test]
    fn flush_without_pending_frames_does_not_signal_end_of_stream() {
        let mut run = Run::new(Script::default());
        run.session.flush(&mut run.packets).unwrap();
        assert_eq!(run.session.engine().eos, 0);
    }

    #[test]
    fn refused_frame_keeps_the_keyframe_pending() {
        let mut run = Run::new(Script {
            encode_errors: vec![(0, EngineError::Busy)],
            ..Script::default()
        });
        assert_eq!(run.submit(), Err(EncodeError::Busy));
        run.submit().unwrap();
        run.session.flush(&mut run.packets).unwrap();
        assert!(run.packets[0].keyframe());
        assert_eq!(run.session.stats().submitted, 1);
    }

    #[test]
    fn device_loss_ends_the_session() {
        let mut run = Run::new(Script {
            encode_errors: vec![(1, EngineError::DeviceLost)],
            ..Script::default()
        });
        run.submit().unwrap();
        assert_eq!(run.submit(), Err(EncodeError::DeviceLost));
        assert_eq!(run.submit(), Err(EncodeError::DeviceLost));
        assert_eq!(
            run.session.drain(&mut run.packets),
            Err(EncodeError::DeviceLost)
        );
    }

    #[test]
    fn output_failure_is_unrecoverable() {
        let mut run = Run::new(Script {
            output_error: Some(EngineError::Failed("LockBitstream", 20)),
            ..Script::default()
        });
        run.submit().unwrap();
        let e = run.session.drain(&mut run.packets).unwrap_err();
        assert!(!e.is_recoverable());
        assert!(run.submit().is_err());
    }

    #[test]
    fn a_refused_lock_ends_the_session_instead_of_retrying() {
        for error in [EngineError::Unsupported, EngineError::Busy] {
            let mut run = Run::new(Script {
                output_error: Some(error),
                ..Script::default()
            });
            run.submit().unwrap();
            // Without waiting, Busy means "not finished yet".
            let drained = run.session.drain(&mut run.packets);
            if drained.is_ok() {
                assert!(run.packets.is_empty());
            }
            let e = run.session.flush(&mut run.packets).unwrap_err();
            assert!(!e.is_recoverable(), "{e:?}");
            assert!(run.submit().is_err());
        }
    }

    #[test]
    fn the_first_frame_after_a_flush_is_a_keyframe() {
        let mut run = Run::new(Script::default());
        for _ in 0..3 {
            run.submit().unwrap();
        }
        run.session.flush(&mut run.packets).unwrap();
        run.submit().unwrap();
        run.submit().unwrap();
        run.session.flush(&mut run.packets).unwrap();
        let keyframes: Vec<bool> = run.packets.iter().map(EncodedPacket::keyframe).collect();
        assert_eq!(keyframes, [true, false, false, true, false]);
    }

    #[test]
    fn reordered_output_is_refused() {
        let mut run = Run::new(Script {
            reorder: true,
            ..Script::default()
        });
        run.submit().unwrap();
        assert!(
            !run.session
                .drain(&mut run.packets)
                .unwrap_err()
                .is_recoverable()
        );
        assert!(run.packets.is_empty());
    }

    #[test]
    fn timestamps_must_increase() {
        let mut run = Run::new(Script::default());
        let f = run.frame();
        run.session.submit(&f).unwrap();
        assert!(matches!(
            run.session.submit(&f),
            Err(EncodeError::InvalidInput(_))
        ));
    }

    #[test]
    fn bitrate_changes() {
        let mut run = Run::new(Script::default());
        let half = RateControl::Cbr {
            bitrate_bps: 500_000,
        };
        run.session
            .reconfigure(&EncoderChange::Bitrate(half))
            .unwrap();
        assert_eq!(run.session.engine().rc, Some(Rc::Cbr { bitrate: 500_000 }));
        let vbr = RateControl::Vbr {
            target_bps: 1,
            max_bps: 2,
        };
        assert_eq!(
            run.session.reconfigure(&EncoderChange::Bitrate(vbr)),
            Err(EncodeError::Unsupported)
        );
        assert!(matches!(
            run.session
                .reconfigure(&EncoderChange::Bitrate(RateControl::Cbr { bitrate_bps: 0 })),
            Err(EncodeError::InvalidInput(_))
        ));

        let mut refused = Run::new(Script {
            rate_error: Some(EngineError::Unsupported),
            ..Script::default()
        });
        assert_eq!(
            refused.session.reconfigure(&EncoderChange::Bitrate(half)),
            Err(EncodeError::Unsupported)
        );
        assert!(refused.submit().is_ok());
    }

    #[test]
    fn static_bitrate_device_refuses_changes() {
        let mut device = geforce();
        for c in &mut device.codecs {
            c.dynamic_bitrate = false;
        }
        let mut s = session(&config(), &device, Script::default()).unwrap();
        assert_eq!(
            s.reconfigure(&EncoderChange::Bitrate(RateControl::Cbr {
                bitrate_bps: 500_000
            })),
            Err(EncodeError::Unsupported)
        );
    }
}
