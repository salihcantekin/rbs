//! RFC 6455 framing for the server side: client frames are masked, server frames are
//! not. Only text messages carry the protocol; control frames are handled here.
//!
//! The reader never buffers more of a message than the connection's current limit: a
//! longer message is skipped as it arrives and reported with its length.

/// Close codes the reader answers protocol errors with.
pub(crate) const PROTOCOL_ERROR: u16 = 1002;
/// A binary message: the protocol is text only.
pub(crate) const UNSUPPORTED_DATA: u16 = 1003;
/// A text message that is not UTF-8.
pub(crate) const INVALID_PAYLOAD: u16 = 1007;
/// A normal close.
pub(crate) const NORMAL: u16 = 1000;

const OP_CONTINUATION: u8 = 0x0;
const OP_TEXT: u8 = 0x1;
const OP_BINARY: u8 = 0x2;
const OP_CLOSE: u8 = 0x8;
const OP_PING: u8 = 0x9;
const OP_PONG: u8 = 0xA;

/// What the reader found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Incoming {
    /// A whole text message.
    Text(String),
    /// A text message longer than the limit, skipped; its length in bytes.
    Oversized(usize),
    /// A ping with its payload, to answer with a pong.
    Ping(Vec<u8>),
    /// The peer closes, with its code when it sent one.
    Close(Option<u16>),
}

struct Frame {
    opcode: u8,
    fin: bool,
    mask: [u8; 4],
    remaining: u64,
    offset: usize,
}

/// Reads frames from the bytes a connection received.
#[derive(Default)]
pub(crate) struct FrameReader {
    input: Vec<u8>,
    start: usize,
    frame: Option<Frame>,
    /// A text message is open (its first frame had no FIN).
    in_message: bool,
    message: Vec<u8>,
    /// The open message went over the limit: bytes seen so far, none kept.
    skipped: Option<usize>,
    control: Vec<u8>,
}

impl FrameReader {
    /// Adds received bytes.
    pub(crate) fn feed(&mut self, bytes: &[u8]) {
        if self.start > 0 && self.start * 2 >= self.input.len() {
            self.input.drain(..self.start);
            self.start = 0;
        }
        self.input.extend_from_slice(bytes);
    }

    /// The next message or control frame; `Ok(None)` when more bytes are needed.
    /// `Err` carries the close code for a protocol violation; the connection is done.
    pub(crate) fn next(&mut self, limit: usize) -> Result<Option<Incoming>, u16> {
        loop {
            if self.frame.is_none() {
                match self.header(limit)? {
                    Some(f) => self.frame = Some(f),
                    None => return Ok(None),
                }
            }
            let Some(frame) = self.frame.as_mut() else {
                return Ok(None);
            };
            let available = self.input.len() - self.start;
            let take = usize::try_from(frame.remaining)
                .unwrap_or(usize::MAX)
                .min(available);
            let bytes = &self.input[self.start..self.start + take];
            if frame.opcode >= OP_CLOSE {
                unmask_into(&mut self.control, bytes, frame.mask, frame.offset);
            } else if let Some(skipped) = self.skipped.as_mut() {
                *skipped = skipped.saturating_add(take);
            } else {
                unmask_into(&mut self.message, bytes, frame.mask, frame.offset);
            }
            self.start += take;
            frame.offset += take;
            frame.remaining -= take as u64;
            if frame.remaining > 0 {
                return Ok(None);
            }
            let (opcode, fin) = (frame.opcode, frame.fin);
            self.frame = None;
            match opcode {
                OP_CLOSE => {
                    let payload = std::mem::take(&mut self.control);
                    let code = match payload.as_slice() {
                        [] => None,
                        [a, b, reason @ ..] => {
                            let code = u16::from_be_bytes([*a, *b]);
                            // RFC 6455 section 7.4: codes a peer may send.
                            if !matches!(code, 1000..=1003 | 1007..=1011 | 3000..=4999) {
                                return Err(PROTOCOL_ERROR);
                            }
                            if std::str::from_utf8(reason).is_err() {
                                return Err(INVALID_PAYLOAD);
                            }
                            Some(code)
                        }
                        [_] => return Err(PROTOCOL_ERROR),
                    };
                    return Ok(Some(Incoming::Close(code)));
                }
                OP_PING => {
                    return Ok(Some(Incoming::Ping(std::mem::take(&mut self.control))));
                }
                OP_PONG => self.control.clear(),
                _ if fin => {
                    self.in_message = false;
                    if let Some(len) = self.skipped.take() {
                        return Ok(Some(Incoming::Oversized(len)));
                    }
                    let bytes = std::mem::take(&mut self.message);
                    return String::from_utf8(bytes)
                        .map(|s| Some(Incoming::Text(s)))
                        .map_err(|_| INVALID_PAYLOAD);
                }
                _ => {}
            }
        }
    }

