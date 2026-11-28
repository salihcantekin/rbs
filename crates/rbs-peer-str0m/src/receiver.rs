//! The port's receiving half over [`GuestReceiver`] (spec `guest-source.md`). Kept out
//! of `link.rs`, a hot-path file: this runs on the guest source's worker.

use crate::link::GuestReceiver;
use crate::media::Track;

impl rbs_peer::PeerReceiver for GuestReceiver {
    fn poll(&mut self) -> Option<rbs_peer::ReceivedFrame> {
        let r = GuestReceiver::poll(self)?;
        let track = match r.track {
            Track::Video => rbs_peer::FrameTrack::Video,
            Track::Audio => rbs_peer::FrameTrack::Audio,
        };
        Some(rbs_peer::ReceivedFrame::new(
            track,
            r.data,
            r.rtp_time,
            r.capture_nanos,
            r.keyframe,
            r.contiguous,
        ))
    }

    fn request_keyframe(&self) {
        GuestReceiver::request_keyframe(self);
    }

    fn confirmed(&self) -> bool {
        GuestReceiver::confirmed(self)
    }

    fn ended(&self) -> bool {
        GuestReceiver::ended(self)
    }

    fn dropped(&self) -> rbs_peer::Dropped {
        rbs_peer::Dropped::new(
            GuestReceiver::overflow(self),
            GuestReceiver::overflow_bytes(self),
        )
    }
}
