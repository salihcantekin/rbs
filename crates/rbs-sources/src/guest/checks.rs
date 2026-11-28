//! Checks of the guest's untrusted bytes before any decoder sees them (spec
//! `guest-source.md`, worker steps 5 and 8): the H.264 parameter sets against the
//! allowed list, the measured frame rate, and the length of an Opus packet.

use std::collections::VecDeque;

use rbs_codec::h264::{self, NAL_PPS, NAL_SPS, Sps};
use rbs_core::media::Size;

/// Largest picture a guest may send, either way round.
pub const MAX_GUEST_SIZE: Size = Size::new(3840, 2160);
/// Video frames allowed in any [`RATE_WINDOW_NS`] of sender time: 60 fps plus 10 %.
pub const MAX_FRAMES_IN_WINDOW: usize = 132;
/// The window of the frame rate check.
pub const RATE_WINDOW_NS: u64 = 2_000_000_000;

/// Why an access unit was refused. Any refusal ends the pairing with `guest_limits`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// An SPS outside the allowed list, or one that does not parse.
    Sps,
    /// A PPS too large, out of range, or naming an SPS not seen.
    Pps,
}

/// The parameter sets seen so far, checked against ADR-0139 decision 11's list.
#[derive(Debug, Clone, Default)]
pub struct ParamSets {
    sps: [Option<Sps>; 32],
    /// The cropped size of the last valid SPS.
    size: Option<Size>,
}

impl ParamSets {
    /// No parameter set seen.
    pub fn new() -> Self {
        Self::default()
    }

    /// The cropped size of the last valid SPS: every decoded picture must have it.
    pub fn size(&self) -> Option<Size> {
        self.size
    }

