//! Guest shares (ADR-0139 decisions 4, 6, 9, 10, 13 and 14; specs `control-api.md`
//! "Protocol 1.116", `recording.md` "Outputs that own their chain", `peer-port.md`):
//! this RBS sends a canvas's program to another RBS over a pairing.
//!
//! A share runs through *Share* (an invite, a reserved output and chain slot), the
//! answer, the check phrase and the output. Nothing exists before *Share*; a share
//! costs one socket and its keep-alives while it waits, and the encoder exists only
//! once this side confirmed (decision 10). The pairing itself is the adapter's; the
//! studio sees the share as an output to `rbs-guest://<session id>`
//! (peer-port.md, "Share transport").
//!
//! [`GuestShares`] is the state machine. It reaches the studio through [`ShareStudio`],
//! so tests drive it with a fake peer factory and fake slots.

use std::sync::Arc;
use std::time::Duration;

use rbs_core::log::Area;
use rbs_core::time::Timestamp;
use rbs_peer::{
    CodeError, FailureReason, InviteSettings, InviteState, PeerError, PeerEvent, PeerFactory,
    PeerSession, PeerState, PendingInvite, SessionId, SideSettings,
};

/// Most shares at once (ADR-0139 decision 12).
pub const MAX_SHARES: usize = 2;
/// STUN servers used unless the share is local only (ADR-0139 decision 7,
/// `user-data.md`).
pub const DEFAULT_STUN_SERVERS: &[&str] = &["stun.l.google.com:19302", "stun.cloudflare.com:3478"];
/// The URL scheme of a share's output (peer-port.md, "Share transport").
pub const SHARE_SCHEME: &str = "rbs-guest";
/// How often a share is looked at while it gathers, connects or checks the phrase.
pub const PAIRING_POLL: Duration = Duration::from_millis(100);
/// How often a share is looked at while its invite waits for the answer (up to 30
/// minutes); taking the answer looks at once.
pub const WAITING_POLL: Duration = Duration::from_secs(1);
/// Longest time from the answer to this side's confirmation: the longest answer
/// lifetime (ADR-0139 decision 4). The slots are held no longer (decision 9).
pub const CONFIRM_LIMIT: Duration = Duration::from_secs(600);
/// Looks at a confirmed share whose output cannot start yet (a stopped output's
/// chain still drains) before it fails: 5 s at [`PAIRING_POLL`].
pub const START_TRIES: u32 = 50;
/// How often a live share is looked at.
pub const LIVE_POLL: Duration = Duration::from_millis(500);

/// Video of a share: default 1280 x 720 at 2.5 Mbit/s, at most 1920 x 1080 and
/// 8 Mbit/s (ADR-0139 decision 9). The frame rate is the canvas's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShareVideo {
    /// Width, even, 320 to 1920.
    pub width: u32,
    /// Height, even, 180 to 1080.
    pub height: u32,
    /// Start and highest bitrate, 300 kbit/s to 8 Mbit/s.
    pub bitrate_bps: u64,
}

impl Default for ShareVideo {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 720,
            bitrate_bps: 2_500_000,
        }
    }
}

/// Lowest bitrate the network may set (ADR-0139 decision 9).
pub const FLOOR_BPS: u64 = 300_000;
/// Highest share bitrate.
pub const MAX_BPS: u64 = 8_000_000;

impl ShareVideo {
    /// Why the values are out of bounds, in words.
    pub fn problem(&self) -> Option<String> {
        let even = self.width.is_multiple_of(2) && self.height.is_multiple_of(2);
        if !even || !(320..=1920).contains(&self.width) || !(180..=1080).contains(&self.height) {
            return Some(format!(
                "a share's picture is even and from 320x180 to 1920x1080, not {}x{}",
                self.width, self.height
            ));
        }
        if !(FLOOR_BPS..=MAX_BPS).contains(&self.bitrate_bps) {
            return Some(format!(
                "a share's bitrate is {FLOOR_BPS} to {MAX_BPS} bit/s, not {}",
                self.bitrate_bps
            ));
        }
        None
    }
}

/// What *Share* asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareRequest {
    /// How long the invite stays valid; `None`: the default (10 minutes).
    pub lifetime: Option<Duration>,
    /// Compare the check phrase before media flows (on by default).
    pub check_phrase: bool,
    /// Private addresses only, no STUN query.
    pub local_only: bool,
    /// The picture.
    pub video: ShareVideo,
}

impl Default for ShareRequest {
    fn default() -> Self {
        Self {
            lifetime: None,
            check_phrase: true,
            local_only: false,
            video: ShareVideo::default(),
        }
    }
}

