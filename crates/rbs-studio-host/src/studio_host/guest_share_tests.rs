//! The guest share state machine against a fake pairing adapter and fake slots.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::sync::{Arc, Mutex};

use rbs_peer::{
    AcceptSettings, CheckPhrase, CodeError, CodeKind, Envelope, FailureReason, InviteSettings,
    InviteState, MediaHandle, PairingCode, PeerError, PeerEvent, PeerFactory, PeerRole,
    PeerSession, PeerState, PendingInvite, SessionId, TransportKind,
};

use super::guest_share::*;

/// What the fake adapter's invite and session show; the test moves it along.
#[derive(Default)]
struct Script {
    invite: Option<InviteState>,
    session: Option<PeerState>,
    events: Vec<PeerEvent>,
    confirmed: bool,
    closed: u32,
    settings: Option<InviteSettings>,
}

type Shared = Arc<Mutex<Script>>;

fn id() -> SessionId {
    SessionId::new([7; 16]).unwrap()
}

struct FakeFactory(Shared);

impl PeerFactory for FakeFactory {
    fn type_id(&self) -> &'static str {
        "test.peer"
    }
    fn transport(&self) -> TransportKind {
        TransportKind::WebRtc
    }
    fn create_invite(
        &self,
        settings: &InviteSettings,
    ) -> Result<Box<dyn PendingInvite>, PeerError> {
        let mut s = self.0.lock().unwrap();
        s.invite = Some(InviteState::Gathering);
        s.settings = Some(settings.clone());
        let code = Envelope::new(
            CodeKind::Invite,
            TransportKind::WebRtc,
            id(),
            1_000,
            vec![1],
        )
        .encode()
        .unwrap();
        Ok(Box::new(FakeInvite(Arc::clone(&self.0), code)))
    }
    fn accept_invite(
        &self,
        _: &str,
        _: &AcceptSettings,
    ) -> Result<Box<dyn PeerSession>, PeerError> {
        Err(PeerError::AlreadyUsed)
    }
}

struct FakeInvite(Shared, PairingCode);

impl PendingInvite for FakeInvite {
    fn session_id(&self) -> SessionId {
        id()
    }
    fn state(&self) -> InviteState {
        self.0.lock().unwrap().invite.unwrap()
    }
    fn code(&self) -> Option<&PairingCode> {
        (self.state() != InviteState::Gathering).then_some(&self.1)
    }
    fn expires_at_unix(&self) -> u64 {
        1_000
    }
    fn accept_answer(&mut self, answer: &str) -> Result<Box<dyn PeerSession>, PeerError> {
        if answer != "good" {
            return Err(PeerError::Code(CodeError::WrongSession));
        }
        let mut s = self.0.lock().unwrap();
        s.invite = Some(InviteState::Used);
        s.session = Some(PeerState::Connecting);
        Ok(Box::new(FakeSession(Arc::clone(&self.0))))
    }
    fn close(&mut self) {
        self.0.lock().unwrap().closed += 1;
    }
}

struct FakeSession(Shared);

impl PeerSession for FakeSession {
    fn session_id(&self) -> SessionId {
        id()
    }
    fn role(&self) -> PeerRole {
        PeerRole::Offerer
    }
    fn state(&self) -> PeerState {
        self.0.lock().unwrap().session.unwrap()
    }
    fn answer_code(&self) -> Option<&PairingCode> {
        None
    }
    fn poll_event(&mut self) -> Option<PeerEvent> {
        let mut s = self.0.lock().unwrap();
        (!s.events.is_empty()).then(|| s.events.remove(0))
    }
    fn check_phrase(&self) -> Option<CheckPhrase> {
        None
    }
    fn confirm_phrase(&mut self) -> Result<(), PeerError> {
        let mut s = self.0.lock().unwrap();
        if !matches!(s.session, Some(PeerState::Connected { .. })) {
            return Err(PeerError::NotConnected);
        }
        s.confirmed = true;
        s.session = Some(PeerState::Connected { confirmed: true });
        s.events.push(PeerEvent::Confirmed);
        Ok(())
    }
    fn media(&self) -> MediaHandle {
        MediaHandle::new(Arc::new(()))
    }
    fn close(&mut self) {
        self.0.lock().unwrap().closed += 1;
    }
}

/// Slots and outputs, counted.
#[derive(Default)]
struct Slots {
    free: u32,
    reserved: Vec<u64>,
    next: u64,
    started: Vec<(String, u64)>,
    stopped: Vec<u64>,
    fail_start: bool,
    failed_output: bool,
    guest_drawn: bool,
}

