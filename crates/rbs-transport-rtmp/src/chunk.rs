//! RTMP chunk stream: message types, the outbound chunker and a bounded inbound
//! parser (Adobe RTMP specification 1.0, section 5.3).

/// Protocol control messages (Set Chunk Size, Window Acknowledgement Size, ...).
pub(crate) const CSID_CONTROL: u8 = 2;
/// Command messages.
pub(crate) const CSID_COMMAND: u8 = 3;
/// Audio messages.
pub(crate) const CSID_AUDIO: u8 = 4;
/// Data messages (`@setDataFrame`).
pub(crate) const CSID_DATA: u8 = 5;
/// Video messages.
pub(crate) const CSID_VIDEO: u8 = 6;

/// Set Chunk Size.
pub(crate) const MSG_SET_CHUNK_SIZE: u8 = 1;
/// Abort Message.
pub(crate) const MSG_ABORT: u8 = 2;
/// Acknowledgement.
pub(crate) const MSG_ACK: u8 = 3;
/// User Control Message.
pub(crate) const MSG_USER_CONTROL: u8 = 4;
/// Window Acknowledgement Size.
pub(crate) const MSG_WINDOW_ACK_SIZE: u8 = 5;
/// Set Peer Bandwidth.
pub(crate) const MSG_SET_PEER_BANDWIDTH: u8 = 6;
/// Audio data.
pub(crate) const MSG_AUDIO: u8 = 8;
/// Video data.
pub(crate) const MSG_VIDEO: u8 = 9;
/// AMF3 command (a 0x00 byte, then AMF0 values).
pub(crate) const MSG_COMMAND_AMF3: u8 = 17;
/// AMF0 data.
pub(crate) const MSG_DATA_AMF0: u8 = 18;
/// AMF0 command.
pub(crate) const MSG_COMMAND_AMF0: u8 = 20;

/// User control event: ping request (4-byte timestamp).
pub(crate) const UC_PING_REQUEST: u16 = 6;
/// User control event: ping response.
pub(crate) const UC_PING_RESPONSE: u16 = 7;

/// Chunk size both sides start with.
pub(crate) const DEFAULT_CHUNK_SIZE: usize = 128;
/// Largest message body the 24-bit length field can carry.
pub(crate) const MAX_BODY: usize = 0x00ff_ffff;
const EXTENDED: u32 = 0x00ff_ffff;

/// Bytes [`write_message`] appends for a body of `len` bytes.
pub(crate) fn message_size(len: usize, chunk_size: usize, timestamp: u32) -> usize {
    let ext = if timestamp >= EXTENDED { 4 } else { 0 };
    let chunks = len.div_ceil(chunk_size).max(1);
    12 + ext + (chunks - 1) * (1 + ext) + len
}

/// Appends one message as chunks of `chunk_size` on chunk stream `csid` (2..=63):
/// a type 0 header with the absolute timestamp, then type 3 headers. Timestamps from
/// 0xFFFFFF up use the extended field, repeated after every type 3 header (as FFmpeg
/// and librtmp read it). `body.len()` must be at most [`MAX_BODY`].
pub(crate) fn write_message(
    out: &mut Vec<u8>,
    csid: u8,
    timestamp: u32,
    type_id: u8,
    stream_id: u32,
    body: &[u8],
    chunk_size: usize,
) {
    debug_assert!((2..64).contains(&csid) && body.len() <= MAX_BODY);
    let extended = timestamp >= EXTENDED;
    let [_, t2, t1, t0] = timestamp.min(EXTENDED).to_be_bytes();
    let [_, l2, l1, l0] = (body.len() as u32).to_be_bytes();
    out.push(csid);
    out.extend_from_slice(&[t2, t1, t0, l2, l1, l0, type_id]);
    out.extend_from_slice(&stream_id.to_le_bytes());
    if extended {
        out.extend_from_slice(&timestamp.to_be_bytes());
    }
    let mut pieces = body.chunks(chunk_size);
    if let Some(first) = pieces.next() {
        out.extend_from_slice(first);
    }
    for piece in pieces {
        out.push(0xc0 | csid);
        if extended {
            out.extend_from_slice(&timestamp.to_be_bytes());
        }
        out.extend_from_slice(piece);
    }
}

