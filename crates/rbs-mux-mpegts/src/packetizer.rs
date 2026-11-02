//! Splits PES packets and PSI sections into 188-byte transport stream packets.

/// Size of a transport stream packet.
pub const TS_PACKET: usize = 188;
const TS_HEADER: usize = 4;
const TS_PAYLOAD: usize = TS_PACKET - TS_HEADER;

/// Per-PID continuity counter.
#[derive(Debug, Default, Clone, Copy)]
pub struct Continuity(u8);

impl Continuity {
    /// Returns the counter for the next packet with payload and advances it.
    pub fn next(&mut self) -> u8 {
        let cc = self.0;
        self.0 = (self.0 + 1) & 0x0f;
        cc
    }

    /// The counter of the last packet with payload (for packets without payload).
    pub fn last(self) -> u8 {
        self.0.wrapping_sub(1) & 0x0f
    }
}

/// Options for the first packet of a payload unit.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct UnitStart {
    /// Program clock reference, in 90 kHz units (33 bits); the extension is zero.
    pub pcr: Option<u64>,
    /// Sets `random_access_indicator`.
    pub random_access: bool,
}

/// The exact number of bytes [`write_unit`] produces for `payload_len` bytes.
pub const fn unit_size(payload_len: usize, start: UnitStart) -> usize {
    let field = if start.pcr.is_some() {
        8 // length, flags, 6-byte PCR
    } else if start.random_access {
        2 // length, flags
    } else {
        0
    };
    let first_room = TS_PAYLOAD - field;
    let packets = if payload_len <= first_room {
        1
    } else {
        1 + (payload_len - first_room).div_ceil(TS_PAYLOAD)
    };
    packets * TS_PACKET
}

/// Writes one payload unit (a PES packet) as transport stream packets, from pieces
/// pushed in order, so the payload never needs to be assembled first. The total length
/// is fixed up front, which decides every packet's layout; the last packet is padded
/// with adaptation-field stuffing. Never allocates if `out` has [`unit_size`] spare
/// capacity.
pub struct UnitWriter<'a> {
    out: &'a mut Vec<u8>,
    pid: u16,
    cc: &'a mut Continuity,
    start: UnitStart,
    first: bool,
    /// Payload bytes still to come, in total.
    remaining: usize,
    /// Payload bytes still to come in the open packet.
    room: usize,
}

impl<'a> UnitWriter<'a> {
    /// Starts a unit of `total` payload bytes on `pid`.
    pub fn new(
        out: &'a mut Vec<u8>,
        pid: u16,
        cc: &'a mut Continuity,
        start: UnitStart,
        total: usize,
    ) -> Self {
        Self {
            out,
            pid,
            cc,
            start,
            first: true,
            remaining: total,
            room: 0,
        }
    }

    /// Appends the next piece of the payload. Pieces beyond the declared total are
    /// cut off (a caller bug, caught by a debug assertion).
    pub fn push(&mut self, mut data: &[u8]) {
        debug_assert!(data.len() <= self.remaining, "more payload than declared");
        while !data.is_empty() && self.remaining > 0 {
            if self.room == 0 {
                self.open_packet();
            }
            let take = data.len().min(self.room);
            self.out.extend_from_slice(&data[..take]);
            self.room -= take;
            self.remaining -= take;
            data = &data[take..];
        }
    }

    /// Ends the unit. Debug builds check that exactly the declared total was pushed.
    pub fn finish(self) {
        debug_assert_eq!(self.remaining, 0, "less payload than declared");
    }

    fn open_packet(&mut self) {
        // Adaptation-field contents (flags and PCR), without the length byte.
        let mut fields = [0u8; 7];
        let mut field_len = 0;
        if self.first && (self.start.pcr.is_some() || self.start.random_access) {
            field_len = 1;
            if self.start.random_access {
                fields[0] |= 0x40;
            }
            if let Some(pcr) = self.start.pcr {
                fields[0] |= 0x10;
                fields[1..7].copy_from_slice(&pcr_bytes(pcr));
                field_len = 7;
            }
        }
        let room = TS_PAYLOAD - if field_len > 0 { 1 + field_len } else { 0 };
        let take = self.remaining.min(room);
        let stuffing = room - take;
        let has_field = field_len > 0 || stuffing > 0;

        let pusi = if self.first { 0x40 } else { 0 };
        let control = if has_field { 0x30 } else { 0x10 };
        let out = &mut *self.out;
        out.extend_from_slice(&[
            0x47,
            pusi | ((self.pid >> 8) as u8 & 0x1f),
            self.pid as u8,
            control | self.cc.next(),
        ]);
        if has_field {
            if field_len == 0 {
                // Stuffing only: a length byte, then (if there is room) empty flags.
                let length = stuffing - 1;
                out.push(length as u8);
                if length > 0 {
                    out.push(0);
                    out.resize(out.len() + length - 1, 0xff);
                }
            } else {
                out.push((field_len + stuffing) as u8);
                out.extend_from_slice(&fields[..field_len]);
                out.resize(out.len() + stuffing, 0xff);
            }
        }
        self.first = false;
        self.room = take;
    }
}

