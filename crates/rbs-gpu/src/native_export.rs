//! Native export with the readback's interface (ADR-0147 decision 5).
//!
//! The video export node drives a [`YuvDownload`](crate::download::YuvDownload) through
//! `submit_at`, `submit_repeat`, `take_ready`, `take_oldest` and `in_flight`; busy,
//! repeats, holds and the end of a stream are built on those calls. [`ExportQueue`]
//! gives a native exporter the same calls: it keeps the exports and repeat entries in
//! the order queued and hands frames out like the readback does, so the node's rules
//! keep their meaning in native mode. The queue logic is platform-neutral and tested
//! without a GPU; [`NativeDownload`] (macOS) joins it to the IOSurface exporter.
//!
//! Hot path: no call allocates. The entry ring is sized once.

use std::collections::VecDeque;

use rbs_core::media::VideoFrame;
use rbs_core::time::Timestamp;

use crate::GpuError;
use crate::download::DownloadStats;

/// One queued frame of the export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Entry {
    /// A conversion into the exporter's ring, delivered in submission order.
    Export,
    /// The picture of the entry before it again (render on change, R-03).
    Repeat { timestamp: Timestamp, sequence: u64 },
}

/// The order of a native export's frames and its counters.
#[derive(Debug)]
pub struct ExportQueue {
    entries: VecDeque<Entry>,
    capacity: usize,
    /// The frame delivered last, which a repeat delivers again (a second lease on the
    /// same surface, no copy).
    last: Option<VideoFrame>,
    stats: DownloadStats,
}

