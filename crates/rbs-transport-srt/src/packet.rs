//! SRT packet layout (the SRT protocol draft, draft-sharabayko-srt, section 3, as
//! implemented by libsrt 1.5): the 16-byte header shared by data and control
//! packets, the handshake and its extensions, and the control packets a live-mode
//! sender reads and writes. Pure functions over byte slices; writers append to a
//! caller-owned `Vec` and never allocate once it has its working capacity.

/// Bytes of the header of every packet.
pub(crate) const HEADER_LEN: usize = 16;
/// Sequence numbers are 31 bits and wrap.
pub(crate) const SEQ_MASK: u32 = 0x7FFF_FFFF;
/// Message numbers are 26 bits and wrap to 1 (0 means "no message").
pub(crate) const MSG_MASK: u32 = 0x03FF_FFFF;

/// Control packet types.
pub(crate) mod ctrl {
    pub(crate) const HANDSHAKE: u16 = 0x0000;
    pub(crate) const KEEPALIVE: u16 = 0x0001;
    pub(crate) const ACK: u16 = 0x0002;
    pub(crate) const NAK: u16 = 0x0003;
    pub(crate) const SHUTDOWN: u16 = 0x0005;
    pub(crate) const ACKACK: u16 = 0x0006;
    pub(crate) const DROPREQ: u16 = 0x0007;
}

/// Handshake types (the "Handshake Type" field). Values from 1000 up are rejections.
pub(crate) mod hs {
    pub(crate) const INDUCTION: u32 = 0x0000_0001;
    pub(crate) const CONCLUSION: u32 = 0xFFFF_FFFF;
    /// Rejections are sent as this plus the reason (`URQ_FAILURE_TYPE`).
    pub(crate) const REJECT_FIRST: u32 = 1000;
    /// The magic the listener puts in the extension field of its induction answer
    /// to say it speaks handshake version 5.
    pub(crate) const SRT_MAGIC: u16 = 0x4A17;
    /// Extension field of the caller's induction: `UDT_DGRAM` socket type.
    pub(crate) const UDT_DGRAM: u16 = 2;
    /// Extension field flags of the conclusion.
    pub(crate) const EXT_HSREQ: u16 = 0x1;
    pub(crate) const EXT_CONFIG: u16 = 0x4;
    /// Extension block types.
    pub(crate) const CMD_HSREQ: u16 = 1;
    pub(crate) const CMD_HSRSP: u16 = 2;
    pub(crate) const CMD_SID: u16 = 5;
}

/// SRT version announced in the HSREQ extension (1.5.0, the libsrt line this was
/// checked against).
pub(crate) const SRT_VERSION: u32 = 0x0001_0500;
/// HSREQ flags: TSBPD sender and receiver, crypto support, too-late packet drop,
/// periodic NAK reports and the retransmission flag (the set libsrt announces for
/// live mode, without packet filters).
pub(crate) const HSREQ_FLAGS: u32 = 0x01 | 0x02 | 0x04 | 0x08 | 0x10 | 0x20;
/// Longest stream id (bytes) the SID extension carries.
pub(crate) const MAX_STREAM_ID: usize = 512;
/// Bytes of the handshake's fixed part (the CIF before extensions).
pub(crate) const HANDSHAKE_LEN: usize = 48;

/// `a + n` in sequence space.
pub(crate) const fn seq_add(a: u32, n: u32) -> u32 {
    a.wrapping_add(n) & SEQ_MASK
}

/// Signed distance `a - b` in sequence space.
pub(crate) const fn seq_diff(a: u32, b: u32) -> i32 {
    let d = a.wrapping_sub(b) & SEQ_MASK;
    if d > SEQ_MASK / 2 {
        d as i32 - (SEQ_MASK as i32) - 1
    } else {
        d as i32
    }
}

/// The message number after `m`.
pub(crate) const fn msg_next(m: u32) -> u32 {
    let n = (m + 1) & MSG_MASK;
    if n == 0 { 1 } else { n }
}

