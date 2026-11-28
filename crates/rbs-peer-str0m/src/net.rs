//! The owner thread of one pairing (ADR-0139 decision 2, Threading) and the port's
//! facades over it. The thread owns the UDP socket and the [`Engine`]; the control
//! thread reads a small shared snapshot and sends commands. Created only by
//! `create_invite` or `accept_invite` (P13); it ends with the pairing.

use std::collections::VecDeque;
use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::panic::AssertUnwindSafe;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rbs_peer::{
    AcceptSettings, CheckPhrase, CodeKind, Envelope, FailureReason, InviteSettings, InviteState,
    MediaHandle, PairingCode, PeerError, PeerEvent, PeerFactory, PeerRole, PeerSession, PeerState,
    PendingInvite, SessionId, SideSettings, TransportKind,
};
use ring::rand::{SecureRandom, SystemRandom};

use crate::engine::{Engine, Phase, Setup};
use crate::link::{OwnerSide, WebRtcMedia};
use crate::stun::TransactionId;

/// Registry type id of this adapter.
pub const TYPE_ID: &str = "org.rbs.peer.webrtc";
/// Longest wait of the socket loop, so commands are seen promptly.
const LOOP_WAIT: Duration = Duration::from_millis(20);
/// How long a control-thread call waits for the owner thread's reply.
const REPLY_WAIT: Duration = Duration::from_secs(2);
/// A UDP datagram's largest size.
const RECV_BUFFER: usize = 2048;

/// What the control thread sees of a pairing.
#[derive(Debug)]
struct Snapshot {
    phase: Phase,
    code: Option<PairingCode>,
    phrase: Option<CheckPhrase>,
    events: VecDeque<PeerEvent>,
    expires_at_unix: u64,
    answer_taken: bool,
    /// Closed by this side: the owner thread's later updates are ignored.
    closed: bool,
    /// Registered with `rbs_crash`; removed when the owner thread ends.
    secrets: Vec<String>,
}

impl Snapshot {
    fn is_over(&self) -> bool {
        matches!(self.phase, Phase::Failed(_) | Phase::Closed)
    }

    /// Ends the pairing with `reason` unless it already ended.
    fn fail(&mut self, reason: FailureReason) {
        if !self.is_over() {
            self.phase = Phase::Failed(reason);
            self.events.push_back(PeerEvent::Failed(reason));
        }
    }
}

type Shared = Arc<Mutex<Snapshot>>;

fn lock(shared: &Shared) -> MutexGuard<'_, Snapshot> {
    // A poisoned lock means the owner thread panicked mid-update; the snapshot is
    // still a plain value, so reading it on is safe.
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

enum Command {
    AcceptAnswer(String, SyncSender<Result<(), PeerError>>),
    Confirm(SyncSender<Result<(), PeerError>>),
    Close,
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn random<const N: usize>(rng: &SystemRandom) -> [u8; N] {
    let mut out = [0u8; N];
    // SystemRandom only fails when the OS has no entropy source at all; a zero id is
    // then refused by `SessionId::new`, and a zero transaction id only costs a retry.
    let _ = rng.fill(&mut out);
    out
}

/// The address of the interface the default route uses, found without sending
/// anything (connecting a UDP socket only picks a route).
fn primary_interface() -> Option<IpAddr> {
    let probe = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    probe.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).ok()?;
    let ip = probe.local_addr().ok()?.ip();
    (!ip.is_unspecified()).then_some(ip)
}

/// Resolves `host:port` or `stun:host:port` entries to IPv4 addresses. Runs on the
/// owner thread (DNS may block).
fn resolve(servers: &[String]) -> Vec<SocketAddr> {
    servers
        .iter()
        .filter_map(|s| s.strip_prefix("stun:").unwrap_or(s).to_socket_addrs().ok())
        .flat_map(|addrs| addrs.filter(SocketAddr::is_ipv4).take(1))
        .collect()
}

/// Built for each pairing on the owner thread.
type Build = Box<
    dyn FnOnce(Setup, Instant, u64, &mut dyn FnMut() -> TransactionId) -> Result<Engine, PeerError>
        + Send,
>;

