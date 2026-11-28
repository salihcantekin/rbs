# Spec: Peer port (guest pairing)

- Status: Draft, 2026-11-27. Lands with the first code of ADR-0139 phase 1b (`rbs-peer`), before any adapter. Independent review round 1 (2026-11-27, approve with changes, 13 points) applied; see Review at the end.
- Owner crate(s): `rbs-peer` (port), adapter `rbs-peer-str0m` (pairing and connection since 2026-11-27; media path, share transport and guest receiver since 2026-11-28)
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

- The whole code is at most **4096 bytes** (`MAX_CODE_LEN`). A reader first removes whitespace of any kind and the characters chat apps insert into long text (U+200B, U+200C, U+200D, U+2060, U+FEFF, U+00AD; none is in base64url), then checks the length, then the prefix, then decodes. Input over 16 KiB before whitespace removal is refused without scanning it.
- Version 1 is the only version. A reader refuses a higher version with `CodeError::NewerVersion` (UI text: "This code is from a newer RBS. Update RBS and try again."). Version 0, a missing version, a leading zero (`01`) or a version with anything but ASCII digits is `Malformed`, so a code has one spelling. A version of digits too large for `u32` is `NewerVersion` too. A new version is a minor change only when old readers can refuse it cleanly by the prefix alone (ADR-0139 decision 5).
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
- The expiry inside a code is advisory. A reader refuses a code whose expiry is more than **120 s** (`EXPIRY_TOLERANCE`) in the past by its own wall clock, so moderate clock skew between two computers is accepted. It refuses as `Malformed` an expiry further ahead than its kind's longest lifetime plus the tolerance (30 min for an invite, 10 min for an answer), since the host has no issuer to ask about an invite.
- An answer is bound to the invite's session id (checked by `Envelope::expect`) and to the offerer's fingerprint (checked by the adapter, inside DTLS).
- An invite is single-use. After `accept_answer` succeeds, any further answer is `AlreadyUsed`. A malformed or wrong answer does not use up the invite, so a bad paste can be retried.

## Settings and bounds

| Setting | Default | Bounds | Side |
| --- | --- | --- | --- |
| Invite lifetime | 10 min | 1 to 30 min | sharing |
| Answer lifetime | 3 min | 1 to 10 min | host |
| Candidate gathering timeout | 3 s | 0.5 to 10 s | both |

The minimums (1 min lifetime, 0.5 s gathering) and the 10 s gathering maximum are not in ADR-0139; they are this spec's choice and part of the owner's acceptance of it.
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
    /// Validates the settings, opens one socket and starts the owner thread, which
    /// gathers candidates. Returns at once (never blocks the UI or an API worker).
    fn create_invite(&self, settings: &InviteSettings) -> Result<Box<dyn PendingInvite>, PeerError>;
    /// Reads and checks the invite, then starts the answering session the same way.
    fn accept_invite(&self, invite: &str, settings: &AcceptSettings) -> Result<Box<dyn PeerSession>, PeerError>;
}

pub enum InviteState { Gathering, Waiting, Used, Failed(FailureReason), Closed }

pub trait PendingInvite: Send {
    fn session_id(&self) -> SessionId;
    fn state(&self) -> InviteState;
    /// The invite code once gathering is done. A secret (ADR-0139 decision 14).
    fn code(&self) -> Option<&PairingCode>;
    fn expires_at_unix(&self) -> u64;
    /// Wrong or malformed answers leave the invite usable. On success the returned
    /// session owns the socket and the owner thread, the invite is `Used`, and
    /// `close` no longer touches the connection.
    fn accept_answer(&mut self, answer: &str) -> Result<Box<dyn PeerSession>, PeerError>;
    fn close(&mut self);
}

