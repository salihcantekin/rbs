//! One pairing, sans-IO: str0m plus the STUN client, the code, the check phrase and the
//! timers of ADR-0139 decisions 4, 6, 7 and 10. The owner thread (`net`) feeds it
//! datagrams and time; tests drive two engines through a simulated network.
//!
//! No socket, thread or clock read happens here: every call takes `now`.

use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use rbs_peer::{
    CheckPhrase, CodeKind, Envelope, FailureReason, PairingCode, PeerError, PeerEvent, PeerRole,
    PeerState, SessionId, SideSettings, TransportKind, phrase_input,
};
use str0m::change::{SdpAnswer, SdpOffer, SdpPendingOffer};
use str0m::media::{Direction, MediaKind};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, Input, Output, Rtc};

use crate::stun::{Gatherer, TransactionId};
use crate::{crypto, payload};

/// Without any datagram from the peer for this long, consent is lost (RFC 7675); the
/// pairing ends and new codes are needed (ADR-0139 decision 10).
pub const CONSENT_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest time between connectivity check retransmissions while waiting.
const MAX_CHECK_INTERVAL: Duration = Duration::from_secs(3);
/// Most connectivity check retransmissions str0m is given (see `Engine::new`).
const MAX_RETRANSMITS: usize = 96;
/// While the answering side waits, it sends a small STUN request to every remote
/// candidate this often, so its own NAT keeps admitting the offerer until the answer
/// is pasted (ADR-0139 decision 4: checks kept running until the answer expires).
pub const PUNCH_INTERVAL: Duration = Duration::from_secs(10);

/// Where a pairing is, for both roles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Gathering candidates; no code yet.
    Gathering,
    /// The offerer's code is ready; no answer taken yet.
    Waiting,
    /// Connectivity checks and DTLS.
    Connecting,
    /// DTLS up.
    Connected {
        /// This side may use media.
        confirmed: bool,
    },
    /// Failed for good.
    Failed(FailureReason),
    /// Closed by this side.
    Closed,
}

impl Phase {
    /// The port's session state for this phase.
    pub fn peer_state(self) -> PeerState {
        match self {
            Phase::Gathering => PeerState::Gathering,
            Phase::Waiting | Phase::Connecting => PeerState::Connecting,
            Phase::Connected { confirmed } => PeerState::Connected { confirmed },
            Phase::Failed(reason) => PeerState::Failed(reason),
            Phase::Closed => PeerState::Closed,
        }
    }

    fn is_over(self) -> bool {
        matches!(self, Phase::Failed(_) | Phase::Closed)
    }
}

/// What a new engine needs.
#[derive(Debug, Clone)]
pub struct Setup {
    /// The pairing session (the offerer picks it; the answerer reads it from the invite).
    pub session_id: SessionId,
    /// Network settings and whether this side checks the phrase.
    pub side: SideSettings,
    /// How long this side's code stays valid.
    pub lifetime: Duration,
    /// The local socket's address (the host candidate's base).
    pub local: SocketAddr,
    /// Resolved STUN servers.
    pub stun_servers: Vec<SocketAddr>,
}

/// One pairing.
pub struct Engine {
    role: PeerRole,
    session_id: SessionId,
    rtc: Rtc,
    local_fingerprint: [u8; 32],
    remote_fingerprint: Option<Vec<u8>>,
    gatherer: Gatherer,
    local: SocketAddr,
    omit_private: bool,
    check_phrase: bool,
    phase: Phase,
    gather_deadline: Instant,
    /// When this side's code (or, for an offerer that took an answer, the answer)
    /// expires on the monotonic clock.
    expiry: Instant,
    expires_at_unix: u64,
    lifetime: Duration,
    pending_offer: Option<SdpPendingOffer>,
    remote_offer: Option<SdpOffer>,
    code: Option<PairingCode>,
    phrase: Option<CheckPhrase>,
    events: VecDeque<PeerEvent>,
    last_heard: Option<Instant>,
    transmits: VecDeque<(SocketAddr, Vec<u8>)>,
    answer_taken: bool,
    rtc_timeout: Option<Instant>,
    punch_targets: Vec<SocketAddr>,
    next_punch: Option<Instant>,
    /// ICE credentials seen so far (local and remote), for the owner of the engine to
    /// register as secrets (peer-port.md, Secrets).
    secrets: Vec<String>,
    /// Media sections in this side's offer, which a usable answer matches.
    offer_media: usize,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("role", &self.role)
            .field("session_id", &self.session_id)
            .field("phase", &self.phase)
            .finish_non_exhaustive()
    }
}

