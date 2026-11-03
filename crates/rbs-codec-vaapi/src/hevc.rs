//! HEVC video, sequence and picture parameter sets and slice segment headers for
//! the VA-API encoder (ITU-T H.265 7.3.2.1, 7.3.2.2, 7.3.2.3 and 7.3.6.1), and the
//! values the VA parameter buffers repeat. Pure, so the bitstream is unit-tested on
//! every build.
//!
//! The stream has one temporal layer, P pictures (or, where the driver needs them,
//! B pictures whose two lists hold the same previous picture: "generalised P and
//! B") with one reference, the picture order count equal to the pictures since the
//! last IDR, the reference picture set in each slice header, one slice per picture,
//! and a VUI with the colour description, timing and bitstream restrictions,
//! without HRD parameters. Block sizes and coding tools come from the driver
//! (`VAConfigAttribEncHEVCBlockSizes`, `VAConfigAttribEncHEVCFeatures`), with
//! FFmpeg's defaults where it reports none.

use rbs_codec_hw::plan::{Plan, Profile, Rc};
use rbs_media_format::h273::ColorCodes;

use crate::bits::{BitWriter, annex_b_nal};

/// `log2_max_pic_order_cnt_lsb_minus4`: the POC LSBs wrap at 4096.
pub(crate) const LOG2_MAX_POC_LSB_MINUS4: u32 = 8;

/// NAL unit types (Table 7-1).
const TRAIL_R: u8 = 1;
const IDR_W_RADL: u8 = 19;
const VPS_NUT: u8 = 32;
const SPS_NUT: u8 = 33;
const PPS_NUT: u8 = 34;

/// Picture alignment when the driver does not report its block sizes (FFmpeg's).
const DEFAULT_ALIGNMENT: u32 = 16;

/// The two-byte NAL unit header: layer 0, temporal id 0.
const fn nal_header(nal_unit_type: u8) -> [u8; 2] {
    [nal_unit_type << 1, 1]
}

/// Coding block and transform sizes, as log2 of the side in luma samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Blocks {
    /// Coding tree block (CTU).
    pub log2_ctb: u8,
    /// Smallest coding block.
    pub log2_min_cb: u8,
    /// Smallest and largest transform blocks.
    pub log2_min_tb: u8,
    pub log2_max_tb: u8,
    /// `max_transform_hierarchy_depth_inter` / `_intra`.
    pub depth_inter: u8,
    pub depth_intra: u8,
    /// The picture is padded to a multiple of this (the driver's smallest coding
    /// block, else 16).
    pub alignment: u32,
}

impl Blocks {
    /// FFmpeg's choice without the attribute: CTUs of 32, coding blocks from 8,
    /// transforms from 4 to 32 with the full hierarchy, pictures padded to 16.
    pub(crate) const DEFAULT: Blocks = Blocks {
        log2_ctb: 5,
        log2_min_cb: 3,
        log2_min_tb: 2,
        log2_max_tb: 5,
        depth_inter: 3,
        depth_intra: 3,
        alignment: DEFAULT_ALIGNMENT,
    };

    /// From the fields of `VAConfigAttribValEncHEVCBlockSizes`, as the driver's
    /// largest CTU, its smallest coding block, its transform range and its deepest
    /// transform hierarchies (each limited to what the sizes allow). `None` when
    /// the sizes break the limits of Main and Main 10 (7.4.3.2.1).
    pub(crate) fn from_attribute(
        log2_max_ctb_minus3: u8,
        log2_min_cb_minus3: u8,
        log2_min_tb_minus2: u8,
        log2_max_tb_minus2: u8,
        depth_inter: u8,
        depth_intra: u8,
    ) -> Option<Blocks> {
        let log2_ctb = log2_max_ctb_minus3 + 3;
        let log2_min_cb = log2_min_cb_minus3 + 3;
        let log2_min_tb = log2_min_tb_minus2 + 2;
        let log2_max_tb = log2_max_tb_minus2 + 2;
        let valid = (4..=6).contains(&log2_ctb)
            && (3..=log2_ctb).contains(&log2_min_cb)
            && log2_min_tb >= 2
            && log2_min_tb < log2_min_cb
            && (log2_min_tb..=log2_ctb.min(5)).contains(&log2_max_tb);
        if !valid {
            return None;
        }
        let max_depth = log2_ctb - log2_min_tb;
        Some(Blocks {
            log2_ctb,
            log2_min_cb,
            log2_min_tb,
            log2_max_tb,
            depth_inter: depth_inter.min(max_depth),
            depth_intra: depth_intra.min(max_depth),
            alignment: 1 << log2_min_cb,
        })
    }
}

