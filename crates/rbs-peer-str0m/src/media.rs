//! The media half of a pairing (ADR-0139 decisions 8, 9 and 11): what the sharing
//! side writes, what the host side receives, and the feedback (keyframe requests,
//! bandwidth estimate) the share's encoder needs. Sans-IO, driven by [`Engine`].
//!
//! Gates (decision 6): nothing is written before this side is confirmed, and nothing
//! received is handed on before this side is confirmed. Both are counted.
//!
//! [`Engine`]: crate::engine::Engine

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use str0m::format::Codec;
use str0m::media::{Frequency, KeyframeRequestKind, MediaData, MediaKind, MediaTime, Mid, Pt};
use str0m::rtp::AbsCaptureTime;
use str0m::{Event, Rtc};

/// Frames received but not yet taken by the guest source; older ones are dropped.
pub const MAX_RECEIVED: usize = 256;

/// Which track a frame belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Track {
    /// H.264 video, 90 kHz RTP clock.
    Video,
    /// Opus audio, 48 kHz RTP clock.
    Audio,
}

impl Track {
    fn clock(self) -> Frequency {
        match self {
            Track::Video => Frequency::NINETY_KHZ,
            Track::Audio => Frequency::FORTY_EIGHT_KHZ,
        }
    }

    fn codec(self) -> Codec {
        match self {
            Track::Video => Codec::H264,
            Track::Audio => Codec::Opus,
        }
    }

    /// RTP ticks of a master-clock time, wrapping like an RTP timestamp.
    pub fn ticks(self, nanos: u64) -> u64 {
        let rate = u128::from(self.clock().get());
        ((u128::from(nanos) * rate + 500_000_000) / 1_000_000_000) as u64
    }
}

/// One frame from the peer: an H.264 access unit (Annex B) or one Opus packet.
#[derive(Debug, Clone)]
pub struct Received {
    /// Its track.
    pub track: Track,
    /// The frame.
    pub data: Arc<[u8]>,
    /// RTP time in the track's clock (90 kHz or 48 kHz), extended past wrap.
    pub rtp_time: u64,
    /// The sender's master-clock capture time in nanoseconds (abs-capture-time),
    /// which the guest source maps to this side's master clock (decision 11, Sync).
    pub capture_nanos: Option<u64>,
    /// An H.264 IDR access unit.
    pub keyframe: bool,
    /// False after lost packets: the decoder may need a keyframe.
    pub contiguous: bool,
    /// When its first packet arrived.
    pub arrived: Instant,
}

/// Why a frame was not written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteError {
    /// This side is not confirmed yet (or is not the sharing side).
    NotConfirmed,
    /// The track was not negotiated, or str0m refused the frame.
    Refused,
}

/// Counters, read by the owner thread for stats.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MediaCounters {
    /// Frames written to the peer.
    pub written: u64,
    /// Frames received and handed on.
    pub received: u64,
    /// Frames received before this side was confirmed, dropped.
    pub gated: u64,
    /// Frames dropped because the guest source did not take them in time.
    pub overflow: u64,
    /// Bytes of those frames.
    pub overflow_bytes: u64,
    /// Keyframe requests from the peer (PLI or FIR).
    pub keyframe_requests: u64,
}

/// The media state of one engine.
#[derive(Debug, Default)]
pub struct MediaState {
    video: Option<Mid>,
    audio: Option<Mid>,
    video_pt: Option<Pt>,
    audio_pt: Option<Pt>,
    received: VecDeque<Received>,
    estimate: Option<u64>,
    counters: MediaCounters,
    /// A received frame was dropped: the next one is marked not contiguous.
    gap_next: bool,
}

impl MediaState {
    /// The sharing side's own media sections, from its offer.
    pub fn set_mids(&mut self, video: Mid, audio: Mid) {
        self.video = Some(video);
        self.audio = Some(audio);
    }

    fn mid(&self, track: Track) -> Option<Mid> {
        match track {
            Track::Video => self.video,
            Track::Audio => self.audio,
        }
    }

