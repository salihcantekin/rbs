//! Two engines through a simulated network on simulated time (ADR-0139 decision 16,
//! ADR-0032): NATs with a mapping timeout and address-restricted filtering, a STUN
//! server, loss. Nothing sleeps; time only moves when the test moves it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use rbs_peer::{CodeError, Envelope, FailureReason, PeerError, PeerEvent, SessionId, SideSettings};

use crate::engine::{Engine, Phase, Setup, sdp_candidates, sdp_fingerprint};
use crate::payload;
use crate::stun::{self, TransactionId};

const UNIX_START: u64 = 1_800_000_000;
const STUN_SERVER: &str = "198.51.100.1:3478";
const STEP: Duration = Duration::from_millis(20);
/// A NAT forgets an idle mapping after this long (decision 16's test model).
const NAT_TIMEOUT: Duration = Duration::from_secs(30);

fn addr(s: &str) -> SocketAddr {
    s.parse().unwrap()
}

/// An address-restricted NAT with one public port per private socket.
#[derive(Debug)]
struct Nat {
    public: SocketAddr,
    /// Last time the private side sent to each remote IP.
    sent_to: HashMap<IpAddr, Instant>,
    last_out: Option<Instant>,
}

impl Nat {
    fn mapped(&self, now: Instant) -> bool {
        self.last_out.is_some_and(|t| now < t + NAT_TIMEOUT)
    }
    fn admits(&self, from: SocketAddr, now: Instant) -> bool {
        self.mapped(now)
            && self
                .sent_to
                .get(&from.ip())
                .is_some_and(|t| now < *t + NAT_TIMEOUT)
    }
}

struct Peer {
    engine: Engine,
    nat: Option<Nat>,
    local: SocketAddr,
}

impl Peer {
    /// How the outside world sees this peer.
    fn outside(&self) -> SocketAddr {
        self.nat.as_ref().map_or(self.local, |n| n.public)
    }
    /// Whether a datagram sent to `to` reaches this peer.
    fn is(&self, to: SocketAddr) -> bool {
        self.outside() == to
    }
}

struct World {
    t0: Instant,
    now: Instant,
    peers: Vec<Peer>,
    ids: u64,
    /// Drop every datagram while set.
    cut: bool,
}

fn next_id(counter: &mut u64) -> TransactionId {
    *counter += 1;
    let mut id = [0u8; 12];
    id[..8].copy_from_slice(&counter.to_be_bytes());
    id
}

fn side(stun: bool) -> SideSettings {
    let mut side = SideSettings::with_stun_servers(if stun {
        vec![STUN_SERVER.to_owned()]
    } else {
        Vec::new()
    });
    side.check_phrase = true;
    side
}

fn setup(local: SocketAddr, stun: bool, lifetime: Duration) -> Setup {
    Setup {
        session_id: SessionId::new([9; 16]).unwrap(),
        side: side(stun),
        lifetime,
        local,
        stun_servers: if stun {
            vec![addr(STUN_SERVER)]
        } else {
            Vec::new()
        },
    }
}

impl World {
    fn new() -> Self {
        let t0 = Instant::now();
        Self {
            t0,
            now: t0,
            peers: Vec::new(),
            ids: 0,
            cut: false,
        }
    }

    fn unix(&self) -> u64 {
        UNIX_START + (self.now - self.t0).as_secs()
    }

    fn nat(public: &str) -> Option<Nat> {
        Some(Nat {
            public: addr(public),
            sent_to: HashMap::new(),
            last_out: None,
        })
    }

    fn add_offerer(&mut self, private: &str, nat: Option<&str>, s: Setup) -> usize {
        let mut ids = self.ids;
        let engine = Engine::offerer(
            Setup {
                local: addr(private),
                ..s
            },
            self.now,
            self.unix(),
            || next_id(&mut ids),
        )
        .unwrap();
        self.ids = ids + 100;
        self.push(engine, private, nat)
    }