pub trait PeerSession: Send {
    fn session_id(&self) -> SessionId;
    fn role(&self) -> PeerRole;          // Offerer (the sharing side) or Answerer (the host)
    fn state(&self) -> PeerState;        // Gathering (answerer only), Connecting, Connected { confirmed }, Failed, Closed
    /// The answering side's answer code once gathering is done; None on the offerer.
    fn answer_code(&self) -> Option<&PairingCode>;
    fn poll_event(&mut self) -> Option<PeerEvent>;   // AnswerReady, Connected { phrase }, Confirmed, Failed, Closed
    fn check_phrase(&self) -> Option<CheckPhrase>;
    fn confirm_phrase(&mut self) -> Result<(), PeerError>;
    /// Opaque media path for the adapter's transport or the guest source.
    fn media(&self) -> MediaHandle;
    fn close(&mut self);
}
```

`Confirmed` follows `Connected` at once on a side whose check is off. Expiry while gathering or waiting is `InviteState::Failed(Expired)` or `PeerState::Failed(Expired)`.

Media: `MediaHandle` wraps an `Arc` of the adapter's own type and is downcast only by that adapter's code (its `Transport` on the sending side, built by the composition root; its packet reader in the guest source on the receiving side). The sending half is a `Transport` (input = packets); the receiving half gives encoded packets with the sender's master-clock capture time through a bounded lock-free queue. Those types belong to the adapter, not to this crate (`rbs-peer-str0m`: `WebRtcMedia::transport` and `WebRtcMedia::receiver`, below). The adapter enforces the gates: its transport sends nothing and its packet reader yields nothing while its side is not confirmed, and the adapter's tests prove it (ADR-0139 decision 16).

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

Which error for an expired or unknown pairing:

| Situation | Result |
| --- | --- |
| A reader's advisory check of a pasted code (`Envelope::expect`) | `CodeError::Expired` (or `Malformed` for a far-future expiry) |
| `accept_answer` for an invite whose session expired, closed or is from before a restart | `PeerError::UnknownSession` (the issuer is the authority) |
| An invite or session that expires while gathering, waiting or connecting | `InviteState::Failed` / `PeerState::Failed` / `PeerEvent::Failed` with `FailureReason::Expired` |


`CodeError` (reading a code): `NotACode`, `TooLarge`, `Malformed`, `NewerVersion { version }`, `UnknownTransport`, `WrongKind`, `WrongSession`, `Expired`.

`guest_return_feed`, `guest_decode_cap` and `NoEncoderSlot` are refusals of the guest share output and the guest source before or outside a pairing; they belong to the control API and the studio, not to this port.

`PeerError`: `Code(CodeError)`, `Settings(SettingsError)`, `UnknownSession`, `AlreadyUsed`, `NotConnected`, `Closed`, `Failed(FailureReason)`, `Io(String)` (socket errors with no secret in the text).

### Secrets

`PairingCode` holds a code. Its `Debug` and `Display` print `PairingCode(<redacted>)`; the text is read only through `expose()`. While a code is live the composition root registers it with `rbs_crash::add_secret` (ADR-0139 decision 14). The adapter registers the ICE username fragments and passwords of both sides the same way, as soon as it makes or reads them, and removes them with `rbs_crash::remove_secret` when the pairing ends (`rbs_crash` keeps 64 secrets).

## Tests

In `rbs-peer`: codes round-trip; every malformed field, an oversized code, a newer version, a wrong kind and a wrong session are refused with their typed error; whitespace inside a pasted code is ignored; expiry within the tolerance is accepted and outside it refused; settings bounds; the check phrase is the same for the same input, differs when any input byte differs, matches a fixed test vector, and `phrase_input` is unambiguous (moving a byte between the two fingerprints changes it). A seeded generator round-trips a few thousand random envelopes (no new dependency). The adapter's tests (in-memory network on `TestClock`, NAT expiry, consent loss) are listed in ADR-0139 decision 16. The adapter's own tests, its payload bounds and the fuzz target are listed under the adapter below.

## Conformance suite

`rbs_peer::conformance` (feature `conformance`, enabled from an adapter's dev-dependencies) drives a real factory. The adapter supplies a `Harness`: the factory, settings with which two pairings on one machine connect, and `wait`, which returns once a condition holds (a hang guard only, ADR-0032). `run_all` checks: bad invites (not a code, broken, an answer pasted as an invite, expired) refused at once with their typed error; out-of-range settings refused; a full pairing (the invite waits with its code, the code never prints its text, one session id, the invite takes one answer and refuses a second, only the answerer has an answer code, one phrase, each side unconfirmed until its own user confirms, the events `Connected` then `Confirmed` on the offerer and `AnswerReady`, `Connected`, `Confirmed` on the answerer, a closed session refuses calls); and an answer made for another invite refused as `WrongSession` without using the invite; a closed session yields one `Closed` event; a closed invite takes no answer.

## Adapter: `rbs-peer-str0m` (WebRTC)

Type id `org.rbs.peer.webrtc`, transport `WebRtc`. Built on str0m 0.26 (sans-IO; ADR-0139 decision 2), exact pins on str0m, str0m-proto and dimpl.

- **Crypto.** No str0m crypto feature: each one pulls aws-lc-sys or OpenSSL (ADR-0046). The adapter's private `crypto` module is adapted from `str0m-rust-crypto` 0.7.0 (MIT OR Apache-2.0): DTLS 1.2 through dimpl's RustCrypto backend, SRTP, STUN HMAC-SHA1 and fingerprints on RustCrypto crates, and a fresh self-signed P-256 certificate per pairing through rcgen on ring. `cargo tree -p rbs-peer-str0m` shows no aws-lc or OpenSSL.
- **Fingerprint.** A code's SDP may carry its DTLS fingerprint at session or media level, but every fingerprint line must be SHA-256 and name the same digest, or the code is `Malformed`. str0m checks a media-level fingerprint before the session-level one, so two different ones would let the phrase cover a certificate DTLS never checks.
- **Answer checks.** Before str0m sees an answer, the adapter checks its fingerprint, its ICE credentials and that it has as many media sections as the offer; str0m consumes the pending offer even when it refuses an answer, so a damaged or foreign answer is refused this way and the invite keeps waiting for the right one.
- **Payload.** Version byte `1`, then the raw-deflate SDP (`miniz_oxide`). Decoding stops at 16 KiB of SDP (no deflate bomb) and refuses more than 8 candidates. The SDP offers only H.264 and Opus and three header extensions (MID, absolute capture time, transport-wide sequence number). A code is about 1.5 KB with one candidate; one with 8 long IPv6 candidates stays under the 4 KiB limit (tested).
- **Engine.** `engine::Engine` is sans-IO: phases Gathering, Waiting (offerer with a code), Connecting, Connected { confirmed }, Failed, Closed. Gathering sends STUN binding requests (the adapter's own client, `stun`), ends at the gathering timeout, when a server reports this side's public address, or when every server stayed silent after its retries, and fails `NoPublicAddress` when private addresses are left out and no server answered. Then:
  - Keep-alive: a STUN binding refresh every 20 s while a code is pending. The answerer also sends a connectivity check to each of the offerer's candidates every 10 s, so its NAT stays open until the answer is pasted.
  - Retransmits: ICE checks back off to at most 3 s apart, and their count is capped (str0m shifts by it), so an answer pasted up to its lifetime later still connects.
  - DTLS handshake timeout: 35 minutes (dimpl's default is 40 s), so the answer's lifetime, not DTLS, bounds the wait.
  - Expiry: an invite that expires while it waits is `Expired`; a connection attempt that outlives its answer is `Unreachable`.
  - Consent: 30 s with nothing heard ends the session as `Disconnected`, which is final.
  - Check off: `Confirmed` follows `Connected` at once.
- **Threads.** One owner thread per pairing, named `rbs-peer`, started by `create_invite` or `accept_invite` and ended with the pairing (P13). It owns the UDP socket (IPv4, any port) and the engine, wakes at least every 20 ms for commands, and resolves STUN names itself. The control thread reads a small snapshot under a mutex and sends `accept_answer` and `confirm` as commands (each waits at most 2 s for its reply); the thread updates the snapshot before it replies, so a state read right after a call shows its effect. `close` never blocks: it marks the snapshot `Closed` at once and the thread ends at its next turn (it may be inside a DNS lookup). A pairing whose thread ends without an end state, or panics (reported by `rbs_crash`), shows `Failed`. STUN transaction ids come from the system random source; without one the pairing fails instead of starting. `accept_invite` refuses a bad code before any socket or thread exists. The host candidate is the interface of the default route.
- **Media path.** The offer has one send-only H.264 section and one send-only Opus section; the host answers receive-only. Each frame carries the sender's master-clock capture time (its packet's `pts`) as abs-capture-time, which travels as NTP Q32.32 (exact to well under a microsecond) and is opaque to the receiver, which only maps it (ADR-0139 decision 11, Sync). RTP time is the same capture time in the track's clock (90 kHz video, 48 kHz audio), rounded to the nearest tick. Annex B input must carry SPS and PPS in band on keyframes, as every RBS H.264 encoder does; length-prefixed input gets them from its avcC record. Congestion control is transport-wide (TWCC); its estimate and the peer's PLI/FIR requests are the feedback.
  - `WebRtcMedia::transport(&MediaHandle)` gives the sharing side's `rbs_transport::Transport` once (the composition root builds it from the session's handle; a plugin given the handle can do the same). `start` takes one H.264 stream (Annex B, or length-prefixed with an avcC record, whose SPS and PPS are then sent in band before each keyframe) and one Opus stream; anything else is `InvalidInput`. `send` pushes a clone of the packet (its bytes are shared, not copied) into a 256-slot single-producer ring and wakes the owner thread with one byte sent from its own socket to itself, at most once per wait of that thread (a 1-byte `0` from the socket's own loopback address is never taken as network input). Nothing is queued before both pairing gates allow it on this side (`Dropped`, reason `Disconnected`). Video starts at a keyframe: until the first one, and after any lost video packet, non-keyframes are dropped and a keyframe is requested. When the ring is full the new packet is dropped (the producer cannot drop the oldest, unlike the staged queues of the RTMP and SRT transports); when str0m refuses a packet the owner thread counts it in `items_dropped` the same way. A session that ended makes `send` return `Closed`. Events: `Connected` once live, followed by one `KeyframeRequested`; `Disconnected` once ended; `KeyframeRequested` at least once per `poll_events` after any PLI or FIR or lost video; `BitrateHint` when the estimate changes; `Dropped` per burst. `bytes_sent` counts the frame bytes handed to str0m; `queued_bytes` is not reported. `finish` drops what is queued (stale for real-time media) and leaves the session to its owner. A session has one transport: a share output that stops and starts again needs a new pairing (in phase 1 a share is one output for its whole life).
  - `WebRtcMedia::receiver(&MediaHandle)` gives the host side's `GuestReceiver` once: `poll` yields each received frame (a whole H.264 access unit in Annex B as str0m reassembles it, or one Opus packet) with its RTP time, capture time, keyframe and `contiguous` flags and arrival instant, only after this side is confirmed (earlier frames are dropped and counted); `request_keyframe` sends a PLI; `ended` turns true when the session failed or closed. Up to 256 frames wait for the guest source; when the ring is full the newest frame is dropped and counted, the next frame is marked not contiguous and a keyframe is requested.
  - The owner thread moves queued packets to str0m (converting length-prefixed video to Annex B in a reused buffer), received frames to the receiver's ring and the counters and estimate to atomics, every loop turn. str0m takes each frame as its own buffer, so the owner thread copies a frame once; the share output's thread never copies.
  - A close reaches the peer only through consent loss (30 s): DTLS close and RTCP BYE are not sent in phase 1b.
- **Logging.** The adapter logs nothing; the composition root logs pairing life-cycle steps and failure reasons, never a code or credential.
- **Tests.** Simulated (in-memory network with address-restricted NATs and a STUN server, simulated time, a seeded DTLS random source, so every run is the same): pairing on one network, through NATs with STUN, an answer pasted 2 min 50 s later through NATs, a substituted invite giving different phrases, a second or non-SHA-256 fingerprint refused, a damaged answer leaving the invite waiting, wrong, reused and expired codes, an invite expiring, consent loss, no public address, the check off, an answer never pasted, and ICE credentials handed out as secrets. Payload round trip and bounds; STUN parsing, retries and keep-alive; a seeded mutation test feeding 300 damaged real codes and payloads through the parsers. Media (simulated): access units split over several RTP packets arrive whole with their capture time, audio too; nothing is written or handed on before confirmation; a PLI reaches the sharer. Transport (unit): handed out once, only H.264 and Opus, gate before live, a full ring drops video until the next keyframe and asks for one, an ended session closes it once, a packet clone shares its bytes. `tests/slow_loopback.rs` (ignored, `slow_`) runs the conformance suite over real sockets on one machine, and `tests/slow_media.rs` sends access units through the transport to the receiver. `benches/send.rs` measures `send` over a real pairing. The cargo-fuzz target `fuzz/fuzz_targets/code_parser.rs` (nightly, outside the workspace) feeds any bytes to `Envelope::decode`, the payload decoder and the SDP readers.

### Share transport

`WebRtcPeerFactory::share_transports()` gives a `TransportFactory` (`org.rbs.transport.guest-share`, scheme `rbs-guest`, packet input, H.264 and Opus, bandwidth estimate, no reconnect) that the composition root registers with the other transports. The studio starts a confirmed share as an output to `share_url(session id)`, `rbs-guest://<session id in hex>`; the factory opens that sharing session's media path of the same peer factory, once. An unknown or closed session, or one already opened, is `CreateError::InvalidConfig`. The URL carries no code, address or credential.

