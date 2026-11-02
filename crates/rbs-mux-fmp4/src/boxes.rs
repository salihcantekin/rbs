//! ISO BMFF box writing helpers.

/// Starts a box: writes a placeholder size and the type, returns the box start.
pub fn open(out: &mut Vec<u8>, kind: &[u8; 4]) -> usize {
    let start = out.len();
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(kind);
    start
}

/// Starts a full box (version and 24-bit flags).
pub fn open_full(out: &mut Vec<u8>, kind: &[u8; 4], version: u8, flags: u32) -> usize {
    let start = open(out, kind);
    out.push(version);
    out.extend_from_slice(&flags.to_be_bytes()[1..]);
    start
}

/// Ends the box started at `start` by writing its size. Boxes this muxer writes stay
/// far below 4 GiB (one fragment of media at most).
pub fn close(out: &mut [u8], start: usize) {
    let size = (out.len() - start) as u32;
    out[start..start + 4].copy_from_slice(&size.to_be_bytes());
}

/// Appends a big-endian `u16`.
pub fn u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_be_bytes());
}

/// Appends a big-endian `u32`.
pub fn u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}

/// Appends a big-endian `u64`.
pub fn u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_be_bytes());
}

/// Appends `n` zero bytes.
pub fn zeros(out: &mut Vec<u8>, n: usize) {
    out.resize(out.len() + n, 0);
}

/// The unity transformation matrix of `mvhd` and `tkhd`.
pub fn matrix(out: &mut Vec<u8>) {
    for v in [0x0001_0000_u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000] {
        u32(out, v);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    #[test]
    fn nested_boxes_get_their_sizes() {
        let mut out = Vec::new();
        let outer = open(&mut out, b"moov");
        let inner = open_full(&mut out, b"mvhd", 1, 3);
        u32(&mut out, 7);
        close(&mut out, inner);
        close(&mut out, outer);
        assert_eq!(&out[..8], &[0, 0, 0, 24, b'm', b'o', b'o', b'v']);
        assert_eq!(
            &out[8..20],
            &[0, 0, 0, 16, b'm', b'v', b'h', b'd', 1, 0, 0, 3]
        );
    }
}