fn put(out: &mut Vec<u8>, word: u32) {
    out.extend_from_slice(&word.to_be_bytes());
}

fn word(buf: &[u8], i: usize) -> Option<u32> {
    let b = buf.get(i * 4..i * 4 + 4)?;
    Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

/// Appends a data packet: a whole message in one packet ("solo", PP = 11), not in
/// order (O = 0), not encrypted, with the retransmission flag `rexmit`.
pub(crate) fn write_data(
    out: &mut Vec<u8>,
    seq: u32,
    msg: u32,
    rexmit: bool,
    ts: u32,
    dest: u32,
    payload: &[u8],
) {
    put(out, seq & SEQ_MASK);
    put(
        out,
        0xC000_0000 | (u32::from(rexmit) << 26) | (msg & MSG_MASK),
    );
    put(out, ts);
    put(out, dest);
    out.extend_from_slice(payload);
}

/// Appends the header of a control packet; the caller appends its CIF.
pub(crate) fn write_control(out: &mut Vec<u8>, kind: u16, info: u32, ts: u32, dest: u32) {
    put(out, 0x8000_0000 | (u32::from(kind) << 16));
    put(out, info);
    put(out, ts);
    put(out, dest);
}

/// Appends a control packet whose CIF is four zero bytes, as libsrt sends keep-alive,
/// shutdown and ACKACK packets.
pub(crate) fn write_short_control(out: &mut Vec<u8>, kind: u16, info: u32, ts: u32, dest: u32) {
    write_control(out, kind, info, ts, dest);
    put(out, 0);
}

/// Appends a DROPREQ for the sequence range `first..=last` (message number 0: the
/// receiver drops by range).
pub(crate) fn write_dropreq(out: &mut Vec<u8>, first: u32, last: u32, ts: u32, dest: u32) {
    write_control(out, ctrl::DROPREQ, 0, ts, dest);
    put(out, first & SEQ_MASK);
    put(out, last & SEQ_MASK);
}

/// A received control packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Control<'a> {
    pub kind: u16,
    /// Type-specific information (the ACK number of an ACK).
    pub info: u32,
    pub dest: u32,
    pub cif: &'a [u8],
}

/// Reads a received packet: `Some` for a control packet, `None` for a data packet or
/// anything shorter than a header (a sender gets no data packets).
pub(crate) fn parse_control(buf: &[u8]) -> Option<Control<'_>> {
    let w0 = word(buf, 0)?;
    if w0 & 0x8000_0000 == 0 || buf.len() < HEADER_LEN {
        return None;
    }
    Some(Control {
        kind: ((w0 >> 16) & 0x7FFF) as u16,
        info: word(buf, 1)?,
        dest: word(buf, 3)?,
        cif: &buf[HEADER_LEN..],
    })
}

/// The fixed part of a handshake CIF.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Handshake {
    pub version: u32,
    pub encryption: u16,
    pub extension: u16,
    pub isn: u32,
    pub mtu: u32,
    pub flow_window: u32,
    pub kind: u32,
    pub socket_id: u32,
    pub cookie: u32,
    /// The peer address as libsrt writes it: an IPv4 address in the first word with
    /// its bytes in reverse order, an IPv6 address as four such words.
    pub peer_ip: [u32; 4],
}

impl Handshake {
    /// Appends the 48-byte CIF.
    pub(crate) fn write(&self, out: &mut Vec<u8>) {
        put(out, self.version);
        put(
            out,
            (u32::from(self.encryption) << 16) | u32::from(self.extension),
        );
        put(out, self.isn);
        put(out, self.mtu);
        put(out, self.flow_window);
        put(out, self.kind);
        put(out, self.socket_id);
        put(out, self.cookie);
        for w in self.peer_ip {
            put(out, w);
        }
    }

    /// Reads the fixed part of a handshake CIF.
    pub(crate) fn parse(cif: &[u8]) -> Option<Self> {
        let w = |i| word(cif, i);
        let fields = w(1)?;
        Some(Self {
            version: w(0)?,
            encryption: (fields >> 16) as u16,
            extension: fields as u16,
            isn: w(2)?,
            mtu: w(3)?,
            flow_window: w(4)?,
            kind: w(5)?,
            socket_id: w(6)?,
            cookie: w(7)?,
            peer_ip: [w(8)?, w(9)?, w(10)?, w(11)?],
        })
    }
}

