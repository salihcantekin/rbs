//! Guest shares on the host's studio (ADR-0139 decisions 9, 10 and 13): the real
//! slots, outputs and program check behind [`guest_share::ShareStudio`], and the
//! timer in the control loop (none while no share runs, P13).

use std::sync::Arc;
use std::time::Duration;

use rbs_codec::RateControl;
use rbs_core::media::CodecId;
use rbs_core::time::Timestamp;
use rbs_peer::PeerFactory;
use rbs_record::{
    AudioSettings, OutputId, OutputSettings, OwnChain, RecordError, SlotReservation, Studio,
    VideoSettings,
};
use rbs_scene::SceneCollection;
use rbs_scene::model::CanvasId;

use super::Host;
use super::guest_share::{
    FLOOR_BPS, GuestShares, ShareRefusal, ShareRequest, ShareStatus, ShareStudio, ShareVideo,
};

/// The guest source's type id (ADR-0139 decision 12).
pub const GUEST_SOURCE_TYPE: &str = "org.rbs.source.guest";
/// A share's sound: Opus at 128 kbit/s (ADR-0139 decision 9).
const SHARE_AUDIO_BPS: u64 = 128_000;

/// The pairing adapter a composition root hands the host ([`HostConfig::peers`]).
///
/// [`HostConfig::peers`]: super::HostConfig::peers
#[derive(Clone)]
pub struct PeerFactoryHandle(pub Arc<dyn PeerFactory>);

impl std::fmt::Debug for PeerFactoryHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("PeerFactoryHandle")
            .field(&self.0.type_id())
            .finish()
    }
}

/// The host's guest shares and the studio ids behind their numbers.
pub(super) struct GuestHost {
    pub(super) shares: GuestShares,
    reservations: Vec<(u64, SlotReservation)>,
    outputs: Vec<(u64, OutputId)>,
    next: u64,
    /// When the shares are looked at next; `None` while none runs.
    pub(super) due: Option<Timestamp>,
    /// The status last published, for `guest.changed`.
    pub(super) published: Vec<ShareStatus>,
}

impl GuestHost {
    pub(super) fn new(peers: Option<Arc<dyn PeerFactory>>) -> Self {
        Self {
            shares: GuestShares::new(peers),
            reservations: Vec::new(),
            outputs: Vec::new(),
            next: 1,
            due: None,
            published: Vec::new(),
        }
    }
}

/// The studio as a share sees it: the main canvas.
struct Slots<'a> {
    studio: &'a mut Studio,
    adapters: &'a rbs_record::RecordAdapters,
    collection: &'a SceneCollection,
    canvas: CanvasId,
    reservations: &'a mut Vec<(u64, SlotReservation)>,
    outputs: &'a mut Vec<(u64, OutputId)>,
    next: &'a mut u64,
}

impl Slots<'_> {
    fn number(&mut self) -> u64 {
        let n = *self.next;
        *self.next += 1;
        n
    }
}

/// The share's output settings: its own low-latency H.264 chain at `video`'s size,
/// CBR, no B-frames, a keyframe every 2 s, and Opus sound (ADR-0139 decision 9).
pub(super) fn share_output(url: &str, video: ShareVideo) -> OutputSettings {
    let mut out = OutputSettings::packets(url);
    let mut v = VideoSettings::default();
    v.codec = CodecId::H264;
    v.rate_control = RateControl::Cbr {
        bitrate_bps: video.bitrate_bps,
    };
    v.max_b_frames = 0;
    out.video = Some(v);
    let mut a = AudioSettings::default();
    a.codec = CodecId::Opus;
    a.bitrate_bps = SHARE_AUDIO_BPS;
    out.audio = Some(a);
    out.size = Some((video.width, video.height));
    out.own_chain = Some(OwnChain::new(FLOOR_BPS, video.bitrate_bps));
    out
}

