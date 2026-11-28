//! Peer port: pairing two RBS installations with no server of ours (ADR-0139).
//!
//! The sharing side makes an invite code, the host answers it, and the two connect
//! peer to peer; both users then compare a check phrase before media is used. This
//! crate holds what every pairing transport shares: the versioned code format, the
//! pairing settings and their bounds, the check phrase, the session traits and the
//! typed failure reasons. The transport's own part of a code is an opaque payload
//! owned by its adapter (`rbs-peer-str0m` for WebRTC).
//!
//! The `conformance` feature adds the suite every adapter passes ([`conformance`]).
//!
//! No network, codec or crypto dependency (principle P1). Contract:
//! `docs/specs/peer-port.md`.

pub mod code;
#[cfg(feature = "conformance")]
pub mod conformance;
pub mod error;
pub mod phrase;
pub mod receive;
pub mod session;
pub mod settings;
mod words;

pub use code::{
    CODE_FORMAT_VERSION, CODE_PREFIX, CodeKind, EXPIRY_TOLERANCE, Envelope, MAX_CODE_LEN,
    PairingCode, SessionId, TransportKind, check_expiry,
};
pub use error::{CodeError, FailureReason, PeerError, PhraseError, SettingsError};
pub use phrase::{CheckPhrase, PHRASE_DOMAIN, phrase_input};
pub use receive::{FrameTrack, PeerReceiver, ReceivedFrame};
pub use session::{
    InviteState, MediaHandle, PeerEvent, PeerFactory, PeerRole, PeerSession, PeerState,
    PendingInvite,
};
pub use settings::{AcceptSettings, InviteSettings, SideSettings};
