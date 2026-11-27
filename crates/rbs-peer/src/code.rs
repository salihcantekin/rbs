//! The pairing code format (docs/specs/peer-port.md, "Code format").
//!
//! `rbs-guest:<version>:<base64url body>`. The body holds the envelope every
//! transport shares and an opaque payload that belongs to the transport adapter.

use std::fmt;
use std::time::Duration;

use crate::error::CodeError;
use crate::settings::{MAX_ANSWER_LIFETIME, MAX_INVITE_LIFETIME};

/// The text every code starts with.
pub const CODE_PREFIX: &str = "rbs-guest:";
/// The only code format version this crate writes and reads.
pub const CODE_FORMAT_VERSION: u32 = 1;
/// The longest code, in bytes, after whitespace is removed.
pub const MAX_CODE_LEN: usize = 4096;
/// Pasted input longer than this is refused without being scanned.
pub const MAX_INPUT_LEN: usize = 16 * 1024;
/// How far in the past a code's advisory expiry may be (clock skew between computers).
pub const EXPIRY_TOLERANCE: Duration = Duration::from_secs(120);

/// Size of the fixed part of a version 1 body, before the payload.
const HEADER_LEN: usize = 28;

/// Which half of a pairing a code is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CodeKind {
    /// Made by the sharing side with *Share*.
    Invite,
    /// Made by the host from an invite.
    Answer,
}

impl CodeKind {
    fn wire(self) -> u8 {
        match self {
            Self::Invite => 1,
            Self::Answer => 2,
        }
    }

    fn from_wire(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Invite),
            2 => Some(Self::Answer),
            _ => None,
        }
    }
}

/// The transport a code is for. Its payload belongs to that transport's adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TransportKind {
    /// Peer-to-peer WebRTC (`rbs-peer-str0m`).
    WebRtc,
}

impl TransportKind {
    fn wire(self) -> u8 {
        match self {
            Self::WebRtc => 1,
        }
    }

    fn from_wire(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::WebRtc),
            _ => None,
        }
    }
}

/// A pairing session's id: 16 random bytes chosen by the inviting side. Never all zero.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionId([u8; 16]);

impl SessionId {
    /// Wraps 16 bytes from a cryptographic random source. `None` for all zero bytes.
    pub fn new(bytes: [u8; 16]) -> Option<Self> {
        (bytes != [0; 16]).then_some(Self(bytes))
    }

    /// The raw bytes.
    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Debug for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SessionId({self})")
    }
}

impl fmt::Display for SessionId {
    /// Lower-case hex. A session id is not a secret on its own: it is useless without
    /// the code's credentials, and it names the session in logs.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// The part of a code every transport shares, plus the adapter's payload.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Envelope {
    /// Invite or answer.
    pub kind: CodeKind,
    /// Which adapter reads the payload.
    pub transport: TransportKind,
    /// The pairing session. An answer carries its invite's id.
    pub session_id: SessionId,
    /// Advisory expiry, seconds since the Unix epoch. The issuer's live state decides.
    pub expires_at_unix: u64,
    /// Transport-specific data (for WebRTC a compressed SDP). Never a stream key,
    /// token, user name, device name or file path.
    pub payload: Vec<u8>,
}

impl fmt::Debug for Envelope {
    /// Leaves the payload out: it holds the transport's credentials.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Envelope")
            .field("kind", &self.kind)
            .field("transport", &self.transport)
            .field("session_id", &self.session_id)
            .field("expires_at_unix", &self.expires_at_unix)
            .field("payload_len", &self.payload.len())
            .finish()
    }
}

impl Envelope {
    /// A new envelope.
    pub fn new(
        kind: CodeKind,
        transport: TransportKind,
        session_id: SessionId,
        expires_at_unix: u64,
        payload: Vec<u8>,
    ) -> Self {
        Self {
            kind,
            transport,
            session_id,
            expires_at_unix,
            payload,
        }
    }

