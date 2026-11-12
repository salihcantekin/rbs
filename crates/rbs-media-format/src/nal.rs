//! H.264 / HEVC NAL units: iteration and conversion between Annex B (start codes) and
//! length-prefixed (AVCC / HVCC) framing.

use crate::FormatError;

/// Iterates over the NAL units of an Annex B buffer (`00 00 01` or `00 00 00 01`
/// start codes), without the start codes. Never allocates.
pub fn annexb_units(data: &[u8]) -> AnnexBUnits<'_> {
    AnnexBUnits {
        data,
        pos: find_start(data, 0).map_or(data.len(), |(_, after)| after),
    }
}

/// The first byte (the NAL unit header) of the first unit of an Annex B buffer. Reads
/// only up to the first start code, unlike `annexb_units(data).next()`, which also
/// searches for the end of the unit.
pub fn annexb_first_header(data: &[u8]) -> Option<u8> {
    let (_, after) = find_start(data, 0)?;
    data.get(after).copied()
}

/// The header bytes (first byte) of the units of an Annex B buffer, in order. Each step
/// searches only for the next start code, so stopping early (for example at the first
/// slice) never reads the rest of the buffer.
pub fn annexb_unit_headers(data: &[u8]) -> impl Iterator<Item = u8> + '_ {
    let mut pos = 0;
    std::iter::from_fn(move || {
        let (_, after) = find_start(data, pos)?;
        pos = after;
        data.get(after).copied()
    })
}

/// Iterator returned by [`annexb_units`].
#[derive(Debug, Clone)]
pub struct AnnexBUnits<'a> {
    data: &'a [u8],
    pos: usize,
}

/// Finds the next start code at or after `from`; returns (start of the code, first
/// byte after it).
///
/// Two skips keep this fast on slice data, where zero bytes are rare: eight bytes at a
/// time while a word holds no zero byte (no start code can begin there), and the usual
/// skip-ahead step of three bytes while no `00` or `01` is in sight.
fn find_start(data: &[u8], from: usize) -> Option<(usize, usize)> {
    const LOW: u64 = 0x0101_0101_0101_0101;
    const HIGH: u64 = 0x8080_8080_8080_8080;
    let mut i = from;
    loop {
        while let Some(word) = data.get(i..i + 8) {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(word);
            let x = u64::from_le_bytes(bytes);
            // Classic "has a zero byte" test.
            if x.wrapping_sub(LOW) & !x & HIGH != 0 {
                break;
            }
            i += 8;
        }
        if i + 3 > data.len() {
            return None;
        }
        match data[i + 2] {
            0 => i += 1,
            1 if data[i] == 0 && data[i + 1] == 0 => {
                // `00 00 00 01`: the code starts one byte earlier.
                let start = if i > from && data[i - 1] == 0 {
                    i - 1
                } else {
                    i
                };
                return Some((start, i + 3));
            }
            _ => i += 3,
        }
    }
}

impl<'a> Iterator for AnnexBUnits<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        while self.pos < self.data.len() {
            let start = self.pos;
            let (end, next) = match find_start(self.data, start) {
                Some((code, after)) => (code, after),
                None => (self.data.len(), self.data.len()),
            };
            self.pos = next;
            // Trailing zero bytes belong to the next start code, not to this unit.
            let mut unit_end = end;
            while unit_end > start && self.data[unit_end - 1] == 0 {
                unit_end -= 1;
            }
            if unit_end > start {
                return Some(&self.data[start..unit_end]);
            }
        }
        None
    }
}

/// Iterates over the NAL units of a length-prefixed buffer. Yields an error and stops
/// when a length runs past the end.
pub fn length_prefixed_units(data: &[u8], length_size: u8) -> LengthPrefixedUnits<'_> {
    LengthPrefixedUnits {
        data,
        pos: 0,
        length_size: usize::from(length_size),
    }
}

/// Iterator returned by [`length_prefixed_units`].
#[derive(Debug, Clone)]
pub struct LengthPrefixedUnits<'a> {
    data: &'a [u8],
    pos: usize,
    length_size: usize,
}

impl<'a> Iterator for LengthPrefixedUnits<'a> {
    type Item = Result<&'a [u8], FormatError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.data.len() {
            return None;
        }
        if !matches!(self.length_size, 1 | 2 | 4) {
            self.pos = self.data.len();
            return Some(Err(FormatError::Invalid("NAL length size")));
        }
        let Some(prefix) = self.data.get(self.pos..self.pos + self.length_size) else {
            self.pos = self.data.len();
            return Some(Err(FormatError::Truncated("NAL length")));
        };
        let len = prefix
            .iter()
            .fold(0_usize, |acc, b| (acc << 8) | usize::from(*b));
        let start = self.pos + self.length_size;
        // `checked_add`: on 32-bit targets a 4-byte length can overflow `usize`.
        let Some(unit) = start
            .checked_add(len)
            .and_then(|end| self.data.get(start..end))
        else {
            self.pos = self.data.len();
            return Some(Err(FormatError::Truncated("NAL unit")));
        };
        self.pos = start + len;
        Some(Ok(unit))
    }
}