/// Where a share is (the control API's `state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareState {
    /// Gathering candidates; no code yet.
    Gathering,
    /// The invite code is ready and waits for the answer.
    WaitingForAnswer,
    /// The answer was taken; connectivity checks and DTLS run.
    Connecting,
    /// Connected; this side has not confirmed the phrase.
    CheckPhrase,
    /// Sending.
    Live,
    /// It ended for `reason` and does not recover.
    Failed,
    /// Stopped by this side or closed by the other.
    Closed,
}

impl ShareState {
    /// The control API's name.
    pub fn id(self) -> &'static str {
        match self {
            Self::Gathering => "gathering",
            Self::WaitingForAnswer => "waiting_for_answer",
            Self::Connecting => "connecting",
            Self::CheckPhrase => "check_phrase",
            Self::Live => "live",
            Self::Failed => "failed",
            Self::Closed => "closed",
        }
    }

    /// The share holds nothing any more.
    pub fn is_over(self) -> bool {
        matches!(self, Self::Failed | Self::Closed)
    }
}

/// Why a share was refused or ended, beside the pairing's [`FailureReason`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareReason {
    /// A pairing failure.
    Pairing(FailureReason),
    /// The program or the bound hold scene draws a guest (ADR-0139 decision 13).
    ReturnFeed,
    /// The share's output failed or did not start.
    Output,
}

impl ShareReason {
    /// The `guest_*` id.
    pub fn id(self) -> &'static str {
        match self {
            Self::Pairing(r) => r.id(),
            Self::ReturnFeed => "guest_return_feed",
            Self::Output => "guest_output_failed",
        }
    }

    /// In words, for the UI.
    pub fn text(self) -> String {
        match self {
            Self::Pairing(r) => r.to_string(),
            Self::ReturnFeed => RETURN_FEED_TEXT.to_owned(),
            Self::Output => "The share's encoder or output stopped.".to_owned(),
        }
    }
}

/// UI text of [`ShareReason::ReturnFeed`] (ADR-0139 decision 13).
pub const RETURN_FEED_TEXT: &str =
    "Sharing a program that contains a guest needs a return mix, which comes later.";

/// One share as the UI and `guest.status` show it. Never holds a code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareStatus {
    /// The share's id.
    pub id: u32,
    /// Where it is.
    pub state: ShareState,
    /// The six check words, once connected (not a secret).
    pub phrase: Option<String>,
    /// Why it failed or was closed.
    pub reason: Option<ShareReason>,
    /// When the invite expires, seconds since the Unix epoch, while it waits.
    pub expires_at: Option<u64>,
    /// The picture.
    pub video: ShareVideo,
}

/// Why a share call was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShareRefusal {
    /// No pairing adapter in this build.
    Unavailable,
    /// The id names no share.
    NotFound,
    /// Not now: too many shares, a code still gathering, not connected yet.
    Conflict(String),
    /// The request is out of bounds, or a bad answer code (`reason` is its id).
    Invalid {
        /// In words.
        text: String,
        /// The `guest_*` id of a bad code, when it is one.
        reason: Option<&'static str>,
    },
    /// Refused with a share reason (`guest_return_feed`).
    Reason(ShareReason),
    /// No output or chain slot (`too_many_outputs`, `no_encoder_slot`).
    Slot {
        /// In words.
        text: String,
        /// `too_many_outputs` or `no_encoder_slot`.
        reason: &'static str,
    },
    /// The adapter failed (a socket, a thread).
    Failed(String),
}

/// What the state machine needs from the studio.
pub trait ShareStudio {
    /// Reserves an output and chain slot of the canvas (ADR-0139 decision 9).
    fn reserve(&mut self) -> Result<u64, ShareRefusal>;
    /// Gives a reservation back.
    fn release(&mut self, reservation: u64);
    /// Starts the share's output to `url` on `reservation`; an output id. The
    /// reservation stays held on an error.
    fn start(&mut self, url: &str, reservation: u64, video: ShareVideo) -> Result<u64, StartError>;
    /// Stops an output.
    fn stop(&mut self, output: u64);
    /// The output failed for good.
    fn failed(&self, output: u64) -> bool;
    /// The canvas's program or bound hold scene draws a guest source (ADR-0139
    /// decision 13).
    fn draws_guest(&self) -> bool;
}

/// Why [`ShareStudio::start`] failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartError {
    /// A free chain still waits for a stopped output's drain; try again soon.
    Busy,
    /// It cannot start.
    Failed(String),
}

