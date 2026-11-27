# Spec: Peer port (guest pairing)

- Status: Draft, 2026-11-27. Lands with the first code of ADR-0139 phase 1b (`rbs-peer`), before any adapter. Needs an independent review (P14) before the adapter PR builds on it.
- Owner crate(s): `rbs-peer` (port), adapter `rbs-peer-str0m` (planned, phase 1b)
- Related ADRs: [0139](../adr/0139-remote-guests-rbs-to-rbs-over-webrtc.md), [0013](../adr/0013-zero-cost-when-unused.md), [0019](../adr/0019-encode-mux-transport-ports.md), [0046](../adr/0046-network-access-over-tls.md), [0055](../adr/0055-crash-resilience.md)
- Related specs: [output-ports.md](output-ports.md), [backend-ports.md](backend-ports.md), [control-api.md](control-api.md)

## Purpose

Two RBS installations pair with no server of ours (ADR-0139): the sharing side makes an **invite code**, the host turns it into an **answer code**, the sharing side pastes the answer back, and the two connect peer to peer. Both sides then show a **check phrase** that the users compare by voice before any media is used. This port defines what is the same for every transport that can do that: the code format, the pairing session, its events, its failure reasons and the check phrase. What a transport needs inside a code (for WebRTC: ICE credentials, candidates, the DTLS fingerprint, codecs) is an opaque payload owned by the adapter.

## Scope

In scope: the code format and its version rule, expiry and the live-session rules, pairing settings and their bounds, the check phrase, the pairing session traits, typed errors and failure reasons.

Out of scope: how media is packetised and sent (the adapter, and `Transport` in [output-ports.md](output-ports.md) for the sending half), live decoding ([backend-ports.md](backend-ports.md), phase 1c), the guest share output and the guest source (their own specs), the control API methods (phase 1b, [control-api.md](control-api.md)).

The port crate has no network, codec or crypto dependency (ADR-0139 decision 2; its only external crate may be `bytes`, as for every port). Hashing for the check phrase is done by the adapter, which already owns a crypto backend.

## Code format (contract, versioned)

A code is text: the prefix `rbs-guest:`, the decimal format version, `:`, then the body in base64url without padding (RFC 4648 section 5).

```text
rbs-guest:1:<base64url body>
```

- The whole code is at most **4096 bytes** (`MAX_CODE_LEN`). A reader first removes all ASCII whitespace (chat apps wrap long lines), then checks the length, then the prefix, then decodes. Input over 16 KiB before whitespace removal is refused without scanning it.
- Version 1 is the only version. A reader refuses a higher version with `CodeError::NewerVersion` (UI text: "This code is from a newer RBS. Update RBS and try again."). Version 0, a missing version or a version with anything but ASCII digits is `Malformed`. A new version is a minor change only when old readers can refuse it cleanly by the prefix alone (ADR-0139 decision 5).
- The prefix is matched case-sensitively; base64url is decoded strictly (no padding, no `+` or `/`, no non-zero trailing bits).

Version 1 body, all integers big-endian:

| Offset | Size | Field |
| --- | --- | --- |
| 0 | 1 | kind: `1` invite, `2` answer; anything else is `Malformed` |
| 1 | 1 | transport: `1` WebRTC; anything else is `UnknownTransport` |
| 2 | 16 | session id, chosen by the inviting side from a cryptographic random source; all zero is `Malformed` |
| 18 | 8 | expiry, seconds since the Unix epoch (advisory, see below) |
| 26 | 2 | payload length `n` |
| 28 | `n` | payload, owned by the transport adapter |

Nothing may follow the payload (`Malformed`). A code carries only these fields. It must never carry a stream key, an API token, a user name, a device name or a file path; the adapter's payload rule is the same (ADR-0139 decision 4). The WebRTC adapter's payload is a compressed SDP with at most 8 candidates; its format belongs to `rbs-peer-str0m` and is versioned inside the payload.

## Expiry and live sessions

