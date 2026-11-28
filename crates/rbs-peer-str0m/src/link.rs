//! What crosses threads for a session's media (ADR-0139 decisions 2, 8 and 11).
//!
//! - **Send**: the share output's worker calls [`WebRtcTransport::send`], which pushes
//!   a packet clone (no copy of its bytes) into a single-producer ring and wakes the
//!   owner thread; the owner thread writes it to str0m.
//! - **Receive**: the owner thread pushes each received frame into a second ring; the
//!   guest source's worker takes it through [`GuestReceiver`].
//! - **Feedback**: counters and the congestion estimate are atomics.
//!
//! Nothing here allocates, blocks or takes a contended lock on the send path. The
//! wake is one non-blocking datagram to the owner thread's own socket, sent only
//! while that thread is waiting on it.

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering, fence};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use rbs_core::media::{BitstreamFormat, CodecId, EncodedPacket, StreamDescriptor};
use rbs_peer::MediaHandle;
use rbs_transport::{
    DropReason, SendOutcome, Transport, TransportError, TransportEvent, TransportItem,
    TransportStats,
};

use crate::media::{Received, Track};

/// Packets queued towards the owner thread (about 2 s of video and sound).
pub const SEND_SLOTS: usize = 256;
/// Frames queued towards the guest source.
pub const RECEIVE_SLOTS: usize = 256;
/// The wake datagram (a single byte from the owner socket to itself).
pub(crate) const WAKE: [u8; 1] = [0];

/// A packet on its way to the peer.
#[derive(Debug)]
pub(crate) struct Outgoing {
    pub track: Track,
    pub packet: EncodedPacket,
}

/// Where the session is, as the transport and the guest source see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum LinkState {
    /// Not connected or not confirmed yet.
    Waiting = 0,
    /// Both gates open on this side.
    Live = 1,
    /// Failed or closed; final.
    Ended = 2,
}

/// Shared by the owner thread, the transport and the guest receiver.
#[derive(Debug)]
pub struct WebRtcMedia {
    send: Mutex<Option<rtrb::Producer<Outgoing>>>,
    pub(crate) send_rx: Mutex<Option<rtrb::Consumer<Outgoing>>>,
    receive: Mutex<Option<rtrb::Consumer<Received>>>,
    pub(crate) receive_tx: Mutex<Option<rtrb::Producer<Received>>>,
    /// SPS and PPS of the video stream when its packets are length-prefixed (AVCC):
    /// written once by `start`, read by the owner thread before a keyframe.
    pub(crate) parameter_sets: OnceLock<Vec<Vec<u8>>>,
    state: AtomicU8,
    sleeping: AtomicBool,
    /// A wake datagram is on its way for this wait: at most one per sleep.
    wake_sent: AtomicBool,
    /// The owner thread lost a video packet (str0m refused it, or its conversion
    /// failed): the transport waits for a keyframe and asks for one.
    owner_lost_video: AtomicBool,
    /// Packets the owner thread could not hand to str0m.
    owner_dropped: AtomicU64,
    /// The guest's ring was full: its next frame is marked not contiguous.
    receive_gap: AtomicBool,
    keyframe_wanted: AtomicBool,
    pub(crate) bytes_sent: AtomicU64,
    pub(crate) keyframe_requests: AtomicU64,
    pub(crate) estimate_bps: AtomicU64,
    pub(crate) receive_overflow: AtomicU64,
    /// Bytes of the frames the guest's ring dropped.
    receive_overflow_bytes: AtomicU64,
    /// Frames (and their bytes) the engine's own queue dropped, as last published.
    engine_overflow: AtomicU64,
    engine_overflow_bytes: AtomicU64,
    /// Set once, before the owner thread starts: read without a lock.
    waker: OnceLock<(UdpSocket, SocketAddr)>,
}