impl ShareStudio for Slots<'_> {
    fn reserve(&mut self) -> Result<u64, ShareRefusal> {
        let reservation = self
            .studio
            .reserve_slot(Some(self.canvas))
            .map_err(slot_refusal)?;
        let n = self.number();
        self.reservations.push((n, reservation));
        Ok(n)
    }

    fn release(&mut self, reservation: u64) {
        if let Some(at) = self
            .reservations
            .iter()
            .position(|(n, _)| *n == reservation)
        {
            let (_, r) = self.reservations.remove(at);
            self.studio.release_slot(r);
        }
    }

    fn start(&mut self, url: &str, reservation: u64, video: ShareVideo) -> Result<u64, String> {
        let at = self
            .reservations
            .iter()
            .position(|(n, _)| *n == reservation)
            .ok_or_else(|| "the share's slot is gone".to_owned())?;
        let mut out = share_output(url, video);
        out.canvas = Some(self.canvas);
        out.reserved = Some(self.reservations[at].1);
        let id = self
            .studio
            .start_output(self.adapters, &out)
            .map_err(|e| e.to_string())?;
        self.reservations.remove(at);
        let n = self.number();
        self.outputs.push((n, id));
        Ok(n)
    }

    fn stop(&mut self, output: u64) {
        if let Some(at) = self.outputs.iter().position(|(n, _)| *n == output) {
            let (_, id) = self.outputs.remove(at);
            self.studio.stop_output(id);
        }
    }

    fn failed(&self, output: u64) -> bool {
        self.outputs
            .iter()
            .find(|(n, _)| *n == output)
            .is_none_or(|(_, id)| self.studio.output_failure(*id).is_some())
    }

    fn draws_guest(&self) -> bool {
        draws_guest(self.collection, self.canvas)
    }
}

/// The canvas's program scene draws a guest source (ADR-0139 decision 13).
pub(super) fn draws_guest(collection: &SceneCollection, canvas: CanvasId) -> bool {
    let Some(program) = collection.canvas(canvas).and_then(|c| c.program) else {
        return false;
    };
    collection
        .visible_sources(canvas, program)
        .is_ok_and(|drawn| {
            drawn.iter().any(|id| {
                collection
                    .source(*id)
                    .is_some_and(|s| s.type_id.as_str() == GUEST_SOURCE_TYPE)
            })
        })
}

fn slot_refusal(e: RecordError) -> ShareRefusal {
    let reason = match e {
        RecordError::TooManyOutputs => "too_many_outputs",
        RecordError::NoEncoderSlot => "no_encoder_slot",
        other => return ShareRefusal::Failed(other.to_string()),
    };
    let text = match reason {
        "no_encoder_slot" => {
            "All encoders on this canvas are in use. Stop a recording or stream destination to share."
        }
        _ => "All outputs on this canvas are in use. Stop a recording or stream to share.",
    };
    ShareRefusal::Slot {
        text: text.to_owned(),
        reason,
    }
}

impl Host {
    /// Runs `f` on the shares with the studio's slots.
    fn with_shares<T>(&mut self, f: impl FnOnce(&mut GuestShares, &mut dyn ShareStudio) -> T) -> T {
        let guests = &mut self.guests;
        let mut slots = Slots {
            studio: &mut self.studio,
            adapters: &self.adapters,
            collection: &self.collection,
            canvas: self.canvas,
            reservations: &mut guests.reservations,
            outputs: &mut guests.outputs,
            next: &mut guests.next,
        };
        f(&mut guests.shares, &mut slots)
    }

    /// *Share*: an invite of the main canvas's program.
    pub(super) fn guest_share(&mut self, request: &ShareRequest) -> Result<u32, ShareRefusal> {
        let id = self.with_shares(|shares, slots| shares.share(slots, request))?;
        self.guests.due = Some(self.clock.now());
        Ok(id)
    }

    /// *Stop* of one share.
    pub(super) fn guest_share_stop(&mut self, id: u32) -> Result<(), ShareRefusal> {
        self.with_shares(|shares, slots| shares.stop(slots, id))
    }

    /// Ends every share: the studio restarts or stopped (its outputs and reservations
    /// go with it).
    pub(super) fn end_guest_shares(&mut self) {
        self.with_shares(|shares, slots| shares.stop_all(slots));
        self.guests.reservations.clear();
        self.guests.outputs.clear();
    }

    /// Looks at the shares when due; the time left until the next look, `None` while
    /// none runs. True in the second value when a status changed.
    pub(super) fn drive_guest_shares(&mut self, now: Timestamp) -> (Option<Duration>, bool) {
        let Some(due) = self.guests.due else {
            return (None, false);
        };
        if now < due {
            return (Some(due.saturating_since(now)), false);
        }
        let (changed, next) = self.with_shares(|shares, slots| shares.drive(slots));
        self.guests.due = next.map(|d| now + d);
        (next, changed)
    }
}

impl Host {
    /// Sends `guest.changed` when a share's status changed (never a code).
    pub(super) fn guest_changed(&mut self, _notify: &dyn Fn(super::Update)) {
        let now = self.guests.shares.status();
        if now == self.guests.published {
            return;
        }
        let payload = super::control_api::api_guest::api_guest_status(&now);
        self.guests.published = now;
        self.events
            .changed(super::StateKey::topic(super::Topic::GuestChanged), || {
                super::EventPayload::GuestChanged(payload)
            });
    }
}
