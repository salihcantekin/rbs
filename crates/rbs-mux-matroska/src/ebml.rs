//! EBML element writing helpers (RFC 8794).

/// Writes an element id (its bytes already carry the length marker).
pub fn id(out: &mut Vec<u8>, id: u32) {
    let bytes = id.to_be_bytes();
    let skip = bytes.iter().take_while(|b| **b == 0).count();
    out.extend_from_slice(&bytes[skip..]);
}

/// The unknown size (all value bits set), for a live Segment.
pub const UNKNOWN_SIZE: [u8; 8] = [0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff];

/// Writes `size` as an 8-byte variable-length integer. Sizes up to 2^56 - 2 fit.
pub fn size8(out: &mut Vec<u8>, size: u64) {
    out.push(0x01);
    out.extend_from_slice(&size.to_be_bytes()[1..]);
}

/// Writes `size` as a 4-byte variable-length integer. Callers keep `size` below
/// 2^28 - 1.
pub fn size4(out: &mut Vec<u8>, size: u32) {
    out.extend_from_slice(&(size | 0x1000_0000).to_be_bytes());
}

/// Writes `size` (below 127) as a 1-byte variable-length integer.
pub fn size1(out: &mut Vec<u8>, size: u8) {
    out.push(0x80 | size);
}

/// Unsigned integer element with an 8-byte payload.
pub fn uint(out: &mut Vec<u8>, element: u32, value: u64) {
    id(out, element);
    size1(out, 8);
    out.extend_from_slice(&value.to_be_bytes());
}

/// Float element with an 8-byte payload.
pub fn float(out: &mut Vec<u8>, element: u32, value: f64) {
    id(out, element);
    size1(out, 8);
    out.extend_from_slice(&value.to_bits().to_be_bytes());
}

/// A `Void` element of exactly `total` bytes (2 to 128): id, 1-byte size, zeros.
pub fn void(out: &mut Vec<u8>, total: usize) {
    debug_assert!((2..=128).contains(&total));
    out.push(0xec);
    size1(out, (total - 2) as u8);
    out.resize(out.len() + total - 2, 0);
}

/// String or binary element.
pub fn bytes(out: &mut Vec<u8>, element: u32, value: &[u8]) {
    id(out, element);
    size8(out, value.len() as u64);
    out.extend_from_slice(value);
}

/// Starts a master element with an 8-byte size placeholder; returns where its
/// payload starts.
pub fn open(out: &mut Vec<u8>, element: u32) -> usize {
    id(out, element);
    size8(out, 0);
    out.len()
}

/// Ends the master element whose payload starts at `start`.
pub fn close(out: &mut [u8], start: usize) {
    let size = (out.len() - start) as u64;
    out[start - 7..start].copy_from_slice(&size.to_be_bytes()[1..]);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    #[test]
    fn writes_ids_sizes_and_masters() {
        let mut out = Vec::new();
        id(&mut out, 0x1a45_dfa3);
        id(&mut out, 0xa3);
        assert_eq!(out, [0x1a, 0x45, 0xdf, 0xa3, 0xa3]);
        out.clear();
        size4(&mut out, 300);
        assert_eq!(out, [0x10, 0, 0x01, 0x2c]);
        out.clear();
        let m = open(&mut out, 0xe0);
        uint(&mut out, 0xb0, 320);
        close(&mut out, m);
        assert_eq!(&out[..9], &[0xe0, 0x01, 0, 0, 0, 0, 0, 0, 10]);
        assert_eq!(&out[9..], &[0xb0, 0x88, 0, 0, 0, 0, 0, 0, 1, 0x40]);
    }
}