impl ShareStudio for Slots {
    fn reserve(&mut self) -> Result<u64, ShareRefusal> {
        if self.free == 0 {
            return Err(ShareRefusal::Slot {
                text: "no slot".into(),
                reason: "no_encoder_slot",
            });
        }
        self.free -= 1;
        self.next += 1;
        self.reserved.push(self.next);
        Ok(self.next)
    }
    fn release(&mut self, reservation: u64) {
        self.reserved.retain(|r| *r != reservation);
        self.free += 1;
    }
    fn start(&mut self, url: &str, reservation: u64, _: ShareVideo) -> Result<u64, String> {
        if self.fail_start {
            return Err("no encoder".into());
        }
        self.reserved.retain(|r| *r != reservation);
        self.started.push((url.to_owned(), reservation));
        Ok(100 + reservation)
    }
    fn stop(&mut self, output: u64) {
        self.stopped.push(output);
        self.free += 1;
    }
    fn failed(&self, _: u64) -> bool {
        self.failed_output
    }
    fn draws_guest(&self) -> bool {
        self.guest_drawn
    }
}

fn phrase() -> CheckPhrase {
    CheckPhrase::from_digest(&[9; 32]).unwrap()
}

fn rig() -> (GuestShares, Shared, Slots) {
    let script = Shared::default();
    let shares = GuestShares::new(Some(Arc::new(FakeFactory(Arc::clone(&script)))));
    let slots = Slots {
        free: 4,
        ..Slots::default()
    };
    (shares, script, slots)
}

fn state(shares: &GuestShares) -> ShareState {
    shares.status()[0].state
}

/// Up to the check phrase.
fn connect(shares: &mut GuestShares, script: &Shared, slots: &mut Slots) -> u32 {
    let share = shares.share(slots, &ShareRequest::default()).unwrap();
    script.lock().unwrap().invite = Some(InviteState::Waiting);
    shares.drive(slots);
    shares.accept_answer(share, "good").unwrap();
    {
        let mut s = script.lock().unwrap();
        s.session = Some(PeerState::Connected { confirmed: false });
        s.events.push(PeerEvent::Connected { phrase: phrase() });
    }
    shares.drive(slots);
    share
}

#[test]
fn a_share_goes_live_only_after_this_side_confirms_and_stop_frees_everything() {
    let (mut shares, script, mut slots) = rig();
    let share = shares.share(&mut slots, &ShareRequest::default()).unwrap();
    assert_eq!(state(&shares), ShareState::Gathering);
    assert_eq!(slots.reserved.len(), 1, "the slot is held from Share on");
    assert!(matches!(shares.code(share), Err(ShareRefusal::Conflict(_))));
    let stun = &script
        .lock()
        .unwrap()
        .settings
        .clone()
        .unwrap()
        .side
        .stun_servers;
    assert!(!stun.is_empty(), "the default STUN list");

    script.lock().unwrap().invite = Some(InviteState::Waiting);
    let (changed, next) = shares.drive(&mut slots);
    assert!(changed);
    assert_eq!(next, Some(PAIRING_POLL));
    assert_eq!(state(&shares), ShareState::WaitingForAnswer);
    let (code, expires) = shares.code(share).unwrap();
    assert!(code.starts_with("rbs-guest:1:"));
    assert_eq!(expires, 1_000);
    assert_eq!(shares.status()[0].expires_at, Some(1_000));

    // A bad answer leaves the share waiting.
    assert!(matches!(
        shares.accept_answer(share, "bad"),
        Err(ShareRefusal::Invalid {
            reason: Some("guest_code_wrong_session"),
            ..
        })
    ));
    assert_eq!(state(&shares), ShareState::WaitingForAnswer);
    assert_eq!(
        shares.accept_answer(share, "good").unwrap(),
        ShareState::Connecting
    );
    {
        let mut s = script.lock().unwrap();
        s.session = Some(PeerState::Connected { confirmed: false });
        s.events.push(PeerEvent::Connected { phrase: phrase() });
    }
    shares.drive(&mut slots);
    let status = &shares.status()[0];
    assert_eq!(status.state, ShareState::CheckPhrase);
    assert_eq!(
        status.phrase.as_deref(),
        Some(phrase().to_string().as_str())
    );
    assert!(slots.started.is_empty(), "no output before confirmation");

    shares.confirm(share).unwrap();
    shares.drive(&mut slots);
    assert_eq!(state(&shares), ShareState::Live);
    assert_eq!(
        slots.started,
        [(format!("rbs-guest://{}", id()), 1)],
        "started on its reservation"
    );
    assert_eq!(shares.drive(&mut slots).1, Some(LIVE_POLL));

    shares.stop(&mut slots, share).unwrap();
    assert_eq!(state(&shares), ShareState::Closed);
    assert_eq!(slots.stopped, [101]);
    assert_eq!(slots.free, 4, "every slot is back");
    assert_eq!(script.lock().unwrap().closed, 1);
    assert_eq!(shares.drive(&mut slots).1, None, "nothing runs, no timer");
}

