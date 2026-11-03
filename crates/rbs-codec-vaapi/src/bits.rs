//! A bit writer for H.264 / HEVC parameter sets: fixed-width and Exp-Golomb
//! fields, RBSP trailing bits and NAL units with emulation prevention.

/// Writes bits most significant first.
#[derive(Debug, Default)]
pub(crate) struct BitWriter {
    bytes: Vec<u8>,
    /// Bits used in the last byte (0 when byte-aligned).
    used: u8,
}

impl BitWriter {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// The low `bits` bits of `value` (`bits` up to 32).
    pub(crate) fn u(&mut self, bits: u8, value: u32) {
        debug_assert!(bits <= 32);
        for i in (0..bits).rev() {
            self.bit((value >> i) & 1 == 1);
        }
    }

    pub(crate) fn flag(&mut self, value: bool) {
        self.bit(value);
    }

    /// Unsigned Exp-Golomb `ue(v)`.
    pub(crate) fn ue(&mut self, value: u32) {
        let v = u64::from(value) + 1;
        let len = 64 - v.leading_zeros() as u8;
        for _ in 1..len {
            self.bit(false);
        }
        for i in (0..len).rev() {
            self.bit((v >> i) & 1 == 1);
        }
    }

    /// Signed Exp-Golomb `se(v)`.
    pub(crate) fn se(&mut self, value: i32) {
        let mapped = if value > 0 {
            (value as u32) * 2 - 1
        } else {
            value.unsigned_abs() * 2
        };
        self.ue(mapped);
    }

    /// `rbsp_trailing_bits()`: a one, then zeros to the byte boundary.
    pub(crate) fn trailing(&mut self) {
        self.bit(true);
        while self.used != 0 {
            self.bit(false);
        }
    }

    /// Bits written.
    pub(crate) fn bits(&self) -> u32 {
        let full = self.bytes.len() as u32 * 8;
        if self.used == 0 {
            full
        } else {
            full - 8 + u32::from(self.used)
        }
    }

    /// The bytes written; the last one is padded with zeros.
    pub(crate) fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    fn bit(&mut self, one: bool) {
        if self.used == 0 {
            self.bytes.push(0);
        }
        if one {
            if let Some(last) = self.bytes.last_mut() {
                *last |= 0x80 >> self.used;
            }
        }
        self.used = (self.used + 1) % 8;
    }
}

/// Appends a NAL unit to `out` in Annex B: a four-byte start code, `header` and
/// `rbsp` with emulation prevention bytes inserted.
pub(crate) fn annex_b_nal(out: &mut Vec<u8>, header: &[u8], rbsp: &[u8]) {
    out.extend_from_slice(&[0, 0, 0, 1]);
    out.extend_from_slice(header);
    let mut zeros = 0;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    #[test]
    fn exp_golomb() {
        let mut w = BitWriter::new();
        // ue: 0 -> 1, 1 -> 010, 2 -> 011, 3 -> 00100; se: -1 -> 011, 2 -> 00100.
        w.ue(0);
        w.ue(1);
        w.ue(2);
        w.ue(3);
        w.se(-1);
        w.se(2);
        w.trailing();
        // 1 010 011 00100 011 00100 1 + pad
        // 1010 0110 | 0100 0110 | 0100 1000
        assert_eq!(w.into_bytes(), vec![0xa6, 0x46, 0x48]);
    }

    #[test]
    fn large_values() {
        let mut w = BitWriter::new();
        w.ue(u32::MAX);
        let bytes = w.into_bytes();
        // 32 zeros, then 33 bits of 2^32.
        assert_eq!(bytes.len(), 9);
        assert_eq!(&bytes[..4], &[0, 0, 0, 0]);
        assert_eq!(bytes[4], 0x80);
    }

    #[test]
    fn emulation_prevention() {
        let mut out = Vec::new();
        annex_b_nal(&mut out, &[0x67], &[0, 0, 1, 0, 0, 0, 0, 3, 5]);
        assert_eq!(
            out,
            vec![0, 0, 0, 1, 0x67, 0, 0, 3, 1, 0, 0, 3, 0, 0, 3, 3, 5]
        );
    }
}