    /// Reads a frame header when it is complete, and checks it.
    fn header(&mut self, limit: usize) -> Result<Option<Frame>, u16> {
        let b = &self.input[self.start..];
        if b.len() < 2 {
            return Ok(None);
        }
        let fin = b[0] & 0x80 != 0;
        if b[0] & 0x70 != 0 {
            return Err(PROTOCOL_ERROR); // No extension was negotiated.
        }
        let opcode = b[0] & 0x0F;
        if b[1] & 0x80 == 0 {
            return Err(PROTOCOL_ERROR); // Client frames must be masked.
        }
        let (len, at) = match b[1] & 0x7F {
            126 => {
                if b.len() < 4 {
                    return Ok(None);
                }
                (u64::from(u16::from_be_bytes([b[2], b[3]])), 4)
            }
            127 => {
                if b.len() < 10 {
                    return Ok(None);
                }
                let mut n = [0u8; 8];
                n.copy_from_slice(&b[2..10]);
                let len = u64::from_be_bytes(n);
                if len >> 63 != 0 {
                    return Err(PROTOCOL_ERROR);
                }
                (len, 10)
            }
            n => (u64::from(n), 2),
        };
        if b.len() < at + 4 {
            return Ok(None);
        }
        let mask = [b[at], b[at + 1], b[at + 2], b[at + 3]];
        match opcode {
            OP_CLOSE | OP_PING | OP_PONG => {
                if !fin || len > 125 {
                    return Err(PROTOCOL_ERROR);
                }
                self.control.clear();
            }
            OP_BINARY => return Err(UNSUPPORTED_DATA),
            OP_TEXT => {
                if self.in_message {
                    return Err(PROTOCOL_ERROR);
                }
                self.in_message = true;
                self.message.clear();
                self.skipped = None;
                self.start_data(len, limit);
            }
            OP_CONTINUATION => {
                if !self.in_message {
                    return Err(PROTOCOL_ERROR);
                }
                self.start_data(len, limit);
            }
            _ => return Err(PROTOCOL_ERROR),
        }
        self.start += at + 4;
        Ok(Some(Frame {
            opcode,
            fin,
            mask,
            remaining: len,
            offset: 0,
        }))
    }

    /// A data frame of `len` bytes starts: past the limit, the message is skipped.
    fn start_data(&mut self, len: u64, limit: usize) {
        if self.skipped.is_some() {
            return;
        }
        let total = (self.message.len() as u64).saturating_add(len);
        if total > limit as u64 {
            self.skipped = Some(self.message.len());
            self.message = Vec::new();
        } else {
            self.message.reserve(usize::try_from(len).unwrap_or(0));
        }
    }
}

fn unmask_into(out: &mut Vec<u8>, bytes: &[u8], mask: [u8; 4], offset: usize) {
    out.extend(
        bytes
            .iter()
            .enumerate()
            .map(|(i, b)| b ^ mask[(offset + i) % 4]),
    );
}

/// A server frame (FIN set, not masked) with `payload`.
pub(crate) fn encode(opcode: u8, payload: &[u8], out: &mut Vec<u8>) {
    out.push(0x80 | opcode);
    let len = payload.len();
    if len < 126 {
        out.push(len as u8);
    } else if let Ok(len) = u16::try_from(len) {
        out.push(126);
        out.extend_from_slice(&len.to_be_bytes());
    } else {
        out.push(127);
        out.extend_from_slice(&(len as u64).to_be_bytes());
    }
    out.extend_from_slice(payload);
}

/// A text frame.
pub(crate) fn encode_text(text: &str, out: &mut Vec<u8>) {
    encode(OP_TEXT, text.as_bytes(), out);
}

/// A pong answering a ping.
pub(crate) fn encode_pong(payload: &[u8], out: &mut Vec<u8>) {
    encode(OP_PONG, payload, out);
}