    /// The code text. `TooLarge` when the code would exceed [`MAX_CODE_LEN`].
    pub fn encode(&self) -> Result<PairingCode, CodeError> {
        let payload_len = u16::try_from(self.payload.len()).map_err(|_| CodeError::TooLarge)?;
        let body_len = HEADER_LEN + self.payload.len();
        let text_len = CODE_PREFIX.len() + 2 + base64url_len(body_len);
        if text_len > MAX_CODE_LEN {
            return Err(CodeError::TooLarge);
        }
        let mut body = Vec::with_capacity(body_len);
        body.push(self.kind.wire());
        body.push(self.transport.wire());
        body.extend_from_slice(self.session_id.as_bytes());
        body.extend_from_slice(&self.expires_at_unix.to_be_bytes());
        body.extend_from_slice(&payload_len.to_be_bytes());
        body.extend_from_slice(&self.payload);
        let mut text = String::with_capacity(text_len);
        text.push_str(CODE_PREFIX);
        text.push_str("1:");
        base64url_encode(&body, &mut text);
        Ok(PairingCode(text))
    }

    /// Reads a pasted code. ASCII whitespace anywhere is ignored.
    pub fn decode(input: &str) -> Result<Self, CodeError> {
        if input.len() > MAX_INPUT_LEN {
            return Err(CodeError::TooLarge);
        }
        let text: String = input.chars().filter(|&c| !is_paste_noise(c)).collect();
        if text.len() > MAX_CODE_LEN {
            return Err(CodeError::TooLarge);
        }
        let rest = text.strip_prefix(CODE_PREFIX).ok_or(CodeError::NotACode)?;
        let (version, body) = rest.split_once(':').ok_or(CodeError::Malformed)?;
        let version = parse_version(version)?;
        if version > CODE_FORMAT_VERSION {
            return Err(CodeError::NewerVersion { version });
        }
        let body = base64url_decode(body).ok_or(CodeError::Malformed)?;
        Self::from_body(&body)
    }

    fn from_body(body: &[u8]) -> Result<Self, CodeError> {
        let header = body.get(..HEADER_LEN).ok_or(CodeError::Malformed)?;
        let kind = CodeKind::from_wire(header[0]).ok_or(CodeError::Malformed)?;
        let transport = TransportKind::from_wire(header[1]).ok_or(CodeError::UnknownTransport)?;
        let mut id = [0u8; 16];
        id.copy_from_slice(&header[2..18]);
        let session_id = SessionId::new(id).ok_or(CodeError::Malformed)?;
        let mut expiry = [0u8; 8];
        expiry.copy_from_slice(&header[18..26]);
        let payload_len = usize::from(u16::from_be_bytes([header[26], header[27]]));
        let payload = &body[HEADER_LEN..];
        if payload.len() != payload_len {
            return Err(CodeError::Malformed);
        }
        Ok(Self {
            kind,
            transport,
            session_id,
            expires_at_unix: u64::from_be_bytes(expiry),
            payload: payload.to_vec(),
        })
    }

    /// Checks that this is the expected kind of code, for `session` when given, and
    /// that its advisory expiry has not passed by more than [`EXPIRY_TOLERANCE`] and
    /// is not further ahead than its kind's longest lifetime allows.
    pub fn expect(
        &self,
        kind: CodeKind,
        session: Option<SessionId>,
        now_unix: u64,
    ) -> Result<(), CodeError> {
        if self.kind != kind {
            return Err(CodeError::WrongKind);
        }
        if session.is_some_and(|id| id != self.session_id) {
            return Err(CodeError::WrongSession);
        }
        let longest = match kind {
            CodeKind::Invite => MAX_INVITE_LIFETIME,
            CodeKind::Answer => MAX_ANSWER_LIFETIME,
        };
        check_expiry(self.expires_at_unix, now_unix, longest)
    }
}

/// `Expired` when `expires_at_unix` is more than [`EXPIRY_TOLERANCE`] before
/// `now_unix`; `Malformed` when it is more than `longest` plus the tolerance after it
/// (no issuer makes such a code, and the reader has no issuer to ask).
pub fn check_expiry(
    expires_at_unix: u64,
    now_unix: u64,
    longest: Duration,
) -> Result<(), CodeError> {
    let tolerance = EXPIRY_TOLERANCE.as_secs();
    if now_unix > expires_at_unix.saturating_add(tolerance) {
        return Err(CodeError::Expired);
    }
    let latest = now_unix
        .saturating_add(longest.as_secs())
        .saturating_add(tolerance);
    if expires_at_unix > latest {
        return Err(CodeError::Malformed);
    }
    Ok(())
}

