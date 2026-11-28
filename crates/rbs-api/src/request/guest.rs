//! Guest shares (protocol 1.116, ADR-0139 decision 14): this RBS sends the main
//! canvas's program to another RBS. Codes are returned only to `admin` tokens and are
//! never in an event or a log line.

use serde::{Deserialize, Serialize};

/// `guest.share` (`admin`, `live`): an invite of the main canvas's program. Refused
/// with `conflict` and `data.reason` `guest_return_feed` while the program draws a
/// guest, `no_encoder_slot` or `too_many_outputs` when no slot is free; nothing is
/// opened then.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct GuestShare {
    /// How long the invite stays valid, 60 to 1800 seconds; default 600.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifetime_s: Option<u32>,
    /// Compare the check phrase before the program is sent; default true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_phrase: Option<bool>,
    /// Private addresses only, no STUN query; default false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_only: Option<bool>,
    /// The picture; default 1280x720 at 2500 kbit/s. The frame rate is the canvas's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video: Option<GuestShareVideo>,
}

/// The picture of [`GuestShare`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct GuestShareVideo {
    /// Even, 320 to 1920.
    pub width: u32,
    /// Even, 180 to 1080.
    pub height: u32,
    /// Start and highest bitrate, 300 to 8000 kbit/s; the network may lower it.
    pub bitrate_kbps: u32,
}

/// The result of `guest.share`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GuestShareStarted {
    /// The share's id.
    pub share: u32,
    /// Its state, `gathering` at first.
    pub state: GuestState,
}

/// One share, for `guest.share.code`, `guest.share.confirm` and `guest.share.stop`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct GuestShareRef {
    /// The share's id.
    pub share: u32,
}

/// `guest.share.code` (`admin`, `read`): the invite code. `conflict` while it is
/// still being made or after the answer was taken.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GuestCode {
    /// The code, a secret: show it, copy it, never log it.
    pub code: String,
    /// When it expires, seconds since the Unix epoch.
    pub expires_at: u64,
}

impl std::fmt::Debug for GuestCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GuestCode")
            .field("code", &"<secret>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// `guest.share.accept_answer` (`admin`, `live`): the guest's answer code. A bad
/// answer is `invalid_params` with `data.reason` set to a `guest_*` code reason, and
/// the share keeps waiting.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct GuestAcceptAnswer {
    /// The share's id.
    pub share: u32,
    /// The answer code, a secret.
    pub answer: String,
}

impl std::fmt::Debug for GuestAcceptAnswer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GuestAcceptAnswer")
            .field("share", &self.share)
            .field("answer", &"<secret>")
            .finish()
    }
}

/// The state after `guest.share.accept_answer` or `guest.share.confirm`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GuestShareState {
    /// Its state.
    pub state: GuestState,
}

/// Where a share is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum GuestState {
    /// Gathering candidates; no code yet.
    Gathering,
    /// The invite code is ready and waits for the answer.
    WaitingForAnswer,
    /// Connectivity checks and DTLS run.
    Connecting,
    /// Connected; this side has not confirmed the phrase.
    CheckPhrase,
    /// Sending.
    Live,
    /// Ended for `reason`; new codes are needed.
    Failed,
    /// Stopped or closed.
    Closed,
    /// A state this client does not know.
    #[serde(other)]
    Other,
}

/// `guest.status` (`read`) and the shares of `guest.changed`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GuestStatus {
    /// The shares, oldest first; an ended one stays until the next `guest.share`.
    pub shares: Vec<GuestShareRow>,
}

/// One share of [`GuestStatus`]. Never holds a code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct GuestShareRow {
    /// The share's id.
    pub share: u32,
    /// Its state.
    pub state: GuestState,
    /// The six check words, once connected; not a secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phrase: Option<String>,
    /// Why it failed or closed: a `guest_*` id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// When the invite expires, seconds since the Unix epoch, while it waits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    /// The picture's width.
    pub width: u32,
    /// The picture's height.
    pub height: u32,
    /// The start and highest bitrate in kbit/s.
    pub bitrate_kbps: u32,
}

/// The guest methods (protocol 1.116): one [`Command`](super::Command) variant, so
/// the namespace's dispatch stays in one place.
#[derive(Debug, Clone, PartialEq)]
pub enum GuestCommand {
    /// `guest.share`.
    Share(GuestShare),
    /// `guest.share.code`.
    ShareCode(GuestShareRef),
    /// `guest.share.accept_answer`.
    AcceptAnswer(GuestAcceptAnswer),
    /// `guest.share.confirm`.
    Confirm(GuestShareRef),
    /// `guest.share.stop`.
    Stop(GuestShareRef),
    /// `guest.status`.
    Status,
}

impl GuestCommand {
    /// The method's wire name.
    pub fn method(&self) -> &'static str {
        match self {
            Self::Share(_) => "guest.share",
            Self::ShareCode(_) => "guest.share.code",
            Self::AcceptAnswer(_) => "guest.share.accept_answer",
            Self::Confirm(_) => "guest.share.confirm",
            Self::Stop(_) => "guest.share.stop",
            Self::Status => "guest.status",
        }
    }

    /// It opens a connection to a computer the caller chooses (ADR-0139 decision 14).
    pub fn reaches_network(&self) -> bool {
        matches!(self, Self::Share(_) | Self::AcceptAnswer(_))
    }
}
