//! Any pasted text reaches `Envelope::decode`, and any payload that passes it reaches
//! the adapter's payload decoder and SDP readers. None may panic, hang or allocate
//! without bound (ADR-0139 decision 16).

#![no_main]

use libfuzzer_sys::fuzz_target;
use rbs_peer::Envelope;
use rbs_peer_str0m::engine::{sdp_candidates, sdp_fingerprint, sdp_ice_credentials};
use rbs_peer_str0m::payload;

fn sdp(bytes: &[u8]) {
    if let Ok(sdp) = payload::decode(bytes) {
        let _ = (sdp_fingerprint(&sdp), sdp_candidates(&sdp), sdp_ice_credentials(&sdp));
    }
}

fuzz_target!(|data: &[u8]| {
    // The raw bytes as a payload, and as pasted text.
    sdp(data);
    if let Ok(text) = std::str::from_utf8(data) {
        if let Ok(envelope) = Envelope::decode(text) {
            sdp(&envelope.payload);
        }
    }
});
