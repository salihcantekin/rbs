//! Turns a [`VideoEncoderConfig`] into NVENC settings, refusing what the device or the
//! adapter cannot do. Pure, so the mapping is tested without hardware.

use rbs_codec::{CreateError, EncoderPreset, RateControl, VideoEncoderConfig};
use rbs_core::media::{CodecId, MemoryKind, PixelFormat};
use rbs_media_format::h273::ColorCodes;

use crate::device::DeviceCaps;

/// Codec profile, mapped to an NVENC profile GUID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Profile {
    /// H.264 Constrained Baseline (CAVLC, no B-frames).
    H264Baseline,
    /// H.264 Main.
    H264Main,
    /// H.264 High.
    H264High,
    /// HEVC Main (8 bits).
    HevcMain,
    /// HEVC Main 10.
    HevcMain10,
}

/// NVENC rate control (`NV_ENC_RC_PARAMS`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rc {
    /// `NV_ENC_PARAMS_RC_CBR`: average = max = `bitrate`, VBV of one second.
    Cbr {
        /// Bits per second.
        bitrate: u32,
    },
    /// `NV_ENC_PARAMS_RC_VBR` with a peak, VBV of one second at the peak.
    Vbr {
        /// Average bits per second.
        average: u32,
        /// Peak bits per second.
        max: u32,
    },
    /// `NV_ENC_PARAMS_RC_VBR` with no bitrate target and `targetQuality`.
    Quality {
        /// 1 (best) ..= 51; NVENC reads 0 as "automatic", so level 0 becomes 1.
        target: u8,
    },
}

/// NVENC tuning (`NV_ENC_TUNING_INFO`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tuning {
    /// `NV_ENC_TUNING_INFO_HIGH_QUALITY`.
    HighQuality,
    /// `NV_ENC_TUNING_INFO_LOW_LATENCY`.
    LowLatency,
}

/// Everything the engine needs to open a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Plan {
    /// `h264` or `hevc`.
    pub codec: CodecId,
    /// Picture size.
    pub width: u32,
    /// Picture size.
    pub height: u32,
    /// True for P010 input (`NV_ENC_BUFFER_FORMAT_YUV420_10BIT`), else NV12.
    pub ten_bit: bool,
    /// Frame rate numerator and denominator.
    pub fps: (u32, u32),
    /// Preset `P1` (fastest) ..= `P7` (best).
    pub preset: u8,
    /// Tuning.
    pub tuning: Tuning,
    /// Profile.
    pub profile: Profile,
    /// Rate control.
    pub rc: Rc,
    /// Frames between IDR pictures (`gopLength` = `idrPeriod`).
    pub gop: u32,
    /// VUI colour description.
    pub color: ColorCodes,
}

fn unsupported(what: impl Into<String>) -> CreateError {
    CreateError::Unsupported { what: what.into() }
}

/// The NVENC rate control for `rc`, or why it is invalid.
pub(crate) fn rate_control(rc: &RateControl) -> Result<Rc, String> {
    let bps = |v: u64, name: &str| -> Result<u32, String> {
        match u32::try_from(v) {
            Ok(0) => Err(format!("{name} is zero")),
            Ok(v) => Ok(v),
            Err(_) => Err(format!("{name} {v} bit/s is above NVENC's 32-bit limit")),
        }
    };
    match *rc {
        RateControl::Cbr { bitrate_bps } => Ok(Rc::Cbr {
            bitrate: bps(bitrate_bps, "bitrate")?,
        }),
        RateControl::Vbr {
            target_bps,
            max_bps,
        } => {
            let average = bps(target_bps, "target bitrate")?;
            let max = bps(max_bps, "peak bitrate")?;
            if max < average {
                return Err(format!(
                    "peak {max} bit/s is below the target {average} bit/s"
                ));
            }
            Ok(Rc::Vbr { average, max })
        }
        RateControl::ConstantQuality { level } if level <= 51 => Ok(Rc::Quality {
            target: level.max(1),
        }),
        RateControl::ConstantQuality { level } => Err(format!("quality level {level} > 51")),
        _ => Err("unknown rate-control mode".into()),
    }
}

