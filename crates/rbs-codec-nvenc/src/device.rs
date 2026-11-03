//! What one NVENC device can do, as read by the probe, and the capability set the
//! adapter declares from it. Pure data, so it is tested on machines without NVIDIA
//! hardware.

use rbs_codec::caps;
use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::media::CodecId;

/// Limits of one codec on one device (`NV_ENC_CAPS_*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CodecCaps {
    /// `h264` or `hevc`.
    pub codec: CodecId,
    /// Smallest and largest picture sides.
    pub width: (u32, u32),
    /// Smallest and largest picture heights.
    pub height: (u32, u32),
    /// Macroblocks (16x16) per second the encoder sustains.
    pub mb_per_sec: u64,
    /// 10-bit input and the Main10 profile.
    pub ten_bit: bool,
    /// Constant bitrate.
    pub cbr: bool,
    /// Variable bitrate (also used for constant quality, `targetQuality`).
    pub vbr: bool,
    /// Bitrate changes without a new session.
    pub dynamic_bitrate: bool,
}

impl CodecCaps {
    /// True when `width` x `height` is inside the device limits.
    pub(crate) fn fits(&self, width: u32, height: u32) -> bool {
        (self.width.0..=self.width.1).contains(&width)
            && (self.height.0..=self.height.1).contains(&height)
    }
}

/// The probed device: its name and the codecs it encodes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct DeviceCaps {
    /// GPU name for logs and diagnostics.
    pub name: String,
    /// Codecs in the order H.264, HEVC; absent codecs are not listed.
    pub codecs: Vec<CodecCaps>,
}

impl DeviceCaps {
    /// The limits of `codec`, if the device encodes it.
    pub(crate) fn codec(&self, codec: CodecId) -> Option<&CodecCaps> {
        self.codecs.iter().find(|c| c.codec == codec)
    }
}

fn set<'a>(values: impl IntoIterator<Item = &'a str>) -> CapabilityValue {
    CapabilityValue::Set(values.into_iter().map(str::to_owned).collect())
}