struct Owner {
    commands: Sender<Command>,
    shared: Shared,
    media: Arc<WebRtcMedia>,
    /// Dropped, not joined, on close: the thread may be inside a DNS lookup, and a
    /// close must not block the caller. It ends at its next loop turn.
    thread: Option<JoinHandle<()>>,
}

impl Owner {
    fn spawn(
        session_id: SessionId,
        side: SideSettings,
        lifetime: Duration,
        shared: Shared,
        build: Build,
    ) -> Result<Self, PeerError> {
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
            .map_err(|_| PeerError::Io("cannot open a UDP socket".into()))?;
        let (commands, rx) = mpsc::channel();
        let owned = Arc::clone(&shared);
        let media = WebRtcMedia::new();
        let link = OwnerSide::new(&media, &socket);
        let pairing = Pairing {
            session_id,
            side,
            lifetime,
        };
        let thread = std::thread::Builder::new()
            .name("rbs-peer".into())
            .spawn(move || owner_main(socket, pairing, owned, build, rx, link))
            .map_err(|_| PeerError::Io("cannot start the pairing thread".into()))?;
        Ok(Self {
            commands,
            shared,
            media,
            thread: Some(thread),
        })
    }

    fn ask(
        &self,
        make: impl FnOnce(SyncSender<Result<(), PeerError>>) -> Command,
    ) -> Result<(), PeerError> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.commands
            .send(make(tx))
            .map_err(|_| PeerError::Closed)?;
        rx.recv_timeout(REPLY_WAIT).map_err(|_| PeerError::Closed)?
    }

    fn close(&mut self) {
        let _ = self.commands.send(Command::Close);
        self.thread = None;
        let mut s = lock(&self.shared);
        if !s.is_over() {
            s.phase = Phase::Closed;
            s.events.push_back(PeerEvent::Closed);
        }
        s.closed = true;
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        self.close();
    }
}

/// The owner thread: runs the pairing, then makes sure the snapshot shows an end
/// (also after a panic, which `rbs_crash` reports) and forgets the secrets.
/// What one pairing starts with.
struct Pairing {
    session_id: SessionId,
    side: SideSettings,
    lifetime: Duration,
}

fn owner_main(
    socket: UdpSocket,
    pairing: Pairing,
    shared: Shared,
    build: Build,
    commands: Receiver<Command>,
    mut link: OwnerSide,
) {
    let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
        run(socket, pairing, &shared, build, commands, &mut link);
    }));
    link.end();
    let mut s = lock(&shared);
    s.fail(if outcome.is_err() {
        FailureReason::Disconnected
    } else {
        FailureReason::Unreachable
    });
    for secret in s.secrets.drain(..) {
        rbs_crash::remove_secret(&secret);
    }
}