    fn add_answerer(
        &mut self,
        invite: &str,
        private: &str,
        nat: Option<&str>,
        s: Setup,
    ) -> Result<usize, PeerError> {
        let mut ids = self.ids;
        let engine = Engine::answerer(
            invite,
            Setup {
                local: addr(private),
                ..s
            },
            self.now,
            self.unix(),
            || next_id(&mut ids),
        )?;
        self.ids = ids + 100;
        Ok(self.push(engine, private, nat))
    }

    fn push(&mut self, engine: Engine, private: &str, nat: Option<&str>) -> usize {
        let nat = nat.and_then(Self::nat);
        self.peers.push(Peer {
            engine,
            nat,
            local: addr(private),
        });
        self.peers.len() - 1
    }

    /// Moves time by `by` in steps, delivering every datagram.
    fn run(&mut self, by: Duration) {
        let end = self.now + by;
        while self.now < end {
            self.now += STEP;
            for i in 0..self.peers.len() {
                let mut ids = self.ids;
                let now = self.now;
                self.peers[i]
                    .engine
                    .handle_timeout(now, || next_id(&mut ids));
                self.ids = ids;
                self.flush(i);
            }
        }
    }

    fn flush(&mut self, i: usize) {
        while let Some((to, bytes)) = self.peers[i].engine.poll_transmit() {
            let now = self.now;
            let from = self.peers[i].outside();
            if let Some(nat) = self.peers[i].nat.as_mut() {
                nat.last_out = Some(now);
                nat.sent_to.insert(to.ip(), now);
            }
            if self.cut {
                continue;
            }
            if to == addr(STUN_SERVER) {
                // The STUN server answers with the address it saw.
                if let Some(id) = stun::tests::request_id(&bytes) {
                    let reply = stun::tests::success(&id, from);
                    self.peers[i].engine.handle_datagram(now, to, &reply);
                }
                continue;
            }
            let target = self.peers.iter().position(|p| p.is(to));
            let Some(j) = target else { continue };
            if let Some(nat) = &self.peers[j].nat
                && !nat.admits(from, now)
            {
                continue;
            }
            self.peers[j].engine.handle_datagram(now, from, &bytes);
            self.flush(j);
        }
    }

    fn engine(&mut self, i: usize) -> &mut Engine {
        &mut self.peers[i].engine
    }

    fn code(&self, i: usize) -> String {
        self.peers[i].engine.code().unwrap().expose().to_owned()
    }

    fn events(&mut self, i: usize) -> Vec<PeerEvent> {
        std::iter::from_fn(|| self.peers[i].engine.poll_event()).collect()
    }
}

/// Pairs an offerer and an answerer; returns their indices.
fn pair(w: &mut World, nat: bool, stun: bool) -> (usize, usize) {
    let life = Duration::from_secs(600);
    let a = if nat {
        w.add_offerer(
            "192.168.1.10:5000",
            Some("203.0.113.10:40000"),
            setup(addr("0.0.0.0:0"), stun, life),
        )
    } else {
        w.add_offerer("10.0.0.1:5000", None, setup(addr("0.0.0.0:0"), stun, life))
    };
    w.run(Duration::from_secs(4));
    let invite = w.code(a);
    let h = if nat {
        w.add_answerer(
            &invite,
            "192.168.7.20:6000",
            Some("203.0.113.20:50000"),
            setup(addr("0.0.0.0:0"), stun, Duration::from_secs(180)),
        )
    } else {
        w.add_answerer(
            &invite,
            "10.0.0.2:6000",
            None,
            setup(addr("0.0.0.0:0"), stun, Duration::from_secs(180)),
        )
    }
    .unwrap();
    w.run(Duration::from_secs(4));
    (a, h)
}

