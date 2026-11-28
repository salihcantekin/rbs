//! One guest source's worker (spec `guest-source.md`, "The worker"; ADR-0139
//! decision 11): it takes the peer's frames, checks them, maps their times onto the
//! master clock, holds them in the jitter buffers and drives the decoders. Pictures
//! and sound leave through a [`GuestSink`], which the source's nodes read.
//!
//! [`GuestWorker::turn`] is one pass at a given time, so tests drive it on an
//! injected clock with fakes of the receiver and both decoders. [`spawn`] runs it on
//! its own thread, `rbs-guest-<n>`, only while the session is connected (P13).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use rbs_codec::{AudioDecoder, CreateError, VideoDecodeError, VideoDecoder, VideoDecoderConfig};
use rbs_core::media::{BitstreamFormat, Bytes, CodecId, EncodedPacket, Size, VideoFrame};
use rbs_core::time::{Clock, Timestamp};
use rbs_peer::{FrameTrack, PeerReceiver, ReceivedFrame};

use super::checks::{FrameRate, MAX_GUEST_SIZE, ParamSets, opus_samples};
use super::jitter::{JitterBuffer, Pushed, Queued};
use super::{ClockMap, TokenBucket, TrackTiming};

/// The longest wait between two turns.
pub const MAX_WAIT: Duration = Duration::from_millis(10);
/// A decoder not shown for this long is released (`backend-ports.md` invariant 10).
pub const HIDDEN_RELEASE: Duration = Duration::from_secs(2);
/// Missing sound up to this long is concealed; longer restarts the sound.
pub const MAX_CONCEAL: Duration = Duration::from_millis(100);
/// Concealment comes in multiples of 2.5 ms at 48 kHz.
const CONCEAL_STEP: u32 = 120;
/// Opus decodes at 48 kHz.
const OPUS_RATE: u64 = 48_000;
/// The least and the starting decode lead.
const MIN_LEAD: Duration = Duration::from_millis(5);
/// Decoder restarts back off from 1 s up to 30 s (ADR-0024).
const RESTART_BACKOFF: (Duration, Duration) = (Duration::from_secs(1), Duration::from_secs(30));

/// Where the worker's output goes: the source's picture and sound nodes.
pub trait GuestSink: Send {
    /// A decoded picture, due at its timestamp (its mapped master time plus the target).
    fn picture(&mut self, frame: VideoFrame);
    /// Decoded or concealed sound, interleaved at the decoder's format, whose first
    /// sample plays at master time `at`.
    fn sound(&mut self, at: Timestamp, samples: &[f32], channels: u16);
    /// The sound stopped for longer than can be concealed: the aligner starts over.
    fn sound_restart(&mut self);
}

/// Makes a video decoder session; the composition root picks the adapter.
pub type MakeVideoDecoder =
    Box<dyn FnMut(&VideoDecoderConfig) -> Result<Box<dyn VideoDecoder>, CreateError> + Send>;

/// Why the worker stopped. The host closes the pairing and shows the reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuestEnd {
    /// `guest_limits`: the peer sent more, or other, than the limits allow.
    Limits,
    /// `guest_disconnected`: the session ended.
    Disconnected,
}

impl GuestEnd {
    /// The reason id (`guest.status`).
    pub fn reason(self) -> &'static str {
        match self {
            Self::Limits => "guest_limits",
            Self::Disconnected => "guest_disconnected",
        }
    }
}

/// Frames dropped or refused, by cause. Read on a cold thread and summarised in one
/// log line a minute, never per frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GuestCounts {
    /// Before this side was confirmed.
    pub gated: u64,
    /// Of a track kind the worker does not take.
    pub other_track: u64,
    /// Times that disagree or go back.
    pub outliers: u64,
    /// Before the clock map was valid.
    pub before_map: u64,
    /// Video while the source is not shown.
    pub hidden: u64,
    /// Refused by a jitter buffer (late, before a keyframe).
    pub jitter: u64,
    /// Decoded pictures whose size differs from the SPS's.
    pub wrong_size: u64,
    /// Decoder failures (each makes a new decoder later).
    pub decoder_faults: u64,
    /// Sound stretches concealed.
    pub concealed: u64,
    /// Sound restarts after a gap too long to conceal.
    pub sound_restarts: u64,
    /// Sound packets the decoder refused.
    pub sound_corrupt: u64,
    /// Pictures handed on.
    pub pictures: u64,
}