fn failed_io(what: &str) -> PeerError {
    PeerError::Io(what.to_owned())
}

/// The SHA-256 fingerprint in an SDP (`a=fingerprint:sha-256 AB:CD:...`).
///
/// `None` unless every fingerprint line in the SDP, at session or media level, is
/// SHA-256 and names the same digest: str0m authenticates DTLS against a media-level
/// fingerprint before the session-level one, so an SDP that carried two could make
/// the check phrase cover a certificate DTLS never checks.
pub fn sdp_fingerprint(sdp: &str) -> Option<Vec<u8>> {
    let mut found: Option<Vec<u8>> = None;
    for line in sdp.lines().map(str::trim) {
        let Some(value) = line.strip_prefix("a=fingerprint:") else {
            continue;
        };
        let hex = value.trim().strip_prefix("sha-256 ")?;
        let bytes: Vec<u8> = hex
            .trim()
            .split(':')
            .map(|h| u8::from_str_radix(h, 16).ok())
            .collect::<Option<_>>()?;
        if bytes.len() != 32 || found.as_ref().is_some_and(|f| *f != bytes) {
            return None;
        }
        found = Some(bytes);
    }
    found
}

/// The number of media sections (`m=` lines) in an SDP.
fn sdp_media_count(sdp: &str) -> usize {
    sdp.lines().filter(|l| l.trim().starts_with("m=")).count()
}

/// The ICE username fragment and password in an SDP (`a=ice-ufrag:`, `a=ice-pwd:`),
/// each once.
pub fn sdp_ice_credentials(sdp: &str) -> Vec<String> {
    sdp.lines()
        .filter_map(|l| {
            let l = l.trim();
            l.strip_prefix("a=ice-ufrag:")
                .or_else(|| l.strip_prefix("a=ice-pwd:"))
        })
        .map(|v| v.trim().to_owned())
        .fold(Vec::new(), |mut out, v| {
            // Each media section repeats the same pair.
            if !out.contains(&v) {
                out.push(v);
            }
            out
        })
}

/// The candidate addresses in an SDP (`a=candidate:<f> <c> udp <prio> <ip> <port> typ ...`).
pub fn sdp_candidates(sdp: &str) -> Vec<SocketAddr> {
    sdp.lines()
        .filter_map(|l| l.trim().strip_prefix("a=candidate:"))
        .filter_map(|c| {
            let mut parts = c.split_whitespace();
            let ip: IpAddr = parts.nth(4)?.parse().ok()?;
            let port: u16 = parts.next()?.parse().ok()?;
            Some(SocketAddr::new(ip, port))
        })
        .take(crate::payload::MAX_CANDIDATES)
        .collect()
}

fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_loopback() || v4.is_link_local(),
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

impl Engine {
    fn new(
        role: PeerRole,
        setup: Setup,
        now: Instant,
        unix_now: u64,
        ids: impl FnMut() -> TransactionId,
    ) -> Result<Self, PeerError> {
        let cert = crypto::certificate::self_signed().map_err(|_| failed_io("certificate"))?;
        let local_fingerprint = crypto::sha256(&cert.certificate);
        let mut config = payload::rtc_config()
            .set_crypto_provider(crypto::provider())
            .set_dtls_cert(cert)
            .enable_bwe(None);
        // Keep connectivity checks going for the whole lifetime of the code, so the
        // peer's mapping stays open until the other side pastes (decision 4); consent
        // is judged by `CONSENT_TIMEOUT`, not by str0m's retransmission limit.
        config.set_max_stun_rto(MAX_CHECK_INTERVAL);
        // str0m computes `initial_rto << retransmits`; past about 110 that overflows,
        // so the count is capped and `punch` keeps the path open beyond it.
        let checks = (setup.lifetime.as_secs() / MAX_CHECK_INTERVAL.as_secs()) as usize + 16;
        config.set_max_stun_retransmits(checks.min(MAX_RETRANSMITS));
        let rtc = config.build(now);
        let gatherer = Gatherer::start(setup.stun_servers, now, ids);
        Ok(Self {
            role,
            session_id: setup.session_id,
            rtc,
            local_fingerprint,
            remote_fingerprint: None,
            gatherer,
            local: setup.local,
            omit_private: setup.side.omit_private_addresses,
            check_phrase: setup.side.check_phrase,
            phase: Phase::Gathering,
            gather_deadline: now + setup.side.gathering_timeout,
            expiry: now + setup.lifetime,
            expires_at_unix: unix_now.saturating_add(setup.lifetime.as_secs()),
            lifetime: setup.lifetime,
            pending_offer: None,
            remote_offer: None,
            code: None,
            phrase: None,
            events: VecDeque::new(),
            last_heard: None,
            transmits: VecDeque::new(),
            answer_taken: false,
            rtc_timeout: None,
            punch_targets: Vec::new(),
            next_punch: None,
            secrets: Vec::new(),
            offer_media: 0,
        })
    }

