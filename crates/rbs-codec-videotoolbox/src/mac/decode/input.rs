//! What one Annex B access unit carries, checked before VideoToolbox sees a byte
//! (backend-ports.md, live video decode invariant 2), and its conversion to the
//! length-prefixed (AVCC) samples VideoToolbox decodes. Safe Rust only.

use rbs_codec::VideoDecodeError;
use rbs_codec::h264::{self, NAL_IDR, NAL_PPS, NAL_SPS, Sps, SpsError};
use rbs_core::media::{ColorPrimaries, ColorRange, ColorSpace, MatrixCoefficients, Size};
use rbs_core::media::{TransferFunction, VideoFormat};
use rbs_core::time::Timestamp;

use super::MAX_DECODE_SIZE;

/// NAL type of a non-IDR slice.
pub(super) const NAL_SLICE: u8 = 1;
/// NAL type of supplemental enhancement information, passed to the decoder.
const NAL_SEI: u8 = 6;

/// NAL types refused outright: unspecified (0), SPS extension (13), prefix (14),
/// subset SPS (15) and coded slice extension (20), i.e. SVC and MVC streams.
const REFUSED_NAL_TYPES: [u8; 5] = [0, 13, 14, 15, 20];

/// VUI `transfer_characteristics` of HDR curves: PQ (16) and HLG (18).
const HDR_TRANSFERS: [u8; 2] = [16, 18];

/// Most picture parameter sets one packet may carry.
pub(super) const MAX_PPS: usize = 4;

/// Bytes of the length prefix of every NAL unit in a sample.
pub(super) const LENGTH_SIZE: usize = 4;

/// What one packet carries, after its SPS passed the checks.
#[derive(Debug, Clone, Default)]
pub(super) struct Seen<'a> {
    /// A slice (NAL type 1 or 5): the packet makes a picture.
    pub(super) vcl: bool,
    /// An IDR slice.
    pub(super) idr: bool,
    /// The last SPS of the packet, with its parsed fields.
    pub(super) sps: Option<(&'a [u8], Sps)>,
    /// Its picture parameter sets.
    pub(super) pps: [Option<&'a [u8]>; MAX_PPS],
    /// Length of the AVCC sample made of the NAL units VideoToolbox decodes.
    pub(super) sample_len: usize,
}

impl Seen<'_> {
    /// True when the packet carries at least one PPS.
    pub(super) fn has_pps(&self) -> bool {
        self.pps[0].is_some()
    }
}

pub(super) fn invalid(what: &str) -> VideoDecodeError {
    VideoDecodeError::InvalidInput(what.into())
}

pub(super) fn unsupported(what: &str) -> VideoDecodeError {
    VideoDecodeError::Unsupported(what.into())
}

/// True for NAL units copied into the sample: slices and SEI. Parameter sets go to the
/// format description; delimiters, filler and the rest are dropped.
fn decoded(nal_type: u8) -> bool {
    matches!(nal_type, NAL_SLICE | NAL_IDR | NAL_SEI)
}

/// Checks the NAL units of `data` and every SPS in it (`max_size` is the cropped size
/// limit of the session).
pub(super) fn inspect(data: &[u8], max_size: Size) -> Result<Seen<'_>, VideoDecodeError> {
    let mut seen = Seen::default();
    let mut any = false;
    let mut pps = 0;
    for nal in h264::nal_units(data) {
        any = true;
        let header = nal.first().copied().unwrap_or(0x80);
        if header & 0x80 != 0 {
            return Err(invalid("forbidden_zero_bit set"));
        }
        let nal_type = header & 0x1f;
        match nal_type {
            t if REFUSED_NAL_TYPES.contains(&t) => {
                return Err(invalid(&format!("NAL type {t}")));
            }
            NAL_SPS => seen.sps = Some((nal, check_sps(nal, max_size)?)),
            NAL_PPS => {
                let slot = seen
                    .pps
                    .get_mut(pps)
                    .ok_or_else(|| invalid("too many picture parameter sets"))?;
                *slot = Some(nal);
                pps += 1;
            }
            NAL_IDR => {
                seen.idr = true;
                seen.vcl = true;
            }
            NAL_SLICE => seen.vcl = true,
            _ => {}
        }
        if decoded(nal_type) {
            if u32::try_from(nal.len()).is_err() {
                return Err(invalid("NAL unit above 4 GiB"));
            }
            seen.sample_len = seen
                .sample_len
                .checked_add(LENGTH_SIZE + nal.len())
                .ok_or_else(|| invalid("packet too large"))?;
        }
    }
    if !any {
        return Err(invalid("no Annex B start code"));
    }
    Ok(seen)
}