/// The six PCR bytes: 33-bit base in 90 kHz units, reserved bits, extension 0.
fn pcr_bytes(pcr: u64) -> [u8; 6] {
    let base = pcr & ((1 << 33) - 1);
    [
        (base >> 25) as u8,
        (base >> 17) as u8,
        (base >> 9) as u8,
        (base >> 1) as u8,
        (((base & 1) as u8) << 7) | 0x7e,
        0,
    ]
}

/// Appends a packet on `pid` with only an adaptation field carrying `pcr` (no payload,
/// so the continuity counter does not advance).
pub fn write_pcr_only(out: &mut Vec<u8>, pid: u16, cc: Continuity, pcr: u64) {
    out.extend_from_slice(&[
        0x47,
        (pid >> 8) as u8 & 0x1f,
        pid as u8,
        // Adaptation field only; the counter repeats the last payload packet's value.
        0x20 | cc.last(),
        183,
        0x10,
    ]);
    out.extend_from_slice(&pcr_bytes(pcr));
    out.resize(out.len() + 183 - 7, 0xff);
}

#[cfg(test)]
/// Appends `payload` as one unit (see [`UnitWriter`]).
pub fn write_unit(
    out: &mut Vec<u8>,
    pid: u16,
    cc: &mut Continuity,
    start: UnitStart,
    payload: &[u8],
) {
    let mut writer = UnitWriter::new(out, pid, cc, start, payload.len());
    writer.push(payload);
    writer.finish();
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    fn packets(out: &[u8]) -> Vec<&[u8]> {
        assert_eq!(out.len() % TS_PACKET, 0);
        out.chunks(TS_PACKET).collect()
    }

    #[test]
    fn payload_lengths_around_packet_boundaries() {
        for len in [1, 182, 183, 184, 185, 367, 368, 369, 1000] {
            for start in [
                UnitStart::default(),
                UnitStart {
                    pcr: Some(123_456),
                    random_access: true,
                },
            ] {
                let payload: Vec<u8> = (0..len).map(|i| i as u8).collect();
                let mut out = Vec::new();
                let mut cc = Continuity::default();
                write_unit(&mut out, 0x100, &mut cc, start, &payload);
                assert_eq!(out.len(), unit_size(len, start), "len {len}");
                let mut recovered = Vec::new();
                for (i, p) in packets(&out).iter().enumerate() {
                    assert_eq!(p[0], 0x47);
                    assert_eq!(p[1] & 0x40 != 0, i == 0);
                    assert_eq!(usize::from(p[3] & 0x0f), i % 16);
                    let mut at = 4;
                    if p[3] & 0x20 != 0 {
                        at += 1 + usize::from(p[4]);
                    }
                    recovered.extend_from_slice(&p[at..]);
                }
                assert_eq!(recovered, payload, "len {len}");
            }
        }
    }

    #[test]
    fn first_packet_carries_pcr_and_random_access() {
        let mut out = Vec::new();
        let pcr = 0x1_2345_6789_u64;
        let start = UnitStart {
            pcr: Some(pcr),
            random_access: true,
        };
        write_unit(
            &mut out,
            0x101,
            &mut Continuity::default(),
            start,
            &[0; 400],
        );
        let p = &out[..TS_PACKET];
        assert_eq!(p[5], 0x50);
        let base = (u64::from(p[6]) << 25)
            | (u64::from(p[7]) << 17)
            | (u64::from(p[8]) << 9)
            | (u64::from(p[9]) << 1)
            | u64::from(p[10] >> 7);
        assert_eq!(base, pcr);
        assert_eq!(
            out[TS_PACKET + 3] & 0x20,
            0,
            "second packet has no adaptation field"
        );
    }

    #[test]
    fn pieces_give_the_same_packets_as_one_payload() {
        let payload: Vec<u8> = (0..1000).map(|i| (i * 7) as u8).collect();
        let start = UnitStart {
            pcr: Some(99),
            random_access: true,
        };
        let mut whole = Vec::new();
        write_unit(
            &mut whole,
            0x100,
            &mut Continuity::default(),
            start,
            &payload,
        );
        let mut pieces = Vec::new();
        let mut cc = Continuity::default();
        let mut w = UnitWriter::new(&mut pieces, 0x100, &mut cc, start, payload.len());
        for piece in payload.chunks(37) {
            w.push(piece);
        }
        w.finish();
        assert_eq!(whole, pieces);
    }

    #[test]
    fn pcr_only_packet_keeps_the_counter() {
        let mut out = Vec::new();
        let mut cc = Continuity::default();
        write_unit(&mut out, 0x100, &mut cc, UnitStart::default(), &[1; 10]);
        write_pcr_only(&mut out, 0x100, cc, 1234);
        let p = &out[TS_PACKET..];
        assert_eq!(p.len(), TS_PACKET);
        assert_eq!(
            p[3], 0x20,
            "adaptation only, counter of the previous packet (0)"
        );
        assert_eq!((p[4], p[5]), (183, 0x10));
    }

    #[test]
    fn stays_within_reserved_capacity() {
        let start = UnitStart {
            pcr: Some(1),
            random_access: true,
        };
        let mut out = Vec::with_capacity(unit_size(5000, start));
        let ptr = out.as_ptr();
        write_unit(
            &mut out,
            0x100,
            &mut Continuity::default(),
            start,
            &[7; 5000],
        );
        assert_eq!(out.as_ptr(), ptr);
        assert_eq!(out.len(), out.capacity());
    }
}