    /// The sharing side: starts gathering for an invite.
    pub fn offerer(
        setup: Setup,
        now: Instant,
        unix_now: u64,
        ids: impl FnMut() -> TransactionId,
    ) -> Result<Self, PeerError> {
        Self::new(PeerRole::Offerer, setup, now, unix_now, ids)
    }

    /// The host: reads an invite and starts gathering for its answer.
    pub fn answerer(
        invite: &str,
        mut setup: Setup,
        now: Instant,
        unix_now: u64,
        ids: impl FnMut() -> TransactionId,
    ) -> Result<Self, PeerError> {
        let envelope = Envelope::decode(invite)?;
        envelope.expect(CodeKind::Invite, None, unix_now)?;
        if envelope.transport != TransportKind::WebRtc {
            return Err(PeerError::Code(rbs_peer::CodeError::UnknownTransport));
        }
        let sdp = payload::decode(&envelope.payload).map_err(payload_error)?;
        let remote_fingerprint =
            sdp_fingerprint(&sdp).ok_or(PeerError::Code(rbs_peer::CodeError::Malformed))?;
        let offer = SdpOffer::from_sdp_string(&sdp)
            .map_err(|_| PeerError::Code(rbs_peer::CodeError::Malformed))?;
        setup.session_id = envelope.session_id;
        let mut engine = Self::new(PeerRole::Answerer, setup, now, unix_now, ids)?;
        engine.remote_fingerprint = Some(remote_fingerprint);
        engine.punch_targets = sdp_candidates(&sdp);
        engine.secrets = sdp_ice_credentials(&sdp);
        engine.remote_offer = Some(offer);
        Ok(engine)
    }

    /// This side's role.
    pub fn role(&self) -> PeerRole {
        self.role
    }

    /// The pairing session.
    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    /// The current phase.
    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// This side's code, once gathering is done.
    pub fn code(&self) -> Option<&PairingCode> {
        self.code.as_ref()
    }

    /// The advisory expiry written in this side's code.
    pub fn expires_at_unix(&self) -> u64 {
        self.expires_at_unix
    }

    /// The check phrase, once DTLS is connected.
    pub fn phrase(&self) -> Option<CheckPhrase> {
        self.phrase
    }

    /// ICE credentials found since the last call, to be kept out of logs and reports.
    pub fn take_secrets(&mut self) -> Vec<String> {
        std::mem::take(&mut self.secrets)
    }

    /// The next event.
    pub fn poll_event(&mut self) -> Option<PeerEvent> {
        self.events.pop_front()
    }

    /// The sharing side takes the answer.
    pub fn accept_answer(
        &mut self,
        answer: &str,
        now: Instant,
        unix_now: u64,
    ) -> Result<(), PeerError> {
        if self.role != PeerRole::Offerer {
            return Err(PeerError::NotConnected);
        }
        if self.answer_taken {
            return Err(PeerError::AlreadyUsed);
        }
        match self.phase {
            Phase::Waiting => {}
            Phase::Gathering => return Err(PeerError::NotConnected),
            Phase::Failed(_) | Phase::Closed => return Err(PeerError::UnknownSession),
            _ => return Err(PeerError::AlreadyUsed),
        }
        let envelope = Envelope::decode(answer)?;
        envelope.expect(CodeKind::Answer, Some(self.session_id), unix_now)?;
        let sdp = payload::decode(&envelope.payload).map_err(payload_error)?;
        let fingerprint =
            sdp_fingerprint(&sdp).ok_or(PeerError::Code(rbs_peer::CodeError::Malformed))?;
        let answer = SdpAnswer::from_sdp_string(&sdp)
            .map_err(|_| PeerError::Code(rbs_peer::CodeError::Malformed))?;
        // str0m consumes the pending offer even when it refuses the answer, so check
        // what it would refuse first: a damaged or foreign answer then leaves the
        // invite waiting for the right one.
        if sdp_ice_credentials(&sdp).len() != 2 || sdp_media_count(&sdp) != self.offer_media {
            return Err(PeerError::Code(rbs_peer::CodeError::Malformed));
        }
        let pending = self.pending_offer.take().ok_or(PeerError::Closed)?;
        if let Err(_e) = self.rtc.sdp_api().accept_answer(pending, answer) {
            // Not caught above: str0m consumed the pending offer, so this invite
            // cannot take another answer.
            self.fail(FailureReason::Unreachable);
            return Err(PeerError::Code(rbs_peer::CodeError::Malformed));
        }
        self.answer_taken = true;
        self.secrets.extend(sdp_ice_credentials(&sdp));
        self.remote_fingerprint = Some(fingerprint);
        // From here, the answer's lifetime bounds the connection attempt.
        let left = envelope.expires_at_unix.saturating_sub(unix_now);
        self.expiry = now + Duration::from_secs(left).min(self.lifetime);
        self.phase = Phase::Connecting;
        Ok(())
    }