/// The settings of one worker.
#[derive(Debug, Clone, Copy)]
pub struct WorkerSettings {
    /// The jitter target (the `jitter_ms` setting).
    pub target: Duration,
}

/// Video waiting for its decode, with the bytes it arrived in.
type VideoUnit = Arc<[u8]>;
/// Sound waiting for its release: the bytes and the RTP time.
type SoundUnit = (Arc<[u8]>, u64);

/// One guest source's worker state.
pub struct GuestWorker {
    rx: Box<dyn PeerReceiver>,
    sink: Box<dyn GuestSink>,
    clock: Arc<dyn Clock>,
    make_video: MakeVideoDecoder,
    audio_decoder: Box<dyn AudioDecoder>,
    bucket: TokenBucket,
    dropped_bytes: u64,
    video_timing: TrackTiming,
    audio_timing: TrackTiming,
    rate: FrameRate,
    params: ParamSets,
    map: ClockMap,
    video: JitterBuffer<VideoUnit>,
    audio: JitterBuffer<SoundUnit>,
    decoder: Option<Box<dyn VideoDecoder>>,
    /// Earliest time a new decoder may be made, after a fault.
    restart_at: Option<Timestamp>,
    backoff: Duration,
    lead: Duration,
    shown: bool,
    hidden_since: Option<Timestamp>,
    /// The RTP time where the last released sound ends.
    sound_end: Option<u64>,
    pcm: Vec<f32>,
    pictures: Vec<VideoFrame>,
    counts: GuestCounts,
    ended: Option<GuestEnd>,
}

impl GuestWorker {
    /// A worker for a connected session's receiver. Makes no video decoder until a
    /// shown picture needs one.
    pub fn new(
        rx: Box<dyn PeerReceiver>,
        sink: Box<dyn GuestSink>,
        clock: Arc<dyn Clock>,
        make_video: MakeVideoDecoder,
        audio_decoder: Box<dyn AudioDecoder>,
        settings: WorkerSettings,
    ) -> Self {
        let pcm = Vec::with_capacity(audio_decoder.max_frames() * 2);
        Self {
            rx,
            sink,
            clock,
            make_video,
            audio_decoder,
            bucket: TokenBucket::guest(),
            dropped_bytes: 0,
            video_timing: TrackTiming::new(90_000),
            audio_timing: TrackTiming::new(OPUS_RATE),
            rate: FrameRate::new(),
            params: ParamSets::new(),
            map: ClockMap::new(),
            video: JitterBuffer::new(settings.target, true),
            audio: JitterBuffer::new(settings.target, false),
            decoder: None,
            restart_at: None,
            backoff: RESTART_BACKOFF.0,
            lead: MIN_LEAD,
            shown: true,
            hidden_since: None,
            sound_end: None,
            pcm,
            pictures: Vec::with_capacity(4),
            counts: GuestCounts::default(),
            ended: None,
        }
    }

    /// Why it stopped, once it did.
    pub fn ended(&self) -> Option<GuestEnd> {
        self.ended
    }

    /// The counts so far.
    pub fn counts(&self) -> GuestCounts {
        self.counts
    }

    /// True while a video decoder session exists.
    pub fn decoding(&self) -> bool {
        self.decoder.is_some()
    }

    /// Whether a shown scene draws the source. Shown again: a keyframe is asked for
    /// and the next picture needs a new decoder (spec "Shown or not").
    pub fn set_shown(&mut self, shown: bool, now: Timestamp) {
        if shown == self.shown {
            return;
        }
        self.shown = shown;
        if shown {
            self.hidden_since = None;
            self.video.restart();
            self.rx.request_keyframe();
        } else {
            self.hidden_since = Some(now);
        }
    }

    /// One pass at `now`: takes what arrived, releases what is due. The next time it
    /// should run, at most [`MAX_WAIT`] away; `None` once it ended.
    pub fn turn(&mut self, now: Timestamp) -> Option<Timestamp> {
        if self.ended.is_some() {
            return None;
        }
        self.take_frames(now);
        if self.ended.is_none() && self.rx.ended() {
            self.ended = Some(GuestEnd::Disconnected);
        }
        if self.ended.is_some() {
            self.decoder = None;
            return None;
        }
        if self.video.keyframe_request(now) {
            self.rx.request_keyframe();
        }
        self.release_sound(now);
        self.release_video(now);
        self.take_frames(now);
        self.release_hidden(now);
        self.ended.is_none().then(|| self.next_wake(now))
    }

