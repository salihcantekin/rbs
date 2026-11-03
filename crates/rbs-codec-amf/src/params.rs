//! Maps the shared [`Plan`] to AMF encoder properties. Pure (no runtime calls), so
//! the mapping is unit-tested on every Linux and Windows build.

use rbs_codec_hw::plan::{Plan, Preset, Profile, Rc};
use rbs_core::media::CodecId;
use rbs_media_format::h273::ColorCodes;

use crate::sys::{self, Name, Names};

/// A property value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Value {
    Int(i64),
    Bool(bool),
    Size(i32, i32),
    Rate(u32, u32),
}

/// The property names of `codec`'s encoder.
pub(crate) fn names(codec: CodecId) -> &'static Names {
    if codec == CodecId::Hevc {
        &sys::HEVC
    } else {
        &sys::H264
    }
}

/// The AMF colour profile (matrix and range) of `color`, or `None` for a matrix AMF
/// cannot signal (it has BT.601, BT.709 and BT.2020 non-constant luminance).
pub(crate) fn color_profile(color: &ColorCodes) -> Option<i64> {
    let profile = match (color.matrix, color.full_range) {
        (1, false) => sys::AMF_VIDEO_CONVERTER_COLOR_PROFILE_709,
        (1, true) => sys::AMF_VIDEO_CONVERTER_COLOR_PROFILE_FULL_709,
        (5 | 6, false) => sys::AMF_VIDEO_CONVERTER_COLOR_PROFILE_601,
        (5 | 6, true) => sys::AMF_VIDEO_CONVERTER_COLOR_PROFILE_FULL_601,
        (9, false) => sys::AMF_VIDEO_CONVERTER_COLOR_PROFILE_2020,
        (9, true) => sys::AMF_VIDEO_CONVERTER_COLOR_PROFILE_FULL_2020,
        _ => return None,
    };
    Some(i64::from(profile))
}

/// The rate-control properties of `rc`: CBR with target = peak and a one-second
/// VBV, peak-constrained VBR with a one-second VBV at the peak, or constant QP with
/// I and P pictures at `level`. The VBV starts full.
pub(crate) fn rate_properties(codec: CodecId, rc: Rc) -> Vec<(Name, Value)> {
    let n = names(codec);
    let hevc = codec == CodecId::Hevc;
    let method = |cbr: bool| -> i64 {
        match (hevc, cbr) {
            (false, true) => sys::AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_CBR as i64,
            (false, false) => {
                sys::AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_PEAK_CONSTRAINED_VBR as i64
            }
            (true, true) => sys::AMF_VIDEO_ENCODER_HEVC_RATE_CONTROL_METHOD_CBR as i64,
            (true, false) => {
                sys::AMF_VIDEO_ENCODER_HEVC_RATE_CONTROL_METHOD_PEAK_CONSTRAINED_VBR as i64
            }
        }
    };
    let rates = |cbr: bool, target: u32, peak: u32| {
        vec![
            (n.rate_control, Value::Int(method(cbr))),
            (n.target_bitrate, Value::Int(i64::from(target))),
            (n.peak_bitrate, Value::Int(i64::from(peak))),
            (n.vbv_size, Value::Int(i64::from(peak))),
            // 64 = 100 %.
            (n.vbv_fullness, Value::Int(64)),
        ]
    };
    match rc {
        Rc::Cbr { bitrate } => rates(true, bitrate, bitrate),
        Rc::Vbr { average, max } => rates(false, average, max),
        Rc::Quality { level } => {
            let cqp = if hevc {
                sys::AMF_VIDEO_ENCODER_HEVC_RATE_CONTROL_METHOD_CONSTANT_QP as i64
            } else {
                sys::AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_CONSTANT_QP as i64
            };
            vec![
                (n.rate_control, Value::Int(cqp)),
                (n.qp_i, Value::Int(i64::from(level))),
                (n.qp_p, Value::Int(i64::from(level))),
            ]
        }
    }
}