    /// This side's user confirmed the phrase.
    pub fn confirm(&mut self) -> Result<(), PeerError> {
        match self.phase {
            Phase::Connected { confirmed: false } => {
                self.phase = Phase::Connected { confirmed: true };
                self.events.push_back(PeerEvent::Confirmed);
                Ok(())
            }
            Phase::Connected { confirmed: true } => Ok(()),
            Phase::Failed(_) | Phase::Closed => Err(PeerError::Closed),
            _ => Err(PeerError::NotConnected),
        }
    }

    /// Ends the pairing.
    pub fn close(&mut self) {
        if !self.phase.is_over() {
            self.rtc.disconnect();
            self.gatherer.set_keepalive(false, self.gather_deadline);
            self.phase = Phase::Closed;
            self.events.push_back(PeerEvent::Closed);
        }
    }

    fn fail(&mut self, reason: FailureReason) {
        if !self.phase.is_over() {
            self.rtc.disconnect();
            self.gatherer.set_keepalive(false, self.gather_deadline);
            self.phase = Phase::Failed(reason);
            self.events.push_back(PeerEvent::Failed(reason));
        }
    }

    /// A datagram from `from` arrived at the local socket.
    pub fn handle_datagram(&mut self, now: Instant, from: SocketAddr, bytes: &[u8]) {
        if self.phase.is_over() {
            return;
        }
        if crate::stun::is_stun(bytes) && self.gatherer.handle(from, bytes) {
            self.drive(now);
            return;
        }
        if let Ok(rx) = Receive::new(Protocol::Udp, from, self.local, bytes) {
            let input = Input::Receive(now, rx);
            if self.rtc.accepts(&input) && self.rtc.handle_input(input).is_ok() {
                self.last_heard = Some(now);
            }
        }
        self.drive(now);
    }

    /// Time passed.
    pub fn handle_timeout(&mut self, now: Instant, mut ids: impl FnMut() -> TransactionId) {
        if self.phase.is_over() {
            return;
        }
        while let Some(send) = self.gatherer.poll_send(now, &mut ids) {
            self.transmits.push_back((send.to, send.bytes.to_vec()));
        }
        if self.phase == Phase::Connecting
            && let Some(at) = self.next_punch
            && now >= at
        {
            self.next_punch = Some(now + PUNCH_INTERVAL);
            for &to in &self.punch_targets {
                self.transmits
                    .push_back((to, crate::stun::binding_request(&ids()).to_vec()));
            }
        }
        let _ = self.rtc.handle_input(Input::Timeout(now));
        self.drive(now);
    }

    /// The next datagram to send.
    pub fn poll_transmit(&mut self) -> Option<(SocketAddr, Vec<u8>)> {
        self.transmits.pop_front()
    }

    /// When [`Engine::handle_timeout`] should run next.
    pub fn next_timeout(&mut self, now: Instant) -> Instant {
        let mut next = now + Duration::from_secs(1);
        if let Some(t) = self.gatherer.next_timeout() {
            next = next.min(t);
        }
        if matches!(self.phase, Phase::Gathering) {
            next = next.min(self.gather_deadline);
        }
        if let Some(t) = self.next_punch.filter(|_| self.phase == Phase::Connecting) {
            next = next.min(t);
        }
        next.min(self.rtc_timeout.unwrap_or(next)).max(now)
    }

    fn drive(&mut self, now: Instant) {
        if self.phase == Phase::Gathering && (self.gatherer.done() || now >= self.gather_deadline) {
            self.finish_gathering(now);
        }
        self.pump_rtc(now);
        self.check_timers(now);
    }

