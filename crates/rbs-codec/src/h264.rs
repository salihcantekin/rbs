//! Safe-Rust Annex B splitting and SPS parsing, shared by the live decoder adapters
//! (ADR-0139 decision 11), so untrusted parameter sets are parsed in one place.
//!
//! Only what a decoder needs to refuse a stream early: NAL boundaries, profile, level,
//! chroma format, bit depth and the cropped picture size. It never reads outside its
//! input and never panics; every failure is a [`SpsError`].

use rbs_core::media::Size;

/// Iterates the NAL units of an Annex B buffer (without start codes), lazily and
/// without allocating. A buffer with no start code yields nothing.
pub fn nal_units(data: &[u8]) -> NalUnits<'_> {
    NalUnits {
        data,
        next: find_start(data, 0),
    }
}

/// Iterator returned by [`nal_units`].
#[derive(Debug, Clone)]
pub struct NalUnits<'a> {
    data: &'a [u8],
    /// Payload start of the next NAL, if a start code was found.
    next: Option<usize>,
}

fn find_start(data: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 3 <= data.len() {
        if data[i + 2] > 1 {
            i += 3;
        } else if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            return Some(i + 3);
        } else {
            i += 1;
        }
    }
    None
}

impl<'a> Iterator for NalUnits<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        loop {
            let begin = self.next?;
            let following = find_start(self.data, begin);
            let mut end = following.map_or(self.data.len(), |p| p - 3);
            self.next = following;
            // Trailing zero bytes belong to the next start code (4-byte form) or padding.
            while end > begin && self.data[end - 1] == 0 {
                end -= 1;
            }
            if end > begin {
                return Some(&self.data[begin..end]);
            }
        }
    }
}

/// NAL unit type (the low five bits of the header byte).
pub fn nal_type(nal: &[u8]) -> Option<u8> {
    nal.first().map(|b| b & 0x1f)
}

/// NAL type of a sequence parameter set.
pub const NAL_SPS: u8 = 7;
/// NAL type of a picture parameter set.
pub const NAL_PPS: u8 = 8;
/// NAL type of an IDR slice.
pub const NAL_IDR: u8 = 5;

/// Why an SPS was refused.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpsError {
    /// Truncated or malformed.
    Malformed,
    /// A value outside what the parser accepts (sizes, profiles it cannot read).
    Unsupported(&'static str),
}

/// The fields of an SPS a decoder checks.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sps {
    /// `profile_idc` (66 Baseline, 77 Main, 100 High, ...).
    pub profile_idc: u8,
    /// Constraint flags byte.
    pub constraint_flags: u8,
    /// `level_idc` (40 = level 4.0, 52 = 5.2).
    pub level_idc: u8,
    /// 0 monochrome, 1 for 4:2:0, 2 for 4:2:2, 3 for 4:4:4.
    pub chroma_format_idc: u8,
    /// Luma bit depth.
    pub bit_depth: u8,
    /// Cropped (display) size.
    pub size: Size,
    /// Coded size (macroblock multiples).
    pub coded_size: Size,
    /// Left edge of the cropped picture inside the coded one, in pixels.
    pub crop_left: u32,
    /// Top edge of the cropped picture inside the coded one, in pixels.
    pub crop_top: u32,
    /// VUI colour description, when the stream states one.
    pub color: Option<SpsColor>,
}

/// Colour fields of the VUI (H.264 Table E-3 code points, as in the bitstream).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpsColor {
    /// `colour_primaries`.
    pub primaries: u8,
    /// `transfer_characteristics`.
    pub transfer: u8,
    /// `matrix_coefficients`.
    pub matrix: u8,
    /// `video_full_range_flag`.
    pub full_range: bool,
}