/// Validates `config` against `device` and maps it.
pub(crate) fn plan(config: &VideoEncoderConfig, device: &DeviceCaps) -> Result<Plan, CreateError> {
    let codec = config.codec;
    let caps = device
        .codec(codec)
        .ok_or_else(|| unsupported(format!("codec {codec} on {}", device.name)))?;
    if config.memory != MemoryKind::System {
        return Err(unsupported(format!("input memory {}", config.memory)));
    }
    let ten_bit = match config.format.pixel_format {
        PixelFormat::Nv12 => false,
        PixelFormat::P010 if codec == CodecId::Hevc && caps.ten_bit => true,
        other => return Err(unsupported(format!("pixel format {other} with {codec}"))),
    };
    let size = config.format.size;
    if !size.width.is_multiple_of(2) || !size.height.is_multiple_of(2) {
        return Err(unsupported(format!(
            "odd size {}x{} (4:2:0 needs even sides)",
            size.width, size.height
        )));
    }
    if !caps.fits(size.width, size.height) {
        return Err(unsupported(format!(
            "size {}x{} outside {}x{} ..= {}x{}",
            size.width, size.height, caps.width.0, caps.height.0, caps.width.1, caps.height.1
        )));
    }
    let profile = match (codec, config.profile.as_deref(), ten_bit) {
        (CodecId::H264, Some("baseline"), _) => Profile::H264Baseline,
        (CodecId::H264, Some("main"), _) => Profile::H264Main,
        (CodecId::H264, Some("high") | None, _) => Profile::H264High,
        (CodecId::Hevc, Some("main") | None, false) => Profile::HevcMain,
        (CodecId::Hevc, Some("main10") | None, true) => Profile::HevcMain10,
        (_, Some(p), _) => {
            return Err(unsupported(format!(
                "profile {p} for {codec} with {}",
                config.format.pixel_format
            )));
        }
        _ => return Err(unsupported(format!("codec {codec}"))),
    };
    let rc = rate_control(&config.rate_control).map_err(CreateError::InvalidConfig)?;
    if matches!(rc, Rc::Cbr { .. }) && !caps.cbr || !matches!(rc, Rc::Cbr { .. }) && !caps.vbr {
        return Err(unsupported(format!(
            "rate control {} on {}",
            config.rate_control.name(),
            device.name
        )));
    }
    let color = ColorCodes::of(&config.format.color)
        .ok_or_else(|| unsupported(format!("colour description {:?}", config.format.color)))?;
    let per_second = config.frame_rate.per_second();
    let fps = (per_second.num(), per_second.den());
    if fps.0 == 0 || fps.1 == 0 {
        return Err(CreateError::InvalidConfig("zero frame rate".into()));
    }
    let preset = match config.tuning.preset {
        EncoderPreset::Quality => 5,
        EncoderPreset::Speed => 2,
        _ => 4,
    };
    let tuning = if config.tuning.low_latency {
        Tuning::LowLatency
    } else {
        Tuning::HighQuality
    };
    Ok(Plan {
        codec,
        width: size.width,
        height: size.height,
        ten_bit,
        fps,
        preset,
        tuning,
        profile,
        rc,
        gop: config.keyframe_interval.frames_at(config.frame_rate),
        color,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
mod tests {
    use super::*;
    use crate::device::tests::geforce;
    use rbs_codec::{EncoderTuning, KeyframeInterval};
    use rbs_core::media::{ColorSpace, Size, VideoFormat};
    use rbs_core::timing::FrameRate;

    fn config(codec: CodecId, pixel_format: PixelFormat) -> VideoEncoderConfig {
        VideoEncoderConfig::new(
            codec,
            VideoFormat::new(
                Size::new(1920, 1080),
                pixel_format,
                ColorSpace::BT709_LIMITED,
            ),
            FrameRate::FPS_60,
            MemoryKind::System,
            RateControl::Cbr {
                bitrate_bps: 6_000_000,
            },
        )
    }

    #[test]
    fn streaming_defaults() {
        let plan = plan(&config(CodecId::H264, PixelFormat::Nv12), &geforce()).unwrap();
        assert_eq!(plan.profile, Profile::H264High);
        assert_eq!(plan.rc, Rc::Cbr { bitrate: 6_000_000 });
        assert_eq!(plan.gop, 120);
        assert_eq!(plan.fps, (60, 1));
        assert_eq!(plan.preset, 4);
        assert_eq!(plan.tuning, Tuning::HighQuality);
        assert!(!plan.ten_bit);
        assert_eq!((plan.color.primaries, plan.color.matrix), (1, 1));
    }

    #[test]
    fn hevc_main10_from_p010() {
        let config = config(CodecId::Hevc, PixelFormat::P010)
            .with_keyframe_interval(KeyframeInterval::Frames(30))
            .with_tuning({
                let mut t = EncoderTuning::default();
                t.preset = EncoderPreset::Quality;
                t.low_latency = true;
                t
            });
        let plan = plan(&config, &geforce()).unwrap();
        assert_eq!(plan.profile, Profile::HevcMain10);
        assert!(plan.ten_bit);
        assert_eq!(plan.gop, 30);
        assert_eq!((plan.preset, plan.tuning), (5, Tuning::LowLatency));
    }

    #[test]
    fn refusals() {
        let device = geforce();
        let refused = |c: VideoEncoderConfig| plan(&c, &device).unwrap_err();
        assert!(matches!(
            refused(config(CodecId::H264, PixelFormat::P010)),
            CreateError::Unsupported { .. }
        ));
        assert!(matches!(
            refused(config(CodecId::Av1, PixelFormat::Nv12)),
            CreateError::Unsupported { .. }
        ));
        assert!(matches!(
            refused(config(CodecId::Hevc, PixelFormat::Nv12).with_profile("main10")),
            CreateError::Unsupported { .. }
        ));
        let mut odd = config(CodecId::H264, PixelFormat::Nv12);
        odd.format.size = Size::new(641, 360);
        assert!(matches!(refused(odd), CreateError::Unsupported { .. }));
        let mut big = config(CodecId::H264, PixelFormat::Nv12);
        big.format.size = Size::new(8192, 4320);
        assert!(matches!(refused(big), CreateError::Unsupported { .. }));
        let mut gpu = config(CodecId::H264, PixelFormat::Nv12);
        gpu.memory = MemoryKind::D3d12;
        assert!(matches!(refused(gpu), CreateError::Unsupported { .. }));
        let mut rgb = config(CodecId::H264, PixelFormat::Nv12);
        rgb.format.color = ColorSpace::SRGB;
        assert!(matches!(refused(rgb), CreateError::Unsupported { .. }));
        let mut zero = config(CodecId::H264, PixelFormat::Nv12);
        zero.rate_control = RateControl::Cbr { bitrate_bps: 0 };
        assert!(matches!(refused(zero), CreateError::InvalidConfig(_)));
    }

    #[test]
    fn rate_control_mapping() {
        assert_eq!(
            rate_control(&RateControl::Vbr {
                target_bps: 4_000_000,
                max_bps: 8_000_000
            }),
            Ok(Rc::Vbr {
                average: 4_000_000,
                max: 8_000_000
            })
        );
        assert!(
            rate_control(&RateControl::Vbr {
                target_bps: 4_000_000,
                max_bps: 1_000_000
            })
            .is_err()
        );
        assert_eq!(
            rate_control(&RateControl::ConstantQuality { level: 0 }),
            Ok(Rc::Quality { target: 1 })
        );
        assert!(rate_control(&RateControl::ConstantQuality { level: 52 }).is_err());
        assert!(
            rate_control(&RateControl::Cbr {
                bitrate_bps: u64::from(u32::MAX) + 1
            })
            .is_err()
        );
    }
}