/// Every property of `plan`, in the order they must be set (usage first: it resets
/// the others to its defaults). `Err` names what AMF cannot signal.
///
/// IDR pictures are placed by the engine, not by AMF (H.264 `IDRPeriod = 0`, HEVC
/// `GOPSPerIDR = 0`): it forces one with parameter sets every `gop` frames and on
/// request, so the GOP needs no AMF limit (H.264's is 1000 frames).
pub(crate) fn properties(plan: &Plan) -> Result<Vec<(Name, Value)>, String> {
    let n = names(plan.codec);
    let hevc = plan.codec == CodecId::Hevc;
    let profile = color_profile(&plan.color)
        .ok_or_else(|| format!("matrix coefficients {} with AMF", plan.color.matrix))?;
    let usage = match (hevc, plan.low_latency) {
        (false, false) => sys::AMF_VIDEO_ENCODER_USAGE_TRANSCODING as i64,
        (false, true) => sys::AMF_VIDEO_ENCODER_USAGE_LOW_LATENCY as i64,
        (true, false) => sys::AMF_VIDEO_ENCODER_HEVC_USAGE_TRANSCODING as i64,
        (true, true) => sys::AMF_VIDEO_ENCODER_HEVC_USAGE_LOW_LATENCY as i64,
    };
    let codec_profile = i64::from(match plan.profile {
        Profile::H264Baseline => sys::AMF_VIDEO_ENCODER_PROFILE_CONSTRAINED_BASELINE,
        Profile::H264Main => sys::AMF_VIDEO_ENCODER_PROFILE_MAIN,
        Profile::H264High => sys::AMF_VIDEO_ENCODER_PROFILE_HIGH,
        Profile::HevcMain => sys::AMF_VIDEO_ENCODER_HEVC_PROFILE_MAIN,
        Profile::HevcMain10 => sys::AMF_VIDEO_ENCODER_HEVC_PROFILE_MAIN_10,
    });
    let preset = i64::from(match (hevc, plan.preset) {
        (false, Preset::Speed) => sys::AMF_VIDEO_ENCODER_QUALITY_PRESET_SPEED,
        (false, Preset::Balanced) => sys::AMF_VIDEO_ENCODER_QUALITY_PRESET_BALANCED,
        (false, Preset::Quality) => sys::AMF_VIDEO_ENCODER_QUALITY_PRESET_QUALITY,
        (true, Preset::Speed) => sys::AMF_VIDEO_ENCODER_HEVC_QUALITY_PRESET_SPEED,
        (true, Preset::Balanced) => sys::AMF_VIDEO_ENCODER_HEVC_QUALITY_PRESET_BALANCED,
        (true, Preset::Quality) => sys::AMF_VIDEO_ENCODER_HEVC_QUALITY_PRESET_QUALITY,
    });
    let side = |v: u32| i32::try_from(v).unwrap_or(i32::MAX);
    let mut list = vec![
        (n.usage, Value::Int(usage)),
        (n.profile, Value::Int(codec_profile)),
        (n.quality_preset, Value::Int(preset)),
        (
            n.frame_size,
            Value::Size(side(plan.width), side(plan.height)),
        ),
        (n.frame_rate, Value::Rate(plan.fps.0, plan.fps.1)),
    ];
    if hevc {
        list.extend([
            (sys::HEVC_GOP_SIZE, Value::Int(i64::from(plan.gop))),
            (sys::HEVC_GOPS_PER_IDR, Value::Int(0)),
            (
                sys::HEVC_HEADER_MODE,
                Value::Int(sys::AMF_VIDEO_ENCODER_HEVC_HEADER_INSERTION_MODE_IDR_ALIGNED as i64),
            ),
            (
                n.bit_depth,
                Value::Int(if plan.ten_bit {
                    sys::AMF_COLOR_BIT_DEPTH_10 as i64
                } else {
                    sys::AMF_COLOR_BIT_DEPTH_8 as i64
                }),
            ),
            (
                sys::HEVC_NOMINAL_RANGE,
                Value::Int(i64::from(plan.color.full_range)),
            ),
        ]);
    } else {
        list.extend([
            // P frames only: no reordering, one output per input in order.
            (sys::H264_B_PIC_PATTERN, Value::Int(0)),
            (sys::H264_IDR_PERIOD, Value::Int(0)),
            (sys::H264_HEADER_SPACING, Value::Int(0)),
            (sys::H264_FULL_RANGE, Value::Bool(plan.color.full_range)),
        ]);
    }
    list.extend([
        (n.output_color_profile, Value::Int(profile)),
        (
            n.output_transfer,
            Value::Int(i64::from(plan.color.transfer)),
        ),
        (
            n.output_primaries,
            Value::Int(i64::from(plan.color.primaries)),
        ),
    ]);
    list.extend(rate_properties(plan.codec, plan.rc));
    Ok(list)
}