enum Pairing {
    Invite(Box<dyn PendingInvite>),
    Session(Box<dyn PeerSession>),
    None,
}

struct Share {
    id: u32,
    pairing: Pairing,
    reservation: Option<u64>,
    output: Option<u64>,
    state: ShareState,
    phrase: Option<String>,
    reason: Option<ShareReason>,
    video: ShareVideo,
    /// The answer's deadline for this side's confirmation, set on the first look
    /// after the answer.
    confirm_by: Option<Timestamp>,
    /// Looks at a confirmed share whose output start was busy.
    start_tries: u32,
}

impl Share {
    fn status(&self) -> ShareStatus {
        let expires_at = match (&self.pairing, self.state) {
            (Pairing::Invite(i), ShareState::Gathering | ShareState::WaitingForAnswer) => {
                Some(i.expires_at_unix())
            }
            _ => None,
        };
        ShareStatus {
            id: self.id,
            state: self.state,
            phrase: self.phrase.clone(),
            reason: self.reason,
            expires_at,
            video: self.video,
        }
    }

    /// Ends the share: closes the pairing, stops the output, gives the slots back.
    fn end(
        &mut self,
        studio: &mut dyn ShareStudio,
        state: ShareState,
        reason: Option<ShareReason>,
    ) {
        match &mut self.pairing {
            Pairing::Invite(i) => i.close(),
            Pairing::Session(s) => s.close(),
            Pairing::None => {}
        }
        self.pairing = Pairing::None;
        if let Some(output) = self.output.take() {
            studio.stop(output);
        }
        if let Some(r) = self.reservation.take() {
            studio.release(r);
        }
        self.state = state;
        self.reason = reason;
    }

    /// One look at the pairing and the output. True when the status changed.
    fn drive(&mut self, studio: &mut dyn ShareStudio, now: Timestamp) -> bool {
        let before = self.status();
        if self.state.is_over() {
            return false;
        }
        match &mut self.pairing {
            Pairing::Invite(invite) => match invite.state() {
                InviteState::Gathering => {}
                InviteState::Waiting => self.state = ShareState::WaitingForAnswer,
                InviteState::Failed(r) => {
                    self.end(studio, ShareState::Failed, Some(ShareReason::Pairing(r)));
                }
                InviteState::Used | InviteState::Closed => {
                    self.end(studio, ShareState::Closed, None);
                }
                _ => {}
            },
            Pairing::Session(session) => {
                while let Some(event) = session.poll_event() {
                    match event {
                        PeerEvent::Connected { phrase } => {
                            self.phrase = Some(phrase.to_string());
                        }
                        PeerEvent::Failed(r) => {
                            self.reason = Some(ShareReason::Pairing(r));
                        }
                        _ => {}
                    }
                }
                let state = session.state();
                let session_id = session.session_id();
                self.follow(studio, state, session_id);
                if matches!(self.state, ShareState::Connecting | ShareState::CheckPhrase)
                    && now >= *self.confirm_by.get_or_insert(now + CONFIRM_LIMIT)
                {
                    let expired = Some(ShareReason::Pairing(FailureReason::Expired));
                    self.end(studio, ShareState::Failed, expired);
                }
            }
            Pairing::None => {}
        }
        if self.state == ShareState::Live {
            if studio.draws_guest() {
                self.end(studio, ShareState::Failed, Some(ShareReason::ReturnFeed));
            } else if self.output.is_some_and(|o| studio.failed(o)) {
                self.end(studio, ShareState::Failed, Some(ShareReason::Output));
            }
        }
        self.status() != before
    }

    /// Follows the session's state: the output starts once this side is confirmed.
    fn follow(&mut self, studio: &mut dyn ShareStudio, state: PeerState, session_id: SessionId) {
        match state {
            PeerState::Gathering | PeerState::Connecting => self.state = ShareState::Connecting,
            PeerState::Connected { confirmed: false } => self.state = ShareState::CheckPhrase,
            PeerState::Connected { confirmed: true } => {
                if self.output.is_some() {
                    return;
                }
                if studio.draws_guest() {
                    self.end(studio, ShareState::Failed, Some(ShareReason::ReturnFeed));
                    return;
                }
                let Some(reservation) = self.reservation.take() else {
                    self.end(studio, ShareState::Failed, Some(ShareReason::Output));
                    return;
                };
                let url = format!("{SHARE_SCHEME}://{session_id}");
                self.reservation = Some(reservation);
                match studio.start(&url, reservation, self.video) {
                    Ok(output) => {
                        self.reservation = None;
                        self.output = Some(output);
                        self.state = ShareState::Live;
                    }
                    Err(StartError::Busy) if self.start_tries < START_TRIES => {
                        self.start_tries += 1;
                        self.state = ShareState::CheckPhrase;
                    }
                    Err(e) => {
                        if let StartError::Failed(text) = e {
                            rbs_core::warn!(
                                Area::Remote,
                                "guest share {}: the output did not start: {text}",
                                self.id
                            );
                        }
                        self.end(studio, ShareState::Failed, Some(ShareReason::Output));
                    }
                }
            }
            PeerState::Failed(r) => {
                self.end(studio, ShareState::Failed, Some(ShareReason::Pairing(r)));
            }
            PeerState::Closed => {
                let reason = self.reason;
                self.end(studio, ShareState::Closed, reason);
            }
            _ => {}
        }
    }
}