/// The capabilities the adapter declares for `device`. Empty when the device encodes
/// nothing. Width and height are the range every declared codec supports, so a size
/// in range works with each of them.
pub(crate) fn capability_set(device: &DeviceCaps) -> CapabilitySet {
    let codecs = &device.codecs;
    if codecs.is_empty() {
        return CapabilitySet::new();
    }
    let all = |f: fn(&CodecCaps) -> bool| codecs.iter().all(f);
    let min_width = codecs.iter().map(|c| c.width.0).max().unwrap_or(0);
    let max_width = codecs.iter().map(|c| c.width.1).min().unwrap_or(0);
    let min_height = codecs.iter().map(|c| c.height.0).max().unwrap_or(0);
    let max_height = codecs.iter().map(|c| c.height.1).min().unwrap_or(0);
    // Frame rate at the largest common size, from the slowest codec.
    let macroblocks = u64::from(max_width.div_ceil(16)) * u64::from(max_height.div_ceil(16));
    let fps_max = codecs
        .iter()
        .map(|c| c.mb_per_sec.checked_div(macroblocks).unwrap_or(0))
        .min()
        .unwrap_or(0)
        .max(1);

    let mut profiles = Vec::new();
    let mut ten_bit = false;
    for c in codecs {
        match c.codec {
            CodecId::H264 => profiles.extend(["h264.baseline", "h264.main", "h264.high"]),
            CodecId::Hevc => {
                profiles.push("hevc.main");
                if c.ten_bit {
                    profiles.push("hevc.main10");
                    ten_bit = true;
                }
            }
            _ => {}
        }
    }
    let mut rate_controls = Vec::new();
    if all(|c| c.cbr) {
        rate_controls.push("cbr");
    }
    if all(|c| c.vbr) {
        rate_controls.extend(["vbr", "constant_quality"]);
    }
    let mut reconfigure = vec![caps::RECONFIGURE_KEYFRAME];
    if all(|c| c.dynamic_bitrate) {
        reconfigure.insert(0, caps::RECONFIGURE_BITRATE);
    }

    CapabilitySet::new()
        .with(caps::HARDWARE, CapabilityValue::Flag(true))
        .with(
            caps::VIDEO_ENCODE,
            set(codecs.iter().map(|c| c.codec.as_str())),
        )
        .with(caps::VIDEO_PROFILES, set(profiles))
        .with(
            caps::VIDEO_BIT_DEPTH,
            set(if ten_bit {
                &["8", "10"][..]
            } else {
                &["8"][..]
            }
            .iter()
            .copied()),
        )
        .with(
            caps::VIDEO_WIDTH,
            CapabilityValue::IntRange {
                min: i64::from(min_width),
                max: i64::from(max_width),
            },
        )
        .with(
            caps::VIDEO_HEIGHT,
            CapabilityValue::IntRange {
                min: i64::from(min_height),
                max: i64::from(max_height),
            },
        )
        .with(
            caps::VIDEO_FPS_MAX,
            CapabilityValue::Int(i64::try_from(fps_max).unwrap_or(i64::MAX)),
        )
        .with(caps::VIDEO_RATE_CONTROL, set(rate_controls))
        // Frame reordering stays off until its depth is measured on hardware.
        .with(caps::VIDEO_B_FRAMES_MAX, CapabilityValue::Int(0))
        .with(caps::RECONFIGURE, set(reconfigure))
        .with(caps::INPUT_MEMORY, set(["system"]))
        .with(
            caps::INPUT_PIXEL_FORMAT,
            set(if ten_bit {
                &["nv12", "p010"][..]
            } else {
                &["nv12"][..]
            }
            .iter()
            .copied()),
        )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A device like a current GeForce: H.264 to 4096, HEVC to 8192 with 10 bits.
    pub(crate) fn geforce() -> DeviceCaps {
        DeviceCaps {
            name: "Test GPU".into(),
            codecs: vec![
                CodecCaps {
                    codec: CodecId::H264,
                    width: (145, 4096),
                    height: (49, 4096),
                    mb_per_sec: 4096 / 16 * 4096 / 16 * 60,
                    ten_bit: false,
                    cbr: true,
                    vbr: true,
                    dynamic_bitrate: true,
                },
                CodecCaps {
                    codec: CodecId::Hevc,
                    width: (129, 8192),
                    height: (33, 8192),
                    mb_per_sec: 8192 / 16 * 8192 / 16 * 30,
                    ten_bit: true,
                    cbr: true,
                    vbr: true,
                    dynamic_bitrate: true,
                },
            ],
        }
    }

    fn values(set: &CapabilitySet, key: &rbs_core::capability::CapabilityKey) -> Vec<String> {
        match set.get(key) {
            Some(CapabilityValue::Set(v)) => {
                let mut v: Vec<String> = v.to_vec();
                v.sort();
                v
            }
            other => panic!("{key:?} is {other:?}"),
        }
    }

    #[test]
    fn declares_the_common_range_and_both_codecs() {
        let set = capability_set(&geforce());
        assert_eq!(values(&set, &caps::VIDEO_ENCODE), ["h264", "hevc"]);
        assert_eq!(
            set.get(&caps::VIDEO_WIDTH),
            Some(&CapabilityValue::IntRange {
                min: 145,
                max: 4096
            })
        );
        assert_eq!(
            set.get(&caps::VIDEO_FPS_MAX),
            Some(&CapabilityValue::Int(60))
        );
        assert_eq!(values(&set, &caps::INPUT_PIXEL_FORMAT), ["nv12", "p010"]);
        assert_eq!(
            values(&set, &caps::VIDEO_RATE_CONTROL),
            ["cbr", "constant_quality", "vbr"]
        );
        assert_eq!(values(&set, &caps::RECONFIGURE), ["bitrate", "keyframe"]);
        assert_eq!(set.get(&caps::HARDWARE), Some(&CapabilityValue::Flag(true)));
    }

    #[test]
    fn h264_only_device_has_no_ten_bit() {
        let mut device = geforce();
        device.codecs.truncate(1);
        device.codecs[0].dynamic_bitrate = false;
        let set = capability_set(&device);
        assert_eq!(values(&set, &caps::VIDEO_BIT_DEPTH), ["8"]);
        assert_eq!(values(&set, &caps::INPUT_PIXEL_FORMAT), ["nv12"]);
        assert_eq!(values(&set, &caps::RECONFIGURE), ["keyframe"]);
    }

    #[test]
    fn no_codecs_declares_nothing() {
        assert!(capability_set(&DeviceCaps::default()).is_empty());
    }
}