- The issuer's live session state is the only authority (ADR-0139 decision 4). An unknown, closed, used or expired session id is refused by the issuer whatever the code says. Pending invites and answers are never saved; an app restart invalidates them.
- The expiry inside a code is advisory. A reader refuses a code whose expiry is more than **120 s** (`EXPIRY_TOLERANCE`) in the past by its own wall clock, so moderate clock skew between two computers is accepted.
- An answer is bound to the invite's session id (checked by `Envelope::expect`) and to the offerer's fingerprint (checked by the adapter, inside DTLS).
- An invite is single-use. After `accept_answer` succeeds, any further answer is `AlreadyUsed`. A malformed or wrong answer does not use up the invite, so a bad paste can be retried.

## Settings and bounds

| Setting | Default | Bounds | Side |
| --- | --- | --- | --- |
| Invite lifetime | 10 min | 1 to 30 min | sharing |
| Answer lifetime | 3 min | 1 to 10 min | host |
| Candidate gathering timeout | 3 s | 0.5 to 10 s | both |
| STUN servers | the default list in [user-data.md](user-data.md) | at most 8 entries, each `host:port` or `stun:host:port`, at most 253 bytes; empty means local network only | both |
| Leave out private host addresses | off | | both |
| Check phrase | on | | each side for itself |

Settings are validated by the port (`InviteSettings::validate`, `AcceptSettings::validate`) before an adapter sees them; an out-of-range value is `SettingsError`, never clamped. No STUN query, socket or thread exists before `create_invite` or `accept_invite` is called (P13, ADR-0139 decision 7). Keep-alive timing (a STUN binding refresh every 20 s while a code is pending, connectivity checks kept running on the answering side until the answer expires) is the adapter's duty and is tested there.

## Check phrase

Both sides show six words derived from the session and the two DTLS-authenticated certificate fingerprints, never from the code text (ADR-0139 decision 6):

```text
input  = "rbs-guest-phrase-v1" ‖ session id (16 bytes)
         ‖ u16 len ‖ offerer fingerprint ‖ u16 len ‖ answerer fingerprint
digest = SHA-256(input)
words  = the first 66 bits of digest, as six 11-bit indices, most significant bit first,
         into the BIP-39 English word list
```

- `phrase_input` builds `input` (lengths big-endian; a fingerprint is the raw certificate digest bytes, 1 to 64 bytes). The adapter hashes it with SHA-256 and passes the digest to `CheckPhrase::from_digest`, which needs at least 9 bytes.
- The word list and its order are part of this contract. A phrase is shown as the six words separated by single spaces.
- Gates: the sharing side sends no media until its user confirms the phrase; the guest source publishes nothing (shown as *Not verified*) until the host confirms. A side that turned its check off is confirmed at connect. A session exposes `confirm_phrase` and its `PeerState` says whether this side is confirmed.

## Contract

All public enums and structs that may grow are `#[non_exhaustive]`. Every call returns a typed error; none panics (P9).