/// The peer address field for `ip`, in libsrt's layout (see [`Handshake::peer_ip`]).
pub(crate) fn peer_ip(ip: std::net::IpAddr) -> [u32; 4] {
    let mut words = [0; 4];
    match ip {
        std::net::IpAddr::V4(v4) => words[0] = u32::from_le_bytes(v4.octets()),
        std::net::IpAddr::V6(v6) => {
            for (w, b) in words.iter_mut().zip(v6.octets().chunks_exact(4)) {
                *w = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
            }
        }
    }
    words
}

/// Appends the HSREQ extension: version, flags, and the receiver and sender TSBPD
/// delays (both `latency_ms`, as libsrt does).
pub(crate) fn write_hsreq(out: &mut Vec<u8>, latency_ms: u16) {
    put(out, (u32::from(hs::CMD_HSREQ) << 16) | 3);
    put(out, SRT_VERSION);
    put(out, HSREQ_FLAGS);
    put(out, (u32::from(latency_ms) << 16) | u32::from(latency_ms));
}

/// Appends the SID extension. libsrt converts the whole handshake to network order
/// word by word, so the characters of each 4-byte group are reversed on the wire;
/// the last group is padded with zero bytes first.
pub(crate) fn write_stream_id(out: &mut Vec<u8>, sid: &[u8]) {
    let words = sid.len().div_ceil(4);
    put(out, (u32::from(hs::CMD_SID) << 16) | words as u32);
    for group in sid.chunks(4) {
        let mut b = [0_u8; 4];
        b[..group.len()].copy_from_slice(group);
        out.extend_from_slice(&[b[3], b[2], b[1], b[0]]);
    }
}

/// Iterates the extension blocks after the fixed handshake part: `(type, content)`.
/// Stops at the first malformed block.
pub(crate) fn extensions(cif: &[u8]) -> impl Iterator<Item = (u16, &[u8])> {
    let mut rest = cif.get(HANDSHAKE_LEN..).unwrap_or_default();
    std::iter::from_fn(move || {
        let head = word(rest, 0)?;
        let len = (head & 0xFFFF) as usize * 4;
        let content = rest.get(4..4 + len)?;
        rest = &rest[4 + len..];
        Some(((head >> 16) as u16, content))
    })
}

/// The receiver TSBPD delay (milliseconds) of an HSRSP extension: the latency the
/// listener applies to what we send.
pub(crate) fn hsrsp_receiver_delay(content: &[u8]) -> Option<u16> {
    word(content, 2).map(|w| (w >> 16) as u16)
}

/// The fields of an ACK that a sender uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Ack {
    /// The next sequence number the receiver expects: everything before it arrived.
    pub seq: u32,
    /// Round-trip time in microseconds (full and small ACKs).
    pub rtt_us: Option<u32>,
    /// Round-trip time variance in microseconds (full and small ACKs).
    pub rtt_var_us: Option<u32>,
    /// Free space of the receiver's buffer in packets (full and small ACKs).
    pub available: Option<u32>,
    /// A light ACK (sequence number only) is not answered with an ACKACK.
    pub light: bool,
}

/// Reads an ACK's CIF.
pub(crate) fn parse_ack(cif: &[u8]) -> Option<Ack> {
    let seq = word(cif, 0)? & SEQ_MASK;
    Some(Ack {
        seq,
        rtt_us: word(cif, 1),
        rtt_var_us: word(cif, 2),
        available: word(cif, 3),
        light: cif.len() < 16,
    })
}