fn run(
    socket: UdpSocket,
    pairing: Pairing,
    shared: &Shared,
    build: Build,
    commands: Receiver<Command>,
    link: &mut OwnerSide,
) {
    let Pairing {
        session_id,
        side,
        lifetime,
    } = pairing;
    let rng = SystemRandom::new();
    // Transaction ids must be unpredictable (an off-path host could otherwise forge a
    // STUN answer); with no working random source the pairing does not start.
    if rng.fill(&mut [0u8; 12]).is_err() {
        lock(shared).fail(FailureReason::Unreachable);
        return;
    }
    let mut ids = || random::<12>(&rng);
    let port = socket.local_addr().map_or(0, |a| a.port());
    let local = SocketAddr::new(
        primary_interface().unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        port,
    );
    let setup = Setup {
        session_id,
        stun_servers: resolve(&side.stun_servers),
        side,
        lifetime,
        local,
    };
    let mut engine = match build(setup, Instant::now(), unix_now(), &mut ids) {
        Ok(engine) => engine,
        Err(_) => {
            lock(shared).fail(FailureReason::Unreachable);
            return;
        }
    };
    lock(shared).expires_at_unix = engine.expires_at_unix();
    let mut buf = vec![0u8; RECV_BUFFER];
    loop {
        loop {
            // The snapshot is updated before each reply, so a caller that reads the
            // state right after a call sees its effect.
            match commands.try_recv() {
                Ok(Command::AcceptAnswer(answer, reply)) => {
                    let result = engine.accept_answer(&answer, Instant::now(), unix_now());
                    if result.is_ok() {
                        lock(shared).answer_taken = true;
                    }
                    publish(&mut engine, shared);
                    let _ = reply.send(result);
                }
                Ok(Command::Confirm(reply)) => {
                    let result = engine.confirm();
                    publish(&mut engine, shared);
                    let _ = reply.send(result);
                }
                // Closed, or every handle dropped.
                Ok(Command::Close) | Err(TryRecvError::Disconnected) => engine.close(),
                Err(TryRecvError::Empty) => break,
            }
            if engine.phase() == Phase::Closed {
                break;
            }
        }
        let now = Instant::now();
        engine.handle_timeout(now, &mut ids);
        link.pump(&mut engine, now);
        while let Some((to, bytes)) = engine.poll_transmit() {
            // A send error (no route, buffer full) is a lost datagram; ICE retries.
            let _ = socket.send_to(&bytes, to);
        }
        publish(&mut engine, shared);
        if matches!(engine.phase(), Phase::Failed(_) | Phase::Closed) {
            return;
        }
        let wait = engine.next_timeout(now).saturating_duration_since(now);
        let wait = wait.clamp(Duration::from_millis(1), LOOP_WAIT);
        if link.before_wait() {
            link.after_wait();
            continue;
        }
        if socket.set_read_timeout(Some(wait)).is_err() {
            return; // `owner_main` ends the pairing.
        }
        let got = socket.recv_from(&mut buf);
        link.after_wait();
        match got {
            Ok((n, from)) if link.is_wake(from, &buf[..n]) => {}
            Ok((n, from)) => engine.handle_datagram(Instant::now(), from, &buf[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::ConnectionReset
                ) => {}
            Err(_) => engine.close(),
        }
    }
}

fn publish(engine: &mut Engine, shared: &Shared) {
    let mut s = lock(shared);
    for secret in engine.take_secrets() {
        rbs_crash::add_secret(&secret);
        s.secrets.push(secret);
    }
    if s.closed {
        return;
    }
    s.phase = engine.phase();
    if s.code.is_none() {
        s.code = engine.code().cloned();
    }
    s.phrase = engine.phrase();
    while let Some(event) = engine.poll_event() {
        s.events.push_back(event);
    }
}

fn snapshot(expires_at_unix: u64) -> Shared {
    Arc::new(Mutex::new(Snapshot {
        phase: Phase::Gathering,
        code: None,
        phrase: None,
        events: VecDeque::new(),
        expires_at_unix,
        answer_taken: false,
        closed: false,
        secrets: Vec::new(),
    }))
}

/// The WebRTC pairing factory (`org.rbs.peer.webrtc`).
#[derive(Debug, Default)]
pub struct WebRtcPeerFactory;

impl PeerFactory for WebRtcPeerFactory {
    fn type_id(&self) -> &'static str {
        TYPE_ID
    }

    fn transport(&self) -> TransportKind {
        TransportKind::WebRtc
    }

    fn create_invite(
        &self,
        settings: &InviteSettings,
    ) -> Result<Box<dyn PendingInvite>, PeerError> {
        settings.validate()?;
        let rng = SystemRandom::new();
        let session_id = SessionId::new(random::<16>(&rng))
            .ok_or_else(|| PeerError::Io("no random source".into()))?;
        let shared = snapshot(unix_now().saturating_add(settings.lifetime.as_secs()));
        let build: Build = Box::new(|setup, now, unix, ids| Engine::offerer(setup, now, unix, ids));
        let owner = Owner::spawn(
            session_id,
            settings.side.clone(),
            settings.lifetime,
            Arc::clone(&shared),
            build,
        )?;
        Ok(Box::new(Invite {
            session_id,
            shared,
            owner: Some(owner),
            code: OnceLock::new(),
        }))
    }

    fn accept_invite(
        &self,
        invite: &str,
        settings: &AcceptSettings,
    ) -> Result<Box<dyn PeerSession>, PeerError> {
        settings.validate()?;
        // Refuse a bad code at once, before any socket or thread exists.
        let envelope = Envelope::decode(invite)?;
        envelope.expect(CodeKind::Invite, None, unix_now())?;
        if envelope.transport != TransportKind::WebRtc {
            return Err(PeerError::Code(rbs_peer::CodeError::UnknownTransport));
        }
        let sdp = crate::payload::decode(&envelope.payload)
            .map_err(|_| PeerError::Code(rbs_peer::CodeError::Malformed))?;
        if crate::engine::sdp_fingerprint(&sdp).is_none() {
            return Err(PeerError::Code(rbs_peer::CodeError::Malformed));
        }
        let shared = snapshot(unix_now().saturating_add(settings.lifetime.as_secs()));
        let text = invite.to_owned();
        let build: Build =
            Box::new(move |setup, now, unix, ids| Engine::answerer(&text, setup, now, unix, ids));
        let owner = Owner::spawn(
            envelope.session_id,
            settings.side.clone(),
            settings.lifetime,
            Arc::clone(&shared),
            build,
        )?;
        let media = owner.media.handle();
        Ok(Box::new(Session {
            session_id: envelope.session_id,
            role: PeerRole::Answerer,
            shared,
            owner,
            code: OnceLock::new(),
            media,
        }))
    }
}

