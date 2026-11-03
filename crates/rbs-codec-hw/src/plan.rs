//! Turns a [`VideoEncoderConfig`] into vendor-neutral settings, refusing what the
//! device or the adapters cannot do. Pure, so the mapping is tested without
//! hardware. Each adapter maps the [`Plan`] to its own parameters.

use rbs_codec::{CreateError, EncoderPreset, RateControl, VideoEncoderConfig};
use rbs_core::media::{CodecId, MemoryKind, PixelFormat};
use rbs_media_format::h273::ColorCodes;

use crate::device::{DeviceCaps, padded_everywhere};

/// Codec profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Profile {
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

impl Profile {
    /// Every profile, in capability order.
    pub const ALL: [Profile; 5] = [
        Profile::H264Baseline,
        Profile::H264Main,
        Profile::H264High,
        Profile::HevcMain,
        Profile::HevcMain10,
    ];

    /// The `codec.video.profiles` value (`h264.high`, ...).
    pub fn capability_name(self) -> &'static str {
        match self {
            Profile::H264Baseline => "h264.baseline",
            Profile::H264Main => "h264.main",
            Profile::H264High => "h264.high",
            Profile::HevcMain => "hevc.main",
            Profile::HevcMain10 => "hevc.main10",
        }
    }
}

/// Rate control, with bitrates that fit the 32-bit fields of every vendor API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rc {
    /// Average = peak = `bitrate`.
    Cbr {
        /// Bits per second.
        bitrate: u32,
    },
    /// Variable bitrate with a peak.
    Vbr {
        /// Average bits per second.
        average: u32,
        /// Peak bits per second.
        max: u32,
    },
    /// Constant quality.
    Quality {
        /// 0 (best) ..= 51, on the codec's QP scale; each adapter maps it to its own
        /// quality parameter.
        level: u8,
    },
}

/// Speed and quality trade-off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    /// Least resource use.
    Speed,
    /// The default.
    Balanced,
    /// Best quality the machine can sustain in real time.
    Quality,
}

/// Everything an engine needs to open a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    /// `h264` or `hevc`.
    pub codec: CodecId,
    /// Picture width.
    pub width: u32,
    /// Picture height.
    pub height: u32,
    /// True for P010 input (Main 10), else NV12.
    pub ten_bit: bool,
    /// Frame rate numerator and denominator.
    pub fps: (u32, u32),
    /// Speed and quality trade-off.
    pub preset: Preset,
    /// Tune for low latency.
    pub low_latency: bool,
    /// Profile.
    pub profile: Profile,
    /// Rate control.
    pub rc: Rc,
    /// Pad the CBR stream with filler data to its full bitrate (`cbr_padded`; `rc` is
    /// then [`Rc::Cbr`]).
    pub cbr_padding: bool,
    /// Frames between IDR pictures (the GOP length; P frames only).
    pub gop: u32,
    /// VUI colour description.
    pub color: ColorCodes,
}

impl Plan {
    /// The input pixel format: P010 for 10 bits, else NV12.
    pub fn pixel_format(&self) -> PixelFormat {
        if self.ten_bit {
            PixelFormat::P010
        } else {
            PixelFormat::Nv12
        }
    }
}

fn unsupported(what: impl Into<String>) -> CreateError {
    CreateError::Unsupported { what: what.into() }
}