## Review

### Round 1

Reviewer: independent AI session. Date: 2026-11-27. Verdict: approve with changes. All 13 points applied the same day:
1. Invite state had no delivery path -> `PendingInvite::state` (`InviteState`).
2. Hand-off after `accept_answer` undefined -> the session owns the connection; `close` on a used invite does not touch it.
3. Gathering blocked the caller up to 10 s -> `create_invite` and `accept_invite` return at once; `Gathering` states, `answer_code`, `AnswerReady`.
4. No media handle -> `PeerSession::media` (`MediaHandle`, opaque, downcast by the adapter).
5. Gates untested at port level -> the adapter enforces and tests them; conformance suite with the first adapter; `Confirmed` at once when the check is off.
6. Three overlapping expiry errors -> table under Errors.
7. Far-future expiry accepted -> refused as `Malformed` past the kind's longest lifetime plus tolerance.
8. Loose version parsing -> no leading zero; all-digit overflow is `NewerVersion`.
9. ASCII-only whitespace -> Unicode whitespace and zero-width characters removed.
10. Loose STUN check -> IPv4/IPv6 parsed with `std::net`, DNS labels checked.
11. Deferred items need an owner -> adapter PR acceptance (Tests, `docs/status.md`).
12. `guest_return_feed` placement -> stated under Errors.
13. Settings naming and invented bounds -> `SideSettings`; minimums marked as this spec's choice.

