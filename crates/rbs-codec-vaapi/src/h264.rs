//! H.264 sequence and picture parameter sets for the VA-API encoder (ITU-T H.264
//! 7.3.2.1 and 7.3.2.2), and the values the VA parameter buffers repeat. Pure, so
//! the bitstream is unit-tested on every build.
//!
//! The stream has P frames only with one reference, `pic_order_cnt_type` 2 (output
//! order = decode order), frame numbers modulo 256, and a VUI with the colour
//! description, timing and bitstream restrictions (no reordering), without HRD
//! parameters.

use rbs_codec_hw::plan::{Plan, Profile, Rc};
use rbs_media_format::h273::ColorCodes;

use crate::bits::{BitWriter, annex_b_nal};

/// `log2_max_frame_num_minus4`: frame numbers wrap at 256.
pub(crate) const LOG2_MAX_FRAME_NUM_MINUS4: u32 = 4;

/// Everything the headers and the VA buffers share.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Sequence {
    pub profile: Profile,
    pub level_idc: u8,
    pub width_mbs: u32,
    pub height_mbs: u32,
    /// Crop of the right and bottom edges, in luma samples / 2 (4:2:0 units).
    pub crop: (u32, u32),
    pub fps: (u32, u32),
    /// CABAC (Main and High).
    pub cabac: bool,
    /// 8x8 transform (High).
    pub transform_8x8: bool,
    /// `pic_init_qp`: the constant QP, else 26.
    pub init_qp: u8,
    pub color: ColorCodes,
    /// Bits per second of the peak rate, 0 for constant QP.
    pub peak_bps: u32,
}

/// (level_idc, MaxMBPS, MaxFS, MaxBR in 1000 bit/s for Baseline / Main) of
/// Table A-1, without level 1b.
const LEVELS: [(u8, u64, u64, u64); 19] = [
    (10, 1_485, 99, 64),
    (11, 3_000, 396, 192),
    (12, 6_000, 396, 384),
    (13, 11_880, 396, 768),
    (20, 11_880, 396, 2_000),
    (21, 19_800, 792, 4_000),
    (22, 20_250, 1_620, 4_000),
    (30, 40_500, 1_620, 10_000),
    (31, 108_000, 3_600, 14_000),
    (32, 216_000, 5_120, 20_000),
    (40, 245_760, 8_192, 20_000),
    (41, 245_760, 8_192, 50_000),
    (42, 522_240, 8_704, 50_000),
    (50, 589_824, 22_080, 135_000),
    (51, 983_040, 36_864, 240_000),
    (52, 2_073_600, 36_864, 240_000),
    (60, 4_177_920, 139_264, 240_000),
    (61, 8_355_840, 139_264, 480_000),
    (62, 16_711_680, 139_264, 800_000),
];

/// The lowest level whose macroblock rate, frame size, frame sides and peak bitrate
/// cover the stream (A.3.1, A.3.3); 6.2 when none does.
pub(crate) fn level(
    width_mbs: u32,
    height_mbs: u32,
    fps: (u32, u32),
    peak_bps: u32,
    high: bool,
) -> u8 {
    let frame = u64::from(width_mbs) * u64::from(height_mbs);
    let mb_rate = (frame * u64::from(fps.0)).div_ceil(u64::from(fps.1.max(1)));
    // High allows 1.25 times the bitrate (Table A-2, cpbBrVclFactor 1250 / 1000).
    let br_factor = if high { 1_250 } else { 1_000 };
    LEVELS
        .iter()
        .find(|&&(_, max_mbps, max_fs, max_br)| {
            let side = (max_fs * 8).isqrt();
            mb_rate <= max_mbps
                && frame <= max_fs
                && u64::from(width_mbs) <= side
                && u64::from(height_mbs) <= side
                && u64::from(peak_bps) <= max_br * br_factor
        })
        .map_or(62, |l| l.0)
}

impl Sequence {
    pub(crate) fn new(plan: &Plan) -> Self {
        let width_mbs = plan.width.div_ceil(16);
        let height_mbs = plan.height.div_ceil(16);
        let peak_bps = match plan.rc {
            Rc::Cbr { bitrate } => bitrate,
            Rc::Vbr { max, .. } => max,
            Rc::Quality { .. } => 0,
        };
        let high = plan.profile == Profile::H264High;
        Sequence {
            profile: plan.profile,
            level_idc: level(width_mbs, height_mbs, plan.fps, peak_bps, high),
            width_mbs,
            height_mbs,
            crop: (
                (width_mbs * 16 - plan.width) / 2,
                (height_mbs * 16 - plan.height) / 2,
            ),
            fps: plan.fps,
            cabac: plan.profile != Profile::H264Baseline,
            transform_8x8: high,
            init_qp: match plan.rc {
                Rc::Quality { level } => level.clamp(1, 51),
                _ => 26,
            },
            color: plan.color,
            peak_bps,
        }
    }

