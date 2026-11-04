//! The IPC framing (control API spec, "Local IPC"): a preamble from the server, then
//! length-prefixed messages and close frames in both directions.

use std::io;

/// What the server sends first on an accepted connection: `RBS` and the framing
/// version 1. A refused connection gets a close frame instead (its first byte has the
/// high bit set, so it never looks like the preamble).
pub const PREAMBLE: [u8; 4] = *b"RBS\x01";

/// Set in a header for a close frame.
pub const CLOSE_BIT: u32 = 1 << 31;

/// The longest message a header can describe.
pub const MAX_LEN: u32 = CLOSE_BIT - 1;

/// The longest close frame payload (code and reason).
pub const MAX_CLOSE_LEN: usize = 125;

/// A frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Header {
    /// A message of this many bytes follows.
    Message(u32),
    /// A close frame of this many bytes follows.
    Close(u32),
}

impl Header {
    /// Reads a header.
    pub fn parse(bytes: [u8; 4]) -> Header {
        let word = u32::from_be_bytes(bytes);
        if word & CLOSE_BIT != 0 {
            Header::Close(word & MAX_LEN)
        } else {
            Header::Message(word)
        }
    }
}

/// The frame of a message (`text` must be at most [`MAX_LEN`] bytes).
pub fn message(text: &str) -> io::Result<Vec<u8>> {
    let len = u32::try_from(text.len())
        .ok()
        .filter(|l| *l <= MAX_LEN)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "message too long"))?;
    let mut out = Vec::with_capacity(4 + text.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(text.as_bytes());
    Ok(out)
}

/// The header of a message of `len` bytes, for writing it without copying.
pub fn message_header(len: usize) -> io::Result<[u8; 4]> {
    u32::try_from(len)
        .ok()
        .filter(|l| *l <= MAX_LEN)
        .map(u32::to_be_bytes)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "message too long"))
}

/// A close frame with `code` and `reason` (cut to fit [`MAX_CLOSE_LEN`] on a
/// character boundary).
pub fn close(code: u16, reason: &str) -> Vec<u8> {
    let mut end = reason.len().min(MAX_CLOSE_LEN - 2);
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    let reason = &reason[..end];
    let len = 2 + reason.len() as u32; // At most 125.
    let mut out = Vec::with_capacity(4 + len as usize);
    out.extend_from_slice(&(CLOSE_BIT | len).to_be_bytes());
    out.extend_from_slice(&code.to_be_bytes());
    out.extend_from_slice(reason.as_bytes());
    out
}

/// The code of a close frame's payload, if it has one.
pub fn close_code_of(payload: &[u8]) -> Option<u16> {
    (payload.len() >= 2).then(|| u16::from_be_bytes([payload[0], payload[1]]))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    #[test]
    fn headers_round_trip() {
        let m = message("çağrı").unwrap();
        assert_eq!(Header::parse([m[0], m[1], m[2], m[3]]), Header::Message(8));
        assert_eq!(&m[4..], "çağrı".as_bytes());
        let c = close(4002, "auth_failed");
        assert_eq!(Header::parse([c[0], c[1], c[2], c[3]]), Header::Close(13));
        assert_eq!(close_code_of(&c[4..]), Some(4002));
        assert!(c[0] >= 0x80 && PREAMBLE[0] < 0x80);
        assert_eq!(message_header(0).unwrap(), [0; 4]);
    }

    #[test]
    fn a_long_reason_is_cut_on_a_character_boundary() {
        let c = close(1001, &"ş".repeat(100));
        assert_eq!(c.len(), 4 + 124);
        assert!(std::str::from_utf8(&c[6..]).is_ok());
    }
}