struct Invite {
    session_id: SessionId,
    shared: Shared,
    owner: Option<Owner>,
    code: OnceLock<PairingCode>,
}

impl PendingInvite for Invite {
    fn session_id(&self) -> SessionId {
        self.session_id
    }

    fn state(&self) -> InviteState {
        let s = lock(&self.shared);
        if s.answer_taken && !s.is_over() {
            return InviteState::Used;
        }
        match s.phase {
            Phase::Gathering => InviteState::Gathering,
            Phase::Waiting => InviteState::Waiting,
            Phase::Failed(reason) => InviteState::Failed(reason),
            Phase::Closed => InviteState::Closed,
            Phase::Connecting | Phase::Connected { .. } => InviteState::Used,
        }
    }

    fn code(&self) -> Option<&PairingCode> {
        lend(&self.code, &self.shared)
    }

    fn expires_at_unix(&self) -> u64 {
        lock(&self.shared).expires_at_unix
    }

    fn accept_answer(&mut self, answer: &str) -> Result<Box<dyn PeerSession>, PeerError> {
        let owner = self.owner.as_ref().ok_or(PeerError::AlreadyUsed)?;
        let text = answer.to_owned();
        owner.ask(|reply| Command::AcceptAnswer(text, reply))?;
        let owner = self.owner.take().ok_or(PeerError::AlreadyUsed)?;
        let media = owner.media.handle();
        Ok(Box::new(Session {
            session_id: self.session_id,
            role: PeerRole::Offerer,
            shared: Arc::clone(&self.shared),
            owner,
            code: OnceLock::new(),
            media,
        }))
    }

    fn close(&mut self) {
        if let Some(mut owner) = self.owner.take() {
            owner.close();
        }
    }
}

/// Lends the code: copied out of the snapshot once the owner thread made it, so a
/// `&self` call can return a reference that outlives the lock.
fn lend<'a>(cell: &'a OnceLock<PairingCode>, shared: &Shared) -> Option<&'a PairingCode> {
    if let Some(code) = cell.get() {
        return Some(code);
    }
    let code = lock(shared).code.clone()?;
    Some(cell.get_or_init(|| code))
}

struct Session {
    session_id: SessionId,
    role: PeerRole,
    shared: Shared,
    owner: Owner,
    code: OnceLock<PairingCode>,
    media: MediaHandle,
}

impl PeerSession for Session {
    fn session_id(&self) -> SessionId {
        self.session_id
    }

    fn role(&self) -> PeerRole {
        self.role
    }

    fn state(&self) -> PeerState {
        lock(&self.shared).phase.peer_state()
    }

    fn answer_code(&self) -> Option<&PairingCode> {
        match self.role {
            PeerRole::Answerer => lend(&self.code, &self.shared),
            _ => None,
        }
    }

    fn poll_event(&mut self) -> Option<PeerEvent> {
        lock(&self.shared).events.pop_front()
    }

    fn check_phrase(&self) -> Option<CheckPhrase> {
        lock(&self.shared).phrase
    }

    fn confirm_phrase(&mut self) -> Result<(), PeerError> {
        self.owner.ask(Command::Confirm)
    }

    fn media(&self) -> MediaHandle {
        self.media.clone()
    }

    fn close(&mut self) {
        self.owner.close();
    }
}