    /// `profile_idc` and the constraint flags byte.
    pub(crate) fn profile_idc(&self) -> (u8, u8) {
        match self.profile {
            // Constrained Baseline: constraint_set0 and constraint_set1.
            Profile::H264Baseline => (66, 0xc0),
            // constraint_set1: decodable by Main decoders (no B or interlace here).
            Profile::H264Main => (77, 0x40),
            _ => (100, 0),
        }
    }

    /// VUI `num_units_in_tick` and `time_scale` (two ticks per frame).
    pub(crate) fn timing(&self) -> (u32, u32) {
        let (num, den) = self.fps;
        // time_scale = 2 * num must fit 32 bits; scale both down if it does not.
        if num <= u32::MAX / 2 {
            (den, num * 2)
        } else {
            (den.div_ceil(2).max(1), num)
        }
    }

    /// The SPS RBSP.
    pub(crate) fn sps(&self) -> Vec<u8> {
        let mut w = BitWriter::new();
        let (profile_idc, constraints) = self.profile_idc();
        w.u(8, u32::from(profile_idc));
        w.u(8, u32::from(constraints));
        w.u(8, u32::from(self.level_idc));
        w.ue(0); // seq_parameter_set_id
        if profile_idc == 100 {
            w.ue(1); // chroma_format_idc 4:2:0
            w.ue(0); // bit_depth_luma_minus8
            w.ue(0); // bit_depth_chroma_minus8
            w.flag(false); // qpprime_y_zero_transform_bypass_flag
            w.flag(false); // seq_scaling_matrix_present_flag
        }
        w.ue(LOG2_MAX_FRAME_NUM_MINUS4);
        w.ue(2); // pic_order_cnt_type
        w.ue(1); // max_num_ref_frames
        w.flag(false); // gaps_in_frame_num_value_allowed_flag
        w.ue(self.width_mbs - 1);
        w.ue(self.height_mbs - 1);
        w.flag(true); // frame_mbs_only_flag
        w.flag(true); // direct_8x8_inference_flag
        let cropped = self.crop != (0, 0);
        w.flag(cropped);
        if cropped {
            w.ue(0);
            w.ue(self.crop.0);
            w.ue(0);
            w.ue(self.crop.1);
        }
        w.flag(true); // vui_parameters_present_flag
        self.vui(&mut w);
        w.trailing();
        w.into_bytes()
    }

    fn vui(&self, w: &mut BitWriter) {
        w.flag(true); // aspect_ratio_info_present_flag
        w.u(8, 1); // aspect_ratio_idc: 1:1
        w.flag(false); // overscan_info_present_flag
        w.flag(true); // video_signal_type_present_flag
        w.u(3, 5); // video_format: unspecified
        w.flag(self.color.full_range);
        w.flag(true); // colour_description_present_flag
        w.u(8, u32::from(self.color.primaries));
        w.u(8, u32::from(self.color.transfer));
        w.u(8, u32::from(self.color.matrix));
        w.flag(false); // chroma_loc_info_present_flag
        let (num_units_in_tick, time_scale) = self.timing();
        w.flag(true); // timing_info_present_flag
        w.u(32, num_units_in_tick);
        w.u(32, time_scale);
        w.flag(true); // fixed_frame_rate_flag
        w.flag(false); // nal_hrd_parameters_present_flag
        w.flag(false); // vcl_hrd_parameters_present_flag
        w.flag(false); // pic_struct_present_flag
        w.flag(true); // bitstream_restriction_flag
        w.flag(true); // motion_vectors_over_pic_boundaries_flag
        w.ue(0); // max_bytes_per_pic_denom: no limit
        w.ue(0); // max_bits_per_mb_denom: no limit
        w.ue(15); // log2_max_mv_length_horizontal
        w.ue(15); // log2_max_mv_length_vertical
        w.ue(0); // max_num_reorder_frames
        w.ue(1); // max_dec_frame_buffering
    }