#[test]
fn two_peers_on_one_network_pair_and_see_the_same_phrase() {
    let mut w = World::new();
    let (a, h) = pair(&mut w, false, false);
    assert_eq!(w.engine(h).phase(), Phase::Connecting);
    let answer = w.code(h);
    let (now, unix) = (w.now, w.unix());
    w.engine(a).accept_answer(&answer, now, unix).unwrap();
    // Both sides' ICE username fragment and password are handed out as secrets.
    for side in [a, h] {
        let secrets = w.engine(side).take_secrets();
        assert_eq!(secrets.len(), 4, "{side}: local and remote ufrag and pwd");
        assert!(secrets.iter().all(|s| s.len() >= 4));
        assert!(w.engine(side).take_secrets().is_empty());
    }
    w.run(Duration::from_secs(3));
    assert_eq!(w.engine(a).phase(), Phase::Connected { confirmed: false });
    assert_eq!(w.engine(h).phase(), Phase::Connected { confirmed: false });
    let pa = w.engine(a).phrase().unwrap();
    assert_eq!(Some(pa), w.engine(h).phrase());
    assert!(w.events(a).contains(&PeerEvent::Connected { phrase: pa }));
    assert!(w.events(h).contains(&PeerEvent::AnswerReady));
    w.engine(a).confirm().unwrap();
    assert_eq!(w.engine(a).phase(), Phase::Connected { confirmed: true });
    assert_eq!(w.events(a), vec![PeerEvent::Confirmed]);
    assert_eq!(
        w.engine(h).phase(),
        Phase::Connected { confirmed: false },
        "each side confirms itself"
    );
}

fn connect(w: &mut World, a: usize, h: usize) {
    let answer = w.code(h);
    let (now, unix) = (w.now, w.unix());
    w.engine(a).accept_answer(&answer, now, unix).unwrap();
    // Wait for the condition, bounded in simulated time only.
    for _ in 0..30 {
        let up = |p: Phase| matches!(p, Phase::Connected { .. } | Phase::Failed(_));
        if up(w.engine(a).phase()) && up(w.engine(h).phase()) {
            return;
        }
        w.run(Duration::from_secs(1));
    }
}

#[test]
fn peers_behind_nats_pair_through_their_stun_addresses() {
    let mut w = World::new();
    let (a, h) = pair(&mut w, true, true);
    let invite = w.code(a);
    let sdp =
        crate::payload::decode(&rbs_peer::Envelope::decode(&invite).unwrap().payload).unwrap();
    assert!(
        sdp.contains("typ srflx"),
        "the invite carries the public address"
    );
    assert!(sdp.contains("203.0.113.10 40000"));
    connect(&mut w, a, h);
    assert!(matches!(w.engine(a).phase(), Phase::Connected { .. }));
    assert_eq!(w.engine(a).phrase(), w.engine(h).phrase());
}

#[test]
fn an_answer_pasted_two_minutes_fifty_later_still_connects_through_nat() {
    let mut w = World::new();
    let (a, h) = pair(&mut w, true, true);
    // Longer than the NAT's 30 s mapping timeout many times over: only the keep-alives
    // (the offerer's STUN refresh, the answerer's checks) keep both paths open.
    w.run(Duration::from_secs(170));
    assert_eq!(w.engine(h).phase(), Phase::Connecting);
    assert_eq!(w.engine(a).phase(), Phase::Waiting);
    connect(&mut w, a, h);
    assert!(
        matches!(w.engine(a).phase(), Phase::Connected { .. }),
        "{:?}",
        w.engine(a).phase()
    );
    assert!(matches!(w.engine(h).phase(), Phase::Connected { .. }));
}

#[test]
fn a_substituted_invite_gives_different_phrases() {
    // Mallory replaces the guest's invite with her own towards the host, and answers
    // the guest's invite herself. Each leg connects, but the phrases differ.
    let mut w = World::new();
    let life = Duration::from_secs(600);
    let guest = w.add_offerer("10.0.0.1:5000", None, setup(addr("0.0.0.0:0"), false, life));
    let mallory_offer = w.add_offerer(
        "10.0.0.66:5000",
        None,
        setup(addr("0.0.0.0:0"), false, life),
    );
    w.run(Duration::from_secs(4));
    let guest_invite = w.code(guest);
    let mallory_invite = w.code(mallory_offer);
    let host = w
        .add_answerer(
            &mallory_invite,
            "10.0.0.2:6000",
            None,
            setup(addr("0.0.0.0:0"), false, life),
        )
        .unwrap();
    let mallory_answer = w
        .add_answerer(
            &guest_invite,
            "10.0.0.67:6000",
            None,
            setup(addr("0.0.0.0:0"), false, life),
        )
        .unwrap();
    w.run(Duration::from_secs(4));
    connect(&mut w, mallory_offer, host);
    connect(&mut w, guest, mallory_answer);
    let host_phrase = w.engine(host).phrase().unwrap();
    let guest_phrase = w.engine(guest).phrase().unwrap();
    assert_ne!(host_phrase, guest_phrase);
    // And the host publishes nothing until its user confirms (decision 6).
    assert_eq!(
        w.engine(host).phase(),
        Phase::Connected { confirmed: false }
    );
}

