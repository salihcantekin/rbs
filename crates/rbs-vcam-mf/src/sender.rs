//! The sender thread's work, platform-neutral: map the camera's ring when the source
//! has created it, and copy each frame the session hands over into it (ADR-0066
//! decision 9, ADR-0070 decision 4). The clock and the mapping are passed in, so tests
//! drive it without Windows.

use std::sync::Arc;

use rbs_core::media::{FramePayload, VideoFrame};
use rbs_vcam::VcamCounters;
use rbs_vcam_ring::{Layout, Region, WriteOutcome, Writer, header_info};

/// How often the sender looks for the ring while it is missing, and how often it
/// renews its heartbeat with no frames (milliseconds).
pub const RETRY_MS: u64 = 250;

/// Something that keeps a mapping alive while its [`Region`] is used.
pub type Keep = Box<dyn Send>;

/// Maps the ring the media source created, if it exists now.
pub trait RingSource: Send {
    /// The mapped section and what keeps it mapped. `None` while no app reads the
    /// camera (the source creates the section when a stream starts).
    fn map(&mut self) -> Option<(Region, Keep)>;
}

/// What happened to one frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sent {
    /// Written into the ring.
    Written,
    /// No ring: no app reads the camera.
    Absent,
    /// The ring announces another format (a format change waits for apps to close the
    /// camera).
    FormatPending,
    /// Every slot was held.
    Busy,
    /// Not a tightly convertible system-memory NV12 frame of the session's size.
    Refused,
}

/// A mapped ring. Field order is drop order: the writer before the mapping.
struct Mapped {
    writer: Option<Writer>,
    region: Region,
    _keep: Keep,
}

/// The sender's state.
pub struct RingSender<S> {
    source: S,
    layout: Layout,
    counters: Arc<VcamCounters>,
    mapped: Option<Mapped>,
    last_look_ms: Option<u64>,
    last_beat_ms: u64,
    /// One frame, packed to the ring's stride: allocated once here.
    scratch: Vec<u8>,
}

impl<S: RingSource> RingSender<S> {
    /// A sender for frames of `layout` (the session's format).
    pub fn new(source: S, layout: Layout, counters: Arc<VcamCounters>) -> Self {
        Self {
            source,
            scratch: vec![0; layout.frame_len()],
            layout,
            counters,
            mapped: None,
            last_look_ms: None,
            last_beat_ms: 0,
        }
    }

    /// Whether the ring is mapped now.
    pub fn is_mapped(&self) -> bool {
        self.mapped.is_some()
    }

    /// Looks for the ring if it is missing and the last look was [`RETRY_MS`] ago.
    fn look(&mut self, now_ms: u64) {
        if self.mapped.is_some()
            || self
                .last_look_ms
                .is_some_and(|t| now_ms.saturating_sub(t) < RETRY_MS)
        {
            return;
        }
        self.last_look_ms = Some(now_ms);
        if let Some((region, keep)) = self.source.map() {
            self.mapped = Some(Mapped {
                writer: None,
                region,
                _keep: keep,
            });
        }
    }

    /// A tick without a frame: keeps the writer's heartbeat fresh and looks for the
    /// ring. Call at least every [`RETRY_MS`].
    pub fn idle(&mut self, now_ms: u64) {
        self.look(now_ms);
        if now_ms.saturating_sub(self.last_beat_ms) >= RETRY_MS
            && let Some(w) = self.mapped.as_ref().and_then(|m| m.writer.as_ref())
        {
            w.heartbeat(now_ms);
            self.last_beat_ms = now_ms;
        }
    }

    /// Sends one frame and counts the outcome.
    pub fn send(&mut self, frame: &VideoFrame, now_ms: u64) -> Sent {
        let sent = self.write(frame, now_ms);
        let c = &self.counters;
        match sent {
            Sent::Written => c.count_sent(),
            Sent::Absent => c.count_consumer_absent(),
            Sent::FormatPending => c.count_format_pending(),
            Sent::Busy => c.count_consumer_busy(),
            // The session's hand-off already refused (and counted) frames of another
            // size, format or memory kind, so this is not counted twice.
            Sent::Refused => {}
        }
        sent
    }