/// Appends a protocol control message with a 4-byte value (Set Chunk Size,
/// Acknowledgement, Window Acknowledgement Size).
pub(crate) fn write_control(out: &mut Vec<u8>, type_id: u8, value: u32) {
    write_message(
        out,
        CSID_CONTROL,
        0,
        type_id,
        0,
        &value.to_be_bytes(),
        DEFAULT_CHUNK_SIZE,
    );
}

/// Appends a User Control ping response echoing `timestamp`.
pub(crate) fn write_ping_response(out: &mut Vec<u8>, timestamp: [u8; 4]) {
    let [e1, e0] = UC_PING_RESPONSE.to_be_bytes();
    let body = [
        e1,
        e0,
        timestamp[0],
        timestamp[1],
        timestamp[2],
        timestamp[3],
    ];
    write_message(
        out,
        CSID_CONTROL,
        0,
        MSG_USER_CONTROL,
        0,
        &body,
        DEFAULT_CHUNK_SIZE,
    );
}

/// A complete inbound message, borrowed from the reader until its next call.
#[derive(Debug)]
pub(crate) struct Message<'a> {
    pub type_id: u8,
    /// Message stream id; only commands on stream 0 or ours arrive, so the client
    /// does not need it (the parser tests check it).
    #[cfg_attr(not(test), allow(dead_code))]
    pub stream_id: u32,
    pub body: &'a [u8],
}

/// Most chunk streams the peer may use at once.
const MAX_STREAMS: usize = 16;

#[derive(Debug, Default)]
struct InStream {
    csid: u32,
    timestamp: u32,
    delta: u32,
    length: usize,
    type_id: u8,
    stream_id: u32,
    extended: bool,
    body: Vec<u8>,
}

/// Parses the peer's chunk stream. Bounded: a message longer than `max_message` is
/// a protocol error before any of it is stored or decoded, at most [`MAX_STREAMS`]
/// chunk streams are tracked, and the unparsed input is at most one chunk plus one
/// read. Reuses its buffers, so steady-state parsing does not allocate.
#[derive(Debug)]
pub(crate) struct ChunkReader {
    input: Vec<u8>,
    pos: usize,
    chunk_size: usize,
    max_message: usize,
    streams: Vec<InStream>,
    /// Stream whose completed body was lent out by the last `next`.
    lent: Option<usize>,
}

impl ChunkReader {
    pub(crate) fn new(max_message: usize) -> Self {
        Self {
            input: Vec::with_capacity(2 * max_message.min(64 * 1024)),
            pos: 0,
            chunk_size: DEFAULT_CHUNK_SIZE,
            max_message,
            streams: Vec::with_capacity(MAX_STREAMS),
            lent: None,
        }
    }