#[test]
fn wrong_reused_and_expired_codes_are_refused() {
    let mut w = World::new();
    let (a, h) = pair(&mut w, false, false);
    let invite = w.code(a);
    let answer = w.code(h);
    let (now, unix) = (w.now, w.unix());
    // An invite where an answer belongs.
    assert_eq!(
        w.engine(a).accept_answer(&invite, now, unix),
        Err(PeerError::Code(CodeError::WrongKind))
    );
    // An answer for another session.
    let other = w.add_offerer(
        "10.0.0.5:5000",
        None,
        Setup {
            session_id: SessionId::new([4; 16]).unwrap(),
            ..setup(addr("0.0.0.0:0"), false, Duration::from_secs(600))
        },
    );
    w.run(Duration::from_secs(4));
    let (now, unix) = (w.now, w.unix());
    assert_eq!(
        w.engine(other).accept_answer(&answer, now, unix),
        Err(PeerError::Code(CodeError::WrongSession))
    );
    assert_eq!(
        w.engine(other).phase(),
        Phase::Waiting,
        "a bad paste leaves the invite usable"
    );
    // The right answer, then the same again.
    w.engine(a).accept_answer(&answer, now, unix).unwrap();
    assert_eq!(
        w.engine(a).accept_answer(&answer, now, unix),
        Err(PeerError::AlreadyUsed)
    );
    // An invite read after its expiry.
    w.run(Duration::from_secs(600 + 121));
    let late = Engine::answerer(
        &w.code(other),
        setup(addr("10.0.0.9:1"), false, Duration::from_secs(180)),
        w.now,
        w.unix(),
        || [1; 12],
    );
    assert_eq!(late.err(), Some(PeerError::Code(CodeError::Expired)));
}

#[test]
fn an_invite_expires_while_it_waits() {
    let mut w = World::new();
    let a = w.add_offerer(
        "10.0.0.1:5000",
        None,
        setup(addr("0.0.0.0:0"), false, Duration::from_secs(60)),
    );
    w.run(Duration::from_secs(61));
    assert_eq!(w.engine(a).phase(), Phase::Failed(FailureReason::Expired));
    assert!(
        w.events(a)
            .contains(&PeerEvent::Failed(FailureReason::Expired))
    );
}

#[test]
fn consent_loss_is_final() {
    let mut w = World::new();
    let (a, h) = pair(&mut w, false, false);
    connect(&mut w, a, h);
    assert!(matches!(w.engine(a).phase(), Phase::Connected { .. }));
    w.cut = true;
    w.run(crate::engine::CONSENT_TIMEOUT + Duration::from_secs(1));
    assert_eq!(
        w.engine(a).phase(),
        Phase::Failed(FailureReason::Disconnected)
    );
    assert_eq!(
        w.engine(h).phase(),
        Phase::Failed(FailureReason::Disconnected)
    );
    w.cut = false;
    w.run(Duration::from_secs(10));
    assert_eq!(
        w.engine(a).phase(),
        Phase::Failed(FailureReason::Disconnected),
        "nothing recovers"
    );
}