/// Parses an SPS and applies the session's and the adapter's limits.
pub(super) fn check_sps(nal: &[u8], max_size: Size) -> Result<Sps, VideoDecodeError> {
    let sps = match h264::parse_sps(nal) {
        Ok(sps) => sps,
        Err(SpsError::Unsupported(what)) => return Err(unsupported(what)),
        Err(e) => return Err(invalid(&format!("SPS: {e:?}"))),
    };
    if sps.size.width > max_size.width || sps.size.height > max_size.height {
        return Err(invalid("picture above max_size"));
    }
    if !sps.is_allowed_profile() {
        return Err(unsupported("profile, level, chroma format or bit depth"));
    }
    if sps.size.width > MAX_DECODE_SIZE.width || sps.size.height > MAX_DECODE_SIZE.height {
        return Err(unsupported("picture above 4096 x 2304"));
    }
    if sps
        .color
        .is_some_and(|c| HDR_TRANSFERS.contains(&c.transfer))
    {
        // Pictures are labelled with the BT.709 transfer; an HDR curve would be shown
        // wrong, so the stream goes to another adapter instead.
        return Err(unsupported("HDR transfer (PQ or HLG)"));
    }
    Ok(sps)
}

/// True when a refused packet leaves the decoder without a valid reference: it carried
/// a slice or an SPS, so the next pictures wait for a keyframe.
pub(super) fn carries_slice_or_sps(data: &[u8]) -> bool {
    h264::nal_units(data).any(|n| matches!(h264::nal_type(n), Some(NAL_SLICE | NAL_IDR | NAL_SPS)))
}

/// Writes the length-prefixed sample of `data` (the NAL units [`decoded`] keeps) into
/// `out`, which is exactly [`Seen::sample_len`] bytes. Returns false when `out` has
/// another length (never written past).
pub(super) fn write_sample(data: &[u8], out: &mut [u8]) -> bool {
    let mut at = 0;
    for nal in h264::nal_units(data) {
        if !h264::nal_type(nal).is_some_and(decoded) {
            continue;
        }
        let Ok(len) = u32::try_from(nal.len()) else {
            return false;
        };
        let end = at + LENGTH_SIZE + nal.len();
        let Some(dst) = out.get_mut(at..end) else {
            return false;
        };
        dst[..LENGTH_SIZE].copy_from_slice(&len.to_be_bytes());
        dst[LENGTH_SIZE..].copy_from_slice(nal);
        at = end;
    }
    at == out.len()
}

/// The output colour of a stream: the VUI's matrix, primaries and range (8-bit SDR, so
/// the transfer is BT.709), BT.709 limited when the SPS states none.
pub(super) fn color_of(sps: &Sps) -> ColorSpace {
    let Some(c) = sps.color else {
        return ColorSpace::BT709_LIMITED;
    };
    let matrix = match c.matrix {
        5 | 6 => MatrixCoefficients::Bt601,
        9 => MatrixCoefficients::Bt2020Ncl,
        _ => MatrixCoefficients::Bt709,
    };
    let primaries = match c.primaries {
        5 => ColorPrimaries::Bt601Pal,
        6 | 7 => ColorPrimaries::Bt601Ntsc,
        9 => ColorPrimaries::Bt2020,
        _ => ColorPrimaries::Bt709,
    };
    let range = if c.full_range {
        ColorRange::Full
    } else {
        ColorRange::Limited
    };
    ColorSpace::new(primaries, TransferFunction::Bt709, matrix, range)
}