    /// Applies the peer's Set Chunk Size (1 to 2^31 - 1).
    pub(crate) fn set_chunk_size(&mut self, size: u32) -> Result<(), &'static str> {
        let size = size & 0x7fff_ffff;
        if size == 0 {
            return Err("chunk size 0");
        }
        self.chunk_size = size as usize;
        Ok(())
    }

    /// Drops the partial message of chunk stream `csid` (Abort Message).
    pub(crate) fn abort(&mut self, csid: u32) {
        if let Some(s) = self.streams.iter_mut().find(|s| s.csid == csid) {
            s.body.clear();
        }
    }

    /// Adds received bytes.
    pub(crate) fn feed(&mut self, data: &[u8]) {
        if self.pos > 0 {
            self.input.drain(..self.pos);
            self.pos = 0;
        }
        self.input.extend_from_slice(data);
    }

    /// The next complete message, `None` when more input is needed.
    pub(crate) fn next(&mut self) -> Result<Option<Message<'_>>, &'static str> {
        if let Some(i) = self.lent.take() {
            self.streams[i].body.clear();
        }
        loop {
            match self.chunk()? {
                Step::NeedMore => return Ok(None),
                Step::Partial => {}
                Step::Complete(i) => {
                    self.lent = Some(i);
                    let s = &self.streams[i];
                    return Ok(Some(Message {
                        type_id: s.type_id,
                        stream_id: s.stream_id,
                        body: &s.body,
                    }));
                }
            }
        }
    }

    /// Parses one chunk from the input.
    fn chunk(&mut self) -> Result<Step, &'static str> {
        let data = &self.input[self.pos..];
        let Some(&b0) = data.first() else {
            return Ok(Step::NeedMore);
        };
        let fmt = b0 >> 6;
        let (csid, mut at) = match b0 & 0x3f {
            0 => match data.get(1) {
                Some(&b1) => (64 + u32::from(b1), 2),
                None => return Ok(Step::NeedMore),
            },
            1 => match data.get(1..3) {
                Some(b) => (64 + u32::from(b[0]) + 256 * u32::from(b[1]), 3),
                None => return Ok(Step::NeedMore),
            },
            id => (u32::from(id), 1),
        };
        let header_len = [11, 7, 3, 0][usize::from(fmt)];
        let Some(header) = data.get(at..at + header_len) else {
            return Ok(Step::NeedMore);
        };
        at += header_len;
        let index = match self.streams.iter().position(|s| s.csid == csid) {
            Some(i) => i,
            None if fmt != 0 => return Err("chunk without a previous header"),
            None if self.streams.len() >= MAX_STREAMS => return Err("too many chunk streams"),
            None => {
                self.streams.push(InStream {
                    csid,
                    ..InStream::default()
                });
                self.streams.len() - 1
            }
        };
        let s = &self.streams[index];
        let u24 = |b: &[u8]| u32::from_be_bytes([0, b[0], b[1], b[2]]);
        let field = (fmt < 3).then(|| u24(header));
        let extended = match field {
            Some(f) => f == EXTENDED,
            None => s.extended,
        };
        let ext_value = if extended {
            let Some(b) = data.get(at..at + 4) else {
                return Ok(Step::NeedMore);
            };
            at += 4;
            Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
        } else {
            None
        };
        let (mut length, mut type_id, mut stream_id) = (s.length, s.type_id, s.stream_id);
        if fmt <= 1 {
            length = u24(&header[3..]) as usize;
            type_id = header[6];
        }
        if fmt == 0 {
            stream_id = u32::from_le_bytes([header[7], header[8], header[9], header[10]]);
        }
        if length > self.max_message {
            return Err("message larger than the limit");
        }
        let starting = s.body.is_empty();
        let have = s.body.len();
        if !starting && fmt != 3 {
            return Err("new message header inside a message");
        }
        let take = (length - have).min(self.chunk_size);
        let Some(payload) = data.get(at..at + take) else {
            return Ok(Step::NeedMore);
        };
        let consumed = at + take;
        let s = &mut self.streams[index];
        if starting {
            let value = ext_value.or(field);
            match (fmt, value) {
                (0, Some(t)) => {
                    s.timestamp = t;
                    s.delta = 0;
                }
                (1 | 2, Some(d)) => {
                    s.delta = d;
                    s.timestamp = s.timestamp.wrapping_add(d);
                }
                _ => s.timestamp = s.timestamp.wrapping_add(s.delta),
            }
            s.length = length;
            s.type_id = type_id;
            s.stream_id = stream_id;
        }
        if fmt < 3 {
            s.extended = extended;
        }
        s.body.extend_from_slice(payload);
        self.pos += consumed;
        Ok(if s.body.len() == s.length {
            Step::Complete(index)
        } else {
            Step::Partial
        })
    }
}

enum Step {
    NeedMore,
    Partial,
    Complete(usize),
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    fn read_all(reader: &mut ChunkReader) -> Vec<(u8, u32, Vec<u8>)> {
        let mut out = Vec::new();
        while let Some(m) = reader.next().unwrap() {
            out.push((m.type_id, m.stream_id, m.body.to_vec()));
        }
        out
    }

    #[test]
    fn round_trips_chunked_messages() {
        let body: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
        for ts in [0, 1000, EXTENDED - 1, EXTENDED, u32::MAX] {
            let mut out = Vec::new();
            write_message(&mut out, CSID_VIDEO, ts, MSG_VIDEO, 1, &body, 4096);
            write_message(&mut out, CSID_AUDIO, ts, MSG_AUDIO, 1, &body[..10], 4096);
            assert_eq!(
                out.len(),
                message_size(body.len(), 4096, ts) + message_size(10, 4096, ts)
            );
            let mut reader = ChunkReader::new(64 * 1024);
            reader.set_chunk_size(4096).unwrap();
            // Byte by byte: every split point must work.
            let mut got = Vec::new();
            for b in &out {
                reader.feed(std::slice::from_ref(b));
                got.extend(read_all(&mut reader));
            }
            assert_eq!(got.len(), 2, "ts {ts}");
            assert_eq!(got[0], (MSG_VIDEO, 1, body.clone()));
            assert_eq!(got[1], (MSG_AUDIO, 1, body[..10].to_vec()));
            let s = &reader.streams[0];
            assert_eq!(s.timestamp, ts);
        }
    }

