//! The RFC 8251 conformance vectors, as a slow test.
//!
//! The vectors (about 75 MB) are not in the repository. Download
//! `opus_testvectors-rfc8251.tar.gz` from <https://opus-codec.org/testvectors/>, unpack it
//! and run
//!
//! ```text
//! RBS_OPUS_TESTVECTORS=/path/to/opus_newvectors cargo test -p rbs-codec-opus -- --ignored slow_
//! ```
//!
//! Each `.bit` packet records the encoder's final range coder state, which must equal the
//! decoder's after the packet: that proves the entropy decoding is bit-exact. The sound is
//! compared with the reference `.dec` by signal-to-noise ratio (a float decoder is not
//! bit-exact with the fixed-point reference; `opus_compare` is the official judge and was
//! run on these outputs, see the commit message).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use crate::ffi::Decoder;

fn dir() -> Option<PathBuf> {
    std::env::var_os("RBS_OPUS_TESTVECTORS").map(PathBuf::from)
}

fn read_packets(bytes: &[u8]) -> Vec<(Vec<u8>, u32)> {
    let mut packets = Vec::new();
    let mut at = 0;
    while at + 8 <= bytes.len() {
        let len = u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
        let range = u32::from_be_bytes(bytes[at + 4..at + 8].try_into().unwrap());
        at += 8;
        packets.push((bytes[at..at + len].to_vec(), range));
        at += len;
    }
    packets
}

fn to_i16(sample: f32) -> i16 {
    (sample * 32768.0).round().clamp(-32768.0, 32767.0) as i16
}

#[test]
#[ignore = "slow: needs the RFC 8251 vectors, see the module docs"]
fn slow_rfc8251_vectors_decode_bit_exact_entropy_and_close_sound() {
    let dir = dir().expect("set RBS_OPUS_TESTVECTORS to the opus_newvectors folder");
    let out_dir = std::env::var_os("RBS_OPUS_VECTOR_OUT").map(PathBuf::from);
    for n in 1..=12 {
        let bit = std::fs::read(dir.join(format!("testvector{n:02}.bit"))).unwrap();
        let reference = std::fs::read(dir.join(format!("testvector{n:02}.dec"))).unwrap();
        let mut decoder = Decoder::new(48_000, 2).unwrap();
        let mut pcm = vec![0.0_f32; 5760 * 2];
        let mut produced: Vec<u8> = Vec::new();
        for (index, (packet, range)) in read_packets(&bit).iter().enumerate() {
            let frames = crate::ffi::packet_samples(packet, 48_000).unwrap();
            let got = decoder.decode(Some(packet), &mut pcm, frames).unwrap();
            assert_eq!(got, frames, "vector {n} packet {index}");
            assert_eq!(
                decoder.final_range().unwrap(),
                *range,
                "vector {n} packet {index}: final range differs"
            );
            for sample in &pcm[..got * 2] {
                produced.extend_from_slice(&to_i16(*sample).to_le_bytes());
            }
        }
        assert_eq!(produced.len(), reference.len(), "vector {n} length");
        if let Some(out) = &out_dir {
            std::fs::write(out.join(format!("out{n:02}.dec")), &produced).unwrap();
        }
        let (mut signal, mut noise) = (0.0_f64, 0.0_f64);
        for (a, b) in produced.chunks_exact(2).zip(reference.chunks_exact(2)) {
            let x = f64::from(i16::from_le_bytes([a[0], a[1]]));
            let y = f64::from(i16::from_le_bytes([b[0], b[1]]));
            signal += y * y;
            noise += (x - y) * (x - y);
        }
        let snr = 10.0 * (signal / noise.max(1.0)).log10();
        eprintln!(
            "vector {n:02}: {} packets, SNR {snr:.1} dB",
            read_packets(&bit).len()
        );
        assert!(snr > 40.0, "vector {n}: SNR {snr:.1} dB");
    }
}