/// Characters chat apps insert into long text: whitespace of any kind, zero-width
/// spaces and joiners, the word joiner and the soft hyphen. None is in base64url.
fn is_paste_noise(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '\u{200b}' | '\u{200c}' | '\u{200d}' | '\u{2060}' | '\u{feff}' | '\u{00ad}'
        )
}

/// Decimal digits with no leading zero. A version too large for `u32` is still a
/// newer version, so old readers refuse it by the prefix alone.
fn parse_version(text: &str) -> Result<u32, CodeError> {
    if text.is_empty() || text.starts_with('0') || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(CodeError::Malformed);
    }
    Ok(text.parse::<u32>().unwrap_or(u32::MAX))
}

/// A pairing code's text. A secret while its session is live: `Debug` and `Display`
/// never print it (ADR-0139 decision 14).
#[derive(Clone, PartialEq, Eq)]
pub struct PairingCode(String);

impl PairingCode {
    /// The code text, to show to the user or copy to the clipboard.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for PairingCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PairingCode(<redacted>)")
    }
}

impl fmt::Display for PairingCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PairingCode(<redacted>)")
    }
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn base64url_len(bytes: usize) -> usize {
    (bytes * 4).div_ceil(3)
}

fn base64url_encode(bytes: &[u8], out: &mut String) {
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        let chars = chunk.len() + 1;
        for i in 0..chars {
            let index = (n >> (18 - 6 * i)) & 0x3f;
            out.push(char::from(ALPHABET[index as usize]));
        }
    }
}

fn sextet(c: u8) -> Option<u32> {
    let value = match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'-' => 62,
        b'_' => 63,
        _ => return None,
    };
    Some(u32::from(value))
}

