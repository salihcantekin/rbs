//! Typed errors and failure reasons (docs/specs/peer-port.md, "Errors").

use std::fmt;

/// Why a pasted code was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CodeError {
    /// The text does not start with `rbs-guest:`.
    NotACode,
    /// The code is longer than the format allows.
    TooLarge,
    /// The code is damaged or incomplete.
    Malformed,
    /// The code uses a newer format than this RBS reads.
    NewerVersion {
        /// The version the code names.
        version: u32,
    },
    /// The code is for a transport this RBS does not have.
    UnknownTransport,
    /// An answer was pasted where an invite belongs, or the other way round.
    WrongKind,
    /// The answer belongs to another invite.
    WrongSession,
    /// The code's expiry has passed.
    Expired,
}

impl fmt::Display for CodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotACode => "This is not an RBS guest code.",
            Self::TooLarge => "This code is too long to be an RBS guest code.",
            Self::Malformed => "This code is damaged or incomplete. Copy it again.",
            Self::NewerVersion { .. } => "This code is from a newer RBS. Update RBS and try again.",
            Self::UnknownTransport => "This code needs a connection type this RBS does not have.",
            Self::WrongKind => "This is the other kind of code: paste the invite code here.",
            Self::WrongSession => "This answer code belongs to another invite.",
            Self::Expired => "This code has expired. Ask for a new one.",
        })
    }
}

impl std::error::Error for CodeError {}

/// A pairing setting outside its bounds (docs/specs/peer-port.md, "Settings and bounds").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SettingsError {
    /// The invite or answer lifetime is outside its bounds.
    Lifetime,
    /// The candidate gathering timeout is outside its bounds.
    GatheringTimeout,
    /// More STUN servers than allowed.
    TooManyStunServers,
    /// A STUN server entry is empty, too long or not `host:port`.
    StunServer,
}

impl fmt::Display for SettingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Lifetime => "the code lifetime is outside its allowed range",
            Self::GatheringTimeout => "the address search time is outside its allowed range",
            Self::TooManyStunServers => "too many STUN servers",
            Self::StunServer => "a STUN server is not written as host:port",
        })
    }
}

impl std::error::Error for SettingsError {}

/// A check phrase could not be made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PhraseError {
    /// The digest is shorter than 9 bytes.
    ShortDigest,
    /// A fingerprint is empty or longer than 64 bytes.
    BadFingerprint,
}

impl fmt::Display for PhraseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ShortDigest => "the check phrase digest is too short",
            Self::BadFingerprint => "a certificate fingerprint has an invalid length",
        })
    }
}

impl std::error::Error for PhraseError {}

/// Why a pairing failed. Each has a stable id for the UI and the control API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FailureReason {
    /// No public address could be found and the peer is not on the local network.
    NoPublicAddress,
    /// No connection path worked before the answer expired.
    Unreachable,
    /// The connection was lost; new codes are needed.
    Disconnected,
    /// The peer exceeded the rate limit or sent a refused stream.
    Limits,
    /// The peer's certificate does not match its code.
    FingerprintMismatch,
    /// The invite or answer expired before the pairing finished.
    Expired,
}

impl FailureReason {
    /// The stable id (`guest_*`).
    pub fn id(self) -> &'static str {
        match self {
            Self::NoPublicAddress => "guest_no_public_address",
            Self::Unreachable => "guest_unreachable",
            Self::Disconnected => "guest_disconnected",
            Self::Limits => "guest_limits",
            Self::FingerprintMismatch => "guest_fingerprint_mismatch",
            Self::Expired => "guest_expired",
        }
    }
}

impl fmt::Display for FailureReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NoPublicAddress => {
                "No public address was found. Check the STUN servers or the network."
            }
            Self::Unreachable => "The other computer could not be reached.",
            Self::Disconnected => "The connection was lost. Make new codes to connect again.",
            Self::Limits => "The other side sent more than RBS accepts.",
            Self::FingerprintMismatch => "The other side is not the one that made the code.",
            Self::Expired => "The code expired before the connection was made.",
        })
    }
}

/// An error from a pairing call.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PeerError {
    /// A pasted code was refused.
    Code(CodeError),
    /// A setting is out of bounds.
    Settings(SettingsError),
    /// The code names a session this RBS does not have (closed, or before a restart).
    UnknownSession,
    /// The invite already took an answer.
    AlreadyUsed,
    /// The call needs a connected session.
    NotConnected,
    /// The session or invite is closed.
    Closed,
    /// The pairing failed.
    Failed(FailureReason),
    /// A socket error. The text holds no code, credential or address.
    Io(String),
}

impl fmt::Display for PeerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Code(e) => e.fmt(f),
            Self::Settings(e) => write!(f, "invalid pairing setting: {e}"),
            Self::UnknownSession => f.write_str("This code is no longer valid. Ask for a new one."),
            Self::AlreadyUsed => f.write_str("This invite was already used. Make a new one."),
            Self::NotConnected => f.write_str("the pairing is not connected yet"),
            Self::Closed => f.write_str("the pairing is closed"),
            Self::Failed(reason) => reason.fmt(f),
            Self::Io(text) => write!(f, "network error: {text}"),
        }
    }
}

impl std::error::Error for PeerError {}

impl From<CodeError> for PeerError {
    fn from(e: CodeError) -> Self {
        Self::Code(e)
    }
}

impl From<SettingsError> for PeerError {
    fn from(e: SettingsError) -> Self {
        Self::Settings(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_ids_are_stable_and_distinct() {
        let all = [
            FailureReason::NoPublicAddress,
            FailureReason::Unreachable,
            FailureReason::Disconnected,
            FailureReason::Limits,
            FailureReason::FingerprintMismatch,
            FailureReason::Expired,
        ];
        let mut ids: Vec<&str> = all.iter().map(|r| r.id()).collect();
        assert!(ids.iter().all(|id| id.starts_with("guest_")));
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), all.len());
        assert_eq!(FailureReason::Disconnected.id(), "guest_disconnected");
    }
}