impl WebRtcMedia {
    pub(crate) fn new() -> Arc<Self> {
        let (send, send_rx) = rtrb::RingBuffer::new(SEND_SLOTS);
        let (receive_tx, receive) = rtrb::RingBuffer::new(RECEIVE_SLOTS);
        Arc::new(Self {
            send: Mutex::new(Some(send)),
            send_rx: Mutex::new(Some(send_rx)),
            receive: Mutex::new(Some(receive)),
            receive_tx: Mutex::new(Some(receive_tx)),
            parameter_sets: OnceLock::new(),
            state: AtomicU8::new(LinkState::Waiting as u8),
            sleeping: AtomicBool::new(false),
            wake_sent: AtomicBool::new(false),
            owner_lost_video: AtomicBool::new(false),
            owner_dropped: AtomicU64::new(0),
            receive_gap: AtomicBool::new(false),
            keyframe_wanted: AtomicBool::new(false),
            bytes_sent: AtomicU64::new(0),
            keyframe_requests: AtomicU64::new(0),
            estimate_bps: AtomicU64::new(0),
            receive_overflow: AtomicU64::new(0),
            receive_overflow_bytes: AtomicU64::new(0),
            engine_overflow: AtomicU64::new(0),
            engine_overflow_bytes: AtomicU64::new(0),
            waker: OnceLock::new(),
        })
    }

    /// The handle the port hands out.
    pub(crate) fn handle(self: &Arc<Self>) -> MediaHandle {
        MediaHandle::new(Arc::clone(self))
    }

    pub(crate) fn set_waker(&self, socket: UdpSocket, own: SocketAddr) {
        let _ = self.waker.set((socket, own));
    }