/// Strict decode: no padding, only the URL-safe alphabet, unused trailing bits zero.
fn base64url_decode(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    if bytes.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut n = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            n |= sextet(c)? << (18 - 6 * i);
        }
        let produced = chunk.len() - 1;
        let unused_mask = match produced {
            1 => 0xffff,
            2 => 0xff,
            _ => 0,
        };
        if n & unused_mask != 0 {
            return None;
        }
        for i in 0..produced {
            out.push(((n >> (16 - 8 * i)) & 0xff) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn id(seed: u8) -> SessionId {
        SessionId::new([seed; 16]).unwrap()
    }

    fn envelope(payload: Vec<u8>) -> Envelope {
        Envelope::new(
            CodeKind::Invite,
            TransportKind::WebRtc,
            id(7),
            1_800_000_000,
            payload,
        )
    }

    fn body_of(code: &PairingCode) -> Vec<u8> {
        let text = code.expose().strip_prefix("rbs-guest:1:").unwrap();
        base64url_decode(text).unwrap()
    }

    fn code_from_body(body: &[u8]) -> String {
        let mut text = String::from("rbs-guest:1:");
        base64url_encode(body, &mut text);
        text
    }

    #[test]
    fn a_code_round_trips() {
        let original = envelope(b"payload".to_vec());
        let code = original.encode().unwrap();
        assert!(code.expose().starts_with("rbs-guest:1:"));
        assert_eq!(Envelope::decode(code.expose()).unwrap(), original);
    }

    #[test]
    fn base64url_matches_rfc_4648_vectors() {
        let vectors: [(&[u8], &str); 7] = [
            (b"", ""),
            (b"f", "Zg"),
            (b"fo", "Zm8"),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg"),
            (b"fooba", "Zm9vYmE"),
            (&[0xfb, 0xff, 0xfe], "-__-"),
        ];
        for (bytes, text) in vectors {
            let mut out = String::new();
            base64url_encode(bytes, &mut out);
            assert_eq!(out, text);
            assert_eq!(base64url_decode(text).unwrap(), bytes);
        }
    }

    #[test]
    fn base64url_refuses_padding_other_alphabets_and_stray_bits() {
        // Padding, the standard alphabet, an impossible length, non-zero unused bits.
        for bad in ["Zg==", "Zm9v+A", "Zm9v/A", "Z", "Zh"] {
            assert!(base64url_decode(bad).is_none(), "{bad} must be refused");
        }
    }

    #[test]
    fn whitespace_from_chat_apps_is_ignored() {
        let code = envelope(vec![1; 300]).encode().unwrap();
        let text = code.expose();
        let wrapped: String = text
            .as_bytes()
            .chunks(40)
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect::<Vec<_>>()
            .join("\r\n  ");
        assert_eq!(
            Envelope::decode(&format!(" {wrapped}\n")).unwrap().payload,
            vec![1; 300]
        );
    }

    #[test]
    fn text_that_is_not_a_code_is_refused() {
        assert_eq!(Envelope::decode("hello"), Err(CodeError::NotACode));
        assert_eq!(Envelope::decode("RBS-GUEST:1:AA"), Err(CodeError::NotACode));
        assert_eq!(Envelope::decode(""), Err(CodeError::NotACode));
    }

    #[test]
    fn a_newer_version_is_refused_cleanly() {
        assert_eq!(
            Envelope::decode("rbs-guest:2:anything-at-all"),
            Err(CodeError::NewerVersion { version: 2 })
        );
        assert_eq!(
            Envelope::decode("rbs-guest:99999999999:AA"),
            Err(CodeError::NewerVersion { version: u32::MAX })
        );
        for bad in [
            "rbs-guest:01:AA",
            "rbs-guest:0:AA",
            "rbs-guest::AA",
            "rbs-guest:1a:AA",
            "rbs-guest:+1:AA",
            "rbs-guest:1",
        ] {
            assert_eq!(Envelope::decode(bad), Err(CodeError::Malformed), "{bad}");
        }
    }

    #[test]
    fn oversized_codes_and_payloads_are_refused() {
        // The largest payload that fits, and one byte more.
        let max_body = (MAX_CODE_LEN - "rbs-guest:1:".len()) * 3 / 4;
        let fits = envelope(vec![9; max_body - HEADER_LEN]);
        let code = fits.encode().unwrap();
        assert!(code.expose().len() <= MAX_CODE_LEN);
        assert_eq!(Envelope::decode(code.expose()).unwrap(), fits);
        let too_big = envelope(vec![9; max_body - HEADER_LEN + 1]);
        assert_eq!(too_big.encode(), Err(CodeError::TooLarge));
        assert_eq!(envelope(vec![0; 70_000]).encode(), Err(CodeError::TooLarge));
        let long = format!("rbs-guest:1:{}", "A".repeat(MAX_CODE_LEN));
        assert_eq!(Envelope::decode(&long), Err(CodeError::TooLarge));
        assert_eq!(
            Envelope::decode(&" ".repeat(MAX_INPUT_LEN + 1)),
            Err(CodeError::TooLarge)
        );
    }

    #[test]
    fn every_malformed_field_is_refused() {
        let good = body_of(&envelope(b"abc".to_vec()).encode().unwrap());

        let mut bad_kind = good.clone();
        bad_kind[0] = 3;
        assert_eq!(
            Envelope::decode(&code_from_body(&bad_kind)),
            Err(CodeError::Malformed)
        );

        let mut bad_transport = good.clone();
        bad_transport[1] = 9;
        assert_eq!(
            Envelope::decode(&code_from_body(&bad_transport)),
            Err(CodeError::UnknownTransport)
        );

        let mut zero_id = good.clone();
        zero_id[2..18].fill(0);
        assert_eq!(
            Envelope::decode(&code_from_body(&zero_id)),
            Err(CodeError::Malformed)
        );

        let mut long_len = good.clone();
        long_len[27] = 4;
        assert_eq!(
            Envelope::decode(&code_from_body(&long_len)),
            Err(CodeError::Malformed)
        );

        let mut trailing = good.clone();
        trailing.push(0);
        assert_eq!(
            Envelope::decode(&code_from_body(&trailing)),
            Err(CodeError::Malformed)
        );

        assert_eq!(
            Envelope::decode(&code_from_body(&good[..HEADER_LEN - 1])),
            Err(CodeError::Malformed)
        );
        assert_eq!(
            Envelope::decode("rbs-guest:1:A*AA"),
            Err(CodeError::Malformed)
        );
    }

    #[test]
    fn kind_session_and_expiry_are_checked() {
        let invite = envelope(Vec::new());
        let now = invite.expires_at_unix;
        assert_eq!(invite.expect(CodeKind::Invite, None, now), Ok(()));
        assert_eq!(
            invite.expect(CodeKind::Answer, None, now),
            Err(CodeError::WrongKind)
        );
        assert_eq!(
            invite.expect(CodeKind::Invite, Some(id(8)), now),
            Err(CodeError::WrongSession)
        );
        assert_eq!(invite.expect(CodeKind::Invite, Some(id(7)), now), Ok(()));
    }

    #[test]
    fn clock_skew_within_the_tolerance_is_accepted() {
        let expiry = 1_000_000;
        let longest = MAX_INVITE_LIFETIME;
        assert_eq!(check_expiry(expiry, expiry - 500, longest), Ok(()));
        assert_eq!(check_expiry(expiry, expiry + 120, longest), Ok(()));
        assert_eq!(
            check_expiry(expiry, expiry + 121, longest),
            Err(CodeError::Expired)
        );
    }

    #[test]
    fn a_far_future_expiry_is_refused() {
        let now = 1_000_000;
        let ahead = MAX_INVITE_LIFETIME.as_secs() + EXPIRY_TOLERANCE.as_secs();
        assert_eq!(check_expiry(now + ahead, now, MAX_INVITE_LIFETIME), Ok(()));
        assert_eq!(
            check_expiry(now + ahead + 1, now, MAX_INVITE_LIFETIME),
            Err(CodeError::Malformed)
        );
        assert_eq!(
            check_expiry(u64::MAX, now, MAX_INVITE_LIFETIME),
            Err(CodeError::Malformed)
        );
        let mut answer = envelope(Vec::new());
        answer.kind = CodeKind::Answer;
        answer.expires_at_unix =
            now + MAX_ANSWER_LIFETIME.as_secs() + EXPIRY_TOLERANCE.as_secs() + 1;
        assert_eq!(
            answer.expect(CodeKind::Answer, None, now),
            Err(CodeError::Malformed)
        );
    }

    #[test]
    fn unicode_noise_from_chat_apps_is_ignored() {
        let code = envelope(vec![2; 50]).encode().unwrap();
        let text = code.expose();
        let (a, b) = text.split_at(30);
        let noisy = format!("\u{feff}{a}\u{200b}\u{00a0}\u{00ad}{b}\u{2060}");
        assert_eq!(Envelope::decode(&noisy).unwrap().payload, vec![2; 50]);
    }

    #[test]
    fn debug_output_never_shows_the_code_or_the_payload() {
        let env = envelope(b"ice-pwd-secret".to_vec());
        let code = env.encode().unwrap();
        assert_eq!(format!("{code:?}"), "PairingCode(<redacted>)");
        assert_eq!(code.to_string(), "PairingCode(<redacted>)");
        let debug = format!("{env:?}");
        assert!(!debug.contains("ice-pwd-secret"), "{debug}");
        assert!(debug.contains("payload_len: 14"), "{debug}");
    }

    #[test]
    fn random_envelopes_round_trip() {
        // A seeded xorshift keeps this deterministic without a new dependency.
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..2000 {
            let mut id = [0u8; 16];
            for byte in &mut id {
                *byte = next() as u8;
            }
            id[0] |= 1;
            let len = (next() % 400) as usize;
            let payload: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            let kind = if next() % 2 == 0 {
                CodeKind::Invite
            } else {
                CodeKind::Answer
            };
            let env = Envelope::new(
                kind,
                TransportKind::WebRtc,
                SessionId::new(id).unwrap(),
                next(),
                payload,
            );
            let code = env.encode().unwrap();
            assert_eq!(Envelope::decode(code.expose()).unwrap(), env);
        }
    }

    #[test]
    fn session_ids_print_as_hex_and_refuse_zero() {
        assert!(SessionId::new([0; 16]).is_none());
        let id = SessionId::new([0xab; 16]).unwrap();
        assert_eq!(id.to_string(), "ab".repeat(16));
    }
}