/// Where one picture lies in the decoded buffer, and what it is stamped with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Placement {
    pub(super) pts: Timestamp,
    /// Cropped (display) size.
    pub(super) size: Size,
    /// Coded size.
    pub(super) coded: Size,
    /// Crop origin in the coded picture.
    pub(super) left: u32,
    pub(super) top: u32,
    pub(super) color: ColorSpace,
}

impl Placement {
    pub(super) fn of(sps: &Sps, pts: Timestamp) -> Self {
        Self {
            pts,
            size: sps.size,
            coded: sps.coded_size,
            left: sps.crop_left,
            top: sps.crop_top,
            color: color_of(sps),
        }
    }

    pub(super) fn format(&self) -> VideoFormat {
        VideoFormat::new(self.size, rbs_core::media::PixelFormat::Nv12, self.color)
    }

    /// The crop origin inside a decoded buffer of `width` x `height`: none when the
    /// buffer already has the display size (VideoToolbox applied the crop), the SPS
    /// offsets when it has the coded size, `None` when the picture does not fit or is
    /// not NV12-aligned.
    pub(super) fn origin_in(&self, width: u32, height: u32) -> Option<(u32, u32)> {
        let even = |v: u32| v.is_multiple_of(2);
        if !even(self.size.width) || !even(self.size.height) {
            return None;
        }
        if width == self.size.width && height == self.size.height {
            return Some((0, 0));
        }
        let fits = even(self.left)
            && even(self.top)
            && u64::from(self.left) + u64::from(self.size.width) <= u64::from(width)
            && u64::from(self.top) + u64::from(self.size.height) <= u64::from(height);
        fits.then_some((self.left, self.top))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// A 1920x1080 High profile level 4.0 SPS (coded 1088), as in `rbs_codec::h264`.
    const SPS_1080: &[u8] = &[
        0x67, 0x64, 0x00, 0x28, 0xAC, 0xD9, 0x40, 0x78, 0x02, 0x27, 0xE5, 0xC0, 0x44, 0x00, 0x00,
        0x03, 0x00, 0x04, 0x00, 0x00, 0x03, 0x00, 0xF0, 0x3C, 0x60, 0xC6, 0x58,
    ];
    const BIG: Size = Size::new(1920, 1088);

    fn annex_b(nals: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for n in nals {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(n);
        }
        out
    }

    #[test]
    fn a_keyframe_is_split_into_parameter_sets_and_a_sample() {
        let data = annex_b(&[
            &[0x09, 0xf0],
            SPS_1080,
            &[0x68, 0xce, 0x3c, 0x80],
            &[0x65, 0x88, 0x84],
        ]);
        let seen = inspect(&data, BIG).unwrap();
        assert!(seen.vcl && seen.idr && seen.has_pps());
        let (sps, info) = seen.sps.unwrap();
        assert_eq!(sps, SPS_1080);
        assert_eq!(info.size, Size::new(1920, 1080));
        // Only the IDR slice goes into the sample.
        assert_eq!(seen.sample_len, 4 + 3);
        let mut out = vec![0; seen.sample_len];
        assert!(write_sample(&data, &mut out));
        assert_eq!(out, [0, 0, 0, 3, 0x65, 0x88, 0x84]);
        let mut short = vec![0; seen.sample_len - 1];
        assert!(!write_sample(&data, &mut short));
    }

    #[test]
    fn limits_and_forbidden_units_are_typed_errors() {
        let data = annex_b(&[SPS_1080]);
        assert!(matches!(
            inspect(&data, Size::new(1280, 720)),
            Err(VideoDecodeError::InvalidInput(_))
        ));
        for t in REFUSED_NAL_TYPES {
            let data = annex_b(&[&[t, 0x42]]);
            assert!(matches!(
                inspect(&data, BIG),
                Err(VideoDecodeError::InvalidInput(_))
            ));
        }
        assert!(inspect(&[0x41; 20], BIG).is_err());
        assert!(inspect(&annex_b(&[&[0x80, 1]]), BIG).is_err());
        let five: Vec<&[u8]> = vec![&[0x68, 0xce]; 5];
        assert!(inspect(&annex_b(&five), BIG).is_err());
        let mut level_6 = SPS_1080.to_vec();
        level_6[3] = 60;
        assert!(matches!(
            inspect(&annex_b(&[&level_6]), BIG),
            Err(VideoDecodeError::Unsupported(_))
        ));
    }

    #[test]
    fn a_picture_sits_at_its_crop_origin_or_in_a_cropped_buffer() {
        let at = |left, top, w, h| Placement {
            pts: Timestamp::ZERO,
            size: Size::new(w, h),
            coded: Size::new(1920, 1088),
            left,
            top,
            color: ColorSpace::BT709_LIMITED,
        };
        assert_eq!(at(0, 0, 1920, 1080).origin_in(1920, 1080), Some((0, 0)));
        assert_eq!(at(0, 0, 1920, 1080).origin_in(1920, 1088), Some((0, 0)));
        assert_eq!(at(8, 4, 1904, 1080).origin_in(1920, 1088), Some((8, 4)));
        assert_eq!(at(8, 4, 1904, 1080).origin_in(1904, 1080), Some((0, 0)));
        assert_eq!(at(8, 4, 1920, 1080).origin_in(1920, 1088), None);
        assert_eq!(at(1, 0, 16, 16).origin_in(64, 64), None);
        assert_eq!(at(0, 0, 15, 16).origin_in(64, 64), None);
    }

    /// Bits of an SPS under construction (`u`: fixed width, `ue`: Exp-Golomb).
    struct Bits(Vec<bool>);

    impl Bits {
        fn u(&mut self, v: u32, n: u32) {
            for i in (0..n).rev() {
                self.0.push((v >> i) & 1 == 1);
            }
        }
        fn ue(&mut self, v: u32) {
            let x = u64::from(v) + 1;
            let len = 64 - x.leading_zeros();
            self.0.extend(std::iter::repeat_n(false, len as usize - 1));
            for i in (0..len).rev() {
                self.0.push((x >> i) & 1 == 1);
            }
        }
        fn nal(mut self) -> Vec<u8> {
            self.0.push(true);
            while !self.0.len().is_multiple_of(8) {
                self.0.push(false);
            }
            let mut out = vec![0x67];
            out.extend(
                self.0
                    .chunks(8)
                    .map(|c| c.iter().fold(0, |a, &b| (a << 1) | u8::from(b))),
            );
            out
        }
    }

    /// A 64x48 High profile SPS whose VUI states `transfer_characteristics`.
    fn sps_with_transfer(transfer: u32) -> Vec<u8> {
        let mut w = Bits(Vec::new());
        w.u(100, 8);
        w.u(0, 8);
        w.u(30, 8);
        w.ue(0); // sps id
        w.ue(1); // 4:2:0
        w.ue(0);
        w.ue(0); // 8-bit
        w.u(0, 2); // no bypass, no scaling matrix
        w.ue(0); // log2_max_frame_num_minus4
        w.ue(2); // poc type 2
        w.ue(1); // refs
        w.u(0, 1);
        w.ue(3); // 4 MBs wide
        w.ue(2); // 3 MBs high
        w.u(1, 1); // frame_mbs_only
        w.u(1, 1); // direct_8x8
        w.u(0, 1); // no cropping
        w.u(1, 1); // vui
        w.u(0, 2); // no aspect, no overscan
        w.u(1, 1); // video signal
        w.u(5, 3);
        w.u(0, 1); // limited
        w.u(1, 1); // colour description
        w.u(9, 8);
        w.u(transfer, 8);
        w.u(9, 8);
        w.nal()
    }

    #[test]
    fn an_hdr_transfer_is_unsupported_not_mislabelled() {
        for transfer in [1, 6, 14] {
            assert!(check_sps(&sps_with_transfer(transfer), BIG).is_ok());
        }
        for transfer in HDR_TRANSFERS {
            assert!(matches!(
                check_sps(&sps_with_transfer(u32::from(transfer)), BIG),
                Err(VideoDecodeError::Unsupported(_))
            ));
        }
    }

    #[test]
    fn colour_defaults_to_bt709_limited() {
        let sps = h264::parse_sps(SPS_1080).unwrap();
        assert_eq!(color_of(&sps), ColorSpace::BT709_LIMITED);
    }
}