    #[test]
    fn reads_compressed_headers_and_long_ids() {
        // fmt 0 on csid 70 (2-byte basic header), then fmt 1, fmt 2 and fmt 3.
        let mut data = vec![0x00, 6, 0, 0, 10, 0, 0, 2, MSG_ACK, 0, 0, 0, 0, 0xaa, 0xbb];
        data.extend_from_slice(&[0x40, 6, 0, 0, 5, 0, 0, 1, MSG_ABORT, 0xcc]);
        data.extend_from_slice(&[0x80, 6, 0, 0, 5, 0xdd]);
        data.extend_from_slice(&[0xc0, 6, 0xee]);
        let mut reader = ChunkReader::new(1024);
        reader.feed(&data);
        let got = read_all(&mut reader);
        assert_eq!(
            got,
            vec![
                (MSG_ACK, 0, vec![0xaa, 0xbb]),
                (MSG_ABORT, 0, vec![0xcc]),
                (MSG_ABORT, 0, vec![0xdd]),
                (MSG_ABORT, 0, vec![0xee]),
            ]
        );
        assert_eq!(reader.streams[0].csid, 70);
        assert_eq!(reader.streams[0].timestamp, 10 + 5 + 5 + 5);
    }

    #[test]
    fn caps_message_size_before_reading_the_body() {
        let mut reader = ChunkReader::new(1000);
        // A header announcing 1001 bytes is refused before any body arrives.
        reader.feed(&[0x03, 0, 0, 0, 0, 0x03, 0xe9, MSG_COMMAND_AMF0, 0, 0, 0, 0]);
        assert!(reader.next().is_err());
        let mut reader = ChunkReader::new(1000);
        reader.feed(&[0xc3]);
        assert!(reader.next().is_err(), "fmt 3 without a header");
    }

    #[test]
    fn limits_chunk_streams_and_handles_abort() {
        let mut reader = ChunkReader::new(1000);
        let mut data = Vec::new();
        for csid in 3..(3 + MAX_STREAMS as u8 + 1) {
            data.extend_from_slice(&[csid, 0, 0, 0, 0, 0, 0, MSG_ACK, 0, 0, 0, 0]);
        }
        reader.feed(&data);
        assert!(read_all_result(&mut reader).is_err());

        let mut reader = ChunkReader::new(1000);
        // 200-byte message, first 128-byte chunk, then aborted, then a new message.
        let mut data = vec![0x04, 0, 0, 0, 0, 0, 200, MSG_AUDIO, 1, 0, 0, 0];
        data.extend(std::iter::repeat_n(1, 128));
        reader.feed(&data);
        assert!(reader.next().unwrap().is_none());
        reader.abort(4);
        reader.feed(&[0x04, 0, 0, 0, 0, 0, 2, MSG_AUDIO, 1, 0, 0, 0, 7, 8]);
        let got = read_all(&mut reader);
        assert_eq!(got, vec![(MSG_AUDIO, 1, vec![7, 8])]);
    }

    fn read_all_result(reader: &mut ChunkReader) -> Result<(), &'static str> {
        while reader.next()?.is_some() {}
        Ok(())
    }

    #[test]
    fn control_messages() {
        let mut out = Vec::new();
        write_control(&mut out, MSG_SET_CHUNK_SIZE, 4096);
        write_ping_response(&mut out, [1, 2, 3, 4]);
        let mut reader = ChunkReader::new(1000);
        reader.feed(&out);
        let got = read_all(&mut reader);
        assert_eq!(
            got,
            vec![
                (MSG_SET_CHUNK_SIZE, 0, vec![0, 0, 0x10, 0]),
                (MSG_USER_CONTROL, 0, vec![0, 7, 1, 2, 3, 4]),
            ]
        );
    }
}