#[test]
fn no_public_address_fails_when_private_addresses_are_left_out() {
    let mut w = World::new();
    let mut s = setup(addr("0.0.0.0:0"), false, Duration::from_secs(600));
    s.side.omit_private_addresses = true;
    let a = w.add_offerer("192.168.1.10:5000", Some("203.0.113.10:40000"), s);
    w.run(Duration::from_secs(4));
    assert_eq!(
        w.engine(a).phase(),
        Phase::Failed(FailureReason::NoPublicAddress)
    );
}

#[test]
fn a_side_without_the_check_is_confirmed_at_connect() {
    let mut w = World::new();
    let life = Duration::from_secs(600);
    let mut s = setup(addr("0.0.0.0:0"), false, life);
    s.side.check_phrase = false;
    let a = w.add_offerer("10.0.0.1:5000", None, s);
    w.run(Duration::from_secs(4));
    let invite = w.code(a);
    let h = w
        .add_answerer(
            &invite,
            "10.0.0.2:6000",
            None,
            setup(addr("0.0.0.0:0"), false, life),
        )
        .unwrap();
    w.run(Duration::from_secs(4));
    connect(&mut w, a, h);
    assert_eq!(w.engine(a).phase(), Phase::Connected { confirmed: true });
    let events = w.events(a);
    let connected = events
        .iter()
        .position(|e| matches!(e, PeerEvent::Connected { .. }))
        .unwrap();
    assert_eq!(events.get(connected + 1), Some(&PeerEvent::Confirmed));
    assert_eq!(w.engine(h).phase(), Phase::Connected { confirmed: false });
}

#[test]
fn an_answer_that_is_never_pasted_ends_unreachable() {
    let mut w = World::new();
    let (_a, h) = pair(&mut w, false, false);
    w.run(Duration::from_secs(180));
    assert_eq!(
        w.engine(h).phase(),
        Phase::Failed(FailureReason::Unreachable)
    );
}

/// A small seeded generator (xorshift64*), so the mutation test is the same each run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

fn mutate(rng: &mut Rng, input: &[u8], alphabet: &[u8]) -> Vec<u8> {
    let mut out = input.to_vec();
    for _ in 0..=rng.below(3) {
        let at = rng.below(out.len());
        match rng.below(4) {
            0 if !out.is_empty() => out[at] = alphabet[rng.below(alphabet.len())],
            1 if !out.is_empty() => {
                out.remove(at);
            }
            2 => out.insert(at, alphabet[rng.below(alphabet.len())]),
            _ => out.truncate(at),
        }
    }
    out
}

/// The in-process stand-in for the fuzz target (ADR-0139 decision 16): thousands of
/// damaged real codes and payloads are refused with a typed error, never a panic,
/// and none of them starts a pairing with the wrong peer.
#[test]
fn damaged_codes_and_payloads_never_panic() {
    const ROUNDS: usize = 300;
    let mut w = World::new();
    let (a, h) = pair(&mut w, false, false);
    let codes = [w.code(a), w.code(h)];
    let payload = Envelope::decode(&codes[0]).unwrap().payload;
    let base64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_ :\n";
    let any: Vec<u8> = (0..=255).collect();
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut accepted = 0;
    for i in 0..ROUNDS {
        let code = mutate(&mut rng, codes[i % 2].as_bytes(), base64);
        let text = String::from_utf8_lossy(&code);
        // The answerer's checks before its engine starts (`Engine::answerer`); what
        // follows them does not depend on the code's bytes.
        let parsed = Envelope::decode(&text)
            .ok()
            .and_then(|env| payload::decode(&env.payload).ok())
            .filter(|sdp| {
                let _ = sdp_candidates(sdp);
                sdp_fingerprint(sdp).is_some()
            })
            .is_some_and(|sdp| str0m::change::SdpOffer::from_sdp_string(&sdp).is_ok());
        accepted += usize::from(parsed);
        let bytes = mutate(&mut rng, &payload, &any);
        if let Ok(sdp) = payload::decode(&bytes) {
            let _ = (sdp_fingerprint(&sdp), sdp_candidates(&sdp));
            let _ = str0m::change::SdpOffer::from_sdp_string(&sdp);
        }
    }
    // A change that still parses (a dropped trailing pad, an edited expiry) may be
    // taken, but such a peer then has a different fingerprint or session; the
    // phrase tests prove that cannot pass unnoticed.
    assert!(
        accepted < ROUNDS / 10,
        "most damaged codes are refused: {accepted}"
    );
}