    fn next_wake(&self, now: Timestamp) -> Timestamp {
        let mut next = now + MAX_WAIT;
        if let Some(t) = self.audio.next_release() {
            next = next.min(t);
        }
        if let Some(t) = self.video.next_release() {
            next = next.min(Timestamp::from_nanos(
                t.as_nanos().saturating_sub(self.lead.as_nanos() as u64),
            ));
        }
        next.max(now)
    }

    fn take_frames(&mut self, now: Timestamp) {
        let dropped = self.rx.dropped().bytes;
        let grown = dropped.saturating_sub(self.dropped_bytes);
        self.dropped_bytes = dropped;
        if grown > 0 && !self.bucket.take(now, grown) {
            self.ended = Some(GuestEnd::Limits);
            return;
        }
        while self.ended.is_none() {
            let Some(frame) = self.rx.poll() else { break };
            self.frame(frame, now);
        }
    }

    /// Worker steps 1 to 7 for one frame.
    fn frame(&mut self, f: ReceivedFrame, now: Timestamp) {
        if !self.rx.confirmed() {
            self.counts.gated += 1;
            return;
        }
        if !self.bucket.take(now, f.data.len() as u64) {
            self.ended = Some(GuestEnd::Limits);
            return;
        }
        let video = match f.track {
            FrameTrack::Video => true,
            FrameTrack::Audio => false,
            _ => {
                self.counts.other_track += 1;
                return;
            }
        };
        let Some(capture) = self.timed(&f, video) else {
            return;
        };
        if video && (self.params.check(&f.data).is_err() || !self.rate.note(capture)) {
            self.ended = Some(GuestEnd::Limits);
            return;
        }
        self.map.observe(capture, now);
        let Some(mapped) = self.map.map(capture) else {
            self.counts.before_map += 1;
            if video {
                self.video.mark_lost();
            }
            return;
        };
        let bytes = f.data.len();
        let pushed = if video {
            if !self.shown {
                self.counts.hidden += 1;
                self.video.mark_lost();
                return;
            }
            self.video.push(
                capture,
                mapped,
                f.keyframe,
                f.contiguous,
                bytes,
                f.data,
                now,
            )
        } else {
            let unit = (f.data, f.rtp_time);
            self.audio
                .push(capture, mapped, false, f.contiguous, bytes, unit, now)
        };
        if pushed != Pushed::Held {
            self.counts.jitter += 1;
        }
    }

    /// Step 4: the frame's checked capture time, or `None` for an outlier.
    fn timed(&mut self, f: &ReceivedFrame, video: bool) -> Option<u64> {
        let timing = if video {
            &mut self.video_timing
        } else {
            &mut self.audio_timing
        };
        match timing.check(f.rtp_time, f.capture_nanos) {
            super::Timed::At(t) => Some(t),
            super::Timed::Outlier => {
                self.counts.outliers += 1;
                if timing.refused() {
                    self.ended = Some(GuestEnd::Limits);
                }
                if video {
                    self.video.mark_lost();
                }
                None
            }
        }
    }

    fn release_sound(&mut self, now: Timestamp) {
        while let Some(q) = self.audio.pop_due(now) {
            self.sound_unit(q);
        }
    }

    fn sound_unit(&mut self, q: Queued<SoundUnit>) {
        let (data, rtp) = q.payload;
        let Some(samples) = opus_samples(&data) else {
            self.counts.sound_corrupt += 1;
            return;
        };
        let channels = self.audio_decoder.format().channels.channels() as u16;
        if let Some(end) = self.sound_end
            && rtp > end
        {
            self.conceal(rtp - end, q.release, channels);
        }
        self.sound_end = Some(rtp + u64::from(samples));
        let Ok(packet) = EncodedPacket::new(
            CodecId::Opus,
            BitstreamFormat::Raw,
            q.release,
            q.release,
            true,
            Bytes::from_owner(data),
        ) else {
            self.counts.sound_corrupt += 1;
            return;
        };
        match self.audio_decoder.decode(&packet, &mut self.pcm) {
            Ok(_) => self.sink.sound(q.release, &self.pcm, channels),
            Err(_) => self.counts.sound_corrupt += 1,
        }
    }