    fn write(&mut self, frame: &VideoFrame, now_ms: u64) -> Sent {
        if !pack(frame, &self.layout, &mut self.scratch) {
            return Sent::Refused;
        }
        self.look(now_ms);
        let Some(m) = self.mapped.as_mut() else {
            return Sent::Absent;
        };
        let Some(info) = header_info(&m.region) else {
            // The source has not initialised it yet, or another version: try again.
            return Sent::Absent;
        };
        if (info.width, info.height) != (self.layout.width(), self.layout.height()) {
            m.writer = None;
            return Sent::FormatPending;
        }
        if m.writer.is_none() {
            m.writer = Writer::attach(m.region.clone(), &self.layout).ok();
        }
        let Some(w) = m.writer.as_mut() else {
            return Sent::Absent;
        };
        let ts = i64::try_from(frame.timestamp().as_nanos()).unwrap_or(i64::MAX);
        let outcome = w.write(now_ms, ts, &self.scratch);
        self.last_beat_ms = now_ms;
        let absent = !info.reader_active(now_ms);
        match outcome {
            Ok(WriteOutcome::Written { .. }) if absent => Sent::Absent,
            Ok(WriteOutcome::Written { .. }) => Sent::Written,
            Ok(WriteOutcome::Busy) => Sent::Busy,
            Ok(WriteOutcome::Reset) => {
                // The source started over: attach again with the next frame.
                m.writer = None;
                Sent::Absent
            }
            Err(_) => Sent::Refused,
        }
    }
}

