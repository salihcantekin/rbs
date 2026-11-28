//! Worker tests with fakes of the receiver and both decoders, on a `TestClock`
//! (spec `guest-source.md`, Tests). No socket, no GPU.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rbs_codec::{
    AudioDecoder, CreateError, DecodeError, DecoderStats, VideoDecodeError, VideoDecoder,
    VideoDecoderConfig,
};
use rbs_core::media::{
    AudioFormat, Bytes, ColorSpace, EncodedPacket, PixelFormat, PlaneLayout, Size, SystemFrame,
    VideoFormat, VideoFrame,
};
use rbs_core::time::{Clock, TestClock, Timestamp};
use rbs_peer::{Dropped, FrameTrack, PeerReceiver, ReceivedFrame};

use super::worker::*;

const SPS_1080: &[u8] = &[
    0x67, 0x64, 0x00, 0x28, 0xAC, 0xD9, 0x40, 0x78, 0x02, 0x27, 0xE5, 0xC0, 0x44, 0x00, 0x00, 0x03,
    0x00, 0x04, 0x00, 0x00, 0x03, 0x00, 0xF0, 0x3C, 0x60, 0xC6, 0x58,
];
const PPS: &[u8] = &[0x68, 0xEB, 0xE3, 0xCB, 0x22, 0xC0];
/// The sender's master clock runs 1000 s ahead of this side's.
const SENDER_AHEAD: u64 = 1_000_000_000_000;
/// One-way delay of every frame.
const DELAY: Duration = Duration::from_millis(30);
const TARGET: Duration = Duration::from_millis(150);

#[derive(Default)]
struct Wire {
    frames: VecDeque<(Timestamp, ReceivedFrame)>,
    confirmed: bool,
    ended: bool,
    keyframe_requests: u32,
    dropped_bytes: u64,
}

struct FakeRx {
    wire: Arc<Mutex<Wire>>,
    clock: Arc<TestClock>,
}

impl PeerReceiver for FakeRx {
    fn poll(&mut self) -> Option<ReceivedFrame> {
        let mut w = self.wire.lock().unwrap();
        let now = self.clock.now();
        if w.frames.front().is_some_and(|(at, _)| *at <= now) {
            return w.frames.pop_front().map(|(_, f)| f);
        }
        None
    }
    fn request_keyframe(&self) {
        self.wire.lock().unwrap().keyframe_requests += 1;
    }
    fn confirmed(&self) -> bool {
        self.wire.lock().unwrap().confirmed
    }
    fn ended(&self) -> bool {
        self.wire.lock().unwrap().ended
    }
    fn dropped(&self) -> Dropped {
        Dropped::new(0, self.wire.lock().unwrap().dropped_bytes)
    }
}

#[derive(Default)]
struct Out {
    pictures: Vec<Timestamp>,
    sound: Vec<(Timestamp, usize)>,
    restarts: u32,
}

struct Sink(Arc<Mutex<Out>>);

impl GuestSink for Sink {
    fn picture(&mut self, frame: VideoFrame) {
        self.0.lock().unwrap().pictures.push(frame.timestamp());
    }
    fn sound(&mut self, at: Timestamp, samples: &[f32], channels: u16) {
        let frames = samples.len() / usize::from(channels);
        self.0.lock().unwrap().sound.push((at, frames));
    }
    fn sound_restart(&mut self) {
        self.0.lock().unwrap().restarts += 1;
    }
}

struct FakeVideo {
    pending: Vec<Timestamp>,
    keyed: bool,
    buffer: Bytes,
    made: Arc<AtomicU32>,
}