/// How the driver supports one coding tool (`VA_FEATURE_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Support {
    No,
    Yes,
    Required,
}

/// The tools `VAConfigAttribValEncHEVCFeatures` describes, in its field order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Features {
    pub separate_colour_planes: Support,
    pub scaling_lists: Support,
    pub amp: Support,
    pub sao: Support,
    pub pcm: Support,
    pub temporal_mvp: Support,
    pub strong_intra_smoothing: Support,
    pub dependent_slices: Support,
    pub sign_data_hiding: Support,
    pub constrained_intra_pred: Support,
    pub transform_skip: Support,
    pub cu_qp_delta: Support,
    pub weighted_prediction: Support,
    pub transquant_bypass: Support,
    pub deblocking_filter_disable: Support,
}

/// The coding tools the stream uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Tools {
    pub amp: bool,
    pub sao: bool,
    pub temporal_mvp: bool,
    pub strong_intra_smoothing: bool,
    pub sign_data_hiding: bool,
    pub constrained_intra_pred: bool,
    pub transform_skip: bool,
    /// CU QP deltas (rate control), with `diff_cu_qp_delta_depth` at its largest.
    pub cu_qp_delta: bool,
    pub transquant_bypass: bool,
}

impl Tools {
    /// FFmpeg's choice without the attribute: asymmetric partitions, and CU QP
    /// deltas unless the QP is constant.
    pub(crate) fn default_for(constant_qp: bool) -> Tools {
        Tools {
            amp: true,
            sao: false,
            temporal_mvp: false,
            strong_intra_smoothing: false,
            sign_data_hiding: false,
            constrained_intra_pred: false,
            transform_skip: false,
            cu_qp_delta: !constant_qp,
            transquant_bypass: false,
        }
    }

    /// From the driver's features, as FFmpeg chooses: asymmetric partitions,
    /// SAO, temporal MV prediction and transform skip where supported, CU QP deltas
    /// where supported unless the QP is constant, any other tool only where
    /// required. `None` when the driver requires a tool the adapter does not
    /// write: separate colour planes, scaling lists, PCM, dependent slices,
    /// weighted prediction or disabled deblocking.
    pub(crate) fn from_features(f: &Features, constant_qp: bool) -> Option<Tools> {
        let unwritten = [
            f.separate_colour_planes,
            f.scaling_lists,
            f.pcm,
            f.dependent_slices,
            f.weighted_prediction,
            f.deblocking_filter_disable,
        ];
        if unwritten.contains(&Support::Required) {
            return None;
        }
        let used = |s: Support| s != Support::No;
        let required = |s: Support| s == Support::Required;
        Some(Tools {
            amp: used(f.amp),
            sao: used(f.sao),
            temporal_mvp: used(f.temporal_mvp),
            strong_intra_smoothing: required(f.strong_intra_smoothing),
            sign_data_hiding: required(f.sign_data_hiding),
            constrained_intra_pred: required(f.constrained_intra_pred),
            transform_skip: used(f.transform_skip),
            cu_qp_delta: if constant_qp {
                required(f.cu_qp_delta)
            } else {
                used(f.cu_qp_delta)
            },
            transquant_bypass: required(f.transquant_bypass),
        })
    }
}

/// Everything the headers and the VA buffers share.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Sequence {
    pub profile: Profile,
    /// `general_level_idc`: 30 times the level.
    pub level_idc: u8,
    /// Coded size in luma samples, a multiple of `blocks.alignment`.
    pub width: u32,
    pub height: u32,
    /// Conformance window on the right and bottom edges, in chroma samples (4:2:0:
    /// luma samples / 2).
    pub crop: (u32, u32),
    pub fps: (u32, u32),
    /// `init_qp`: the constant QP, else 26.
    pub init_qp: u8,
    pub color: ColorCodes,
    /// Bits per second of the peak rate, 0 for constant QP.
    pub peak_bps: u32,
    pub blocks: Blocks,
    pub tools: Tools,
    /// P pictures are coded as B slices with both lists holding the previous
    /// picture (the driver reports `VA_PREDICTION_DIRECTION_BI_NOT_EMPTY`).
    pub gpb: bool,
}