/// The host's shares. Empty until *Share*: no socket, thread or timer (P13).
pub struct GuestShares {
    peers: Option<Arc<dyn PeerFactory>>,
    shares: Vec<Share>,
    next: u32,
}

impl GuestShares {
    /// Shares through `peers`; `None`: *Share* is refused as unavailable.
    pub fn new(peers: Option<Arc<dyn PeerFactory>>) -> Self {
        Self {
            peers,
            shares: Vec::new(),
            next: 1,
        }
    }

    /// Starts a share: checks, reserves the slots, starts the invite.
    pub fn share(
        &mut self,
        studio: &mut dyn ShareStudio,
        request: &ShareRequest,
    ) -> Result<u32, ShareRefusal> {
        let peers = self.peers.clone().ok_or(ShareRefusal::Unavailable)?;
        if let Some(text) = request.video.problem() {
            return Err(ShareRefusal::Invalid { text, reason: None });
        }
        let running = self.shares.iter().filter(|s| !s.state.is_over()).count();
        if running >= MAX_SHARES {
            return Err(ShareRefusal::Conflict(format!(
                "at most {MAX_SHARES} shares run at once"
            )));
        }
        if studio.draws_guest() {
            return Err(ShareRefusal::Reason(ShareReason::ReturnFeed));
        }
        let stun = if request.local_only {
            Vec::new()
        } else {
            DEFAULT_STUN_SERVERS
                .iter()
                .map(|s| (*s).to_owned())
                .collect()
        };
        let mut side = SideSettings::with_stun_servers(stun);
        side.check_phrase = request.check_phrase;
        side.omit_private_addresses = false;
        let mut settings = InviteSettings::new(side);
        if let Some(lifetime) = request.lifetime {
            settings.lifetime = lifetime;
        }
        settings.validate().map_err(|e| ShareRefusal::Invalid {
            text: e.to_string(),
            reason: None,
        })?;
        let reservation = studio.reserve()?;
        let invite = match peers.create_invite(&settings) {
            Ok(invite) => invite,
            Err(e) => {
                studio.release(reservation);
                return Err(ShareRefusal::Failed(e.to_string()));
            }
        };
        // Ended shares are kept until a new one, so their reason can be read.
        self.shares.retain(|s| !s.state.is_over());
        let id = self.next;
        self.next = self.next.wrapping_add(1).max(1);
        self.shares.push(Share {
            id,
            pairing: Pairing::Invite(invite),
            reservation: Some(reservation),
            output: None,
            state: ShareState::Gathering,
            phrase: None,
            reason: None,
            video: request.video,
            confirm_by: None,
            start_tries: 0,
        });
        rbs_core::info!(Area::Remote, "guest share {id}: gathering");
        Ok(id)
    }

    fn find(&mut self, id: u32) -> Result<&mut Share, ShareRefusal> {
        self.shares
            .iter_mut()
            .find(|s| s.id == id)
            .ok_or(ShareRefusal::NotFound)
    }

    /// The invite code and its expiry. A secret: never logged or put in an event.
    pub fn code(&mut self, id: u32) -> Result<(String, u64), ShareRefusal> {
        let share = self.find(id)?;
        match &share.pairing {
            Pairing::Invite(invite) => match invite.code() {
                Some(code) => Ok((code.expose().to_owned(), invite.expires_at_unix())),
                None => Err(ShareRefusal::Conflict(
                    "the code is still being made".to_owned(),
                )),
            },
            _ => Err(ShareRefusal::Conflict(
                "the share is past its invite".to_owned(),
            )),
        }
    }