/// Packs a system-memory NV12 `frame` of `layout`'s size into `out` at the layout's
/// stride. `false` for anything else.
pub fn pack(frame: &VideoFrame, layout: &Layout, out: &mut [u8]) -> bool {
    let FramePayload::System(sys) = frame.payload() else {
        return false;
    };
    let size = frame.format().size;
    let (w, h, stride) = (
        layout.width() as usize,
        layout.height() as usize,
        layout.stride() as usize,
    );
    if (size.width as usize, size.height as usize) != (w, h) || out.len() != layout.frame_len() {
        return false;
    }
    let (Some((y, ys)), Some((uv, uvs))) = (sys.plane(0), sys.plane(1)) else {
        return false;
    };
    let rows = (0..h)
        .map(|r| (y, ys, r))
        .chain((0..h / 2).map(|r| (uv, uvs, r)));
    for (i, (plane, plane_stride, r)) in rows.enumerate() {
        let (Some(src), Some(dst)) = (
            plane.get(r * plane_stride..r * plane_stride + w),
            out.get_mut(i * stride..i * stride + w),
        ) else {
            return false;
        };
        dst.copy_from_slice(src);
    }
    true
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use bytes::Bytes;
    use rbs_core::media::{
        ColorSpace, PixelFormat, PlaneLayout, Size, SystemFrame, VideoFormat, VideoFrame,
    };
    use rbs_core::time::Timestamp;
    use rbs_vcam_ring::{Reader, initialize};

    use super::*;

    const W: u32 = 16;
    const H: u32 = 8;

    /// A frame with a padded stride whose bytes encode `seq`.
    fn frame(seq: u64, w: u32, h: u32) -> VideoFrame {
        let format = VideoFormat::new(
            Size {
                width: w,
                height: h,
            },
            PixelFormat::Nv12,
            ColorSpace::BT709_LIMITED,
        );
        let stride = w as usize + 8;
        let rows = h as usize * 3 / 2;
        let mut data = vec![0xEEu8; stride * rows];
        for r in 0..rows {
            data[r * stride..r * stride + w as usize].fill(seq as u8 + r as u8);
        }
        let planes = [
            PlaneLayout { offset: 0, stride },
            PlaneLayout {
                offset: stride * h as usize,
                stride,
            },
        ];
        let f = SystemFrame::new(format, Bytes::from(data), &planes).unwrap();
        VideoFrame::system(Timestamp::from_nanos(seq), f, None).with_sequence(seq)
    }

    /// A source whose ring "exists" once `created` is set.
    struct Fake {
        region: Region,
        created: Arc<std::sync::atomic::AtomicBool>,
        maps: Arc<std::sync::atomic::AtomicU64>,
    }

    impl RingSource for Fake {
        fn map(&mut self) -> Option<(Region, Keep)> {
            use std::sync::atomic::Ordering;
            self.maps.fetch_add(1, Ordering::Relaxed);
            self.created
                .load(Ordering::Acquire)
                .then(|| (self.region.clone(), Box::new(()) as Keep))
        }
    }

    fn setup() -> (
        RingSender<Fake>,
        Region,
        Layout,
        Arc<std::sync::atomic::AtomicBool>,
        Arc<std::sync::atomic::AtomicU64>,
        Arc<VcamCounters>,
    ) {
        let layout = Layout::nv12(W, H, W).unwrap();
        let region = Region::owned(layout.total_len());
        let created = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let maps = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let counters = Arc::new(VcamCounters::default());
        let fake = Fake {
            region: region.clone(),
            created: Arc::clone(&created),
            maps: Arc::clone(&maps),
        };
        let sender = RingSender::new(fake, layout, Arc::clone(&counters));
        (sender, region, layout, created, maps, counters)
    }

    #[test]
    fn frames_reach_the_reader_once_the_source_creates_the_ring() {
        use std::sync::atomic::Ordering;
        let (mut s, region, layout, created, maps, counters) = setup();
        assert_eq!(s.send(&frame(1, W, H), 1_000), Sent::Absent);
        // Looked once; the next look waits RETRY_MS.
        assert_eq!(s.send(&frame(2, W, H), 1_100), Sent::Absent);
        assert_eq!(maps.load(Ordering::Relaxed), 1);

        initialize(&region, &layout, 7).unwrap();
        created.store(true, Ordering::Release);
        let mut reader = Reader::new(region.clone(), layout).unwrap();
        reader.heartbeat(1_250);
        assert_eq!(s.send(&frame(3, W, H), 1_250), Sent::Written);
        let mut out = vec![0u8; layout.frame_len()];
        let info = reader.read_newest(1_260, &mut out).unwrap().unwrap();
        assert_eq!(info.timestamp_ns, 3);
        // Packed: the padding is gone, each row carries its own value.
        for r in 0..(H * 3 / 2) as usize {
            assert!(
                out[r * W as usize..(r + 1) * W as usize]
                    .iter()
                    .all(|&b| b == 3 + r as u8)
            );
        }
        let st = counters.snapshot();
        assert_eq!((st.sent(), st.consumer_absent()), (1, 2));
    }

    #[test]
    fn another_announced_format_counts_as_pending_and_a_reset_reattaches() {
        use std::sync::atomic::Ordering;
        let (mut s, region, layout, created, _, counters) = setup();
        let other = Layout::nv12(W * 2, H, W * 2).unwrap();
        let big = Region::owned(other.total_len().max(layout.total_len()));
        initialize(&big, &other, 1).unwrap();
        // A sender over a ring announcing another size.
        let fake = Fake {
            region: big.clone(),
            created: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            maps: Arc::default(),
        };
        let mut p = RingSender::new(fake, layout, Arc::clone(&counters));
        assert_eq!(p.send(&frame(1, W, H), 0), Sent::FormatPending);
        assert_eq!(counters.snapshot().format_pending(), 1);

        initialize(&region, &layout, 1).unwrap();
        created.store(true, Ordering::Release);
        let reader = Reader::new(region.clone(), layout).unwrap();
        reader.heartbeat(10);
        assert_eq!(s.send(&frame(2, W, H), 10), Sent::Written);
        // The source starts over (new generation): one frame is lost, then writing resumes.
        initialize(&region, &layout, 3).unwrap();
        reader.heartbeat(20);
        assert_eq!(s.send(&frame(3, W, H), 20), Sent::Absent);
        assert_eq!(s.send(&frame(4, W, H), 30), Sent::Written);
    }

    #[test]
    fn frames_of_another_size_are_refused() {
        let (mut s, ..) = setup();
        assert_eq!(s.send(&frame(1, W * 2, H), 0), Sent::Refused);
    }

    #[test]
    fn idle_ticks_look_for_the_ring_and_keep_the_heartbeat() {
        use std::sync::atomic::Ordering;
        let (mut s, region, layout, created, maps, _) = setup();
        s.idle(0);
        s.idle(100);
        assert_eq!(maps.load(Ordering::Relaxed), 1);
        s.idle(RETRY_MS);
        assert_eq!(maps.load(Ordering::Relaxed), 2);
        initialize(&region, &layout, 1).unwrap();
        created.store(true, Ordering::Release);
        s.idle(2 * RETRY_MS);
        assert!(s.is_mapped());
        let reader = Reader::new(region.clone(), layout).unwrap();
        reader.heartbeat(2 * RETRY_MS);
        assert_eq!(s.send(&frame(1, W, H), 2 * RETRY_MS), Sent::Written);
        s.idle(4 * RETRY_MS);
        // The writer's heartbeat is fresh: the reader sees no "writer gone".
        assert!(reader.status(4 * RETRY_MS + 500).is_ok());
    }
}