    fn pt(&mut self, rtc: &mut Rtc, track: Track) -> Option<Pt> {
        let mid = self.mid(track)?;
        let cached = match track {
            Track::Video => &mut self.video_pt,
            Track::Audio => &mut self.audio_pt,
        };
        if cached.is_none() {
            let writer = rtc.writer(mid)?;
            *cached = writer
                .payload_params()
                .find(|p| p.spec().codec == track.codec())
                .map(|p| p.pt());
        }
        *cached
    }

    /// Writes one frame. `confirmed` is this side's gate. `capture_nanos` is the
    /// master-clock capture time, carried as abs-capture-time (its value is opaque to
    /// the peer, which only maps it).
    pub fn write(
        &mut self,
        rtc: &mut Rtc,
        confirmed: bool,
        now: Instant,
        track: Track,
        capture_nanos: u64,
        data: &[u8],
    ) -> Result<(), WriteError> {
        if !confirmed {
            return Err(WriteError::NotConfirmed);
        }
        let mid = self.mid(track).ok_or(WriteError::Refused)?;
        let pt = self.pt(rtc, track).ok_or(WriteError::Refused)?;
        let writer = rtc.writer(mid).ok_or(WriteError::Refused)?;
        let capture = AbsCaptureTime {
            capture_time: UNIX_EPOCH + Duration::from_nanos(capture_nanos),
            clock_offset: None,
        };
        let time = MediaTime::new(track.ticks(capture_nanos), track.clock());
        writer
            .abs_capture_time(capture)
            .write(pt, now, time, data.to_vec())
            .map_err(|_| WriteError::Refused)?;
        self.counters.written += 1;
        Ok(())
    }

    /// Asks the peer for a keyframe (after loss, or when a hidden guest is shown).
    pub fn request_keyframe(&mut self, rtc: &mut Rtc) -> bool {
        let Some(mid) = self.video else {
            return false;
        };
        rtc.writer(mid)
            .is_some_and(|mut w| w.request_keyframe(None, KeyframeRequestKind::Pli).is_ok())
    }

    /// Handles a str0m event; returns false for events that are not media.
    pub fn on_event(&mut self, event: &Event, confirmed: bool) -> bool {
        match event {
            Event::MediaAdded(added) => {
                match added.kind {
                    MediaKind::Video => self.video = self.video.or(Some(added.mid)),
                    MediaKind::Audio => self.audio = self.audio.or(Some(added.mid)),
                }
                true
            }
            Event::MediaData(data) => {
                self.received_frame(data, confirmed);
                true
            }
            Event::KeyframeRequest(_) => {
                self.counters.keyframe_requests += 1;
                true
            }
            Event::EgressBitrateEstimate(kind) => {
                if let str0m::bwe::BweKind::Twcc { estimate, .. } = kind {
                    self.estimate = Some(estimate.as_u64());
                }
                true
            }
            _ => false,
        }
    }

    fn received_frame(&mut self, data: &MediaData, confirmed: bool) {
        let track = if Some(data.mid) == self.video {
            Track::Video
        } else if Some(data.mid) == self.audio {
            Track::Audio
        } else {
            return;
        };
        if !confirmed {
            self.counters.gated += 1;
            return;
        }
        if self.received.len() >= MAX_RECEIVED
            && let Some(lost) = self.received.pop_front()
        {
            self.counters.overflow += 1;
            self.counters.overflow_bytes += lost.data.len() as u64;
            self.gap_next = true;
        }
        let capture_nanos = data.ext_vals.abs_capture_time.and_then(|t| {
            t.capture_time
                .duration_since(SystemTime::UNIX_EPOCH)
                .ok()
                .and_then(|d| u64::try_from(d.as_nanos()).ok())
        });
        self.received.push_back(Received {
            track,
            data: Arc::clone(&data.data),
            rtp_time: data.time.numer(),
            capture_nanos,
            keyframe: track == Track::Video && data.is_keyframe(),
            contiguous: data.contiguous && !std::mem::take(&mut self.gap_next),
            arrived: data.network_time,
        });
        self.counters.received += 1;
    }

    /// The next received frame.
    pub fn poll_received(&mut self) -> Option<Received> {
        self.received.pop_front()
    }

    /// The latest congestion estimate in bits per second.
    pub fn estimate(&self) -> Option<u64> {
        self.estimate
    }

    /// The counters.
    pub fn counters(&self) -> MediaCounters {
        self.counters
    }
}