    /// Takes the guest's answer code. A bad answer leaves the share waiting.
    pub fn accept_answer(&mut self, id: u32, answer: &str) -> Result<ShareState, ShareRefusal> {
        let share = self.find(id)?;
        let Pairing::Invite(invite) = &mut share.pairing else {
            return Err(ShareRefusal::Conflict(
                "the share already has its answer".to_owned(),
            ));
        };
        let session = invite.accept_answer(answer).map_err(|e| match e {
            PeerError::Code(code) => ShareRefusal::Invalid {
                text: code.to_string(),
                reason: Some(code_reason(code)),
            },
            PeerError::AlreadyUsed => {
                ShareRefusal::Conflict("the share already has its answer".to_owned())
            }
            other => ShareRefusal::Invalid {
                text: other.to_string(),
                reason: None,
            },
        })?;
        share.pairing = Pairing::Session(session);
        share.state = ShareState::Connecting;
        Ok(share.state)
    }

    /// This side's user compared the phrase.
    pub fn confirm(&mut self, id: u32) -> Result<ShareState, ShareRefusal> {
        let share = self.find(id)?;
        let Pairing::Session(session) = &mut share.pairing else {
            return Err(ShareRefusal::Conflict(
                "the share is not connected".to_owned(),
            ));
        };
        session
            .confirm_phrase()
            .map_err(|_| ShareRefusal::Conflict("the share is not connected".to_owned()))?;
        Ok(share.state)
    }

    /// Stops a share and frees all it holds.
    pub fn stop(&mut self, studio: &mut dyn ShareStudio, id: u32) -> Result<(), ShareRefusal> {
        let share = self.find(id)?;
        if !share.state.is_over() {
            share.end(studio, ShareState::Closed, None);
            log_state(share);
        }
        Ok(())
    }

    /// Stops every share (the studio restarts or ends).
    pub fn stop_all(&mut self, studio: &mut dyn ShareStudio) {
        for share in &mut self.shares {
            if !share.state.is_over() {
                share.end(studio, ShareState::Closed, None);
                log_state(share);
            }
        }
    }

    /// One look at every share. Whether any status changed, and when to look again
    /// (`None`: no share runs, nothing to wait for).
    pub fn drive(
        &mut self,
        studio: &mut dyn ShareStudio,
        now: Timestamp,
    ) -> (bool, Option<Duration>) {
        let mut changed = false;
        for share in &mut self.shares {
            let before = share.state;
            changed |= share.drive(studio, now);
            if share.state != before {
                log_state(share);
            }
        }
        let next = self
            .shares
            .iter()
            .filter(|s| !s.state.is_over())
            .map(|s| match s.state {
                ShareState::Live => LIVE_POLL,
                ShareState::WaitingForAnswer => WAITING_POLL,
                _ => PAIRING_POLL,
            })
            .min();
        (changed, next)
    }

    /// The pictures of the shares that hold a reservation but have no output yet:
    /// planned outputs for the ADR-0087 estimate.
    pub fn reserved_videos(&self) -> Vec<ShareVideo> {
        self.shares
            .iter()
            .filter(|s| s.reservation.is_some() && s.output.is_none())
            .map(|s| s.video)
            .collect()
    }

    /// The shares, oldest first, ended ones until the next *Share*.
    pub fn status(&self) -> Vec<ShareStatus> {
        self.shares.iter().map(Share::status).collect()
    }
}

/// One line per state change: the share's id, state and reason id, never a code.
fn log_state(share: &Share) {
    match (share.state, share.reason) {
        (ShareState::Failed, Some(reason)) => rbs_core::warn!(
            Area::Remote,
            "guest share {}: failed ({})",
            share.id,
            reason.id()
        ),
        (state, Some(reason)) => rbs_core::info!(
            Area::Remote,
            "guest share {}: {} ({})",
            share.id,
            state.id(),
            reason.id()
        ),
        (state, None) => {
            rbs_core::info!(Area::Remote, "guest share {}: {}", share.id, state.id());
        }
    }
}

/// The `guest_*` id of a refused code (`data.reason` of `invalid_params`).
pub fn code_reason(error: CodeError) -> &'static str {
    match error {
        CodeError::NotACode => "guest_not_a_code",
        CodeError::TooLarge | CodeError::Malformed => "guest_code_damaged",
        CodeError::NewerVersion { .. } => "guest_code_newer",
        CodeError::UnknownTransport => "guest_code_transport",
        CodeError::WrongKind => "guest_code_wrong_kind",
        CodeError::WrongSession => "guest_code_wrong_session",
        CodeError::Expired => "guest_expired",
        _ => "guest_code_damaged",
    }
}