/// (general_level_idc, MaxLumaPs, MaxLumaSr, MaxBR of the Main tier in 1000
/// bit/s) of Tables A.8 and A.9.
const LEVELS: [(u8, u64, u64, u64); 13] = [
    (30, 36_864, 552_960, 128),
    (60, 122_880, 3_686_400, 1_500),
    (63, 245_760, 7_372_800, 3_000),
    (90, 552_960, 16_588_800, 6_000),
    (93, 983_040, 33_177_600, 10_000),
    (120, 2_228_224, 66_846_720, 12_000),
    (123, 2_228_224, 133_693_440, 20_000),
    (150, 8_912_896, 267_386_880, 25_000),
    (153, 8_912_896, 534_773_760, 40_000),
    (156, 8_912_896, 1_069_547_520, 60_000),
    (180, 35_651_584, 1_069_547_520, 60_000),
    (183, 35_651_584, 2_139_095_040, 120_000),
    (186, 35_651_584, 4_278_190_080, 240_000),
];

/// The lowest Main-tier level whose picture size, sides, luma sample rate and peak
/// bitrate (CpbVclFactor 1000 for Main and Main 10) cover the stream (A.4.1, A.4.2);
/// 6.2 when none does. Two pictures in the DPB always fit.
pub(crate) fn level(width: u32, height: u32, fps: (u32, u32), peak_bps: u32) -> u8 {
    let (w, h) = (u64::from(width), u64::from(height));
    let ps = w * h;
    let sr = (ps * u64::from(fps.0)).div_ceil(u64::from(fps.1.max(1)));
    LEVELS
        .iter()
        .find(|&&(_, max_ps, max_sr, max_br)| {
            ps <= max_ps
                && w * w <= 8 * max_ps
                && h * h <= 8 * max_ps
                && sr <= max_sr
                && u64::from(peak_bps) <= max_br * 1_000
        })
        .map_or(186, |l| l.0)
}

impl Sequence {
    pub(crate) fn new(plan: &Plan, blocks: Blocks, tools: Tools, gpb: bool) -> Self {
        let align = |v: u32| v.div_ceil(blocks.alignment) * blocks.alignment;
        let (width, height) = (align(plan.width), align(plan.height));
        let peak_bps = match plan.rc {
            Rc::Cbr { bitrate } => bitrate,
            Rc::Vbr { max, .. } => max,
            Rc::Quality { .. } => 0,
        };
        Sequence {
            profile: plan.profile,
            level_idc: level(width, height, plan.fps, peak_bps),
            width,
            height,
            crop: ((width - plan.width) / 2, (height - plan.height) / 2),
            fps: plan.fps,
            init_qp: match plan.rc {
                Rc::Quality { level } => level.clamp(1, 51),
                _ => 26,
            },
            color: plan.color,
            peak_bps,
            blocks,
            tools,
            gpb,
        }
    }

    /// `general_profile_idc`: 1 (Main) or 2 (Main 10).
    pub(crate) fn profile_idc(&self) -> u8 {
        if self.profile == Profile::HevcMain10 {
            2
        } else {
            1
        }
    }

    /// `bit_depth_luma_minus8` (and chroma).
    pub(crate) fn bit_depth_minus8(&self) -> u32 {
        if self.profile == Profile::HevcMain10 {
            2
        } else {
            0
        }
    }

    /// CTUs in the picture: the size of its one slice.
    pub(crate) fn ctus(&self) -> u32 {
        let ctb = 1u32 << self.blocks.log2_ctb;
        self.width.div_ceil(ctb) * self.height.div_ceil(ctb)
    }

    /// `diff_cu_qp_delta_depth`: CU QP deltas down to the smallest coding block.
    pub(crate) fn cu_qp_delta_depth(&self) -> u8 {
        if self.tools.cu_qp_delta {
            self.blocks.log2_ctb - self.blocks.log2_min_cb
        } else {
            0
        }
    }

    /// `num_units_in_tick` and `time_scale` (one tick per picture; the POC grows
    /// by one per picture).
    pub(crate) fn timing(&self) -> (u32, u32) {
        (self.fps.1, self.fps.0)
    }

    /// `profile_tier_level(1, 0)`: Main tier.
    fn profile_tier_level(&self, w: &mut BitWriter) {
        let idc = self.profile_idc();
        w.u(2, 0); // general_profile_space
        w.flag(false); // general_tier_flag
        w.u(5, u32::from(idc));
        // general_profile_compatibility_flag[j]: Main streams are also Main 10.
        let compatible = if idc == 1 {
            (1 << 30) | (1 << 29)
        } else {
            1 << 29
        };
        w.u(32, compatible);
        w.flag(true); // general_progressive_source_flag
        w.flag(false); // general_interlaced_source_flag
        w.flag(true); // general_non_packed_constraint_flag
        w.flag(true); // general_frame_only_constraint_flag
        // Main and Main 10 (compatibility flag 2): seven reserved bits,
        // general_one_picture_only_constraint_flag and 35 reserved bits.
        w.u(32, 0);
        w.u(11, 0);
        w.flag(false); // general_inbld_flag
        w.u(8, u32::from(self.level_idc));
    }