#[test]
fn a_program_with_a_guest_is_refused_and_a_live_share_stops_when_one_is_drawn() {
    let (mut shares, script, mut slots) = rig();
    slots.guest_drawn = true;
    assert_eq!(
        shares.share(&mut slots, &ShareRequest::default()),
        Err(ShareRefusal::Reason(ShareReason::ReturnFeed))
    );
    assert_eq!(slots.free, 4);
    slots.guest_drawn = false;
    let share = connect(&mut shares, &script, &mut slots);
    shares.confirm(share).unwrap();
    shares.drive(&mut slots);
    assert_eq!(state(&shares), ShareState::Live);
    slots.guest_drawn = true;
    shares.drive(&mut slots);
    let status = &shares.status()[0];
    assert_eq!(status.state, ShareState::Failed);
    assert_eq!(
        status.reason.map(ShareReason::id),
        Some("guest_return_feed")
    );
    assert_eq!(slots.free, 4);
}

#[test]
fn failures_end_the_share_and_give_the_slots_back() {
    // The invite expires.
    let (mut shares, script, mut slots) = rig();
    shares.share(&mut slots, &ShareRequest::default()).unwrap();
    script.lock().unwrap().invite = Some(InviteState::Failed(FailureReason::Expired));
    shares.drive(&mut slots);
    assert_eq!(
        shares.status()[0].reason.map(ShareReason::id),
        Some("guest_expired")
    );
    assert_eq!(slots.free, 4);

    // The output does not start.
    let (mut shares, script, mut slots) = rig();
    let share = connect(&mut shares, &script, &mut slots);
    slots.fail_start = true;
    shares.confirm(share).unwrap();
    shares.drive(&mut slots);
    assert_eq!(state(&shares), ShareState::Failed);
    assert_eq!(slots.free, 4);

    // Consent is lost while live.
    let (mut shares, script, mut slots) = rig();
    let share = connect(&mut shares, &script, &mut slots);
    shares.confirm(share).unwrap();
    shares.drive(&mut slots);
    script.lock().unwrap().session = Some(PeerState::Failed(FailureReason::Disconnected));
    shares.drive(&mut slots);
    assert_eq!(
        shares.status()[0].reason.map(ShareReason::id),
        Some("guest_disconnected")
    );
    assert_eq!(slots.stopped.len(), 1);
    assert_eq!(slots.free, 4);
}

#[test]
fn limits_and_bounds_are_checked_before_anything_opens() {
    let (mut shares, _script, mut slots) = rig();
    let mut wide = ShareRequest::default();
    wide.video.width = 2560;
    assert!(matches!(
        shares.share(&mut slots, &wide),
        Err(ShareRefusal::Invalid { .. })
    ));
    shares.share(&mut slots, &ShareRequest::default()).unwrap();
    shares.share(&mut slots, &ShareRequest::default()).unwrap();
    assert!(matches!(
        shares.share(&mut slots, &ShareRequest::default()),
        Err(ShareRefusal::Conflict(_))
    ));
    assert_eq!(slots.free, 2);

    let mut none = Slots::default();
    let (mut fresh, _s, _) = rig();
    assert!(matches!(
        fresh.share(&mut none, &ShareRequest::default()),
        Err(ShareRefusal::Slot {
            reason: "no_encoder_slot",
            ..
        })
    ));
    assert_eq!(
        GuestShares::new(None).share(&mut none, &ShareRequest::default()),
        Err(ShareRefusal::Unavailable)
    );

    let mut local = ShareRequest::default();
    local.local_only = true;
    local.check_phrase = false;
    let (mut shares, script, mut slots) = rig();
    shares.share(&mut slots, &local).unwrap();
    let settings = script.lock().unwrap().settings.clone().unwrap();
    assert!(settings.side.stun_servers.is_empty());
    assert!(!settings.side.check_phrase);
}