    pub(crate) fn set_state(&self, state: LinkState) {
        // Ended is final.
        let _ = self
            .state
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |now| {
                (now != LinkState::Ended as u8).then_some(state as u8)
            });
    }

    fn state(&self) -> LinkState {
        match self.state.load(Ordering::Acquire) {
            1 => LinkState::Live,
            2 => LinkState::Ended,
            _ => LinkState::Waiting,
        }
    }

    /// The owner thread is about to wait on its socket.
    pub(crate) fn set_sleeping(&self, sleeping: bool) {
        if sleeping {
            self.wake_sent.store(false, Ordering::SeqCst);
        }
        self.sleeping.store(sleeping, Ordering::SeqCst);
        // Pairs with the fence in `wake`: a push before this store is seen by the
        // owner's next drain, a push after it sees `sleeping`.
        fence(Ordering::SeqCst);
    }

    fn wake(&self) {
        fence(Ordering::SeqCst);
        if !self.sleeping.load(Ordering::SeqCst) || self.wake_sent.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some((socket, own)) = self.waker.get() {
            // Non-blocking in practice (one byte on loopback); a lost wake costs at
            // most one loop wait.
            let _ = socket.send_to(&WAKE, own);
        }
    }

    pub(crate) fn take_keyframe_wanted(&self) -> bool {
        self.keyframe_wanted.swap(false, Ordering::AcqRel)
    }

    /// A handle that is live at once and has no owner thread, for allocation tests and
    /// benchmarks of `send` only: queued packets are never sent.
    #[doc(hidden)]
    pub fn detached_live() -> MediaHandle {
        let media = Self::new();
        media.set_state(LinkState::Live);
        media.handle()
    }

    /// The share output's transport for this session, once.
    pub fn transport(handle: &MediaHandle) -> Option<WebRtcTransport> {
        let media = handle.downcast::<WebRtcMedia>()?;
        let producer = lock(&media.send).take()?;
        Some(WebRtcTransport {
            media,
            producer: Some(producer),
            video: None,
            audio: None,
            started: false,
            seen_live: false,
            seen_ended: false,
            drop_reason: DropReason::Disconnected,
            // The peer's decoder starts at a keyframe (one is asked for when live).
            skipping_video: true,
            dropped_unreported: 0,
            dropped_total: 0,
            last_requests: 0,
            last_estimate: 0,
        })
    }

    /// The guest source's receiver for this session, once.
    pub fn receiver(handle: &MediaHandle) -> Option<GuestReceiver> {
        let media = handle.downcast::<WebRtcMedia>()?;
        let consumer = lock(&media.receive).take()?;
        Some(GuestReceiver { media, consumer })
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The receiving half, owned by the guest source's worker.
#[derive(Debug)]
pub struct GuestReceiver {
    media: Arc<WebRtcMedia>,
    consumer: rtrb::Consumer<Received>,
}

impl GuestReceiver {
    /// The next frame, if one arrived. Frames arrive only after this side confirmed.
    pub fn poll(&mut self) -> Option<Received> {
        self.consumer.pop().ok()
    }

    /// Asks the sharer for a keyframe (PLI): after loss, or when a hidden guest is
    /// shown again (ADR-0139 decision 12).
    pub fn request_keyframe(&self) {
        self.media.keyframe_wanted.store(true, Ordering::Release);
        self.media.wake();
    }

    /// True while this side is connected and confirmed.
    pub fn confirmed(&self) -> bool {
        self.media.state() == LinkState::Live
    }

    /// True once the session failed or closed.
    pub fn ended(&self) -> bool {
        self.media.state() == LinkState::Ended
    }

    /// Frames dropped because they were not taken in time.
    pub fn overflow(&self) -> u64 {
        self.media.receive_overflow.load(Ordering::Relaxed)
            + self.media.engine_overflow.load(Ordering::Relaxed)
    }

    /// Bytes of the frames [`Self::overflow`] counts.
    pub fn overflow_bytes(&self) -> u64 {
        self.media.receive_overflow_bytes.load(Ordering::Relaxed)
            + self.media.engine_overflow_bytes.load(Ordering::Relaxed)
    }
}

/// The sending half: the guest share output's `Transport` (input = packets).
#[derive(Debug)]
pub struct WebRtcTransport {
    media: Arc<WebRtcMedia>,
    producer: Option<rtrb::Producer<Outgoing>>,
    video: Option<usize>,
    audio: Option<usize>,
    started: bool,
    seen_live: bool,
    seen_ended: bool,
    drop_reason: DropReason,
    /// After a dropped video packet: refuse video until the next keyframe.
    skipping_video: bool,
    dropped_unreported: u32,
    dropped_total: u64,
    last_requests: u64,
    last_estimate: u64,
}

impl WebRtcTransport {
    fn track(&self, stream: usize) -> Option<Track> {
        if Some(stream) == self.video {
            Some(Track::Video)
        } else if Some(stream) == self.audio {
            Some(Track::Audio)
        } else {
            None
        }
    }

    fn drop_one(&mut self, reason: DropReason) -> SendOutcome {
        self.dropped_unreported = self.dropped_unreported.saturating_add(1);
        self.dropped_total += 1;
        self.drop_reason = reason;
        SendOutcome::Dropped { reason }
    }
}

impl Transport for WebRtcTransport {
    fn start(&mut self, streams: &[StreamDescriptor]) -> Result<(), TransportError> {
        if self.started {
            return Err(TransportError::InvalidInput("already started".into()));
        }
        for (i, s) in streams.iter().enumerate() {
            match s.codec {
                CodecId::H264 if self.video.is_none() => {
                    if let BitstreamFormat::LengthPrefixed { .. } = s.bitstream {
                        let sets = rbs_media_format::avc::AvcConfig::parse(&s.extradata)
                            .map_err(|e| TransportError::InvalidInput(e.to_string()))?;
                        let sets = sets.sps.iter().chain(&sets.pps).map(|u| u.to_vec());
                        if self.media.parameter_sets.set(sets.collect()).is_err() {
                            return Err(TransportError::InvalidInput("already started".into()));
                        }
                    }
                    self.video = Some(i);
                }
                CodecId::Opus if self.audio.is_none() => self.audio = Some(i),
                _ => {
                    return Err(TransportError::InvalidInput(
                        "a guest share carries one H.264 and one Opus stream".into(),
                    ));
                }
            }
        }
        self.started = true;
        Ok(())
    }

    fn send(&mut self, item: TransportItem<'_>) -> Result<SendOutcome, TransportError> {
        if !self.started {
            return Err(TransportError::NotStarted);
        }
        let TransportItem::Packet { stream, packet } = item else {
            return Err(TransportError::InvalidInput(
                "the guest share takes encoded packets".into(),
            ));
        };
        let track = self
            .track(usize::from(stream.0))
            .ok_or_else(|| TransportError::InvalidInput("unknown stream".into()))?;
        if self.media.owner_lost_video.swap(false, Ordering::AcqRel) {
            self.skipping_video = true;
        }
        match self.media.state() {
            LinkState::Ended => return Err(TransportError::Closed),
            // The gate (ADR-0139 decision 6): nothing leaves before confirmation.
            LinkState::Waiting => return Ok(self.drop_one(DropReason::Disconnected)),
            LinkState::Live => {}
        }
        if track == Track::Video {
            if self.skipping_video && !packet.keyframe() {
                return Ok(self.drop_one(DropReason::QueueFull));
            }
            self.skipping_video = false;
        }
        let Some(producer) = self.producer.as_mut() else {
            return Err(TransportError::Closed);
        };
        let outgoing = Outgoing {
            track,
            packet: packet.clone(),
        };
        if producer.push(outgoing).is_err() {
            // The owner thread fell behind: the stream must not continue from a broken
            // reference, so video waits for a keyframe (and asks for one).
            if track == Track::Video {
                self.skipping_video = true;
                self.media.keyframe_requests.fetch_add(1, Ordering::Relaxed);
            }
            return Ok(self.drop_one(DropReason::QueueFull));
        }
        self.media.wake();
        Ok(SendOutcome::Queued)
    }

    fn poll_events(&mut self, out: &mut Vec<TransportEvent>) {
        let state = self.media.state();
        if state == LinkState::Live && !self.seen_live {
            self.seen_live = true;
            out.push(TransportEvent::Connected);
            out.push(TransportEvent::KeyframeRequested);
        }
        if state == LinkState::Ended && !self.seen_ended {
            self.seen_ended = true;
            out.push(TransportEvent::Disconnected {
                error: TransportError::Closed,
            });
        }
        let requests = self.media.keyframe_requests.load(Ordering::Relaxed);
        if requests != self.last_requests {
            self.last_requests = requests;
            out.push(TransportEvent::KeyframeRequested);
        }
        let estimate = self.media.estimate_bps.load(Ordering::Relaxed);
        if estimate != 0 && estimate != self.last_estimate {
            self.last_estimate = estimate;
            out.push(TransportEvent::BitrateHint { bps: estimate });
        }
        if self.dropped_unreported > 0 {
            out.push(TransportEvent::Dropped {
                items: self.dropped_unreported,
                reason: self.drop_reason,
            });
            self.dropped_unreported = 0;
        }
    }

    fn stats(&self) -> TransportStats {
        let mut stats = TransportStats::default();
        stats.bytes_sent = self.media.bytes_sent.load(Ordering::Relaxed);
        stats.items_dropped = self.dropped_total + self.media.owner_dropped.load(Ordering::Relaxed);
        let estimate = self.media.estimate_bps.load(Ordering::Relaxed);
        stats.bandwidth_bps = (estimate != 0).then_some(estimate);
        stats
    }

    fn finish(&mut self, _timeout: Duration) -> Result<(), TransportError> {
        // Real-time media: what is queued is stale by now. The session itself is
        // closed by its owner (the share), not by the transport.
        self.producer = None;
        Ok(())
    }
}

/// The owner thread's side of the link.
pub(crate) struct OwnerSide {
    media: Arc<WebRtcMedia>,
    send_rx: Option<rtrb::Consumer<Outgoing>>,
    receive_tx: Option<rtrb::Producer<Received>>,
    own: Option<SocketAddr>,
    /// Reused for length-prefixed video converted to Annex B.
    scratch: Vec<u8>,
    last_requests: u64,
}

impl OwnerSide {
    pub(crate) fn new(media: &Arc<WebRtcMedia>, socket: &UdpSocket) -> Self {
        let own = socket
            .local_addr()
            .ok()
            .map(|a| SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, a.port())));
        if let (Some(own), Ok(clone)) = (own, socket.try_clone()) {
            media.set_waker(clone, own);
        }
        Self {
            media: Arc::clone(media),
            send_rx: lock(&media.send_rx).take(),
            receive_tx: lock(&media.receive_tx).take(),
            own,
            scratch: Vec::new(),
            last_requests: 0,
        }
    }

    /// A wake datagram from this thread's own socket, not network input.
    pub(crate) fn is_wake(&self, from: SocketAddr, bytes: &[u8]) -> bool {
        Some(from) == self.own && bytes == WAKE
    }

    /// Moves queued packets to the engine and received frames to the guest source,
    /// and publishes feedback and state.
    pub(crate) fn pump(&mut self, engine: &mut crate::engine::Engine, now: std::time::Instant) {
        use crate::engine::Phase;
        while let Some(out) = self.send_rx.as_mut().and_then(|rx| rx.pop().ok()) {
            let data = match (out.track, out.packet.bitstream()) {
                (Track::Video, BitstreamFormat::LengthPrefixed { length_size }) => {
                    self.scratch.clear();
                    // A keyframe carries its parameter sets in band (the peer's
                    // decoder starts there).
                    if out.packet.keyframe() {
                        for set in self.media.parameter_sets.get().into_iter().flatten() {
                            self.scratch.extend_from_slice(&[0, 0, 0, 1]);
                            self.scratch.extend_from_slice(set);
                        }
                    }
                    if rbs_media_format::nal::length_prefixed_to_annexb(
                        out.packet.data(),
                        length_size,
                        &[],
                        &mut self.scratch,
                    )
                    .is_err()
                    {
                        self.lost(Track::Video);
                        continue;
                    }
                    &self.scratch[..]
                }
                _ => &out.packet.data()[..],
            };
            let capture = out.packet.pts().as_nanos();
            let len = data.len() as u64;
            if engine.write_media(now, out.track, capture, data).is_ok() {
                self.media.bytes_sent.fetch_add(len, Ordering::Relaxed);
            } else {
                self.lost(out.track);
            }
        }
        // A request made before the video section exists waits for the next turn.
        if self.media.take_keyframe_wanted() && !engine.request_keyframe(now) {
            self.media.keyframe_wanted.store(true, Ordering::Release);
        }
        // The state first, so the guest source's own gate does not drop the first
        // frames of the turn that confirmed this side.
        self.publish_state(engine);
        while let Some(mut frame) = engine.poll_received() {
            if self.media.receive_gap.swap(false, Ordering::AcqRel) {
                frame.contiguous = false;
            }
            let len = frame.data.len() as u64;
            let pushed = self
                .receive_tx
                .as_mut()
                .is_some_and(|tx| tx.push(frame).is_ok());
            if !pushed {
                // The newest frame is lost (the ring cannot drop its oldest): mark the
                // gap and ask for a keyframe, so the decoder does not run on across it.
                self.media.receive_overflow.fetch_add(1, Ordering::Relaxed);
                self.media
                    .receive_overflow_bytes
                    .fetch_add(len, Ordering::Relaxed);
                self.media.receive_gap.store(true, Ordering::Release);
                if !engine.request_keyframe(now) {
                    self.media.keyframe_wanted.store(true, Ordering::Release);
                }
            }
        }
        let counters = engine.media_counters();
        self.media
            .engine_overflow
            .store(counters.overflow, Ordering::Relaxed);
        self.media
            .engine_overflow_bytes
            .store(counters.overflow_bytes, Ordering::Relaxed);
        let new_requests = counters.keyframe_requests - self.last_requests;
        if new_requests > 0 {
            self.last_requests = counters.keyframe_requests;
            self.media
                .keyframe_requests
                .fetch_add(new_requests, Ordering::Relaxed);
        }
        if let Some(bps) = engine.bitrate_estimate() {
            self.media.estimate_bps.store(bps, Ordering::Relaxed);
        }
        self.publish_state(engine);
    }

    fn publish_state(&self, engine: &crate::engine::Engine) {
        use crate::engine::Phase;
        self.media.set_state(match engine.phase() {
            Phase::Connected { confirmed: true } => LinkState::Live,
            Phase::Failed(_) | Phase::Closed => LinkState::Ended,
            _ => LinkState::Waiting,
        });
    }

    /// A packet str0m did not take: counted, and for video the stream waits for a
    /// keyframe and asks for one.
    fn lost(&self, track: Track) {
        self.media.owner_dropped.fetch_add(1, Ordering::Relaxed);
        if track == Track::Video {
            self.media.owner_lost_video.store(true, Ordering::Release);
            self.media.keyframe_requests.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Called before the socket wait: true when packets are already queued, so the
    /// wait must be skipped.
    pub(crate) fn before_wait(&self) -> bool {
        self.media.set_sleeping(true);
        self.send_rx.as_ref().is_some_and(|rx| !rx.is_empty())
            || self.media.keyframe_wanted.load(Ordering::Acquire)
    }

    pub(crate) fn after_wait(&self) {
        self.media.set_sleeping(false);
    }

    /// The session ended for good.
    pub(crate) fn end(&self) {
        self.media.set_state(LinkState::Ended);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use rbs_core::media::{
        AudioFormat, Bytes, ColorSpace, PixelFormat, Size, StreamIndex, StreamParams, VideoFormat,
    };
    use rbs_core::time::Timestamp;
    use rbs_core::timing::FrameRate;

    use super::*;

    fn descriptors() -> Vec<StreamDescriptor> {
        let video = StreamParams::video(
            VideoFormat::new(
                Size::new(1280, 720),
                PixelFormat::Nv12,
                ColorSpace::BT709_LIMITED,
            ),
            FrameRate::FPS_30,
        );
        vec![
            StreamDescriptor::new(CodecId::H264, BitstreamFormat::AnnexB, Bytes::new(), video),
            StreamDescriptor::new(
                CodecId::Opus,
                BitstreamFormat::Raw,
                Bytes::new(),
                StreamParams::audio(AudioFormat::STEREO_48K, 960),
            ),
        ]
    }

    fn video(key: bool) -> EncodedPacket {
        let data = Bytes::from_static(&[0, 0, 0, 1, 0x65, 1]);
        EncodedPacket::new(
            CodecId::H264,
            BitstreamFormat::AnnexB,
            Timestamp::ZERO,
            Timestamp::ZERO,
            key,
            data,
        )
        .unwrap()
    }

    fn send(t: &mut WebRtcTransport, key: bool) -> SendOutcome {
        let packet = video(key);
        t.send(TransportItem::Packet {
            stream: StreamIndex(0),
            packet: &packet,
        })
        .unwrap()
    }

    fn events(t: &mut WebRtcTransport) -> Vec<TransportEvent> {
        let mut out = Vec::new();
        t.poll_events(&mut out);
        out
    }

    #[test]
    fn the_transport_and_receiver_are_handed_out_once() {
        let media = WebRtcMedia::new();
        let handle = media.handle();
        assert!(WebRtcMedia::transport(&handle).is_some());
        assert!(WebRtcMedia::transport(&handle).is_none());
        assert!(WebRtcMedia::receiver(&handle).is_some());
        assert!(WebRtcMedia::receiver(&handle).is_none());
    }

    #[test]
    fn only_h264_and_opus_are_taken() {
        let media = WebRtcMedia::new();
        let mut t = WebRtcMedia::transport(&media.handle()).unwrap();
        let mut streams = descriptors();
        streams[1].codec = CodecId::Aac;
        assert!(matches!(
            t.start(&streams),
            Err(TransportError::InvalidInput(_))
        ));
        let mut t = {
            let media = WebRtcMedia::new();
            WebRtcMedia::transport(&media.handle()).unwrap()
        };
        assert_eq!(
            t.send(TransportItem::Packet {
                stream: StreamIndex(0),
                packet: &video(true)
            }),
            Err(TransportError::NotStarted)
        );
        t.start(&descriptors()).unwrap();
    }

    #[test]
    fn nothing_is_queued_before_the_session_is_live() {
        let media = WebRtcMedia::new();
        let mut t = WebRtcMedia::transport(&media.handle()).unwrap();
        t.start(&descriptors()).unwrap();
        assert_eq!(
            send(&mut t, true),
            SendOutcome::Dropped {
                reason: DropReason::Disconnected
            }
        );
        assert!(
            lock(&media.send_rx).as_ref().unwrap().is_empty(),
            "the gate"
        );
        media.set_state(LinkState::Live);
        assert_eq!(send(&mut t, true), SendOutcome::Queued);
        let ev = events(&mut t);
        assert!(ev.contains(&TransportEvent::Connected));
        assert!(ev.contains(&TransportEvent::Dropped {
            items: 1,
            reason: DropReason::Disconnected
        }));
        assert!(events(&mut t).is_empty(), "each event once");
    }

    #[test]
    fn a_full_queue_drops_video_until_the_next_keyframe_and_asks_for_one() {
        let media = WebRtcMedia::new();
        let mut t = WebRtcMedia::transport(&media.handle()).unwrap();
        t.start(&descriptors()).unwrap();
        media.set_state(LinkState::Live);
        assert_eq!(
            send(&mut t, false),
            SendOutcome::Dropped {
                reason: DropReason::QueueFull
            },
            "the stream starts at a keyframe"
        );
        assert_eq!(send(&mut t, true), SendOutcome::Queued);
        for _ in 1..SEND_SLOTS {
            assert_eq!(send(&mut t, false), SendOutcome::Queued);
        }
        assert_eq!(
            send(&mut t, false),
            SendOutcome::Dropped {
                reason: DropReason::QueueFull
            }
        );
        // The owner thread catches up.
        let mut rx = lock(&media.send_rx).take().unwrap();
        while rx.pop().is_ok() {}
        assert_eq!(
            send(&mut t, false),
            SendOutcome::Dropped {
                reason: DropReason::QueueFull
            },
            "no broken reference"
        );
        assert_eq!(send(&mut t, true), SendOutcome::Queued);
        assert_eq!(send(&mut t, false), SendOutcome::Queued);
        let ev = events(&mut t);
        assert!(ev.contains(&TransportEvent::KeyframeRequested));
        assert_eq!(t.stats().items_dropped, 3);
    }

    #[test]
    fn an_ended_session_closes_the_transport_once() {
        let media = WebRtcMedia::new();
        let mut t = WebRtcMedia::transport(&media.handle()).unwrap();
        t.start(&descriptors()).unwrap();
        media.set_state(LinkState::Ended);
        media.set_state(LinkState::Live);
        let packet = video(true);
        assert_eq!(
            t.send(TransportItem::Packet {
                stream: StreamIndex(0),
                packet: &packet
            }),
            Err(TransportError::Closed),
            "Ended is final"
        );
        let ev = events(&mut t);
        assert_eq!(
            ev,
            vec![TransportEvent::Disconnected {
                error: TransportError::Closed
            }]
        );
        assert!(events(&mut t).is_empty());
    }

    #[test]
    fn sending_allocates_nothing() {
        // The packet clone shares its bytes: same pointer, no copy.
        let packet = video(true);
        let clone = Outgoing {
            track: Track::Video,
            packet: packet.clone(),
        };
        assert_eq!(clone.packet.data().as_ptr(), packet.data().as_ptr());
    }
}