/// The code `code` with its SDP changed by `edit`, sealed again.
fn edited(code: &str, edit: impl FnOnce(&str) -> String) -> String {
    let mut env = Envelope::decode(code).unwrap();
    let sdp = payload::decode(&env.payload).unwrap();
    env.payload = payload::encode(&edit(&sdp));
    env.encode().unwrap().expose().to_owned()
}

#[test]
fn an_invite_with_a_second_fingerprint_is_refused() {
    // A media-level fingerprint wins over the session-level one in DTLS, so an
    // attacker could make the phrase cover the real certificate and DTLS check hers.
    let mut w = World::new();
    let (a, _) = pair(&mut w, false, false);
    let forged = edited(&w.code(a), |sdp| {
        let other: Vec<String> = (0..32).map(|i| format!("{:02X}", i + 1)).collect();
        sdp.replacen(
            "\r\nm=",
            &format!("\r\na=fingerprint:sha-256 {}\r\nm=", other.join(":")),
            1,
        )
    });
    let s = setup(addr("0.0.0.0:0"), false, Duration::from_secs(600));
    let result = w.add_answerer(&forged, "10.0.0.9:6000", None, s);
    assert_eq!(
        result.err(),
        Some(PeerError::Code(CodeError::Malformed)),
        "two fingerprints"
    );
    let sha1 = edited(&w.code(a), |sdp| {
        sdp.replace("a=fingerprint:sha-256 ", "a=fingerprint:sha-1 ")
    });
    let s = setup(addr("0.0.0.0:0"), false, Duration::from_secs(600));
    assert!(
        w.add_answerer(&sha1, "10.0.0.9:6001", None, s).is_err(),
        "not SHA-256"
    );
}

#[test]
fn a_damaged_answer_leaves_the_invite_waiting_for_the_right_one() {
    let mut w = World::new();
    let (a, h) = pair(&mut w, false, false);
    let answer = w.code(h);
    let (now, unix) = (w.now, w.unix());
    let one_media = edited(&answer, |sdp| {
        let cut = sdp.rfind("\r\nm=").unwrap();
        sdp[..cut + 2].to_owned()
    });
    assert_eq!(
        w.engine(a).accept_answer(&one_media, now, unix),
        Err(PeerError::Code(CodeError::Malformed))
    );
    let no_password = edited(&answer, |sdp| {
        sdp.lines()
            .filter(|l| !l.starts_with("a=ice-pwd:"))
            .map(|l| format!("{l}\r\n"))
            .collect()
    });
    assert!(w.engine(a).accept_answer(&no_password, now, unix).is_err());
    assert_eq!(w.engine(a).phase(), Phase::Waiting, "still waiting");
    connect(&mut w, a, h);
    assert_eq!(w.engine(a).phase(), Phase::Connected { confirmed: false });
}

#[test]
fn a_code_with_eight_candidates_fits_the_code_limit() {
    let mut w = World::new();
    let (a, _) = pair(&mut w, false, false);
    let full = edited(&w.code(a), |sdp| {
        let lines: String = (0..8)
            .map(|i| {
                format!(
                    "a=candidate:{i}234567890 1 udp 2130706431 2001:db8:ffff:ffff:ffff:ffff:ffff:{i:04x} 65535 typ srflx raddr 2001:db8::1 rport 65535\r\n"
                )
            })
            .collect();
        let kept: String = sdp
            .lines()
            .filter(|l| !l.starts_with("a=candidate:"))
            .map(|l| format!("{l}\r\n"))
            .collect();
        format!("{kept}{lines}")
    });
    assert!(full.len() <= rbs_peer::MAX_CODE_LEN, "{}", full.len());
    assert_eq!(
        sdp_candidates(&payload::decode(&Envelope::decode(&full).unwrap().payload).unwrap()).len(),
        8
    );
}