    fn finish_gathering(&mut self, now: Instant) {
        let reflexive = self.gatherer.reflexive();
        let host_ok = !(self.omit_private && is_private(self.local.ip()));
        if reflexive.is_none() && !host_ok {
            self.fail(FailureReason::NoPublicAddress);
            return;
        }
        if host_ok && let Ok(c) = Candidate::host(self.local, "udp") {
            self.rtc.add_local_candidate(c);
        }
        if let Some(addr) = reflexive.filter(|a| *a != self.local)
            && let Ok(c) = Candidate::server_reflexive(addr, self.local, "udp")
        {
            self.rtc.add_local_candidate(c);
        }
        let (kind, sdp) = match self.role {
            PeerRole::Offerer => {
                let mut change = self.rtc.sdp_api();
                change.add_media(MediaKind::Video, Direction::SendOnly, None, None, None);
                change.add_media(MediaKind::Audio, Direction::SendOnly, None, None, None);
                let Some((offer, pending)) = change.apply() else {
                    self.fail(FailureReason::Unreachable);
                    return;
                };
                self.pending_offer = Some(pending);
                let sdp = offer.to_sdp_string();
                self.offer_media = sdp_media_count(&sdp);
                (CodeKind::Invite, sdp)
            }
            _ => {
                let Some(offer) = self.remote_offer.take() else {
                    self.fail(FailureReason::Unreachable);
                    return;
                };
                match self.rtc.sdp_api().accept_offer(offer) {
                    Ok(answer) => (CodeKind::Answer, answer.to_sdp_string()),
                    Err(_) => {
                        self.fail(FailureReason::Unreachable);
                        return;
                    }
                }
            }
        };
        self.secrets.extend(sdp_ice_credentials(&sdp));
        let envelope = Envelope::new(
            kind,
            TransportKind::WebRtc,
            self.session_id,
            self.expires_at_unix,
            payload::encode(&sdp),
        );
        match envelope.encode() {
            Ok(code) => self.code = Some(code),
            Err(_) => {
                self.fail(FailureReason::Unreachable);
                return;
            }
        }
        self.gatherer.set_keepalive(true, now);
        self.phase = match self.role {
            PeerRole::Offerer => Phase::Waiting,
            _ => Phase::Connecting,
        };
        if self.role != PeerRole::Offerer {
            self.events.push_back(PeerEvent::AnswerReady);
            self.next_punch = Some(now + PUNCH_INTERVAL);
        }
    }

    fn pump_rtc(&mut self, now: Instant) {
        loop {
            match self.rtc.poll_output() {
                Ok(Output::Timeout(t)) => {
                    self.rtc_timeout = Some(t);
                    break;
                }
                Ok(Output::Transmit(t)) => self
                    .transmits
                    .push_back((t.destination, t.contents.to_vec())),
                Ok(Output::Event(event)) => self.on_event(event, now),
                Err(_) => {
                    self.fail(FailureReason::Disconnected);
                    break;
                }
            }
        }
    }

    fn on_event(&mut self, event: Event, now: Instant) {
        if let Event::Connected = event {
            self.connected(now)
        }
    }

    fn connected(&mut self, now: Instant) {
        if !matches!(self.phase, Phase::Connecting) {
            return;
        }
        let Some(remote) = self.remote_fingerprint.as_deref() else {
            self.fail(FailureReason::FingerprintMismatch);
            return;
        };
        let (offerer, answerer) = match self.role {
            PeerRole::Offerer => (&self.local_fingerprint[..], remote),
            _ => (remote, &self.local_fingerprint[..]),
        };
        let phrase = phrase_input(self.session_id, offerer, answerer)
            .ok()
            .and_then(|input| CheckPhrase::from_digest(&crypto::sha256(&input)).ok());
        let Some(phrase) = phrase else {
            self.fail(FailureReason::FingerprintMismatch);
            return;
        };
        self.phrase = Some(phrase);
        self.last_heard = Some(now);
        self.gatherer.set_keepalive(false, now);
        self.phase = Phase::Connected {
            confirmed: !self.check_phrase,
        };
        self.events.push_back(PeerEvent::Connected { phrase });
        if !self.check_phrase {
            self.events.push_back(PeerEvent::Confirmed);
        }
    }

    fn check_timers(&mut self, now: Instant) {
        match self.phase {
            Phase::Waiting if now >= self.expiry => self.fail(FailureReason::Expired),
            Phase::Connecting if now >= self.expiry => self.fail(FailureReason::Unreachable),
            Phase::Connected { .. }
                if self.last_heard.is_some_and(|t| now >= t + CONSENT_TIMEOUT) =>
            {
                self.fail(FailureReason::Disconnected);
            }
            _ => {}
        }
    }
}

fn payload_error(e: payload::PayloadError) -> PeerError {
    PeerError::Code(match e {
        payload::PayloadError::NewerVersion => rbs_peer::CodeError::NewerVersion { version: 2 },
        _ => rbs_peer::CodeError::Malformed,
    })
}
