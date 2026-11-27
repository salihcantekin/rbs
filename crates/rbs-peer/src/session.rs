//! The pairing session traits (docs/specs/peer-port.md, "Contract").
//!
//! An adapter implements these on its own owner thread per session (ADR-0139
//! decision 2, Threading). Every call here is cheap and never blocks.

use crate::code::{PairingCode, SessionId, TransportKind};
use crate::error::{FailureReason, PeerError};
use crate::phrase::CheckPhrase;
use crate::settings::{AcceptSettings, InviteSettings};

/// One pairing transport. Registered by type id, such as `org.rbs.peer.webrtc`.
pub trait PeerFactory: Send + Sync {
    /// The registry type id.
    fn type_id(&self) -> &'static str;
    /// The transport this factory's codes name.
    fn transport(&self) -> TransportKind;
    /// Starts an invite: one socket, candidate gathering bounded by the settings'
    /// timeout, and the session's owner thread. Control thread only.
    fn create_invite(&self, settings: &InviteSettings)
    -> Result<Box<dyn PendingInvite>, PeerError>;
    /// Reads an invite and returns the answer code with its session, which connects
    /// once the inviting side takes the answer. Control thread only.
    fn accept_invite(
        &self,
        invite: &str,
        settings: &AcceptSettings,
    ) -> Result<PendingAnswer, PeerError>;
}

/// An invite waiting for its answer.
pub trait PendingInvite: Send {
    /// The pairing session.
    fn session_id(&self) -> SessionId;
    /// The invite code. A secret: never logged and never in events.
    fn code(&self) -> &PairingCode;
    /// Advisory expiry, seconds since the Unix epoch.
    fn expires_at_unix(&self) -> u64;
    /// Takes the answer. A malformed or wrong answer leaves the invite usable; after
    /// success the invite is used and further answers are [`PeerError::AlreadyUsed`].
    fn accept_answer(&mut self, answer: &str) -> Result<Box<dyn PeerSession>, PeerError>;
    /// Closes the socket and the owner thread. Idempotent.
    fn close(&mut self);
}

/// What accepting an invite gives: the answer code to send back and the session.
#[non_exhaustive]
pub struct PendingAnswer {
    /// The answer code. A secret: never logged and never in events.
    pub code: PairingCode,
    /// The session, [`PeerState::Connecting`] until the inviting side takes the answer.
    pub session: Box<dyn PeerSession>,
}

impl PendingAnswer {
    /// Pairs a code with its session (for adapters).
    pub fn new(code: PairingCode, session: Box<dyn PeerSession>) -> Self {
        Self { code, session }
    }
}

/// Which side of a pairing a session is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PeerRole {
    /// The sharing side, which made the invite and sends media.
    Offerer,
    /// The host, which made the answer and receives media.
    Answerer,
}

/// Where a session is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PeerState {
    /// Codes exchanged; connectivity checks and DTLS are running.
    Connecting,
    /// DTLS is up. `confirmed` says whether this side may use media yet.
    Connected {
        /// This side's check is confirmed by its user, or turned off.
        confirmed: bool,
    },
    /// The pairing failed; it does not recover.
    Failed(FailureReason),
    /// Closed by this side or the other.
    Closed,
}

/// Something that happened to a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PeerEvent {
    /// DTLS is connected and the check phrase is known.
    Connected {
        /// The phrase both users compare.
        phrase: CheckPhrase,
    },
    /// This side is confirmed (by its user, or because its check is off).
    Confirmed,
    /// The pairing failed.
    Failed(FailureReason),
    /// The session closed.
    Closed,
}

/// A paired (or pairing) session.
pub trait PeerSession: Send {
    /// The pairing session id.
    fn session_id(&self) -> SessionId;
    /// This side's role.
    fn role(&self) -> PeerRole;
    /// The current state.
    fn state(&self) -> PeerState;
    /// The next event, oldest first. Never blocks.
    fn poll_event(&mut self) -> Option<PeerEvent>;
    /// The check phrase, once DTLS is connected.
    fn check_phrase(&self) -> Option<CheckPhrase>;
    /// This side's user confirmed that the phrases match.
    /// [`PeerError::NotConnected`] before DTLS is up.
    fn confirm_phrase(&mut self) -> Result<(), PeerError>;
    /// Ends the session and its owner thread. Idempotent.
    fn close(&mut self);
}
