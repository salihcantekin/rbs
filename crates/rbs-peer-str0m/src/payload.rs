//! The WebRTC payload inside a pairing code (docs/specs/peer-port.md, "Code format";
//! ADR-0139 decision 2): a compressed SDP, so the planned WHIP output and browser
//! guests reuse the same adapter.
//!
//! Version 1: one byte `1`, then the SDP as raw deflate (RFC 1951). The SDP is
//! trimmed to what RBS-to-RBS needs (H.264, Opus, three header extensions) so a code
//! with 8 candidates fits the 4 KiB limit.

use str0m::RtcConfig;
use str0m::rtp::Extension;

/// The payload format this adapter writes.
pub const PAYLOAD_VERSION: u8 = 1;
/// Most candidates in one code (ADR-0139 decision 4).
pub const MAX_CANDIDATES: usize = 8;
/// Largest SDP accepted after decompression.
pub const MAX_SDP_LEN: usize = 16 * 1024;

/// Why a payload was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadError {
    /// Empty, an unknown version, broken deflate, too large or not UTF-8.
    Malformed,
    /// A newer payload version.
    NewerVersion,
    /// More than [`MAX_CANDIDATES`] candidates.
    TooManyCandidates,
}

/// The str0m configuration both sides use: only H.264 and Opus, and only the header
/// extensions RBS reads (the media id, the sender's capture time for sync, and the
/// transport sequence number for the bandwidth estimate).
pub fn rtc_config() -> RtcConfig {
    str0m::Rtc::builder()
        .clear_codecs()
        .enable_h264(true)
        .enable_opus(true, false)
        .clear_extension_map()
        .set_extension(1, Extension::RtpMid)
        .set_extension(2, Extension::AbsoluteCaptureTime)
        .set_extension(3, Extension::TransportSequenceNumber)
}

/// Compresses an SDP into a payload.
pub fn encode(sdp: &str) -> Vec<u8> {
    let mut out = vec![PAYLOAD_VERSION];
    out.extend_from_slice(&miniz_oxide::deflate::compress_to_vec(sdp.as_bytes(), 9));
    out
}

/// Reads a payload back into SDP text and checks its candidate count.
pub fn decode(payload: &[u8]) -> Result<String, PayloadError> {
    let (&version, body) = payload.split_first().ok_or(PayloadError::Malformed)?;
    match version {
        PAYLOAD_VERSION => {}
        0 => return Err(PayloadError::Malformed),
        _ => return Err(PayloadError::NewerVersion),
    }
    let bytes = miniz_oxide::inflate::decompress_to_vec_with_limit(body, MAX_SDP_LEN)
        .map_err(|_| PayloadError::Malformed)?;
    let sdp = String::from_utf8(bytes).map_err(|_| PayloadError::Malformed)?;
    let candidates = sdp
        .lines()
        .filter(|l| l.starts_with("a=candidate:"))
        .count();
    if candidates > MAX_CANDIDATES {
        return Err(PayloadError::TooManyCandidates);
    }
    Ok(sdp)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn a_payload_round_trips() {
        let sdp = "v=0\r\na=candidate:1 1 udp 1 10.0.0.1 5000 typ host\r\n";
        assert_eq!(decode(&encode(sdp)).unwrap(), sdp);
    }

    #[test]
    fn bad_payloads_are_refused() {
        assert_eq!(decode(&[]), Err(PayloadError::Malformed));
        assert_eq!(decode(&[0, 1]), Err(PayloadError::Malformed));
        assert_eq!(decode(&[2, 1]), Err(PayloadError::NewerVersion));
        assert_eq!(decode(&[1, 0xff, 0xff]), Err(PayloadError::Malformed));
        let many: String = (0..9)
            .map(|i| format!("a=candidate:{i} 1 udp 1 10.0.0.{i} 5000 typ host\r\n"))
            .collect();
        assert_eq!(decode(&encode(&many)), Err(PayloadError::TooManyCandidates));
        // A deflate bomb stops at the limit.
        let bomb = encode(&"a".repeat(MAX_SDP_LEN + 1));
        assert_eq!(decode(&bomb), Err(PayloadError::Malformed));
    }
}
