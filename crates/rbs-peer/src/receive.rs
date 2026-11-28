//! The receiving half of a session, for the guest source (spec `guest-source.md`,
//! ADR-0139 decision 2). The adapter owns the bytes and the queue; this is only the
//! shape the guest source reads, so a plugin adapter can serve it too.

use std::sync::Arc;

/// The track of a received frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FrameTrack {
    /// An H.264 access unit, Annex B.
    Video,
    /// One Opus packet.
    Audio,
}

/// One frame from the peer. Untrusted: the guest source checks it before any decoder.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ReceivedFrame {
    /// Its track.
    pub track: FrameTrack,
    /// The frame's bytes.
    pub data: Arc<[u8]>,
    /// RTP time in the track's clock (90 kHz video, 48 kHz audio), extended past wrap.
    pub rtp_time: u64,
    /// The sender's master-clock capture time in nanoseconds, when the frame had one.
    pub capture_nanos: Option<u64>,
    /// An H.264 IDR access unit.
    pub keyframe: bool,
    /// False after a loss: the decoder may need a keyframe.
    pub contiguous: bool,
}

impl ReceivedFrame {
    /// A frame (the struct is non-exhaustive, so adapters build it here). The guest
    /// source stamps its arrival with its own injected clock when it polls it.
    pub fn new(
        track: FrameTrack,
        data: Arc<[u8]>,
        rtp_time: u64,
        capture_nanos: Option<u64>,
        keyframe: bool,
        contiguous: bool,
    ) -> Self {
        Self {
            track,
            data,
            rtp_time,
            capture_nanos,
            keyframe,
            contiguous,
        }
    }
}

/// The receiving half of one session, taken once from the answering session with
/// [`PeerSession::take_receiver`](crate::PeerSession::take_receiver). Frames come only
/// after this side is confirmed. The receiver never ends the pairing: the guest source
/// reports a reason (`guest_limits`) to the host, which closes the session.
///
/// Later methods are added as provided methods, so adapters keep compiling.
pub trait PeerReceiver: Send {
    /// The next frame, if one arrived. Never blocks.
    fn poll(&mut self) -> Option<ReceivedFrame>;
    /// Asks the sharer for a keyframe (a PLI).
    fn request_keyframe(&self);
    /// True while this side is connected and confirmed (the guest source drops every
    /// frame otherwise, a second gate behind the adapter's).
    fn confirmed(&self) -> bool;
    /// True once the session failed or closed.
    fn ended(&self) -> bool;
    /// Frames the adapter received and dropped before `poll`, and their bytes, since
    /// the session started (the guest source counts the bytes toward its rate limit).
    fn dropped(&self) -> Dropped;
}

/// Frames an adapter received and dropped before the guest source took them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Dropped {
    /// Frames.
    pub frames: u64,
    /// Their bytes.
    pub bytes: u64,
}

impl Dropped {
    /// Counts of frames and bytes.
    pub fn new(frames: u64, bytes: u64) -> Self {
        Self { frames, bytes }
    }
}