/// 100-nanosecond ticks per frame at `fps` (AMF timestamps), at least 1.
pub(crate) fn frame_duration(fps: (u32, u32)) -> i64 {
    (10_000_000 * i64::from(fps.1) / i64::from(fps.0.max(1))).max(1)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
mod tests {
    use super::*;
    use rbs_core::media::ColorSpace;

    fn plan(codec: CodecId, rc: Rc) -> Plan {
        Plan {
            codec,
            width: 1920,
            height: 1080,
            ten_bit: codec == CodecId::Hevc,
            fps: (60, 1),
            preset: Preset::Quality,
            low_latency: false,
            profile: if codec == CodecId::Hevc {
                Profile::HevcMain10
            } else {
                Profile::H264High
            },
            rc,
            gop: 2000,
            color: ColorCodes::of(&ColorSpace::BT709_LIMITED).unwrap(),
        }
    }

    fn get(list: &[(Name, Value)], name: Name) -> Value {
        list.iter().find(|(n, _)| *n == name).unwrap().1
    }

    #[test]
    fn h264_streaming_properties() {
        let list = properties(&plan(CodecId::H264, Rc::Cbr { bitrate: 6_000_000 })).unwrap();
        assert_eq!(list[0].0, sys::H264.usage, "usage comes first");
        assert_eq!(get(&list, sys::H264.profile), Value::Int(100));
        assert_eq!(get(&list, sys::H264.quality_preset), Value::Int(2));
        assert_eq!(get(&list, sys::H264.frame_size), Value::Size(1920, 1080));
        assert_eq!(get(&list, sys::H264_IDR_PERIOD), Value::Int(0));
        assert_eq!(get(&list, sys::H264_B_PIC_PATTERN), Value::Int(0));
        assert_eq!(get(&list, sys::H264.target_bitrate), Value::Int(6_000_000));
        assert_eq!(get(&list, sys::H264.peak_bitrate), Value::Int(6_000_000));
        assert_eq!(get(&list, sys::H264.output_color_profile), Value::Int(1));
        assert_eq!(get(&list, sys::H264_FULL_RANGE), Value::Bool(false));
    }

    #[test]
    fn hevc_main10_constant_quality() {
        let list = properties(&plan(CodecId::Hevc, Rc::Quality { level: 20 })).unwrap();
        assert_eq!(list[0].0, sys::HEVC.usage);
        assert_eq!(get(&list, sys::HEVC.profile), Value::Int(2));
        assert_eq!(get(&list, sys::HEVC.bit_depth), Value::Int(10));
        assert_eq!(get(&list, sys::HEVC_GOP_SIZE), Value::Int(2000));
        assert_eq!(get(&list, sys::HEVC_GOPS_PER_IDR), Value::Int(0));
        assert_eq!(get(&list, sys::HEVC.qp_i), Value::Int(20));
        assert_eq!(get(&list, sys::HEVC.qp_p), Value::Int(20));
        assert_eq!(get(&list, sys::HEVC.quality_preset), Value::Int(0));
    }

    #[test]
    fn vbr_and_colour() {
        let r = rate_properties(
            CodecId::Hevc,
            Rc::Vbr {
                average: 4_000_000,
                max: 8_000_000,
            },
        );
        assert_eq!(get(&r, sys::HEVC.rate_control), Value::Int(2));
        assert_eq!(get(&r, sys::HEVC.vbv_size), Value::Int(8_000_000));
        let mut p = plan(CodecId::H264, Rc::Cbr { bitrate: 1 });
        p.color.matrix = 0;
        assert!(properties(&p).is_err());
        assert_eq!(frame_duration((60000, 1001)), 166_833);
    }

    #[test]
    fn wide_names_are_nul_terminated() {
        let name = sys::H264.component;
        assert_eq!(name.len(), "AMFVideoEncoderVCE_AVC".len() + 1);
        assert_eq!(name[0], b'A' as sys::wchar_t);
        assert_eq!(*name.last().unwrap(), 0);
    }
}