impl VideoDecoder for FakeVideo {
    fn output_format(&self) -> Option<VideoFormat> {
        None
    }
    fn submit(&mut self, packet: &EncodedPacket) -> Result<(), VideoDecodeError> {
        if !self.keyed && !packet.keyframe() {
            return Err(VideoDecodeError::NeedKeyframe);
        }
        self.keyed = true;
        self.pending.push(packet.pts());
        Ok(())
    }
    fn drain(&mut self, out: &mut Vec<VideoFrame>) -> Result<(), VideoDecodeError> {
        let format = VideoFormat::new(
            Size::new(1920, 1080),
            PixelFormat::Nv12,
            ColorSpace::BT709_LIMITED,
        );
        let planes = [
            PlaneLayout {
                offset: 0,
                stride: 1920,
            },
            PlaneLayout {
                offset: 1920 * 1080,
                stride: 1920,
            },
        ];
        for pts in self.pending.drain(..) {
            let frame = SystemFrame::new(format, self.buffer.clone(), &planes).unwrap();
            out.push(VideoFrame::system(pts, frame, None));
        }
        Ok(())
    }
    fn reset(&mut self) -> Result<(), VideoDecodeError> {
        Ok(())
    }
    fn flush(&mut self, out: &mut Vec<VideoFrame>) -> Result<(), VideoDecodeError> {
        self.drain(out)
    }
    fn stats(&self) -> DecoderStats {
        DecoderStats::default()
    }
}

impl Drop for FakeVideo {
    fn drop(&mut self) {
        self.made.fetch_sub(1, Ordering::Relaxed);
    }
}

struct FakeAudio {
    concealed: Arc<AtomicU32>,
}

impl AudioDecoder for FakeAudio {
    fn format(&self) -> AudioFormat {
        AudioFormat::STEREO_48K
    }
    fn max_frames(&self) -> usize {
        5760
    }
    fn decode(&mut self, _: &EncodedPacket, out: &mut Vec<f32>) -> Result<usize, DecodeError> {
        out.clear();
        out.resize(960 * 2, 0.0);
        Ok(960)
    }
    fn conceal(&mut self, frames: usize, out: &mut Vec<f32>) -> Result<usize, DecodeError> {
        self.concealed.fetch_add(1, Ordering::Relaxed);
        out.clear();
        out.resize(frames * 2, 0.0);
        Ok(frames)
    }
    fn reset(&mut self) {}
}

struct Rig {
    clock: Arc<TestClock>,
    wire: Arc<Mutex<Wire>>,
    out: Arc<Mutex<Out>>,
    live_decoders: Arc<AtomicU32>,
    concealed: Arc<AtomicU32>,
    worker: GuestWorker,
}

impl Rig {
    fn new() -> Self {
        let clock = Arc::new(TestClock::new());
        clock.set(Timestamp::from_nanos(5_000_000_000));
        let wire = Arc::new(Mutex::new(Wire {
            confirmed: true,
            ..Wire::default()
        }));
        let out = Arc::new(Mutex::new(Out::default()));
        let live = Arc::new(AtomicU32::new(0));
        let concealed = Arc::new(AtomicU32::new(0));
        let made = Arc::clone(&live);
        let buffer = Bytes::from(vec![0u8; 1920 * 1080 * 3 / 2]);
        let make: MakeVideoDecoder = Box::new(move |_: &VideoDecoderConfig| {
            made.fetch_add(1, Ordering::Relaxed);
            Ok::<Box<dyn VideoDecoder>, CreateError>(Box::new(FakeVideo {
                pending: Vec::new(),
                keyed: false,
                buffer: buffer.clone(),
                made: Arc::clone(&made),
            }))
        });
        let worker = GuestWorker::new(
            Box::new(FakeRx {
                wire: Arc::clone(&wire),
                clock: Arc::clone(&clock),
            }),
            Box::new(Sink(Arc::clone(&out))),
            Arc::clone(&clock) as Arc<dyn Clock>,
            make,
            Box::new(FakeAudio {
                concealed: Arc::clone(&concealed),
            }),
            WorkerSettings { target: TARGET },
        );
        Self {
            clock,
            wire,
            out,
            live_decoders: live,
            concealed,
            worker,
        }
    }

    /// The sender's capture time of a frame made `at` on this side's clock.
    fn sender(at: Timestamp) -> u64 {
        at.as_nanos() + SENDER_AHEAD
    }