/// Iterates the ranges `(first, last)` of a NAK's loss list: a word with the top bit
/// set starts a range ended by the next word; any other word is a single packet.
pub(crate) fn nak_ranges(cif: &[u8]) -> impl Iterator<Item = (u32, u32)> {
    let mut words = cif
        .chunks_exact(4)
        .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
    std::iter::from_fn(move || {
        let w = words.next()?;
        if w & 0x8000_0000 != 0 {
            let last = words.next()?;
            Some((w & SEQ_MASK, last & SEQ_MASK))
        } else {
            Some((w, w))
        }
    })
}

/// Whether a handshake type is a rejection: libsrt sends 1000 plus the reason, and
/// treats every type from 1000 up (below the special values from `0xFFFF_FFFD`) as
/// one.
pub(crate) const fn is_rejection(kind: u32) -> bool {
    kind >= hs::REJECT_FIRST && kind < 0xFFFF_FFFD
}

/// A readable text for a rejection handshake type: the reason (type minus 1000)
/// and its meaning. Reasons below 1000 are libsrt's own, 1000 to 1999 the access
/// control codes of the stream id convention (1403: bad stream id), 2000 and up
/// the listener application's own.
pub(crate) fn rejection(kind: u32) -> String {
    const NAMES: [&str; 18] = [
        "unknown reason",
        "system error on the listener",
        "rejected by the peer",
        "listener out of resources",
        "rogue peer",
        "listener backlog full",
        "internal program error",
        "listener socket closing",
        "SRT version not supported",
        "rendezvous cookie collision",
        "wrong passphrase",
        "the listener requires encryption",
        "message API mismatch",
        "congestion control mismatch",
        "packet filter mismatch",
        "group mismatch",
        "listener timeout",
        "crypto mismatch",
    ];
    let reason = kind.saturating_sub(hs::REJECT_FIRST);
    let name = match reason {
        0..=17 => NAMES[reason as usize],
        1400 => "bad request (access control)",
        1401 => "unauthorized (access control)",
        1402 => "overloaded (access control)",
        1403 => "forbidden: bad stream id (access control)",
        1404 => "resource not found (access control)",
        1405 => "bad mode (access control)",
        1406 => "unacceptable (access control)",
        1407 => "conflict: resource in use (access control)",
        1415 => "unsupported media type (access control)",
        1423 => "locked (access control)",
        1424 => "failed dependency (access control)",
        1500 => "internal server error (access control)",
        1501 => "not implemented (access control)",
        1502 => "gateway error (access control)",
        1503 => "service unavailable (access control)",
        1505 => "SRT version not supported (access control)",
        1507 => "no room (access control)",
        1000..=1999 => "access control",
        r if r >= 2000 => "rejected by the listener's application",
        _ => "rejected",
    };
    format!("handshake rejected with reason {reason}: {name}")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// The conclusion libsrt 1.5.3 (ffmpeg 6.1) sends as a caller with
    /// `streamid=abcdefghij` and `latency=200 ms`, captured on loopback.
    const LIBSRT_CONCLUSION: &str = "8000000000000000000003d10000000000000005000000056\
        58b9a03000005dc00002000ffffffff20aba1d900c00c1e0100007f000000000000000000000000\
        0001000300010503000000bf00c800c800050003646362616867666500006a69";

    #[test]
    fn writes_the_conclusion_like_libsrt() {
        let wire = hex(LIBSRT_CONCLUSION);
        let control = parse_control(&wire).unwrap();
        assert_eq!((control.kind, control.dest), (ctrl::HANDSHAKE, 0));
        let h = Handshake::parse(control.cif).unwrap();
        assert_eq!(h.version, 5);
        assert_eq!(h.extension, hs::EXT_HSREQ | hs::EXT_CONFIG);
        assert_eq!(h.kind, hs::CONCLUSION);
        assert_eq!(
            h.peer_ip,
            peer_ip(std::net::Ipv4Addr::LOCALHOST.into()),
            "IPv4 in the first word, bytes reversed"
        );
        let exts: Vec<_> = extensions(control.cif).collect();
        assert_eq!(exts.len(), 2);
        assert_eq!(exts[0].0, hs::CMD_HSREQ);
        assert_eq!(exts[1].0, hs::CMD_SID);
        // Our writer produces the same handshake and SID bytes; HSREQ differs only in
        // the version and the packet-filter flag (0x80), which we do not announce.
        let mut out = Vec::new();
        write_control(&mut out, ctrl::HANDSHAKE, 0, 0x3d1, 0);
        h.write(&mut out);
        write_hsreq(&mut out, 200);
        write_stream_id(&mut out, b"abcdefghij");
        assert_eq!(out.len(), wire.len());
        assert_eq!(
            out[..HEADER_LEN + HANDSHAKE_LEN],
            wire[..HEADER_LEN + HANDSHAKE_LEN]
        );
        let hsreq = HEADER_LEN + HANDSHAKE_LEN;
        assert_eq!(out[hsreq..hsreq + 4], wire[hsreq..hsreq + 4]);
        assert_eq!(
            out[hsreq + 12..],
            wire[hsreq + 12..],
            "delays and stream id"
        );
        assert_eq!(word(&wire[hsreq + 4..], 0), Some(0x0001_0503));
        assert_eq!(word(&wire[hsreq + 8..], 0), Some(0xbf));
        assert_eq!(HSREQ_FLAGS, 0x3f);
    }

    #[test]
    fn sequence_arithmetic_wraps_at_31_bits() {
        assert_eq!(seq_add(SEQ_MASK, 1), 0);
        assert_eq!(seq_add(SEQ_MASK - 1, 3), 1);
        assert_eq!(seq_diff(1, SEQ_MASK), 2);
        assert_eq!(seq_diff(SEQ_MASK, 1), -2);
        assert_eq!(seq_diff(100, 100), 0);
        assert_eq!(msg_next(MSG_MASK), 1);
        assert_eq!(msg_next(1), 2);
    }

    #[test]
    fn data_header_marks_a_solo_message() {
        let mut out = Vec::new();
        write_data(&mut out, 0x8000_0005, 7, true, 99, 42, b"ts");
        assert_eq!(word(&out, 0), Some(5), "the top bit is the data marker");
        assert_eq!(word(&out, 1), Some(0xC000_0000 | 1 << 26 | 7));
        assert_eq!((word(&out, 2), word(&out, 3)), (Some(99), Some(42)));
        assert_eq!(&out[16..], b"ts");
        assert!(parse_control(&out).is_none());
    }

    #[test]
    fn reads_acks_and_loss_lists() {
        let mut cif = Vec::new();
        for w in [0x8000_0010_u32, 10_000, 500, 8000, 1, 2, 3] {
            put(&mut cif, w);
        }
        let ack = parse_ack(&cif).unwrap();
        assert_eq!(
            (
                ack.seq,
                ack.rtt_us,
                ack.rtt_var_us,
                ack.available,
                ack.light
            ),
            (0x10, Some(10_000), Some(500), Some(8000), false)
        );
        assert!(parse_ack(&cif[..4]).unwrap().light);
        let mut nak = Vec::new();
        for w in [5_u32, 0x8000_0009, 12, 20] {
            put(&mut nak, w);
        }
        let ranges: Vec<_> = nak_ranges(&nak).collect();
        assert_eq!(ranges, vec![(5, 5), (9, 12), (20, 20)]);
        // A range start without its end is ignored.
        assert_eq!(nak_ranges(&nak[4..8]).count(), 0);
    }

    #[test]
    fn extension_walk_stops_at_truncated_blocks() {
        let mut cif = vec![0; HANDSHAKE_LEN];
        put(&mut cif, (2 << 16) | 3);
        put(&mut cif, 1);
        assert_eq!(extensions(&cif).count(), 0);
        // libsrt puts 1000 + reason on the wire (checked: reason 1403 -> 2403).
        let text = rejection(2403);
        assert!(
            text.contains("1403") && text.contains("stream id"),
            "{text}"
        );
        assert!(rejection(1010).contains("passphrase"));
        assert!(rejection(3005).contains("application"));
        assert!(is_rejection(1000) && is_rejection(3005));
        assert!(!is_rejection(hs::CONCLUSION) && !is_rejection(hs::INDUCTION));
    }
}