    /// Fills a gap of `missing` samples before the sound released at `at`.
    fn conceal(&mut self, missing: u64, at: Timestamp, channels: u16) {
        let max = MAX_CONCEAL.as_nanos() as u64 * OPUS_RATE / 1_000_000_000;
        if missing > max {
            self.counts.sound_restarts += 1;
            self.audio_decoder.reset();
            self.sink.sound_restart();
            return;
        }
        let frames = (missing / u64::from(CONCEAL_STEP)) * u64::from(CONCEAL_STEP);
        if frames == 0 {
            return;
        }
        self.counts.concealed += 1;
        let start = at - Duration::from_nanos(frames * 1_000_000_000 / OPUS_RATE);
        if self
            .audio_decoder
            .conceal(frames as usize, &mut self.pcm)
            .is_ok()
        {
            self.sink.sound(start, &self.pcm, channels);
        }
    }

    fn release_video(&mut self, now: Timestamp) {
        while let Some(q) = self.video.pop_due(now + self.lead) {
            if !self.ensure_decoder(now) {
                self.video.mark_lost();
                continue;
            }
            let started = self.clock.now();
            self.decode(q);
            let took = self.clock.now().saturating_since(started);
            self.lead = (took * 3 / 2).clamp(MIN_LEAD, self.video.target());
        }
    }

    fn ensure_decoder(&mut self, now: Timestamp) -> bool {
        if self.decoder.is_some() {
            return true;
        }
        if self.restart_at.is_some_and(|t| now < t) {
            return false;
        }
        let max = self.params.size().unwrap_or(MAX_GUEST_SIZE);
        let config = VideoDecoderConfig::h264_annex_b(Size::new(
            max.width.max(max.height),
            max.width.max(max.height),
        ));
        match (self.make_video)(&config) {
            Ok(d) => {
                self.decoder = Some(d);
                true
            }
            Err(_) => {
                self.fault(now);
                false
            }
        }
    }

    fn fault(&mut self, now: Timestamp) {
        self.counts.decoder_faults += 1;
        self.decoder = None;
        self.restart_at = Some(now + self.backoff);
        self.backoff = (self.backoff * 2).min(RESTART_BACKOFF.1);
        self.video.restart();
        self.rx.request_keyframe();
    }

    fn decode(&mut self, q: Queued<VideoUnit>) {
        let Ok(packet) = EncodedPacket::new(
            CodecId::H264,
            BitstreamFormat::AnnexB,
            q.release,
            q.release,
            q.keyframe,
            Bytes::from_owner(q.payload),
        ) else {
            return;
        };
        let Some(decoder) = self.decoder.as_mut() else {
            return;
        };
        let mut result = decoder.submit(&packet);
        if result == Err(VideoDecodeError::Busy) {
            let _ = decoder.drain(&mut self.pictures);
            result = decoder.submit(&packet);
        }
        match result {
            Ok(()) => {
                self.backoff = RESTART_BACKOFF.0;
            }
            Err(VideoDecodeError::NeedKeyframe | VideoDecodeError::InvalidInput(_)) => {
                self.video.mark_lost();
            }
            Err(_) => {
                let now = self.clock.now();
                self.fault(now);
                return;
            }
        }
        if decoder.drain(&mut self.pictures).is_err() {
            let now = self.clock.now();
            self.fault(now);
        }
        self.hand_pictures();
    }

    fn hand_pictures(&mut self) {
        let size = self.params.size();
        for frame in self.pictures.drain(..) {
            if size.is_some_and(|s| frame.format().size != s) {
                self.counts.wrong_size += 1;
                self.video.mark_lost();
                continue;
            }
            self.counts.pictures += 1;
            self.sink.picture(frame);
        }
    }

    fn release_hidden(&mut self, now: Timestamp) {
        if let Some(since) = self.hidden_since
            && self.decoder.is_some()
            && now.saturating_since(since) >= HIDDEN_RELEASE
        {
            self.decoder = None;
        }
    }
}

/// Runs `worker` on its own thread `rbs-guest-<n>` until it ends or `stop` is set.
/// Calls `on_end` with the reason (or `None` after a stop) on that thread.
pub fn spawn(
    n: u32,
    mut worker: GuestWorker,
    clock: Arc<dyn Clock>,
    stop: Arc<AtomicBool>,
    on_end: Box<dyn FnOnce(Option<GuestEnd>) + Send>,
) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name(format!("rbs-guest-{n}"))
        .spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let now = clock.now();
                let Some(next) = worker.turn(now) else { break };
                let wait = next.saturating_since(clock.now()).min(MAX_WAIT);
                if !wait.is_zero() {
                    std::thread::sleep(wait);
                }
            }
            on_end(worker.ended());
        })
}
