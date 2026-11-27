//! The pairing session traits (docs/specs/peer-port.md, "Contract").
//!
//! An adapter runs each pairing on one owner thread (ADR-0139 decision 2, Threading).
//! Every call here returns at once: candidate gathering, connectivity checks and DTLS
//! run on that thread, and their progress is read through `state` and `poll_event`.

use std::any::Any;
use std::fmt;
use std::sync::Arc;

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
    /// Starts an invite: validates the settings, opens one socket and starts the owner
    /// thread, which gathers candidates. Returns at once; the code is ready when
    /// [`PendingInvite::state`] is [`InviteState::Waiting`].
    fn create_invite(&self, settings: &InviteSettings)
    -> Result<Box<dyn PendingInvite>, PeerError>;
    /// Reads and checks an invite, then starts the answering session the same way.
    /// Returns at once; the answer code is ready when the session leaves
    /// [`PeerState::Gathering`] ([`PeerSession::answer_code`]).
    fn accept_invite(
        &self,
        invite: &str,
        settings: &AcceptSettings,
    ) -> Result<Box<dyn PeerSession>, PeerError>;
}

/// Where an invite is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum InviteState {
    /// Gathering candidates; no code yet.
    Gathering,
    /// The code is ready and waits for its answer.
    Waiting,
    /// An answer was taken; the session owns the connection now.
    Used,
    /// The invite failed or expired ([`FailureReason::Expired`]); it does not recover.
    Failed(FailureReason),
    /// Closed by this side.
    Closed,
}

/// An invite waiting for its answer.
pub trait PendingInvite: Send {
    /// The pairing session.
    fn session_id(&self) -> SessionId;
    /// The current state.
    fn state(&self) -> InviteState;
    /// The invite code, once gathering is done. A secret: never logged, never in events.
    fn code(&self) -> Option<&PairingCode>;
    /// Advisory expiry, seconds since the Unix epoch.
    fn expires_at_unix(&self) -> u64;
    /// Takes the answer. A malformed or wrong answer leaves the invite usable. After
    /// success the invite is [`InviteState::Used`]: the returned session owns the
    /// socket and the owner thread, further answers are [`PeerError::AlreadyUsed`],
    /// and [`PendingInvite::close`] no longer touches the connection.
    fn accept_answer(&mut self, answer: &str) -> Result<Box<dyn PeerSession>, PeerError>;
    /// Closes the socket and the owner thread unless a session took them. Idempotent.
    fn close(&mut self);
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
    /// The answering side is gathering candidates; no answer code yet.
    Gathering,
    /// Codes exchanged (or the answer code is ready); connectivity checks and DTLS run.
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
    /// The answering side's answer code is ready.
    AnswerReady,
    /// DTLS is connected and the check phrase is known.
    Connected {
        /// The phrase both users compare.
        phrase: CheckPhrase,
    },
    /// This side is confirmed by its user, or, when its check is off, right after
    /// [`PeerEvent::Connected`].
    Confirmed,
    /// The pairing failed.
    Failed(FailureReason),
    /// The session closed.
    Closed,
}

/// An opaque handle to a session's media path. The composition root passes it to the
/// adapter's transport (sending side) or the guest source (receiving side), which
/// downcast it to their own type; engine code never looks inside (ADR-0139 decision 2).
#[derive(Clone)]
pub struct MediaHandle(Arc<dyn Any + Send + Sync>);

impl MediaHandle {
    /// Wraps an adapter's media type.
    pub fn new<T: Any + Send + Sync>(inner: Arc<T>) -> Self {
        Self(inner)
    }

    /// The adapter's media type, when it is `T`.
    pub fn downcast<T: Any + Send + Sync>(&self) -> Option<Arc<T>> {
        Arc::clone(&self.0).downcast::<T>().ok()
    }
}

impl fmt::Debug for MediaHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MediaHandle")
    }
}

/// A paired (or pairing) session.
pub trait PeerSession: Send {
    /// The pairing session id.
    fn session_id(&self) -> SessionId;
    /// This side's role.
    fn role(&self) -> PeerRole;
    /// The current state.
    fn state(&self) -> PeerState;
    /// The answer code of an answering session, once gathering is done; `None` on the
    /// offering side. A secret: never logged, never in events.
    fn answer_code(&self) -> Option<&PairingCode>;
    /// The next event, oldest first. Never blocks.
    fn poll_event(&mut self) -> Option<PeerEvent>;
    /// The check phrase, once DTLS is connected.
    fn check_phrase(&self) -> Option<CheckPhrase>;
    /// This side's user confirmed that the phrases match.
    /// [`PeerError::NotConnected`] before DTLS is up.
    fn confirm_phrase(&mut self) -> Result<(), PeerError>;
    /// The media path. Media flows through it only while this side is confirmed.
    fn media(&self) -> MediaHandle;
    /// Ends the session and its owner thread. Idempotent.
    fn close(&mut self);
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn a_media_handle_downcasts_only_to_its_own_type() {
        let handle = MediaHandle::new(Arc::new(42_u32));
        assert_eq!(*handle.downcast::<u32>().unwrap(), 42);
        assert!(handle.downcast::<u64>().is_none());
        assert_eq!(format!("{handle:?}"), "MediaHandle");
    }
}