    fn send_video(&self, made: Timestamp, keyframe: bool, data: Option<Vec<u8>>) {
        let capture = Self::sender(made);
        let data = data.unwrap_or_else(|| {
            let mut au = Vec::new();
            if keyframe {
                for n in [SPS_1080, PPS] {
                    au.extend_from_slice(&[0, 0, 0, 1]);
                    au.extend_from_slice(n);
                }
            }
            au.extend_from_slice(&[0, 0, 0, 1, if keyframe { 0x65 } else { 0x41 }, 7, 7]);
            au
        });
        let rtp = capture / 1_000_000_000 * 90_000 + capture % 1_000_000_000 * 90 / 1_000_000;
        let f = ReceivedFrame::new(
            FrameTrack::Video,
            Arc::from(data),
            rtp,
            Some(capture),
            keyframe,
            true,
        );
        self.wire
            .lock()
            .unwrap()
            .frames
            .push_back((made + DELAY, f));
    }

    fn send_sound(&self, made: Timestamp) {
        let capture = Self::sender(made);
        let rtp = capture / 1_000_000_000 * 48_000 + capture % 1_000_000_000 * 48 / 1_000_000;
        // CELT 20 ms, one frame.
        let f = ReceivedFrame::new(
            FrameTrack::Audio,
            Arc::from(vec![31u8 << 3, 1, 2]),
            rtp,
            Some(capture),
            false,
            true,
        );
        self.wire
            .lock()
            .unwrap()
            .frames
            .push_back((made + DELAY, f));
    }

    /// Sends `seconds` of 30 fps video (a keyframe every 2 s and on request) and 50/s sound made from
    /// now on, skipping the sound packets `skip` says, and runs the worker every 5 ms.
    fn run(&mut self, seconds: u64, skip: impl Fn(u64) -> bool) {
        let start = self.clock.now();
        let end = start + Duration::from_secs(seconds);
        let mut v = 0u64;
        let mut a = 0u64;
        let mut next_v = start;
        let mut next_a = start;
        let mut asked = self.wire.lock().unwrap().keyframe_requests;
        while self.clock.now() < end {
            let now = self.clock.now();
            while next_v <= now {
                // The sharer answers a keyframe request with the next frame.
                let requests = self.wire.lock().unwrap().keyframe_requests;
                let key = v.is_multiple_of(60) || requests > asked;
                asked = requests;
                self.send_video(next_v, key, None);
                v += 1;
                next_v = start + Duration::from_nanos(v * 1_000_000_000 / 30);
            }
            while next_a <= now {
                if !skip(a) {
                    self.send_sound(next_a);
                }
                a += 1;
                next_a = start + Duration::from_millis(a * 20);
            }
            if self.worker.turn(now).is_none() {
                return;
            }
            self.clock.advance(Duration::from_millis(5));
        }
    }
}

#[test]
fn pictures_and_sound_leave_at_the_mapped_time_plus_the_target() {
    let mut rig = Rig::new();
    rig.run(4, |_| false);
    assert_eq!(rig.worker.ended(), None);
    let out = rig.out.lock().unwrap();
    assert!(out.pictures.len() > 80, "pictures: {}", out.pictures.len());
    assert!(out.sound.len() > 100, "sound: {}", out.sound.len());
    // A picture made at t on this side's clock is due at t + delay + target (the map
    // follows offset plus the least delay) and leaves for the decoder one decode lead
    // before that: the last one handed on is due about now.
    let last = *out.pictures.last().unwrap();
    let now = rig.clock.now();
    assert!(
        last.as_nanos().abs_diff(now.as_nanos()) < 45_000_000,
        "last picture due {last:?}, now {now:?}"
    );
    // Its timestamp is the made time plus delay plus target, on the 30 fps grid.
    let start = Timestamp::from_nanos(5_000_000_000);
    let since = last.as_nanos() - (start + DELAY + TARGET).as_nanos();
    let step = 1_000_000_000u64 / 30;
    let off = since % step;
    assert!(off.min(step - off) < 1_000_000, "on the grid: {since}");
    // Pictures and sound share one map: the stamps lie on the same grid.
    let (sound_at, frames) = *out.sound.last().unwrap();
    assert_eq!(frames, 960);
    assert!(sound_at.as_nanos().abs_diff(last.as_nanos()) < 60_000_000);
    assert_eq!(rig.worker.counts().outliers, 0);
}

