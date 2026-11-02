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

/// Appends `payload` (a whole PES packet, or a pointer field plus a PSI section) as
/// transport stream packets on `pid`. The last packet is padded with adaptation-field
/// stuffing. Never allocates if `out` has [`unit_size`] spare capacity.
pub fn write_unit(
    out: &mut Vec<u8>,
    pid: u16,
    cc: &mut Continuity,
    start: UnitStart,
    mut payload: &[u8],
) {
    let mut first = true;
    loop {
        // Adaptation-field contents (flags and PCR), without the length byte.
        let mut fields = [0u8; 7];
        let mut field_len = 0;
        if first && (start.pcr.is_some() || start.random_access) {
            field_len = 1;
            if start.random_access {
                fields[0] |= 0x40;
            }
            if let Some(pcr) = start.pcr {
                fields[0] |= 0x10;
                let base = pcr & ((1 << 33) - 1);
                fields[1] = (base >> 25) as u8;
                fields[2] = (base >> 17) as u8;
                fields[3] = (base >> 9) as u8;
                fields[4] = (base >> 1) as u8;
                fields[5] = (((base & 1) as u8) << 7) | 0x7e;
                fields[6] = 0;
                field_len = 7;
            }
        }
        let room = TS_PAYLOAD - if field_len > 0 { 1 + field_len } else { 0 };
        let take = payload.len().min(room);
        let stuffing = room - take;
        let has_field = field_len > 0 || stuffing > 0;

        let pusi = if first { 0x40 } else { 0 };
        let control = if has_field { 0x30 } else { 0x10 };
        out.extend_from_slice(&[
            0x47,
            pusi | ((pid >> 8) as u8 & 0x1f),
            pid as u8,
            control | cc.next(),
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
        out.extend_from_slice(&payload[..take]);
        payload = &payload[take..];
        first = false;
        if payload.is_empty() {
            return;
        }
    }
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
