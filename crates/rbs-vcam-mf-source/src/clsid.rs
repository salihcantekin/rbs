//! The per-user source class id (ADR-0070 decision 1): a name-based UUID (RFC 9562
//! version 5, SHA-1) over [`NAMESPACE`] and the user's SID string. The setup tool, the
//! app and the source compute it the same way; the source refuses a class whose id does
//! not match the SID its registry key names.

/// RBS's namespace for virtual camera class ids (a fixed random UUID).
pub const NAMESPACE: [u8; 16] = [
    0x6b, 0x2f, 0x0c, 0x41, 0x93, 0x5e, 0x4b, 0x7a, 0x8d, 0x13, 0x52, 0xe9, 0x0f, 0xa4, 0x77, 0xc8,
];

/// The class id for `sid` (for example `S-1-5-21-...`), as the 16 bytes of the UUID in
/// network order (RFC 9562).
pub fn for_sid(sid: &str) -> [u8; 16] {
    let mut input = Vec::with_capacity(16 + sid.len());
    input.extend_from_slice(&NAMESPACE);
    input.extend_from_slice(sid.as_bytes());
    let hash = sha1(&input);
    let mut uuid = [0u8; 16];
    uuid.copy_from_slice(&hash[..16]);
    uuid[6] = (uuid[6] & 0x0f) | 0x50; // version 5
    uuid[8] = (uuid[8] & 0x3f) | 0x80; // RFC 9562 variant
    uuid
}

/// The class id in registry form: `{xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx}`, upper case.
pub fn braced(uuid: &[u8; 16]) -> String {
    let h = |r: std::ops::Range<usize>| -> String {
        uuid[r].iter().map(|b| format!("{b:02X}")).collect()
    };
    format!(
        "{{{}-{}-{}-{}-{}}}",
        h(0..4),
        h(4..6),
        h(6..8),
        h(8..10),
        h(10..16)
    )
}

/// SHA-1 (FIPS 180-4). Used only to derive a stable id, not for security: the binding
/// of class to user is protected by the registry key's access list (ADR-0070).
fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [
        0x6745_2301,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];
    let mut msg = data.to_vec();
    let bits = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bits.to_be_bytes());
    for block in msg.chunks_exact(64) {
        let mut w = [0u32; 80];
        for (i, word) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let t = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e]) {
            *x = x.wrapping_add(y);
        }
    }
    let mut out = [0u8; 20];
    for (i, x) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&x.to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn sha1_matches_the_fips_vectors() {
        assert_eq!(
            hex(&sha1(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(hex(&sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(
            hex(&sha1(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
    }

    #[test]
    fn uuid_v5_matches_the_rfc_example() {
        // RFC 9562 appendix A.4: DNS namespace, "www.example.com".
        let dns = [
            0x6b, 0xa7, 0xb8, 0x10, 0x9d, 0xad, 0x11, 0xd1, 0x80, 0xb4, 0x00, 0xc0, 0x4f, 0xd4,
            0x30, 0xc8,
        ];
        let mut input = dns.to_vec();
        input.extend_from_slice(b"www.example.com");
        let hash = sha1(&input);
        let mut uuid = [0u8; 16];
        uuid.copy_from_slice(&hash[..16]);
        uuid[6] = (uuid[6] & 0x0f) | 0x50;
        uuid[8] = (uuid[8] & 0x3f) | 0x80;
        assert_eq!(braced(&uuid), "{2ED6657D-E927-568B-95E1-2665A8AEA6A2}");
    }

    #[test]
    fn each_sid_gets_its_own_stable_version_5_id() {
        let a = for_sid("S-1-5-21-1-2-3-1001");
        let b = for_sid("S-1-5-21-1-2-3-1002");
        assert_ne!(a, b);
        assert_eq!(a, for_sid("S-1-5-21-1-2-3-1001"));
        assert_eq!(a[6] >> 4, 5);
        assert_eq!(a[8] >> 6, 0b10);
        let s = braced(&a);
        assert_eq!(s.len(), 38);
        assert!(s.starts_with('{') && s.ends_with('}'));
    }

    #[test]
    fn known_vectors() {
        // ADR-0070 decision 1 and the virtual camera spec list these.
        assert_eq!(
            braced(&for_sid("S-1-5-21-1004336348-1177238915-682003330-1001")),
            "{2C18A9A6-54B7-599A-958F-A50EA1BAB8E3}"
        );
        assert_eq!(
            braced(&for_sid("S-1-5-21-1-2-3-1001")),
            "{AA0E7BC1-FADC-559C-92B3-0272623DF480}"
        );
    }
}