#[test]
fn nothing_is_published_while_this_side_is_not_confirmed() {
    let mut rig = Rig::new();
    rig.wire.lock().unwrap().confirmed = false;
    rig.run(3, |_| false);
    let out = rig.out.lock().unwrap();
    assert!(out.pictures.is_empty() && out.sound.is_empty());
    assert!(rig.worker.counts().gated > 100);
    assert_eq!(rig.live_decoders.load(Ordering::Relaxed), 0, "no decoder");
}

#[test]
fn a_refused_sps_ends_the_pairing_with_guest_limits() {
    let mut rig = Rig::new();
    let mut ext = SPS_1080.to_vec();
    ext[1] = 88; // Extended profile.
    let mut au = vec![0, 0, 0, 1];
    au.extend_from_slice(&ext);
    rig.send_video(rig.clock.now(), true, Some(au));
    rig.clock.advance(DELAY);
    assert_eq!(rig.worker.turn(rig.clock.now()), None);
    assert_eq!(rig.worker.ended(), Some(GuestEnd::Limits));
    assert_eq!(GuestEnd::Limits.reason(), "guest_limits");
}

#[test]
fn a_flood_above_the_rate_limit_ends_the_pairing() {
    let mut rig = Rig::new();
    // 13 MB in one go: more than the 12.5 MB burst.
    rig.wire.lock().unwrap().dropped_bytes = 13_000_000;
    assert_eq!(rig.worker.turn(rig.clock.now()), None);
    assert_eq!(rig.worker.ended(), Some(GuestEnd::Limits));
}

#[test]
fn a_short_sound_gap_is_concealed_and_a_long_one_restarts_the_sound() {
    let mut rig = Rig::new();
    // Packet 100 is lost (20 ms), then packets 150 to 159 (200 ms).
    rig.run(5, |a| a == 100 || (150..160).contains(&a));
    assert_eq!(rig.worker.ended(), None);
    assert_eq!(rig.worker.counts().concealed, 1);
    assert_eq!(rig.concealed.load(Ordering::Relaxed), 1);
    assert_eq!(rig.worker.counts().sound_restarts, 1);
    assert_eq!(rig.out.lock().unwrap().restarts, 1);
}

#[test]
fn a_hidden_source_decodes_nothing_and_asks_for_a_keyframe_when_shown_again() {
    let mut rig = Rig::new();
    rig.run(2, |_| false);
    assert_eq!(rig.live_decoders.load(Ordering::Relaxed), 1);
    rig.worker.set_shown(false, rig.clock.now());
    let before = rig.out.lock().unwrap().pictures.len();
    rig.run(3, |_| false);
    let after = rig.out.lock().unwrap().pictures.len();
    assert!(after - before < 10, "only what was in the buffer");
    assert!(!rig.worker.decoding(), "released after 2 s not shown");
    assert_eq!(rig.live_decoders.load(Ordering::Relaxed), 0);
    assert!(rig.worker.counts().hidden > 60);
    let asked = rig.wire.lock().unwrap().keyframe_requests;
    rig.worker.set_shown(true, rig.clock.now());
    assert_eq!(rig.wire.lock().unwrap().keyframe_requests, asked + 1);
    rig.run(3, |_| false);
    assert!(rig.worker.decoding());
    assert!(rig.out.lock().unwrap().pictures.len() > after + 30);
}

#[test]
fn a_disconnect_ends_the_worker_and_releases_the_decoder() {
    let mut rig = Rig::new();
    rig.run(2, |_| false);
    assert!(rig.worker.decoding());
    rig.wire.lock().unwrap().ended = true;
    assert_eq!(rig.worker.turn(rig.clock.now()), None);
    assert_eq!(rig.worker.ended(), Some(GuestEnd::Disconnected));
    assert!(!rig.worker.decoding());
    assert_eq!(rig.live_decoders.load(Ordering::Relaxed), 0);
}

#[test]
fn the_thread_stops_on_request_and_reports_no_reason() {
    let rig = Rig::new();
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = spawn(
        1,
        rig.worker,
        rig.clock.clone(),
        Arc::clone(&stop),
        Box::new(move |end| {
            let _ = tx.send(end);
        }),
    )
    .unwrap();
    stop.store(true, Ordering::Relaxed);
    // Hang guard only.
    let end = rx.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(end, None);
    handle.join().unwrap();
}