impl ExportQueue {
    /// A queue of at most `capacity` entries (exports and repeats in flight). Not
    /// real-time: allocates the ring.
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            entries: VecDeque::with_capacity(capacity),
            capacity,
            last: None,
            stats: DownloadStats::default(),
        }
    }

    /// Counters, with the readback's meaning (`no_buffer` stays 0).
    pub fn stats(&self) -> DownloadStats {
        self.stats
    }

    /// Entries in flight (not taken yet).
    pub fn in_flight(&self) -> usize {
        self.entries.len()
    }

    /// Whether another entry fits; a full queue counts the refused frame in `busy`.
    pub fn has_room(&mut self) -> bool {
        let room = self.entries.len() < self.capacity;
        if !room {
            self.stats.busy += 1;
        }
        room
    }

    /// Records the result of one export call: `Ok(true)` queued, `Ok(false)` when the
    /// exporter's ring was full (counted in `busy`, the canvas is converted again on the
    /// next tick).
    pub fn exported(&mut self, result: Result<(), GpuError>) -> Result<bool, GpuError> {
        match result {
            Ok(()) => {
                self.entries.push_back(Entry::Export);
                self.stats.converted += 1;
                Ok(true)
            }
            Err(GpuError::ExportBusy { .. }) => {
                self.stats.busy += 1;
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }

    /// Queues the picture before it again, stamped `timestamp` and `sequence`. It takes
    /// no surface of the ring. `false` when the queue is full (counted in `busy`).
    pub fn repeat(&mut self, timestamp: Timestamp, sequence: u64) -> bool {
        if !self.has_room() {
            return false;
        }
        self.entries.push_back(Entry::Repeat {
            timestamp,
            sequence,
        });
        true
    }

    /// Forgets the picture to repeat (after a hold, so the program is converted again).
    pub fn forget_last(&mut self) {
        self.last = None;
    }

    /// The next frame to hand out. `ready` returns the oldest completed export, if any
    /// (frames complete in submission order). With `newest`, every completed export but
    /// the newest is dropped (`stale`); otherwise one frame is taken per call (the end
    /// of a stream, ADR-0045). A repeat is one frame of a constant frame rate: at most
    /// one per call, and one behind a completed export waits for the next call.
    pub fn take(
        &mut self,
        newest: bool,
        mut ready: impl FnMut() -> Result<Option<VideoFrame>, GpuError>,
    ) -> Result<Option<VideoFrame>, GpuError> {
        let mut out: Option<VideoFrame> = None;
        while let Some(&entry) = self.entries.front() {
            match entry {
                Entry::Export => {
                    let frame = match ready() {
                        Ok(Some(frame)) => frame,
                        Ok(None) => break,
                        Err(e) => {
                            // A lost device delivers nothing converted on it.
                            self.entries.retain(|e| *e != Entry::Export);
                            self.last = None;
                            self.stats.failed += 1;
                            return Err(e);
                        }
                    };
                    self.entries.pop_front();
                    if out.replace(frame).is_some() {
                        self.stats.stale += 1;
                    }
                    if !newest {
                        break;
                    }
                }
                Entry::Repeat {
                    timestamp,
                    sequence,
                } => {
                    if out.is_some() {
                        break;
                    }
                    self.entries.pop_front();
                    let again = self.last.as_ref().and_then(VideoFrame::share);
                    match again {
                        Some(frame) => {
                            self.stats.repeated += 1;
                            self.stats.delivered += 1;
                            return Ok(Some(
                                frame.with_timestamp(timestamp).with_sequence(sequence),
                            ));
                        }
                        None => {
                            // No picture before this repeat (its export failed).
                            self.stats.failed += 1;
                            return Ok(None);
                        }
                    }
                }
            }
        }
        if let Some(frame) = &out {
            self.last = frame.share();
            self.stats.delivered += 1;
        }
        Ok(out)
    }
}

/// The IOSurface exporter behind the readback's interface (macOS, ADR-0147).
#[cfg(target_vendor = "apple")]
pub struct NativeDownload {
    exporter: crate::export::SurfaceExporter,
    queue: ExportQueue,
}

#[cfg(target_vendor = "apple")]
impl NativeDownload {
    /// Wraps `exporter`; `entries` bounds exports and repeats in flight.
    pub fn new(exporter: crate::export::SurfaceExporter, entries: usize) -> Self {
        Self {
            exporter,
            queue: ExportQueue::new(entries),
        }
    }

    /// The exporter, for its format, kernel, black surface and lease owner.
    pub fn exporter(&mut self) -> &mut crate::export::SurfaceExporter {
        &mut self.exporter
    }

    /// The scale kernel of the conversion.
    pub fn kernel(&self) -> crate::ScaleKernel {
        self.exporter.kernel()
    }

    /// Counters, with the readback's meaning.
    pub fn stats(&self) -> DownloadStats {
        self.queue.stats()
    }

    /// Exports and repeats in flight.
    pub fn in_flight(&self) -> usize {
        self.queue.in_flight()
    }

    /// Converts `canvas` into a free surface, stamped `timestamp` and `sequence`.
    /// `Ok(false)` when no surface or entry is free (counted in `busy`). Hot path.
    pub fn submit_at(
        &mut self,
        ctx: &crate::GpuContext,
        pool: &crate::TexturePool,
        canvas: &VideoFrame,
        timestamp: Timestamp,
        sequence: u64,
    ) -> Result<bool, GpuError> {
        let rbs_core::media::FramePayload::Texture(id) = *canvas.payload() else {
            return Err(GpuError::Unsupported("export needs a pool texture frame"));
        };
        if !self.queue.has_room() {
            return Ok(false);
        }
        let result = self.exporter.export(ctx, pool, id, timestamp, sequence);
        self.queue.exported(result)
    }

    /// [`Self::submit_at`] with the canvas's own stamp.
    pub fn submit(
        &mut self,
        ctx: &crate::GpuContext,
        pool: &crate::TexturePool,
        canvas: &VideoFrame,
    ) -> Result<bool, GpuError> {
        self.submit_at(ctx, pool, canvas, canvas.timestamp(), canvas.sequence())
    }

    /// Queues the picture before it again. No GPU work, no surface. Hot path.
    pub fn submit_repeat(&mut self, timestamp: Timestamp, sequence: u64) -> bool {
        self.queue.repeat(timestamp, sequence)
    }

    /// The newest completed frame; older completed ones are dropped (`stale`).
    pub fn take_ready(&mut self, ctx: &crate::GpuContext) -> Result<Option<VideoFrame>, GpuError> {
        let exporter = &mut self.exporter;
        self.queue.take(true, || exporter.take_ready(ctx))
    }

    /// The oldest completed frame, one per call (end of a stream, ADR-0045).
    pub fn take_oldest(&mut self, ctx: &crate::GpuContext) -> Result<Option<VideoFrame>, GpuError> {
        let exporter = &mut self.exporter;
        self.queue.take(false, || exporter.take_oldest(ctx))
    }

    /// Forgets the picture to repeat.
    pub fn forget_last(&mut self) {
        self.queue.forget_last();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rbs_core::media::{
        Bytes, ColorSpace, PixelFormat, PlaneLayout, Size, SystemFrame, VideoFormat,
    };

    fn frame(t: u64) -> VideoFrame {
        let format = VideoFormat::new(
            Size::new(2, 2),
            PixelFormat::Nv12,
            ColorSpace::BT709_LIMITED,
        );
        let planes = [
            PlaneLayout {
                offset: 0,
                stride: 2,
            },
            PlaneLayout {
                offset: 4,
                stride: 2,
            },
        ];
        let system = SystemFrame::new(format, Bytes::from_static(&[0; 6]), &planes);
        match system {
            Ok(system) => VideoFrame::system(Timestamp::from_nanos(t), system, None),
            Err(e) => panic!("test frame: {e:?}"),
        }
    }

    #[test]
    fn exports_come_out_in_order_and_the_newest_wins() {
        let mut q = ExportQueue::new(8);
        for _ in 0..3 {
            assert_eq!(q.exported(Ok(())), Ok(true));
        }
        let mut done = vec![frame(1), frame(2)].into_iter();
        let got = q.take(true, || Ok(done.next()));
        assert_eq!(
            got.ok().flatten().map(|f| f.timestamp().as_nanos()),
            Some(2)
        );
        assert_eq!(q.stats().stale, 1);
        assert_eq!(q.in_flight(), 1);
    }

    #[test]
    fn the_end_takes_one_at_a_time() {
        let mut q = ExportQueue::new(8);
        for _ in 0..2 {
            assert_eq!(q.exported(Ok(())), Ok(true));
        }
        let mut done = vec![frame(1), frame(2)].into_iter();
        let first = q.take(false, || Ok(done.next()));
        assert_eq!(
            first.ok().flatten().map(|f| f.timestamp().as_nanos()),
            Some(1)
        );
        let second = q.take(false, || Ok(done.next()));
        assert_eq!(
            second.ok().flatten().map(|f| f.timestamp().as_nanos()),
            Some(2)
        );
        assert_eq!(q.stats().stale, 0);
        assert_eq!(q.in_flight(), 0);
    }

    #[test]
    fn a_repeat_waits_behind_its_export_and_resends_the_picture() {
        let mut q = ExportQueue::new(8);
        assert_eq!(q.exported(Ok(())), Ok(true));
        assert!(q.repeat(Timestamp::from_nanos(20), 7));
        // The export is not complete yet: nothing, and the repeat stays queued.
        assert!(matches!(q.take(true, || Ok(None)), Ok(None)));
        assert_eq!(q.in_flight(), 2);
        let mut done = vec![frame(10)].into_iter();
        let first = q.take(true, || Ok(done.next()));
        assert_eq!(
            first.ok().flatten().map(|f| f.timestamp().as_nanos()),
            Some(10)
        );
        let again = q.take(true, || Ok(None)).ok().flatten();
        assert_eq!(again.as_ref().map(|f| f.timestamp().as_nanos()), Some(20));
        assert_eq!(again.map(|f| f.sequence()), Some(7));
        assert_eq!(q.stats().repeated, 1);
        assert_eq!(q.stats().delivered, 2);
    }

    #[test]
    fn a_busy_ring_is_counted_and_a_full_queue_refuses() {
        let mut q = ExportQueue::new(1);
        assert_eq!(
            q.exported(Err(GpuError::ExportBusy { surfaces: 4 })),
            Ok(false)
        );
        assert_eq!(q.stats().busy, 1);
        assert_eq!(q.exported(Ok(())), Ok(true));
        assert!(!q.repeat(Timestamp::from_nanos(1), 1));
        assert_eq!(q.stats().busy, 2);
    }

    #[test]
    fn a_lost_device_drops_exports_and_the_picture_to_repeat() {
        let mut q = ExportQueue::new(8);
        assert_eq!(q.exported(Ok(())), Ok(true));
        assert!(q.repeat(Timestamp::from_nanos(5), 1));
        assert!(q.take(true, || Err(GpuError::DeviceLost)).is_err());
        // The repeat behind it has no picture: counted, nothing sent.
        assert!(matches!(q.take(true, || Ok(None)), Ok(None)));
        assert_eq!(q.stats().failed, 2);
        assert_eq!(q.in_flight(), 0);
    }
}