### Adapter review (2026-11-28)

Independent review of `rbs-peer-str0m` and the conformance suite (a fresh session given only the change, the ADRs and the specs). Round 1: reject, 12 points; round 2: approve. Applied: fingerprint rule (one SHA-256 digest at any level); snapshot updated before each reply; panic or early end leaves a failed state; secrets removed at the end (`rbs_crash::remove_secret`); answers checked before str0m takes the pending offer; random source probed; close does not block; 8-candidate size test; two more conformance checks; `Invite::state` after use. Accepted as is: the thread wakes every 20 ms (phase 1b), the ported SRTP asserts (inputs come from str0m's key derivation, and a panic is contained).

### Media path review (2026-11-28)

Independent review of the media path (`media.rs`, `link.rs`, the owner-thread pump, the send benchmark and allocation test), by a fresh session given only the change, ADR-0139 and this spec. Verdict: approve with changes. Applied the same day: one wake datagram per owner wait instead of one per send (3.9 µs to about 0.1 µs per send); the video stream starts, and restarts after any drop, at a keyframe; drops on either side count and ask for a keyframe; a full receive ring drops the newest frame and marks the next one not contiguous; one transport and one receiver per session; RTP time rounded to nearest; length-prefixed input gets its parameter sets in band before each keyframe; the allocation test installs its counting allocator; the benchmark starts each burst at a keyframe. Still open: no DTLS close or RTCP BYE (a close is seen by consent loss within 30 s), and the owner thread copies each frame once into str0m.