impl Sps {
    /// True for Constrained Baseline, Main or High, 4:2:0, 8-bit: the allow list of
    /// ADR-0139 decision 11, level at most 5.2; the size check is the caller's.
    pub fn is_allowed_profile(&self) -> bool {
        // Baseline only as Constrained Baseline (constraint_set1_flag).
        let profile = match self.profile_idc {
            66 => self.constraint_flags & 0x40 != 0,
            77 | 100 => true,
            _ => false,
        };
        profile && self.chroma_format_idc == 1 && self.bit_depth == 8 && self.level_idc <= 52
    }
}

/// Parses an SPS NAL unit (with its header byte).
pub fn parse_sps(nal: &[u8]) -> Result<Sps, SpsError> {
    if nal_type(nal) != Some(NAL_SPS) || nal.len() < 5 {
        return Err(SpsError::Malformed);
    }
    let rbsp = unescape(&nal[1..]);
    let mut r = Bits::new(&rbsp);
    let profile_idc = r.u(8)? as u8;
    let constraint_flags = r.u(8)? as u8;
    let level_idc = r.u(8)? as u8;
    if r.ue()? > 31 {
        return Err(SpsError::Malformed);
    }
    let mut chroma_format_idc = 1;
    let mut bit_depth = 8;
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        chroma_format_idc = r.ue()?;
        if chroma_format_idc > 3 {
            return Err(SpsError::Malformed);
        }
        if chroma_format_idc == 3 {
            r.u(1)?;
        }
        let luma_minus8 = r.ue()?;
        let chroma_minus8 = r.ue()?;
        if luma_minus8 > 6 || chroma_minus8 > 6 {
            return Err(SpsError::Malformed);
        }
        bit_depth = 8 + luma_minus8;
        r.u(1)?; // qpprime_y_zero_transform_bypass_flag
        if r.u(1)? == 1 {
            let lists = if chroma_format_idc != 3 { 8 } else { 12 };
            for i in 0..lists {
                if r.u(1)? == 1 {
                    skip_scaling_list(&mut r, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    if r.ue()? > 12 {
        return Err(SpsError::Malformed); // log2_max_frame_num_minus4
    }
    match r.ue()? {
        0 => {
            if r.ue()? > 12 {
                return Err(SpsError::Malformed); // log2_max_pic_order_cnt_lsb_minus4
            }
        }
        1 => {
            r.u(1)?;
            r.se()?;
            r.se()?;
            let n = r.ue()?;
            if n > 255 {
                return Err(SpsError::Malformed);
            }
            for _ in 0..n {
                r.se()?;
            }
        }
        2 => {}
        _ => return Err(SpsError::Malformed),
    }
    if r.ue()? > 16 {
        return Err(SpsError::Malformed); // max_num_ref_frames
    }
    r.u(1)?; // gaps_in_frame_num_value_allowed_flag
    let width_mbs = r.ue()?.checked_add(1).ok_or(SpsError::Malformed)?;
    let height_map_units = r.ue()?.checked_add(1).ok_or(SpsError::Malformed)?;
    let frame_mbs_only = r.u(1)?;
    if frame_mbs_only == 0 {
        return Err(SpsError::Unsupported("interlaced"));
    }
    r.u(1)?; // direct_8x8_inference_flag
    if width_mbs > 1024 || height_map_units > 1024 {
        return Err(SpsError::Unsupported("picture size"));
    }
    let height_mbs = height_map_units * (2 - frame_mbs_only);
    let coded = Size::new(width_mbs * 16, height_mbs * 16);
    let (mut left, mut right, mut top, mut bottom) = (0, 0, 0, 0);
    if r.u(1)? == 1 {
        left = r.ue()?;
        right = r.ue()?;
        top = r.ue()?;
        bottom = r.ue()?;
    }
    let color = if r.u(1)? == 1 {
        parse_vui_color(&mut r)
    } else {
        None
    };
    let (unit_x, unit_y) = match chroma_format_idc {
        0 | 3 => (1, 2 - frame_mbs_only),
        2 => (2, 2 - frame_mbs_only),
        _ => (2, 2 * (2 - frame_mbs_only)),
    };
    let crop_w = left.saturating_add(right).saturating_mul(unit_x);
    let crop_h = top.saturating_add(bottom).saturating_mul(unit_y);
    if crop_w >= coded.width || crop_h >= coded.height {
        return Err(SpsError::Malformed);
    }
    Ok(Sps {
        profile_idc,
        constraint_flags,
        level_idc,
        chroma_format_idc: chroma_format_idc as u8,
        bit_depth: bit_depth.min(255) as u8,
        size: Size::new(coded.width - crop_w, coded.height - crop_h),
        coded_size: coded,
        crop_left: left.saturating_mul(unit_x),
        crop_top: top.saturating_mul(unit_y),
        color,
    })
}

/// Reads the VUI up to the colour description. Any failure or absence is `None`: the
/// colour is a hint, never a reason to refuse a stream.
fn parse_vui_color(r: &mut Bits<'_>) -> Option<SpsColor> {
    if r.u(1).ok()? == 1 && r.u(8).ok()? == 255 {
        r.u(32).ok()?; // sar_width, sar_height
    }
    if r.u(1).ok()? == 1 {
        r.u(1).ok()?; // overscan_appropriate_flag
    }
    if r.u(1).ok()? == 0 {
        return None; // video_signal_type_present_flag
    }
    r.u(3).ok()?; // video_format
    let full_range = r.u(1).ok()? == 1;
    if r.u(1).ok()? == 0 {
        return None; // colour_description_present_flag
    }
    Some(SpsColor {
        primaries: r.u(8).ok()? as u8,
        transfer: r.u(8).ok()? as u8,
        matrix: r.u(8).ok()? as u8,
        full_range,
    })
}

fn skip_scaling_list(r: &mut Bits<'_>, size: usize) -> Result<(), SpsError> {
    let (mut last, mut next) = (8i64, 8i64);
    for _ in 0..size {
        if next != 0 {
            next = (last + i64::from(r.se()?) + 256) % 256;
        }
        last = if next == 0 { last } else { next };
    }
    Ok(())
}

/// Removes emulation prevention bytes (`00 00 03` -> `00 00`).
fn unescape(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0;
    for &b in data {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn bit(&mut self) -> Result<u32, SpsError> {
        let byte = self.data.get(self.pos / 8).ok_or(SpsError::Malformed)?;
        let bit = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Ok(u32::from(bit))
    }

    fn u(&mut self, n: u32) -> Result<u32, SpsError> {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Ok(v)
    }

    fn ue(&mut self) -> Result<u32, SpsError> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return Err(SpsError::Malformed);
            }
        }
        if zeros == 0 {
            return Ok(0);
        }
        let rest = self.u(zeros)?;
        Ok(((1u64 << zeros) - 1 + u64::from(rest)).min(u64::from(u32::MAX)) as u32)
    }

    fn se(&mut self) -> Result<i32, SpsError> {
        let k = i64::from(self.ue()?);
        let v = if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) };
        Ok(v.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;

    /// A common 1920x1080 High profile level 4.0 SPS (coded 1088, cropped to 1080).
    const SPS_1080: &[u8] = &[
        0x67, 0x64, 0x00, 0x28, 0xAC, 0xD9, 0x40, 0x78, 0x02, 0x27, 0xE5, 0xC0, 0x44, 0x00, 0x00,
        0x03, 0x00, 0x04, 0x00, 0x00, 0x03, 0x00, 0xF0, 0x3C, 0x60, 0xC6, 0x58,
    ];

    #[test]
    fn parses_a_1080p_high_sps() {
        let sps = parse_sps(SPS_1080).expect("sps");
        assert_eq!(sps.profile_idc, 100);
        assert_eq!(sps.level_idc, 40);
        assert_eq!(sps.size, Size::new(1920, 1080));
        assert_eq!(sps.coded_size, Size::new(1920, 1088));
        assert!(sps.is_allowed_profile());
    }

    #[test]
    fn splits_three_and_four_byte_start_codes() {
        let data = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 1, 0x65, 9];
        let nals: Vec<&[u8]> = nal_units(&data).collect();
        assert_eq!(
            nals,
            vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 9][..]]
        );
        assert_eq!(nal_units(&[1, 2, 3]).count(), 0);
    }

    #[test]
    fn every_truncation_and_garbage_is_an_error_not_a_panic() {
        for n in 0..SPS_1080.len() {
            let _ = parse_sps(&SPS_1080[..n]);
        }
        let mut junk = SPS_1080.to_vec();
        for i in 1..junk.len() {
            let keep = junk[i];
            junk[i] = 0xff;
            let _ = parse_sps(&junk);
            junk[i] = 0;
            let _ = parse_sps(&junk);
            junk[i] = keep;
        }
        assert!(parse_sps(&[0x68, 0, 0, 0, 0]).is_err());
    }

    struct W(Vec<bool>);
    impl W {
        fn u(&mut self, v: u32, n: u32) {
            for i in (0..n).rev() {
                self.0.push((v >> i) & 1 == 1);
            }
        }
        fn ue(&mut self, v: u32) {
            let x = u64::from(v) + 1;
            let len = 64 - x.leading_zeros();
            for _ in 0..len - 1 {
                self.0.push(false);
            }
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
            for c in self.0.chunks(8) {
                out.push(c.iter().fold(0, |a, &b| (a << 1) | u8::from(b)));
            }
            out
        }
    }

    /// 64x48 High profile SPS with a left crop of 2 pixels and a BT.709 limited VUI.
    fn sps_with_vui() -> Vec<u8> {
        let mut w = W(Vec::new());
        w.u(100, 8);
        w.u(0, 8);
        w.u(30, 8);
        w.ue(0); // sps id
        w.ue(1); // chroma 4:2:0
        w.ue(0);
        w.ue(0); // bit depths
        w.u(0, 1);
        w.u(0, 1); // no scaling matrix
        w.ue(0); // log2_max_frame_num_minus4
        w.ue(2); // poc type 2
        w.ue(1); // refs
        w.u(0, 1);
        w.ue(3); // 4 MBs wide
        w.ue(2); // 3 MBs high
        w.u(1, 1); // frame_mbs_only
        w.u(1, 1); // direct_8x8
        w.u(1, 1); // cropping
        w.ue(1);
        w.ue(0);
        w.ue(0);
        w.ue(0);
        w.u(1, 1); // vui
        w.u(0, 1); // aspect
        w.u(0, 1); // overscan
        w.u(1, 1); // video signal
        w.u(5, 3);
        w.u(0, 1); // limited
        w.u(1, 1);
        w.u(1, 8);
        w.u(1, 8);
        w.u(1, 8);
        w.nal()
    }

    #[test]
    fn reads_crop_offsets_and_vui_colour() {
        let sps = parse_sps(&sps_with_vui()).expect("sps");
        assert_eq!(sps.coded_size, Size::new(64, 48));
        assert_eq!(sps.size, Size::new(62, 48));
        assert_eq!(sps.crop_left, 2);
        assert_eq!(sps.crop_top, 0);
        let c = sps.color.expect("colour");
        assert_eq!(
            (c.primaries, c.transfer, c.matrix, c.full_range),
            (1, 1, 1, false)
        );
    }

    #[test]
    fn an_absurd_bit_depth_is_malformed_not_an_overflow() {
        let mut w = W(Vec::new());
        w.u(100, 8);
        w.u(0, 8);
        w.u(30, 8);
        w.ue(0);
        w.ue(1);
        w.ue(u32::MAX - 1); // bit_depth_luma_minus8
        assert_eq!(parse_sps(&w.nal()), Err(SpsError::Malformed));
    }

    #[test]
    fn a_repeated_start_code_costs_no_allocation_and_ends() {
        let data = [0u8, 0, 1].repeat(10_000);
        assert_eq!(nal_units(&data).count(), 0);
    }
}
