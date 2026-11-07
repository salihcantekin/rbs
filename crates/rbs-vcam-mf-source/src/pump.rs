//! What the source shows on each sample request (ADR-0066 decision 9): the newest
//! complete frame from the ring, the last one again when no new one came, or the still
//! picture when the writer is gone or the ring disagrees with the announced format.
//!
//! Every buffer is allocated in [`FramePump::new`] (when a stream starts); [`FramePump::next`]
//! allocates nothing.

use rbs_vcam_ring::{Layout, NoFrames, Reader, Region, RingError};

use crate::announced::Announced;
use crate::still;

/// Which picture [`FramePump::next`] returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shown {
    /// A frame RBS sent, new since the last request (its ring sequence number).
    New(u64),
    /// The last frame RBS sent, again.
    Repeated(u64),
    /// The still picture.
    Still,
}

/// The source's view of the ring for one running stream.
#[derive(Debug)]
pub struct FramePump {
    reader: Option<Reader>,
    still: Vec<u8>,
    /// The newest frame read; valid while `last` is `Some`.
    frame: Vec<u8>,
    /// A read in progress (discarded when the writer took its slot back).
    scratch: Vec<u8>,
    last: Option<u64>,
}

/// The ring layout for an announced format: tightly packed NV12 (stride = width). The
/// writer uses the same, from the same announced format (spec, "Shared frame ring").
pub fn layout(format: &Announced) -> Result<Layout, RingError> {
    Layout::nv12(format.width(), format.height(), format.width())
}

impl FramePump {
    /// A pump for `format` reading `region` (none: the section could not be created,
    /// so only the still picture is shown). Not real-time: allocates the frames.
    pub fn new(format: &Announced, region: Option<Region>) -> Self {
        let reader = region.and_then(|r| Reader::new(r, layout(format).ok()?).ok());
        Self {
            reader,
            still: still::picture(format),
            frame: vec![0; format.frame_len()],
            scratch: vec![0; format.frame_len()],
            last: None,
        }
    }

    /// Whether a ring is attached.
    pub fn has_ring(&self) -> bool {
        self.reader.is_some()
    }

    /// The picture to deliver now, tightly packed NV12 of the announced format.
    pub fn next(&mut self, now_ms: u64) -> (&[u8], Shown) {
        let Some(reader) = self.reader.as_mut() else {
            return (&self.still, Shown::Still);
        };
        match reader.read_newest(now_ms, &mut self.scratch) {
            Ok(Some(info)) => {
                std::mem::swap(&mut self.frame, &mut self.scratch);
                self.last = Some(info.seq);
                (&self.frame, Shown::New(info.seq))
            }
            Ok(None) => match self.last {
                Some(seq) => (&self.frame, Shown::Repeated(seq)),
                None => (&self.still, Shown::Still),
            },
            Err(NoFrames::WriterStopped | NoFrames::Header(_)) => {
                self.last = None;
                (&self.still, Shown::Still)
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
mod tests {
    use rbs_vcam_ring::{WriteOutcome, Writer, initialize};

    use super::*;

    fn setup() -> (Announced, Writer, FramePump) {
        let format = Announced::new(640, 360, 30, 1).unwrap();
        let layout = layout(&format).unwrap();
        let region = Region::owned(layout.total_len());
        initialize(&region, &layout, 1).unwrap();
        let writer = Writer::attach(region.clone(), &layout).unwrap();
        let pump = FramePump::new(&format, Some(region));
        (format, writer, pump)
    }

    #[test]
    fn new_frames_then_repeats_then_the_still_picture_once_the_writer_stops() {
        let (format, mut writer, mut pump) = setup();
        assert!(pump.has_ring());
        // Nothing written yet: the writer heartbeat is missing, so the still picture.
        assert_eq!(pump.next(0).1, Shown::Still);

        let pixels = vec![77u8; format.frame_len()];
        let outcome = writer.write(10, 1, &pixels).unwrap();
        assert!(matches!(outcome, WriteOutcome::Written { .. }));
        let (frame, shown) = pump.next(20);
        assert!(matches!(shown, Shown::New(_)), "{shown:?}");
        assert!(frame.iter().all(|&b| b == 77));
        let (frame, shown) = pump.next(30);
        assert!(matches!(shown, Shown::Repeated(_)), "{shown:?}");
        assert!(frame.iter().all(|&b| b == 77));

        // The writer's heartbeat goes stale.
        let (frame, shown) = pump.next(20 + rbs_vcam_ring::STALE_MS + 1);
        assert_eq!(shown, Shown::Still);
        assert_eq!(frame, still::picture(&format).as_slice());
    }

    #[test]
    fn a_ring_for_another_format_shows_the_still_picture() {
        let other = Announced::new(1280, 720, 30, 1).unwrap();
        let other_layout = layout(&other).unwrap();
        let region = Region::owned(other_layout.total_len());
        initialize(&region, &other_layout, 1).unwrap();
        let format = Announced::new(640, 360, 30, 1).unwrap();
        let mut pump = FramePump::new(&format, Some(region));
        assert_eq!(pump.next(0).1, Shown::Still);
    }

    #[test]
    fn without_a_ring_only_the_still_picture() {
        let format = Announced::new(640, 360, 30, 1).unwrap();
        let mut pump = FramePump::new(&format, None);
        assert!(!pump.has_ring());
        assert_eq!(pump.next(0).1, Shown::Still);
    }
}