/// Appends `data` (Annex B) to `out` as length-prefixed NAL units with 4-byte lengths.
/// Does not allocate when `out` has capacity for `data.len() + 4` bytes per unit. On
/// error `out` is left as it was.
pub fn annexb_to_length_prefixed(data: &[u8], out: &mut Vec<u8>) -> Result<(), FormatError> {
    let start = out.len();
    for unit in annexb_units(data) {
        let Ok(len) = u32::try_from(unit.len()) else {
            out.truncate(start);
            return Err(FormatError::Invalid("NAL size"));
        };
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(unit);
    }
    Ok(())
}

/// Appends length-prefixed `data` to `out` as Annex B with 4-byte start codes. When
/// `prefix_units` is given (typically SPS and PPS before an IDR for MPEG-TS), those
/// units are written first. Zero-length units are skipped. On error `out` is left as
/// it was.
pub fn length_prefixed_to_annexb(
    data: &[u8],
    length_size: u8,
    prefix_units: &[&[u8]],
    out: &mut Vec<u8>,
) -> Result<(), FormatError> {
    let start = out.len();
    for unit in prefix_units {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(unit);
    }
    for unit in length_prefixed_units(data, length_size) {
        match unit {
            Ok([]) => {}
            Ok(unit) => {
                out.extend_from_slice(&[0, 0, 0, 1]);
                out.extend_from_slice(unit);
            }
            Err(e) => {
                out.truncate(start);
                return Err(e);
            }
        }
    }
    Ok(())
}

/// True for H.264 VCL NAL unit types (coded slices, 1..=5). Parameter sets and other
/// non-VCL units of an access unit come before its first VCL unit.
pub fn h264_is_vcl(kind: u8) -> bool {
    (1..=5).contains(&kind)
}

/// H.264 NAL unit type (low five bits of the first byte).
pub fn h264_type(unit: &[u8]) -> Option<u8> {
    unit.first().map(|b| b & 0x1f)
}

/// H.264 NAL unit types used by muxers.
pub mod h264 {
    /// Coded slice of an IDR picture.
    pub const IDR: u8 = 5;
    /// Supplemental enhancement information.
    pub const SEI: u8 = 6;
    /// Sequence parameter set.
    pub const SPS: u8 = 7;
    /// Picture parameter set.
    pub const PPS: u8 = 8;
    /// Access unit delimiter.
    pub const AUD: u8 = 9;
}

/// The HEVC NAL unit type (6 bits, `ISO/IEC 23008-2`).
pub fn hevc_type(unit: &[u8]) -> Option<u8> {
    unit.first().map(|b| (b >> 1) & 0x3f)
}

/// HEVC NAL unit types used by muxers.
pub mod hevc {
    /// Video parameter set.
    pub const VPS: u8 = 32;
    /// Sequence parameter set.
    pub const SPS: u8 = 33;
    /// Picture parameter set.
    pub const PPS: u8 = 34;
    /// Access unit delimiter.
    pub const AUD: u8 = 35;
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    const ANNEXB: &[u8] = &[
        0, 0, 0, 1, 0x67, 1, 2, // SPS
        0, 0, 1, 0x68, 3, // PPS
        0, 0, 0, 1, 0x65, 4, 5, 0, // IDR with a trailing zero
    ];

    #[test]
    fn iterates_annexb_units() {
        let units: Vec<&[u8]> = annexb_units(ANNEXB).collect();
        assert_eq!(units, [&[0x67, 1, 2][..], &[0x68, 3], &[0x65, 4, 5]]);
        assert_eq!(annexb_units(&[]).count(), 0);
        assert_eq!(annexb_units(&[1, 2, 3]).count(), 0);
    }