    /// The VPS RBSP.
    pub(crate) fn vps(&self) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.u(4, 0); // vps_video_parameter_set_id
        w.flag(true); // vps_base_layer_internal_flag
        w.flag(true); // vps_base_layer_available_flag
        w.u(6, 0); // vps_max_layers_minus1
        w.u(3, 0); // vps_max_sub_layers_minus1
        w.flag(true); // vps_temporal_id_nesting_flag
        w.u(16, 0xffff); // vps_reserved_0xffff_16bits
        self.profile_tier_level(&mut w);
        w.flag(false); // vps_sub_layer_ordering_info_present_flag
        w.ue(1); // vps_max_dec_pic_buffering_minus1: the picture and its reference
        w.ue(0); // vps_max_num_reorder_pics
        w.ue(0); // vps_max_latency_increase_plus1
        w.u(6, 0); // vps_max_layer_id
        w.ue(0); // vps_num_layer_sets_minus1
        let (num_units_in_tick, time_scale) = self.timing();
        w.flag(true); // vps_timing_info_present_flag
        w.u(32, num_units_in_tick);
        w.u(32, time_scale);
        w.flag(true); // vps_poc_proportional_to_timing_flag
        w.ue(0); // vps_num_ticks_poc_diff_one_minus1
        w.ue(0); // vps_num_hrd_parameters
        w.flag(false); // vps_extension_flag
        w.trailing();
        w.into_bytes()
    }

    /// The SPS RBSP.
    pub(crate) fn sps(&self) -> Vec<u8> {
        let mut w = BitWriter::new();
        let b = &self.blocks;
        let t = &self.tools;
        w.u(4, 0); // sps_video_parameter_set_id
        w.u(3, 0); // sps_max_sub_layers_minus1
        w.flag(true); // sps_temporal_id_nesting_flag
        self.profile_tier_level(&mut w);
        w.ue(0); // sps_seq_parameter_set_id
        w.ue(1); // chroma_format_idc 4:2:0
        w.ue(self.width); // pic_width_in_luma_samples
        w.ue(self.height); // pic_height_in_luma_samples
        let cropped = self.crop != (0, 0);
        w.flag(cropped); // conformance_window_flag
        if cropped {
            w.ue(0);
            w.ue(self.crop.0);
            w.ue(0);
            w.ue(self.crop.1);
        }
        w.ue(self.bit_depth_minus8()); // bit_depth_luma_minus8
        w.ue(self.bit_depth_minus8()); // bit_depth_chroma_minus8
        w.ue(LOG2_MAX_POC_LSB_MINUS4);
        w.flag(false); // sps_sub_layer_ordering_info_present_flag
        w.ue(1); // sps_max_dec_pic_buffering_minus1
        w.ue(0); // sps_max_num_reorder_pics
        w.ue(0); // sps_max_latency_increase_plus1
        w.ue(u32::from(b.log2_min_cb - 3));
        w.ue(u32::from(b.log2_ctb - b.log2_min_cb));
        w.ue(u32::from(b.log2_min_tb - 2));
        w.ue(u32::from(b.log2_max_tb - b.log2_min_tb));
        w.ue(u32::from(b.depth_inter));
        w.ue(u32::from(b.depth_intra));
        w.flag(false); // scaling_list_enabled_flag
        w.flag(t.amp); // amp_enabled_flag
        w.flag(t.sao); // sample_adaptive_offset_enabled_flag
        w.flag(false); // pcm_enabled_flag
        w.ue(0); // num_short_term_ref_pic_sets: each slice carries its own
        w.flag(false); // long_term_ref_pics_present_flag
        w.flag(t.temporal_mvp); // sps_temporal_mvp_enabled_flag
        w.flag(t.strong_intra_smoothing); // strong_intra_smoothing_enabled_flag
        w.flag(true); // vui_parameters_present_flag
        self.vui(&mut w);
        w.flag(false); // sps_extension_present_flag
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
        w.flag(false); // neutral_chroma_indication_flag
        w.flag(false); // field_seq_flag
        w.flag(false); // frame_field_info_present_flag
        w.flag(false); // default_display_window_flag
        let (num_units_in_tick, time_scale) = self.timing();
        w.flag(true); // vui_timing_info_present_flag
        w.u(32, num_units_in_tick);
        w.u(32, time_scale);
        w.flag(true); // vui_poc_proportional_to_timing_flag
        w.ue(0); // vui_num_ticks_poc_diff_one_minus1
        w.flag(false); // vui_hrd_parameters_present_flag
        w.flag(true); // bitstream_restriction_flag
        w.flag(false); // tiles_fixed_structure_flag
        w.flag(true); // motion_vectors_over_pic_boundaries_flag
        w.flag(true); // restricted_ref_pic_lists_flag
        w.ue(0); // min_spatial_segmentation_idc
        w.ue(0); // max_bytes_per_pic_denom: no limit
        w.ue(0); // max_bits_per_min_cu_denom: no limit
        w.ue(15); // log2_max_mv_length_horizontal
        w.ue(15); // log2_max_mv_length_vertical
    }

    /// The PPS RBSP.
    pub(crate) fn pps(&self) -> Vec<u8> {
        let mut w = BitWriter::new();
        let t = &self.tools;
        w.ue(0); // pps_pic_parameter_set_id
        w.ue(0); // pps_seq_parameter_set_id
        w.flag(false); // dependent_slice_segments_enabled_flag
        w.flag(false); // output_flag_present_flag
        w.u(3, 0); // num_extra_slice_header_bits
        w.flag(t.sign_data_hiding); // sign_data_hiding_enabled_flag
        w.flag(false); // cabac_init_present_flag
        w.ue(0); // num_ref_idx_l0_default_active_minus1
        w.ue(0); // num_ref_idx_l1_default_active_minus1
        w.se(i32::from(self.init_qp) - 26); // init_qp_minus26
        w.flag(t.constrained_intra_pred); // constrained_intra_pred_flag
        w.flag(t.transform_skip); // transform_skip_enabled_flag
        w.flag(t.cu_qp_delta); // cu_qp_delta_enabled_flag
        if t.cu_qp_delta {
            w.ue(u32::from(self.cu_qp_delta_depth()));
        }
        w.se(0); // pps_cb_qp_offset
        w.se(0); // pps_cr_qp_offset
        w.flag(false); // pps_slice_chroma_qp_offsets_present_flag
        w.flag(false); // weighted_pred_flag
        w.flag(false); // weighted_bipred_flag
        w.flag(t.transquant_bypass); // transquant_bypass_enabled_flag
        w.flag(false); // tiles_enabled_flag
        w.flag(false); // entropy_coding_sync_enabled_flag
        w.flag(true); // pps_loop_filter_across_slices_enabled_flag
        w.flag(false); // deblocking_filter_control_present_flag
        w.flag(false); // pps_scaling_list_data_present_flag
        w.flag(false); // lists_modification_present_flag
        w.ue(0); // log2_parallel_merge_level_minus2
        w.flag(false); // slice_segment_header_extension_present_flag
        w.flag(false); // pps_extension_present_flag
        w.trailing();
        w.into_bytes()
    }

    /// `slice_type` of a picture: 2 (I) for IDR, else 1 (P), or 0 (B) with `gpb`.
    pub(crate) fn slice_type(&self, idr: bool) -> u8 {
        match (idr, self.gpb) {
            (true, _) => 2,
            (false, false) => 1,
            (false, true) => 0,
        }
    }

    /// `nal_unit_type` of a picture: IDR_W_RADL or TRAIL_R.
    pub(crate) fn nal_unit_type(idr: bool) -> u8 {
        if idr { IDR_W_RADL } else { TRAIL_R }
    }

    /// The slice segment header of a picture's single slice as an Annex B NAL unit
    /// prefix: a four-byte start code, the NAL unit header and the header fields up
    /// to and including `byte_alignment()`, with emulation prevention. `poc` is the
    /// picture order count (pictures since the IDR); the reference is the previous
    /// picture. Returns the bytes; their length in bits is a multiple of 8.
    pub(crate) fn slice_header(&self, idr: bool, poc: u32) -> Vec<u8> {
        let t = &self.tools;
        let slice_type = self.slice_type(idr);
        let mut w = BitWriter::new();
        w.flag(true); // first_slice_segment_in_pic_flag
        if idr {
            w.flag(false); // no_output_of_prior_pics_flag (IRAP)
        }
        w.ue(0); // slice_pic_parameter_set_id
        w.ue(u32::from(slice_type));
        if !idr {
            let lsb_bits = LOG2_MAX_POC_LSB_MINUS4 + 4;
            w.u(lsb_bits as u8, poc & ((1 << lsb_bits) - 1)); // slice_pic_order_cnt_lsb
            w.flag(false); // short_term_ref_pic_set_sps_flag
            // st_ref_pic_set(0): the previous picture, used by this one.
            w.ue(1); // num_negative_pics
            w.ue(0); // num_positive_pics
            w.ue(0); // delta_poc_s0_minus1
            w.flag(true); // used_by_curr_pic_s0_flag
            if t.temporal_mvp {
                w.flag(true); // slice_temporal_mvp_enabled_flag
            }
        }
        if t.sao {
            w.flag(true); // slice_sao_luma_flag
            w.flag(true); // slice_sao_chroma_flag
        }
        if !idr {
            w.flag(false); // num_ref_idx_active_override_flag
            if slice_type == 0 {
                w.flag(false); // mvd_l1_zero_flag
            }
            if t.temporal_mvp && slice_type == 0 {
                w.flag(true); // collocated_from_l0_flag
            }
            // One reference: no collocated_ref_idx.
            w.ue(0); // five_minus_max_num_merge_cand
        }
        w.se(0); // slice_qp_delta
        // pps_loop_filter_across_slices_enabled_flag, deblocking on.
        w.flag(false); // slice_loop_filter_across_slices_enabled_flag
        w.trailing(); // byte_alignment()
        let mut out = Vec::new();
        annex_b_nal(
            &mut out,
            &nal_header(Self::nal_unit_type(idr)),
            &w.into_bytes(),
        );
        out
    }

    /// VPS, SPS and PPS as Annex B, with emulation prevention: the extradata and the
    /// packed sequence header of every IDR picture.
    pub(crate) fn headers(&self) -> Vec<u8> {
        let mut out = Vec::new();
        annex_b_nal(&mut out, &nal_header(VPS_NUT), &self.vps());
        annex_b_nal(&mut out, &nal_header(SPS_NUT), &self.sps());
        annex_b_nal(&mut out, &nal_header(PPS_NUT), &self.pps());
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
            codec: CodecId::Hevc,
            width,
            height,
            ten_bit: profile == Profile::HevcMain10,
            fps: (60000, 1001),
            preset: Preset::Balanced,
            low_latency: false,
            profile,
            rc,
            gop: 120,
            color: ColorCodes::of(&ColorSpace::BT709_FULL).unwrap(),
        }
    }

    const CBR: Rc = Rc::Cbr { bitrate: 6_000_000 };

    #[test]
    fn levels() {
        // 1080p60 at 6 Mbit/s: 124.4 M samples/s -> 4.1.
        assert_eq!(level(1920, 1080, (60, 1), 6_000_000), 123);
        // 1080p30: 4.
        assert_eq!(level(1920, 1088, (30, 1), 6_000_000), 120);
        // 720p30 at 3 Mbit/s: 3.1.
        assert_eq!(level(1280, 720, (30, 1), 3_000_000), 93);
        // 2160p60 at 20 Mbit/s: 5.1.
        assert_eq!(level(3840, 2160, (60, 1), 20_000_000), 153);
        // The bitrate raises the level: 1080p30 at 15 Mbit/s -> 4.1.
        assert_eq!(level(1920, 1080, (30, 1), 15_000_000), 123);
        // A side beyond sqrt(8 * MaxLumaPs) of level 4: 4224x128 -> 5.
        assert_eq!(level(4224, 128, (30, 1), 1_000_000), 150);
        // Beyond the table.
        assert_eq!(level(8192, 8192, (240, 1), 0), 186);
    }

    #[test]
    fn block_sizes_from_the_driver() {
        // Intel VDEnc: CTU 64, coding blocks from 8, transforms 4..32, depth 2.
        let b = Blocks::from_attribute(3, 0, 0, 3, 2, 2).unwrap();
        assert_eq!(
            b,
            Blocks {
                log2_ctb: 6,
                log2_min_cb: 3,
                log2_min_tb: 2,
                log2_max_tb: 5,
                depth_inter: 2,
                depth_intra: 2,
                alignment: 8,
            }
        );
        // The depth is limited by the sizes: CTU 16, TB 4 -> at most 2.
        let b = Blocks::from_attribute(1, 0, 0, 2, 3, 3).unwrap();
        assert_eq!((b.depth_inter, b.depth_intra), (2, 2));
        // CTU 128, a transform above 32, a minimum TB not below the minimum CB.
        assert!(Blocks::from_attribute(4, 0, 0, 3, 2, 2).is_none());
        assert!(Blocks::from_attribute(3, 0, 0, 4, 2, 2).is_none());
        assert!(Blocks::from_attribute(3, 0, 1, 3, 2, 2).is_none());
        // CTU 8 with a 32-sample minimum transform: refused, without overflow.
        assert!(Blocks::from_attribute(0, 0, 3, 3, 2, 2).is_none());
    }

    #[test]
    fn tools_from_features() {
        use Support::{No, Required, Yes};
        let mut f = Features {
            separate_colour_planes: No,
            scaling_lists: Yes,
            amp: Yes,
            sao: Yes,
            pcm: Yes,
            temporal_mvp: Yes,
            strong_intra_smoothing: Yes,
            dependent_slices: Yes,
            sign_data_hiding: Yes,
            constrained_intra_pred: Yes,
            transform_skip: Yes,
            cu_qp_delta: Yes,
            weighted_prediction: Yes,
            transquant_bypass: Yes,
            deblocking_filter_disable: Yes,
        };
        let t = Tools::from_features(&f, false).unwrap();
        assert!(t.amp && t.sao && t.temporal_mvp && t.transform_skip && t.cu_qp_delta);
        assert!(!t.strong_intra_smoothing && !t.sign_data_hiding && !t.transquant_bypass);
        assert!(!Tools::from_features(&f, true).unwrap().cu_qp_delta);
        f.sign_data_hiding = Required;
        f.cu_qp_delta = Required;
        f.amp = No;
        let t = Tools::from_features(&f, true).unwrap();
        assert!(t.sign_data_hiding && t.cu_qp_delta && !t.amp);
        f.pcm = Required;
        assert!(Tools::from_features(&f, false).is_none());
    }

    #[test]
    fn sizes_and_crop() {
        let s = Sequence::new(
            &plan(1920, 1080, Profile::HevcMain, CBR),
            Blocks::DEFAULT,
            Tools::default_for(false),
            false,
        );
        assert_eq!((s.width, s.height, s.crop), (1920, 1088, (0, 4)));
        // CTUs of 32: 60 x 34.
        assert_eq!(s.ctus(), 60 * 34);
        assert_eq!(s.cu_qp_delta_depth(), 2);
        assert_eq!(s.timing(), (1001, 60000));
        let vdenc = Blocks::from_attribute(3, 0, 0, 3, 2, 2).unwrap();
        let s = Sequence::new(
            &plan(1920, 1080, Profile::HevcMain10, CBR),
            vdenc,
            Tools::default_for(false),
            true,
        );
        assert_eq!((s.width, s.height, s.crop), (1920, 1080, (0, 0)));
        assert_eq!(s.ctus(), 30 * 17);
        assert_eq!((s.profile_idc(), s.bit_depth_minus8()), (2, 2));
        assert_eq!((s.slice_type(true), s.slice_type(false)), (2, 0));
    }

    fn golden(profile: Profile, rc: Rc, blocks: Blocks, tools: Tools, gpb: bool) -> Sequence {
        let (w, h) = if profile == Profile::HevcMain {
            (1920, 1080)
        } else {
            (642, 362)
        };
        Sequence::new(&plan(w, h, profile, rc), blocks, tools, gpb)
    }

    fn all_tools() -> Tools {
        Tools {
            amp: true,
            sao: true,
            temporal_mvp: true,
            strong_intra_smoothing: true,
            sign_data_hiding: true,
            constrained_intra_pred: true,
            transform_skip: true,
            cu_qp_delta: true,
            transquant_bypass: true,
        }
    }

    // Golden bytes checked with FFmpeg 6.1 (`-bsf:v trace_headers`): every field
    // parses back to the value written above (also for the slice headers).
    #[test]
    fn headers_1080p_main_cbr() {
        let s = golden(
            Profile::HevcMain,
            CBR,
            Blocks::DEFAULT,
            Tools::default_for(false),
            false,
        );
        assert_eq!(s.headers(), GOLDEN_MAIN);
        assert_eq!(s.slice_header(true, 0), GOLDEN_MAIN_IDR);
        assert_eq!(s.slice_header(false, 4097), GOLDEN_MAIN_P);
    }

    #[test]
    fn headers_cropped_main10_constant_qp_all_tools() {
        let vdenc = Blocks::from_attribute(3, 0, 0, 3, 2, 2).unwrap();
        let s = golden(
            Profile::HevcMain10,
            Rc::Quality { level: 30 },
            vdenc,
            all_tools(),
            true,
        );
        assert_eq!(s.headers(), GOLDEN_MAIN10);
        assert_eq!(s.slice_header(true, 0), GOLDEN_MAIN10_IDR);
        assert_eq!(s.slice_header(false, 1), GOLDEN_MAIN10_B);
    }

    const GOLDEN_MAIN: &[u8] = &[
        0x00, 0x00, 0x00, 0x01, 0x40, 0x01, 0x0c, 0x01, 0xff, 0xff, 0x01, 0x60, 0x00, 0x00, 0x03,
        0x00, 0xb0, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x7b, 0x2c, 0x0c, 0x00, 0x00, 0x0f,
        0xa4, 0x00, 0x03, 0xa9, 0x83, 0xa0, 0x00, 0x00, 0x00, 0x01, 0x42, 0x01, 0x01, 0x01, 0x60,
        0x00, 0x00, 0x03, 0x00, 0xb0, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x7b, 0xa0, 0x03,
        0xc0, 0x80, 0x11, 0x07, 0xcb, 0x89, 0x2e, 0xe4, 0x21, 0x12, 0x30, 0x16, 0xe0, 0x20, 0x20,
        0x20, 0x80, 0x00, 0x01, 0xf4, 0x80, 0x00, 0x75, 0x30, 0x6b, 0xe1, 0x00, 0x82, 0x00, 0x00,
        0x00, 0x01, 0x44, 0x01, 0xc0, 0x72, 0xf0, 0x22, 0x40,
    ];
    const GOLDEN_MAIN_IDR: &[u8] = &[0x00, 0x00, 0x00, 0x01, 0x26, 0x01, 0xae, 0x80];
    const GOLDEN_MAIN_P: &[u8] = &[0x00, 0x00, 0x00, 0x01, 0x02, 0x01, 0xd0, 0x00, 0x97, 0x68];
    const GOLDEN_MAIN10: &[u8] = &[
        0x00, 0x00, 0x00, 0x01, 0x40, 0x01, 0x0c, 0x01, 0xff, 0xff, 0x02, 0x20, 0x00, 0x00, 0x03,
        0x00, 0xb0, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x5a, 0x2c, 0x0c, 0x00, 0x00, 0x0f,
        0xa4, 0x00, 0x03, 0xa9, 0x83, 0xa0, 0x00, 0x00, 0x00, 0x01, 0x42, 0x01, 0x01, 0x02, 0x20,
        0x00, 0x00, 0x03, 0x00, 0xb0, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x5a, 0xa0, 0x05,
        0x12, 0x01, 0x71, 0xc9, 0x23, 0x62, 0x4b, 0x92, 0x46, 0xda, 0xf0, 0x16, 0xe0, 0x20, 0x20,
        0x20, 0x80, 0x00, 0x01, 0xf4, 0x80, 0x00, 0x75, 0x30, 0x6b, 0xe1, 0x00, 0x82, 0x00, 0x00,
        0x00, 0x01, 0x44, 0x01, 0xc1, 0x62, 0x39, 0x31, 0x22, 0x40,
    ];
    const GOLDEN_MAIN10_IDR: &[u8] = &[0x00, 0x00, 0x00, 0x01, 0x26, 0x01, 0xaf, 0xa0];
    const GOLDEN_MAIN10_B: &[u8] = &[0x00, 0x00, 0x00, 0x01, 0x02, 0x01, 0xe0, 0x02, 0x5f, 0x9d];

    /// Writes the golden streams for the FFmpeg check:
    /// `RBS_HEVC_GOLDEN_DIR=/tmp/x cargo test -p rbs-codec-vaapi -- dump_golden --ignored`.
    #[test]
    #[ignore = "writes files for a manual FFmpeg check"]
    fn dump_golden() {
        let dir = std::env::var("RBS_HEVC_GOLDEN_DIR").unwrap();
        let vdenc = Blocks::from_attribute(3, 0, 0, 3, 2, 2).unwrap();
        let cases = [
            (
                "main",
                golden(
                    Profile::HevcMain,
                    CBR,
                    Blocks::DEFAULT,
                    Tools::default_for(false),
                    false,
                ),
                4097,
            ),
            (
                "main10",
                golden(
                    Profile::HevcMain10,
                    Rc::Quality { level: 30 },
                    vdenc,
                    all_tools(),
                    true,
                ),
                1,
            ),
        ];
        for (name, s, poc) in cases {
            let print = |label: &str, b: &[u8]| {
                let hex: Vec<String> = b.iter().map(|x| format!("{x:#04x}")).collect();
                println!("{name} {label}: [{}]", hex.join(", "));
            };
            print("headers", &s.headers());
            print("idr", &s.slice_header(true, 0));
            print("p", &s.slice_header(false, poc));
            // Slice data stand-in so the parser sees a complete slice.
            let mut stream = s.headers();
            for idr in [true, false] {
                stream.extend(s.slice_header(idr, if idr { 0 } else { poc }));
                stream.extend([0xaa; 16]);
            }
            std::fs::write(format!("{dir}/{name}.hevc"), stream).unwrap();
        }
    }
}