```rust
/// One pairing transport (ADR-0162 registry style). Type id such as `org.rbs.peer.webrtc`.
pub trait PeerFactory: Send + Sync {
    fn type_id(&self) -> &'static str;
    fn transport(&self) -> TransportKind;
    /// Starts an invite: opens one socket, gathers candidates (bounded by the
    /// gathering timeout) and starts the session's owner thread. Control thread.
    fn create_invite(&self, settings: &InviteSettings) -> Result<Box<dyn PendingInvite>, PeerError>;
    /// Reads an invite, gathers candidates and returns the answer code together with
    /// the session, which connects once the inviting side pastes the answer.
    fn accept_invite(&self, invite: &str, settings: &AcceptSettings) -> Result<PendingAnswer, PeerError>;
}

pub trait PendingInvite: Send {
    fn session_id(&self) -> SessionId;
    /// The invite code. A secret: never logged, never in events (ADR-0139 decision 14).
    fn code(&self) -> &PairingCode;
    fn expires_at_unix(&self) -> u64;
    /// Takes the answer. Wrong or malformed answers leave the invite usable;
    /// after success the invite is used.
    fn accept_answer(&mut self, answer: &str) -> Result<Box<dyn PeerSession>, PeerError>;
    /// Closes the socket and the owner thread. Idempotent.
    fn close(&mut self);
}

#[non_exhaustive]
pub struct PendingAnswer {
    pub code: PairingCode,
    pub session: Box<dyn PeerSession>,
}

pub trait PeerSession: Send {
    fn session_id(&self) -> SessionId;
    fn role(&self) -> PeerRole;          // Offerer (the sharing side) or Answerer (the host)
    fn state(&self) -> PeerState;
    /// Next event, oldest first; never blocks.
    fn poll_event(&mut self) -> Option<PeerEvent>;
    /// Known once DTLS is connected.
    fn check_phrase(&self) -> Option<CheckPhrase>;
    /// This side's user confirmed that the phrases match.
    fn confirm_phrase(&mut self) -> Result<(), PeerError>;
    /// Ends the session and its owner thread. Idempotent.
    fn close(&mut self);
}

#[non_exhaustive]
pub enum PeerState {
    /// Codes exchanged; connectivity checks and DTLS running.
    Connecting,
    /// DTLS up; `confirmed` says whether this side may use media.
    Connected { confirmed: bool },
    Failed(FailureReason),
    Closed,
}

#[non_exhaustive]
pub enum PeerEvent {
    /// DTLS is connected and the check phrase is known.
    Connected { phrase: CheckPhrase },
    /// This side is confirmed (by its user, or because its check is off).
    Confirmed,
    Failed(FailureReason),
    Closed,
}
```

Media (draft, fixed by the adapter PR): the sending half is a `Transport` (input = packets) built by the composition root from an opaque session handle; the receiving half gives encoded packets with the sender's master-clock capture time through a bounded lock-free queue. Neither is a public type of this crate yet.

### Failure reasons

Stable ids, shown in the UI and the control API (ADR-0139 decisions 7, 11, 12):

| Reason | Id | When |
| --- | --- | --- |
| `NoPublicAddress` | `guest_no_public_address` | no server-reflexive candidate could be gathered and the peer is not on the local network |
| `Unreachable` | `guest_unreachable` | no candidate pair succeeded before the answer expired |
| `Disconnected` | `guest_disconnected` | consent lost (RFC 7675, about 30 s without reply); final, new codes are needed |
| `Limits` | `guest_limits` | the peer exceeded the rate limit or sent a refused stream |
| `FingerprintMismatch` | `guest_fingerprint_mismatch` | the DTLS certificate does not match the code |
| `Expired` | `guest_expired` | the invite or answer expired before the pairing finished |

### Errors

`CodeError` (reading a code): `NotACode`, `TooLarge`, `Malformed`, `NewerVersion { version }`, `UnknownTransport`, `WrongKind`, `WrongSession`, `Expired`.

`PeerError`: `Code(CodeError)`, `Settings(SettingsError)`, `UnknownSession`, `AlreadyUsed`, `NotConnected`, `Closed`, `Failed(FailureReason)`, `Io(String)` (socket errors with no secret in the text).

### Secrets

`PairingCode` holds a code. Its `Debug` and `Display` print `PairingCode(<redacted>)`; the text is read only through `expose()`. While a code is live the composition root registers it with `rbs_crash::add_secret` (ADR-0139 decision 14), and the adapter registers the ICE username fragment and password the same way.

## Tests

In `rbs-peer`: codes round-trip; every malformed field, an oversized code, a newer version, a wrong kind and a wrong session are refused with their typed error; whitespace inside a pasted code is ignored; expiry within the tolerance is accepted and outside it refused; settings bounds; the check phrase is the same for the same input, differs when any input byte differs, matches a fixed test vector, and `phrase_input` is unambiguous (moving a byte between the two fingerprints changes it). A seeded generator round-trips a few thousand random envelopes (no new dependency). The adapter's tests (in-memory network on `TestClock`, NAT expiry, consent loss) are listed in ADR-0139 decision 16. A fuzz target for `Envelope::decode` is added with the adapter.