    /// Checks every SPS and PPS of one Annex B access unit.
    pub fn check(&mut self, access_unit: &[u8]) -> Result<(), Refused> {
        for nal in h264::nal_units(access_unit) {
            match h264::nal_type(nal) {
                Some(NAL_SPS) => {
                    let sps = h264::parse_sps(nal).map_err(|_| Refused::Sps)?;
                    if !allowed(&sps) {
                        return Err(Refused::Sps);
                    }
                    self.size = Some(sps.size);
                    self.sps[usize::from(sps.id)] = Some(sps);
                }
                Some(NAL_PPS) => {
                    let pps = h264::parse_pps(nal).map_err(|_| Refused::Pps)?;
                    if self.sps[usize::from(pps.sps_id)].is_none() {
                        return Err(Refused::Pps);
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}

fn allowed(sps: &Sps) -> bool {
    let (w, h) = (sps.size.width, sps.size.height);
    let fits = (w <= MAX_GUEST_SIZE.width && h <= MAX_GUEST_SIZE.height)
        || (h <= MAX_GUEST_SIZE.width && w <= MAX_GUEST_SIZE.height);
    sps.is_allowed_profile() && fits
}

/// The measured video frame rate on sender time: more than [`MAX_FRAMES_IN_WINDOW`]
/// frames in any [`RATE_WINDOW_NS`] is a refusal (the VUI rate may be missing).
#[derive(Debug, Clone, Default)]
pub struct FrameRate {
    times: VecDeque<u64>,
}

impl FrameRate {
    /// No frame seen.
    pub fn new() -> Self {
        Self {
            times: VecDeque::with_capacity(MAX_FRAMES_IN_WINDOW + 1),
        }
    }

    /// Notes a video frame captured at `capture_ns`; false when the rate is over.
    pub fn note(&mut self, capture_ns: u64) -> bool {
        if self.times.len() == MAX_FRAMES_IN_WINDOW + 1 {
            self.times.pop_front();
        }
        self.times.push_back(capture_ns);
        match (self.times.front(), self.times.len()) {
            (Some(&first), n) if n > MAX_FRAMES_IN_WINDOW => {
                capture_ns.saturating_sub(first) >= RATE_WINDOW_NS
            }
            _ => true,
        }
    }
}

/// Samples per channel at 48 kHz of one Opus packet, from its TOC byte (RFC 6716
/// section 3.1); `None` for an empty or malformed packet.
pub fn opus_samples(packet: &[u8]) -> Option<u32> {
    let toc = *packet.first()?;
    let config = toc >> 3;
    // Frame length in units of 2.5 ms.
    let unit = match config {
        0..=11 => [4, 8, 16, 24][usize::from(config % 4)],
        12..=15 => [4, 8][usize::from(config % 2)],
        _ => [1, 2, 4, 8][usize::from(config % 4)],
    };
    let frames = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => u32::from(*packet.get(1)? & 0x3f),
    };
    let samples = frames * unit * 120;
    // At most 120 ms in one packet.
    (frames > 0 && samples <= 5760).then_some(samples)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)] // Tests fail by panicking.

    use super::*;

    /// 1920x1080 High, level 4.0 (as in `rbs_codec::h264`'s tests).
    const SPS_1080: &[u8] = &[
        0x67, 0x64, 0x00, 0x28, 0xAC, 0xD9, 0x40, 0x78, 0x02, 0x27, 0xE5, 0xC0, 0x44, 0x00, 0x00,
        0x03, 0x00, 0x04, 0x00, 0x00, 0x03, 0x00, 0xF0, 0x3C, 0x60, 0xC6, 0x58,
    ];
    /// PPS id 0 naming SPS 0.
    const PPS: &[u8] = &[0x68, 0xEB, 0xE3, 0xCB, 0x22, 0xC0];

    fn au(nals: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for n in nals {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(n);
        }
        out
    }

    #[test]
    fn allowed_parameter_sets_pass_and_others_are_refused() {
        let mut sets = ParamSets::new();
        assert_eq!(sets.check(&au(&[PPS])), Err(Refused::Pps), "no SPS yet");
        sets.check(&au(&[SPS_1080, PPS, &[0x65, 1]])).unwrap();
        assert_eq!(sets.size(), Some(Size::new(1920, 1080)));
        // Level 5.2 is the most allowed; 6.0 is refused.
        let mut high = SPS_1080.to_vec();
        high[3] = 60;
        assert_eq!(sets.check(&au(&[&high])), Err(Refused::Sps));
        // Extended profile is refused.
        let mut ext = SPS_1080.to_vec();
        ext[1] = 88;
        assert_eq!(sets.check(&au(&[&ext])), Err(Refused::Sps));
        assert_eq!(sets.check(&au(&[&[0x67, 0xff]])), Err(Refused::Sps));
    }

    #[test]
    fn more_than_132_frames_in_two_seconds_is_refused() {
        let mut rate = FrameRate::new();
        // 60 fps for 10 s passes.
        for i in 0..600u64 {
            assert!(rate.note(i * 1_000_000_000 / 60));
        }
        let mut fast = FrameRate::new();
        let ok = (0..200u64).all(|i| fast.note(i * 1_000_000_000 / 70));
        assert!(!ok, "70 fps is over the limit");
    }

    #[test]
    fn opus_packet_lengths_come_from_the_toc() {
        // CELT 20 ms, one frame.
        assert_eq!(opus_samples(&[(31 << 3), 0]), Some(960));
        // CELT 2.5 ms, two frames.
        assert_eq!(opus_samples(&[(16 << 3) | 1]), Some(240));
        // SILK 60 ms, code 3 with 2 frames.
        assert_eq!(opus_samples(&[(3 << 3) | 3, 2]), Some(5760));
        // Code 3 with 0 frames, or above 120 ms, or empty.
        assert_eq!(opus_samples(&[(3 << 3) | 3, 0]), None);
        assert_eq!(opus_samples(&[(3 << 3) | 3, 3]), None);
        assert_eq!(opus_samples(&[]), None);
    }
}