    #[test]
    fn converts_both_ways() {
        let mut lp = Vec::new();
        annexb_to_length_prefixed(ANNEXB, &mut lp).unwrap();
        assert_eq!(
            lp,
            [
                0, 0, 0, 3, 0x67, 1, 2, 0, 0, 0, 2, 0x68, 3, 0, 0, 0, 3, 0x65, 4, 5
            ]
        );
        let mut back = Vec::new();
        length_prefixed_to_annexb(&lp, 4, &[], &mut back).unwrap();
        let units: Vec<&[u8]> = annexb_units(&back).collect();
        assert_eq!(units.len(), 3);
        assert_eq!(h264_type(units[2]), Some(h264::IDR));

        let mut with_prefix = Vec::new();
        length_prefixed_to_annexb(&lp[13..], 4, &[&[0x09, 0xf0]], &mut with_prefix).unwrap();
        assert_eq!(&with_prefix[..6], &[0, 0, 0, 1, 0x09, 0xf0]);
    }

    #[test]
    fn length_prefixed_errors_are_reported() {
        let mut it = length_prefixed_units(&[0, 0, 0, 9, 1], 4);
        assert_eq!(it.next(), Some(Err(FormatError::Truncated("NAL unit"))));
        assert_eq!(it.next(), None);
        let mut bad = length_prefixed_units(&[1, 2], 3);
        assert!(matches!(bad.next(), Some(Err(FormatError::Invalid(_)))));
        let two: Vec<_> = length_prefixed_units(&[0, 1, 0xaa, 0, 1, 0xbb], 2)
            .map(Result::unwrap)
            .collect();
        assert_eq!(two, [&[0xaa][..], &[0xbb]]);
    }

    /// The byte-by-byte search the skip-ahead search must match.
    fn naive_find_start(data: &[u8], from: usize) -> Option<(usize, usize)> {
        (from..data.len().saturating_sub(2)).find_map(|i| {
            if data[i..i + 3] != [0, 0, 1] {
                return None;
            }
            let start = if i > from && data[i - 1] == 0 {
                i - 1
            } else {
                i
            };
            Some((start, i + 3))
        })
    }

    #[test]
    fn skip_ahead_search_matches_naive_search() {
        // Pseudo-random bytes drawn mostly from {0, 1, 2} so start codes are dense.
        let mut state = 0x2545_f491_u32;
        let data: Vec<u8> = (0..20_000)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                match state % 8 {
                    0..=3 => 0,
                    4 | 5 => 1,
                    6 => 2,
                    _ => (state >> 8) as u8,
                }
            })
            .collect();
        for from in 0..data.len() {
            assert_eq!(
                find_start(&data, from),
                naive_find_start(&data, from),
                "from {from}"
            );
        }
    }

    #[test]
    fn first_header_reads_past_the_first_start_code() {
        assert_eq!(
            annexb_first_header(&[0, 0, 0, 1, 0x09, 0xf0, 0, 0, 1, 0x65]),
            Some(0x09)
        );
        assert_eq!(annexb_first_header(&[0, 0, 1]), None);
        assert_eq!(annexb_first_header(&[7, 7]), None);
    }

    #[test]
    fn failed_conversions_leave_the_output_unchanged() {
        let mut out = vec![9];
        assert!(
            length_prefixed_to_annexb(&[0, 0, 0, 1, 0x65, 0, 0, 0, 9], 4, &[], &mut out).is_err()
        );
        assert_eq!(out, [9]);
        let mut empty_unit = Vec::new();
        length_prefixed_to_annexb(&[0, 0, 0, 0, 0, 0, 0, 1, 0x41], 4, &[], &mut empty_unit)
            .unwrap();
        assert_eq!(empty_unit, [0, 0, 0, 1, 0x41], "zero-length unit skipped");
    }

    #[test]
    fn word_skip_matches_naive_search_on_sparse_zeros() {
        let mut state = 0x1234_5677_u32;
        let mut data: Vec<u8> = (0..20_000)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                (state >> 24) as u8
            })
            .collect();
        // Plant start codes at every alignment.
        for (n, at) in (100..19_000).step_by(997).enumerate() {
            let code: &[u8] = if n % 2 == 0 {
                &[0, 0, 1]
            } else {
                &[0, 0, 0, 1]
            };
            data[at..at + code.len()].copy_from_slice(code);
        }
        for from in (0..data.len()).step_by(7) {
            assert_eq!(
                find_start(&data, from),
                naive_find_start(&data, from),
                "from {from}"
            );
        }
    }

    #[test]
    fn unit_headers_match_units() {
        let headers: Vec<u8> = annexb_unit_headers(ANNEXB).collect();
        let units: Vec<u8> = annexb_units(ANNEXB).map(|u| u[0]).collect();
        assert_eq!(headers, units);
    }

    #[test]
    fn conversion_reuses_capacity() {
        let mut out = Vec::with_capacity(64);
        let ptr = out.as_ptr();
        annexb_to_length_prefixed(ANNEXB, &mut out).unwrap();
        assert_eq!(out.as_ptr(), ptr);
    }
}