/// A close frame with `code` and a short reason.
pub(crate) fn encode_close(code: u16, reason: &str, out: &mut Vec<u8>) {
    let mut payload = code.to_be_bytes().to_vec();
    let reason = reason.as_bytes();
    payload.extend_from_slice(&reason[..reason.len().min(123)]);
    encode(OP_CLOSE, &payload, out);
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    /// A masked client frame.
    fn client(first: u8, payload: &[u8]) -> Vec<u8> {
        let mask = [0x37, 0xfa, 0x21, 0x3d];
        let mut out = vec![first];
        let len = payload.len();
        if len < 126 {
            out.push(0x80 | len as u8);
        } else if len <= 0xFFFF {
            out.push(0x80 | 126);
            out.extend_from_slice(&(len as u16).to_be_bytes());
        } else {
            out.push(0x80 | 127);
            out.extend_from_slice(&(len as u64).to_be_bytes());
        }
        out.extend_from_slice(&mask);
        out.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        out
    }

    fn read_all(bytes: &[u8], limit: usize) -> Vec<Result<Incoming, u16>> {
        let mut r = FrameReader::default();
        let mut out = Vec::new();
        // One byte at a time: every split point of every frame is exercised.
        for b in bytes {
            r.feed(std::slice::from_ref(b));
            loop {
                match r.next(limit) {
                    Ok(Some(i)) => out.push(Ok(i)),
                    Ok(None) => break,
                    Err(e) => {
                        out.push(Err(e));
                        return out;
                    }
                }
            }
        }
        out
    }

    #[test]
    fn the_rfc_example_hello_is_read() {
        // RFC 6455 section 5.7: a single-frame masked text message "Hello".
        let bytes = [
            0x81, 0x85, 0x37, 0xfa, 0x21, 0x3d, 0x7f, 0x9f, 0x4d, 0x51, 0x58,
        ];
        assert_eq!(read_all(&bytes, 100), [Ok(Incoming::Text("Hello".into()))]);
    }

    #[test]
    fn fragments_with_a_ping_between_them_form_one_message() {
        let mut bytes = client(0x01, b"Hel");
        bytes.extend(client(0x89, b"p"));
        bytes.extend(client(0x80, b"lo"));
        assert_eq!(
            read_all(&bytes, 100),
            [
                Ok(Incoming::Ping(b"p".to_vec())),
                Ok(Incoming::Text("Hello".into()))
            ]
        );
    }

    #[test]
    fn a_message_over_the_limit_is_skipped_and_the_next_one_read() {
        let long = vec![b'x'; 70_000];
        let mut bytes = client(0x01, &long[..300]);
        bytes.extend(client(0x80, &long[300..]));
        bytes.extend(client(0x81, b"ok"));
        bytes.extend(client(0x81, &[b'y'; 200]));
        assert_eq!(
            read_all(&bytes, 200),
            [
                Ok(Incoming::Oversized(70_000)),
                Ok(Incoming::Text("ok".into())),
                Ok(Incoming::Text("y".repeat(200))),
            ]
        );
    }

    #[test]
    fn protocol_violations_close_with_their_code() {
        let unmasked = [0x81, 0x01, b'a'];
        assert_eq!(read_all(&unmasked, 10), [Err(PROTOCOL_ERROR)]);
        assert_eq!(read_all(&client(0x82, b"a"), 10), [Err(UNSUPPORTED_DATA)]);
        assert_eq!(read_all(&client(0x81, &[0xff]), 10), [Err(INVALID_PAYLOAD)]);
        assert_eq!(read_all(&client(0x80, b"a"), 10), [Err(PROTOCOL_ERROR)]);
        assert_eq!(read_all(&client(0xC1, b"a"), 10), [Err(PROTOCOL_ERROR)]);
        assert_eq!(read_all(&client(0x09, b"a"), 10), [Err(PROTOCOL_ERROR)]);
        let mut twice = client(0x01, b"a");
        twice.extend(client(0x81, b"b"));
        assert_eq!(read_all(&twice, 10), [Err(PROTOCOL_ERROR)]);
    }

    #[test]
    fn a_close_carries_its_code() {
        let mut payload = 4002u16.to_be_bytes().to_vec();
        payload.extend_from_slice(b"bye");
        assert_eq!(
            read_all(&client(0x88, &payload), 10),
            [Ok(Incoming::Close(Some(4002)))]
        );
        assert_eq!(
            read_all(&client(0x88, b""), 10),
            [Ok(Incoming::Close(None))]
        );
        assert_eq!(
            read_all(&client(0x88, &1005u16.to_be_bytes()), 10),
            [Err(PROTOCOL_ERROR)]
        );
        let mut bad = 1000u16.to_be_bytes().to_vec();
        bad.push(0xff);
        assert_eq!(read_all(&client(0x88, &bad), 10), [Err(INVALID_PAYLOAD)]);
    }

    #[test]
    fn server_frames_use_the_shortest_length() {
        for (len, header) in [(5usize, 2usize), (126, 4), (65_536, 10)] {
            let mut out = Vec::new();
            encode_text(&"a".repeat(len), &mut out);
            assert_eq!(out.len(), header + len);
            assert_eq!(out[0], 0x81);
        }
    }
}