    /// The PPS RBSP.
    pub(crate) fn pps(&self) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.ue(0); // pic_parameter_set_id
        w.ue(0); // seq_parameter_set_id
        w.flag(self.cabac); // entropy_coding_mode_flag
        w.flag(false); // bottom_field_pic_order_in_frame_present_flag
        w.ue(0); // num_slice_groups_minus1
        w.ue(0); // num_ref_idx_l0_default_active_minus1
        w.ue(0); // num_ref_idx_l1_default_active_minus1
        w.flag(false); // weighted_pred_flag
        w.u(2, 0); // weighted_bipred_idc
        w.se(i32::from(self.init_qp) - 26); // pic_init_qp_minus26
        w.se(0); // pic_init_qs_minus26
        w.se(0); // chroma_qp_index_offset
        w.flag(true); // deblocking_filter_control_present_flag
        w.flag(false); // constrained_intra_pred_flag
        w.flag(false); // redundant_pic_cnt_present_flag
        if self.transform_8x8 {
            w.flag(true); // transform_8x8_mode_flag
            w.flag(false); // pic_scaling_matrix_present_flag
            w.se(0); // second_chroma_qp_index_offset
        }
        w.trailing();
        w.into_bytes()
    }

    /// The slice header of a picture's single slice, as a NAL unit prefix: a
    /// four-byte start code, the NAL header and the header fields, without
    /// emulation prevention (the driver adds it). Returns the bytes (the last one
    /// padded with zeros) and the length in bits.
    pub(crate) fn slice_header(
        &self,
        idr: bool,
        frame_num: u32,
        idr_pic_id: u16,
    ) -> (Vec<u8>, u32) {
        let mut w = BitWriter::new();
        w.u(32, 1); // start code
        // nal_ref_idc 3; IDR (5) or non-IDR (1) slice.
        w.u(8, if idr { 0x65 } else { 0x61 });
        w.ue(0); // first_mb_in_slice
        w.ue(if idr { 7 } else { 5 }); // slice_type: all I / all P
        w.ue(0); // pic_parameter_set_id
        w.u(LOG2_MAX_FRAME_NUM_MINUS4 as u8 + 4, frame_num);
        if idr {
            w.ue(u32::from(idr_pic_id));
        }
        // pic_order_cnt_type 2: no POC fields.
        if !idr {
            w.flag(false); // num_ref_idx_active_override_flag
            w.flag(false); // ref_pic_list_modification_flag_l0
        }
        // dec_ref_pic_marking (nal_ref_idc != 0)
        if idr {
            w.flag(false); // no_output_of_prior_pics_flag
            w.flag(false); // long_term_reference_flag
        } else {
            w.flag(false); // adaptive_ref_pic_marking_mode_flag
        }
        if self.cabac && !idr {
            w.ue(0); // cabac_init_idc
        }
        w.se(0); // slice_qp_delta
        w.ue(0); // disable_deblocking_filter_idc
        w.se(0); // slice_alpha_c0_offset_div2
        w.se(0); // slice_beta_offset_div2
        let bits = w.bits();
        (w.into_bytes(), bits)
    }

    /// SPS and PPS as Annex B, with emulation prevention: the extradata and the
    /// packed sequence header of every IDR picture.
    pub(crate) fn headers(&self) -> Vec<u8> {
        let mut out = Vec::new();
        // nal_ref_idc 3.
        annex_b_nal(&mut out, &[0x67], &self.sps());
        annex_b_nal(&mut out, &[0x68], &self.pps());
        out
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
mod tests {
    use super::*;
    use rbs_codec_hw::plan::Preset;
    use rbs_core::media::{CodecId, ColorSpace};

    fn plan(width: u32, height: u32, profile: Profile, rc: Rc) -> Plan {
        Plan {
            codec: CodecId::H264,
            width,
            height,
            ten_bit: false,
            fps: (60, 1),
            preset: Preset::Balanced,
            low_latency: false,
            profile,
            rc,
            cbr_padding: false,
            gop: 120,
            color: ColorCodes::of(&ColorSpace::BT709_LIMITED).unwrap(),
        }
    }

    #[test]
    fn levels() {
        // 1080p60 at 6 Mbit/s: 8160 MBs * 60 = 489600 MB/s -> 4.2.
        assert_eq!(level(120, 68, (60, 1), 6_000_000, true), 42);
        // 1080p30: 4.0.
        assert_eq!(level(120, 68, (30, 1), 6_000_000, true), 40);
        // 720p30 at 3 Mbit/s: 3600 MBs * 30 = 108000 -> 3.1.
        assert_eq!(level(80, 45, (30, 1), 3_000_000, false), 31);
        // 2160p60: 5.2.
        assert_eq!(level(240, 135, (60, 1), 20_000_000, true), 52);
        // The bitrate raises the level: 1080p30 at 30 Mbit/s Main -> 4.1.
        assert_eq!(level(120, 68, (30, 1), 30_000_000, false), 41);
        // Beyond the table.
        assert_eq!(level(1000, 1000, (240, 1), 0, true), 62);
    }

    #[test]
    fn crop_and_profile() {
        let s = Sequence::new(&plan(
            1920,
            1080,
            Profile::H264High,
            Rc::Cbr { bitrate: 6_000_000 },
        ));
        assert_eq!((s.width_mbs, s.height_mbs, s.crop), (120, 68, (0, 4)));
        assert_eq!(s.profile_idc(), (100, 0));
        assert!(s.cabac && s.transform_8x8);
        assert_eq!(s.timing(), (1, 120));
        let s = Sequence::new(&plan(
            640,
            360,
            Profile::H264Baseline,
            Rc::Quality { level: 30 },
        ));
        assert_eq!(s.profile_idc(), (66, 0xc0));
        assert!(!s.cabac && !s.transform_8x8);
        assert_eq!(s.init_qp, 30);
        assert_eq!(s.crop, (0, 4));
    }

    fn golden(width: u32, height: u32, profile: Profile, rc: Rc) -> Vec<u8> {
        let mut p = plan(width, height, profile, rc);
        p.fps = (60000, 1001);
        p.color = ColorCodes::of(&ColorSpace::BT709_FULL).unwrap();
        Sequence::new(&p).headers()
    }

    // Golden bytes checked with FFmpeg 6.1 (`-bsf:v trace_headers`): every field
    // parses back to the value written above (also for `slice_headers`).
    #[test]
    fn headers_1080p_high_cbr() {
        let h = golden(
            1920,
            1080,
            Profile::H264High,
            Rc::Cbr { bitrate: 6_000_000 },
        );
        assert_eq!(
            h,
            [
                0x00, 0x00, 0x00, 0x01, 0x67, 0x64, 0x00, 0x2a, 0xac, 0x2b, 0x40, 0x3c, 0x01, 0x13,
                0xf2, 0xe0, 0x2d, 0xc0, 0x40, 0x40, 0x50, 0x00, 0x00, 0x3e, 0x90, 0x00, 0x1d, 0x4c,
                0x08, 0xf0, 0x80, 0x42, 0xa0, 0x00, 0x00, 0x00, 0x01, 0x68, 0xee, 0x3c, 0xb0,
            ]
        );
    }

    #[test]
    fn headers_cropped_baseline_constant_qp() {
        let h = golden(642, 362, Profile::H264Baseline, Rc::Quality { level: 30 });
        assert_eq!(
            h,
            [
                0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0xc0, 0x1f, 0x95, 0xa0, 0x29, 0x0b, 0xf8, 0x89,
                0x30, 0x16, 0xe0, 0x20, 0x20, 0x28, 0x00, 0x00, 0x1f, 0x48, 0x00, 0x0e, 0xa6, 0x04,
                0x78, 0x40, 0x21, 0x50, 0x00, 0x00, 0x00, 0x01, 0x68, 0xce, 0x04, 0x72,
            ]
        );
    }

    #[test]
    fn slice_headers() {
        let p = |profile| {
            let mut p = plan(1920, 1080, profile, Rc::Cbr { bitrate: 6_000_000 });
            p.fps = (60000, 1001);
            Sequence::new(&p)
        };
        let high = p(Profile::H264High);
        assert_eq!(
            high.slice_header(true, 0, 3),
            (vec![0, 0, 0, 1, 0x65, 0x88, 0x80, 0x10, 0xf0], 68)
        );
        assert_eq!(
            high.slice_header(false, 255, 3),
            (vec![0, 0, 0, 1, 0x61, 0x9b, 0xfe, 0x3e], 63)
        );
        // CAVLC: no cabac_init_idc.
        let base = p(Profile::H264Baseline);
        assert_eq!(
            base.slice_header(false, 1, 3),
            (vec![0, 0, 0, 1, 0x61, 0x9a, 0x02, 0x3c], 62)
        );
    }
}
