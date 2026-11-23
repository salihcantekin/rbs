//! The Region preview of a source whose adapter has no `region_grab` (a camera): one
//! frame of the running source from the studio (ADR-0176). Started on the control
//! thread when the adapter's grab answered `Unsupported` and the section is still open;
//! at most one at a time; removed when the frame arrives, when it cannot come (the
//! source has no node, a fault, a restart), after [`GRAB_WAIT`], and when the section
//! closes or opens again. Nothing exists while no Region section waits (P13).

use std::sync::Arc;
use std::sync::atomic::Ordering as AtomicOrdering;
use std::time::Duration;

use rbs_core::media::{FramePayload, VideoFrame};
use rbs_core::time::Timestamp;
use rbs_record::{GrabSlot, PictureId};
use rbs_scene::model::SourceId;

use super::{Host, Update};

/// How long a studio grab may wait for its source's first frame, a picture size, or
/// a source that does not run yet, before the section is told no picture comes.
pub(super) const GRAB_WAIT: Duration = Duration::from_secs(3);

/// How often the control thread looks again while a grab waits. The grab node also
/// wakes it when the frame is there.
pub(super) const GRAB_POLL: Duration = Duration::from_millis(100);

/// The studio grab of an open Region section.
pub(super) struct StudioGrab {
    generation: u64,
    id: SourceId,
    /// The grab in the studio, once added, and where its frame goes.
    added: Option<(PictureId, Arc<GrabSlot>)>,
    until: Timestamp,
}

/// The RGBA rows of a grab node's frame as a Region picture.
fn region_picture(frame: &VideoFrame) -> Option<rbs_capture::RegionGrab> {
    let FramePayload::System(system) = frame.payload() else {
        return None;
    };
    let size = frame.format().size;
    let len = usize::try_from(u64::from(size.width) * u64::from(size.height) * 4).ok()?;
    let rgba = system.data().get(..len)?.to_vec();
    rbs_capture::RegionGrab::new(size.width, size.height, rgba)
}

impl Host {
    /// Starts the studio grab of source `id` for Region request `generation`, in place
    /// of one still waiting. Driven by [`Self::drive_studio_grab`].
    pub(super) fn start_studio_grab(&mut self, generation: u64, id: SourceId) {
        self.end_studio_grab();
        self.pickers.studio_grab = Some(StudioGrab {
            generation,
            id,
            added: None,
            until: self.clock.now() + GRAB_WAIT,
        });
    }

    /// After a studio restart: the grab belonged to the old studio (its picture id may
    /// name another picture in the new one), so it is forgotten without a removal and
    /// asked again, with a full [`GRAB_WAIT`] on the new clock.
    pub(super) fn restart_studio_grab(&mut self) {
        let until = self.clock.now() + GRAB_WAIT;
        if let Some(grab) = &mut self.pickers.studio_grab {
            grab.added = None;
            grab.until = until;
        }
    }

    /// Removes the studio grab, if any, without telling the section.
    pub(super) fn end_studio_grab(&mut self) {
        let Some(grab) = self.pickers.studio_grab.take() else {
            return;
        };
        if let Some((picture, _)) = grab.added {
            let col = self.run_collection();
            let running = col.as_ref().unwrap_or(&self.collection);
            let _ = self
                .studio
                .remove_picture(running, &self.adapters.sources, picture);
            self.waiting = self.studio.waiting();
        }
    }

    /// One look at the studio grab: adds it once the source runs, sends its frame, or
    /// gives up. Returns when to look again while it waits.
    pub(super) fn drive_studio_grab(&mut self, notify: &dyn Fn(Update)) -> Option<Duration> {
        let grab = self.pickers.studio_grab.as_ref()?;
        let (generation, id, until) = (grab.generation, grab.id, grab.until);
        if generation != self.pickers.region_grabs.load(AtomicOrdering::Acquire) {
            self.end_studio_grab();
            return None;
        }
        let late = self.clock.now() >= until;
        let picture = match &grab.added {
            None => {
                let col = self.run_collection();
                let running = col.as_ref().unwrap_or(&self.collection);
                let added = self.studio.add_source_grab(
                    running,
                    &self.adapters.sources,
                    id,
                    rbs_capture::REGION_GRAB_LONG,
                );
                self.waiting = self.studio.waiting();
                match added {
                    Ok(added) => {
                        if let Some(grab) = &mut self.pickers.studio_grab {
                            grab.added = Some(added);
                        }
                        return Some(GRAB_POLL);
                    }
                    // Not running yet (no node or no picture size): try again until late.
                    Err(_) if !late => return Some(GRAB_POLL),
                    Err(_) => None,
                }
            }
            Some((picture, slot)) => {
                if let Some(frame) = slot.take() {
                    region_picture(&frame)
                } else {
                    let gone = slot.failed()
                        || self.studio.picture_failure(*picture).is_some()
                        || !self.studio.pictures().contains(picture);
                    if !gone && !late {
                        return Some(GRAB_POLL);
                    }
                    None
                }
            }
        };
        self.end_studio_grab();
        notify(Update::RegionPreview {
            id,
            generation,
            picture,
        });
        None
    }
}

#[cfg(test)]
mod tests {
    use super::region_picture;
    use rbs_core::media::{
        Bytes, ColorSpace, PixelFormat, PlaneLayout, Size, SystemFrame, VideoFormat, VideoFrame,
    };
    use rbs_core::time::Timestamp;

    fn frame(w: u32, h: u32, bytes: usize) -> VideoFrame {
        let format = VideoFormat::new(Size::new(w, h), PixelFormat::Rgba8, ColorSpace::SRGB);
        let planes = [PlaneLayout {
            offset: 0,
            stride: w as usize * 4,
        }];
        let system = SystemFrame::new(format, Bytes::from(vec![7u8; bytes]), &planes).unwrap();
        VideoFrame::system(Timestamp::ZERO, system, None)
    }

    #[test]
    fn a_grab_frame_becomes_a_region_picture_of_its_size() {
        let picture = region_picture(&frame(4, 2, 32)).unwrap();
        assert_eq!((picture.width(), picture.height()), (4, 2));
        assert_eq!(picture.rgba(), &[7u8; 32][..]);
    }

    #[test]
    fn a_frame_over_the_region_size_is_no_picture() {
        assert!(region_picture(&frame(961, 1, 961 * 4)).is_none());
    }
}