/// The rate control for `rc`, or why it is invalid.
pub fn rate_control(rc: &RateControl) -> Result<Rc, String> {
    let bps = |v: u64, name: &str| -> Result<u32, String> {
        match u32::try_from(v) {
            Ok(0) => Err(format!("{name} is zero")),
            Ok(v) => Ok(v),
            Err(_) => Err(format!(
                "{name} {v} bit/s is above the encoder's 32-bit limit"
            )),
        }
    };
    match *rc {
        // Padding is not part of `Rc`: the plan carries it (`Plan::cbr_padding`), and
        // engines keep it through rate changes.
        RateControl::Cbr { bitrate_bps } | RateControl::CbrPadded { bitrate_bps } => Ok(Rc::Cbr {
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
        RateControl::ConstantQuality { level } if level <= 51 => Ok(Rc::Quality { level }),
        RateControl::ConstantQuality { level } => Err(format!("quality level {level} > 51")),
        _ => Err("unknown rate-control mode".into()),
    }
}

/// Validates `config` against `device` and maps it.
///
/// Refused as `Unsupported`: a codec the device does not encode, input memory other
/// than system, a pixel format other than NV12 (or P010 with 10-bit HEVC), odd or
/// out-of-range sizes, a profile the device does not have, a rate-control mode the
/// device does not have (`cbr_padded` needs it on every codec, as declared), and
/// colours without H.273 code points. Refused as `InvalidConfig`: invalid bitrates or
/// quality levels and a zero frame rate. The
/// default profile is H.264 High (else Main, else Baseline) and HEVC Main or Main 10
/// by pixel format.
pub fn plan(config: &VideoEncoderConfig, device: &DeviceCaps) -> Result<Plan, CreateError> {
    let codec = config.codec;
    let caps = device
        .codec(codec)
        .ok_or_else(|| unsupported(format!("codec {codec} on {}", device.name)))?;
    if config.memory != MemoryKind::System {
        return Err(unsupported(format!("input memory {}", config.memory)));
    }
    let ten_bit = match config.format.pixel_format {
        PixelFormat::Nv12 => false,
        PixelFormat::P010 if codec == CodecId::Hevc && caps.ten_bit() => true,
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
    let has = |p: Profile| caps.profiles.contains(p);
    let profile = match (codec, config.profile.as_deref(), ten_bit) {
        (CodecId::H264, Some("baseline"), _) => Some(Profile::H264Baseline),
        (CodecId::H264, Some("main"), _) => Some(Profile::H264Main),
        (CodecId::H264, Some("high"), _) => Some(Profile::H264High),
        (CodecId::H264, None, _) => [Profile::H264High, Profile::H264Main, Profile::H264Baseline]
            .into_iter()
            .find(|p| has(*p)),
        (CodecId::Hevc, Some("main") | None, false) => Some(Profile::HevcMain),
        (CodecId::Hevc, Some("main10") | None, true) => Some(Profile::HevcMain10),
        _ => None,
    }
    .filter(|p| has(*p))
    .ok_or_else(|| {
        unsupported(format!(
            "profile {} for {codec} with {} on {}",
            config.profile.as_deref().unwrap_or("(default)"),
            config.format.pixel_format,
            device.name
        ))
    })?;
    let rc = rate_control(&config.rate_control).map_err(CreateError::InvalidConfig)?;
    let cbr_padding = matches!(config.rate_control, RateControl::CbrPadded { .. });
    let mode_supported = match rc {
        // Device-wide, as declared: a codec alone with filler data is not enough.
        Rc::Cbr { .. } if cbr_padding => padded_everywhere(device),
        Rc::Cbr { .. } => caps.cbr,
        Rc::Vbr { .. } => caps.vbr,
        Rc::Quality { .. } => caps.quality,
    };
    if !mode_supported {
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
        EncoderPreset::Quality => Preset::Quality,
        EncoderPreset::Speed => Preset::Speed,
        _ => Preset::Balanced,
    };
    Ok(Plan {
        codec,
        width: size.width,
        height: size.height,
        ten_bit,
        fps,
        preset,
        low_latency: config.tuning.low_latency,
        profile,
        rc,
        cbr_padding,
        gop: config.keyframe_interval.frames_at(config.frame_rate).max(1),
        color,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
mod tests {
    use super::*;
    use crate::device::Profiles;
    use crate::testing::discrete_gpu;
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
        let plan = plan(&config(CodecId::H264, PixelFormat::Nv12), &discrete_gpu()).unwrap();
        assert_eq!(plan.profile, Profile::H264High);
        assert_eq!(plan.rc, Rc::Cbr { bitrate: 6_000_000 });
        assert_eq!(plan.gop, 120);
        assert_eq!(plan.fps, (60, 1));
        assert_eq!(plan.preset, Preset::Balanced);
        assert!(!plan.low_latency);
        assert!(!plan.ten_bit);
        assert!(!plan.cbr_padding);
        assert_eq!(plan.pixel_format(), PixelFormat::Nv12);
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
        let plan = plan(&config, &discrete_gpu()).unwrap();
        assert_eq!(plan.profile, Profile::HevcMain10);
        assert!(plan.ten_bit);
        assert_eq!(plan.gop, 30);
        assert_eq!((plan.preset, plan.low_latency), (Preset::Quality, true));
    }

    #[test]
    fn default_h264_profile_follows_the_device() {
        let mut device = discrete_gpu();
        device.codecs[0].profiles = Profiles::NONE
            .with(Profile::H264Baseline)
            .with(Profile::H264Main);
        let c = config(CodecId::H264, PixelFormat::Nv12);
        assert_eq!(plan(&c, &device).unwrap().profile, Profile::H264Main);
        assert!(matches!(
            plan(&c.clone().with_profile("high"), &device),
            Err(CreateError::Unsupported { .. })
        ));
    }

    #[test]
    fn refusals() {
        let device = discrete_gpu();
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

        let mut no_quality = discrete_gpu();
        no_quality.codecs[0].quality = false;
        let mut cq = config(CodecId::H264, PixelFormat::Nv12);
        cq.rate_control = RateControl::ConstantQuality { level: 20 };
        assert!(matches!(
            plan(&cq, &no_quality),
            Err(CreateError::Unsupported { .. })
        ));
        assert_eq!(plan(&cq, &device).unwrap().rc, Rc::Quality { level: 20 });
    }

    #[test]
    fn cbr_padding() {
        let device = discrete_gpu();
        let mut padded = config(CodecId::H264, PixelFormat::Nv12);
        padded.rate_control = RateControl::CbrPadded {
            bitrate_bps: 6_000_000,
        };
        let p = plan(&padded, &device).unwrap();
        assert!(p.cbr_padding);
        assert_eq!(p.rc, Rc::Cbr { bitrate: 6_000_000 });

        // HEVC without filler data: the device does not declare `cbr_padded`, so H.264
        // is refused too.
        let mut partial = discrete_gpu();
        partial.codecs[1].cbr_padding = false;
        assert!(matches!(
            plan(&padded, &partial),
            Err(CreateError::Unsupported { .. })
        ));
        // Plain CBR is fine there.
        assert!(
            !plan(&config(CodecId::H264, PixelFormat::Nv12), &partial)
                .unwrap()
                .cbr_padding
        );
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
            Ok(Rc::Quality { level: 0 })
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
