//! What one hardware encoder device can do, as an adapter's probe reads it, and the
//! capability set the adapter declares from it. Pure data, so it is tested on
//! machines without the hardware.

use rbs_codec::caps;
use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::media::CodecId;

use crate::plan::Profile;

/// A set of [`Profile`]s.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Profiles(u32);

impl Profiles {
    /// No profile.
    pub const NONE: Profiles = Profiles(0);
    /// H.264 Constrained Baseline, Main and High.
    pub const H264_ALL: Profiles = Profiles::NONE
        .with(Profile::H264Baseline)
        .with(Profile::H264Main)
        .with(Profile::H264High);
    /// HEVC Main.
    pub const HEVC_MAIN: Profiles = Profiles::NONE.with(Profile::HevcMain);
    /// HEVC Main and Main 10.
    pub const HEVC_ALL: Profiles = Profiles::HEVC_MAIN.with(Profile::HevcMain10);
    /// AV1 Main (8-bit).
    pub const AV1_MAIN: Profiles = Profiles::NONE.with(Profile::Av1Main);

    const fn bit(p: Profile) -> u32 {
        1 << (p as u32)
    }

    /// This set plus `p`.
    #[must_use]
    pub const fn with(self, p: Profile) -> Profiles {
        Profiles(self.0 | Self::bit(p))
    }

    /// This set without `p`.
    #[must_use]
    pub const fn without(self, p: Profile) -> Profiles {
        Profiles(self.0 & !Self::bit(p))
    }

    /// True when `p` is in the set.
    pub const fn contains(self, p: Profile) -> bool {
        self.0 & Self::bit(p) != 0
    }

    /// True when the set is empty.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The profiles in the set, in [`Profile::ALL`] order.
    pub fn iter(self) -> impl Iterator<Item = Profile> {
        Profile::ALL.into_iter().filter(move |p| self.contains(*p))
    }
}

/// Limits of one codec on one device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodecCaps {
    /// `h264`, `hevc` or `av1`.
    pub codec: CodecId,
    /// Smallest and largest picture widths.
    pub width: (u32, u32),
    /// Smallest and largest picture heights.
    pub height: (u32, u32),
    /// Macroblocks (16x16) per second the encoder sustains; 0 when unknown.
    pub mb_per_sec: u64,
    /// Profiles of this codec the device encodes. `HevcMain10` also means P010 input.
    pub profiles: Profiles,
    /// Constant bitrate.
    pub cbr: bool,
    /// Variable bitrate with a peak.
    pub vbr: bool,
    /// Constant quality.
    pub quality: bool,
    /// Bitrate changes without a new session.
    pub dynamic_bitrate: bool,
    /// Filler data that keeps a CBR stream at its full bitrate (only meaningful with
    /// `cbr`).
    pub cbr_padding: bool,
}

impl CodecCaps {
    /// True when `width` x `height` is inside the device limits.
    pub fn fits(&self, width: u32, height: u32) -> bool {
        (self.width.0..=self.width.1).contains(&width)
            && (self.height.0..=self.height.1).contains(&height)
    }

    /// True when the device encodes 10-bit pictures of this codec from P010.
    pub fn ten_bit(&self) -> bool {
        self.profiles.contains(Profile::HevcMain10)
    }

    /// True when the limits describe a usable encoder: a profile and a non-empty
    /// size range.
    pub fn usable(&self) -> bool {
        !self.profiles.is_empty()
            && self.width.0 > 0
            && self.height.0 > 0
            && self.width.1 >= self.width.0
            && self.height.1 >= self.height.0
    }
}

/// The probed device: its name and the codecs it encodes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DeviceCaps {
    /// Device name for logs and diagnostics.
    pub name: String,
    /// Codecs in the order H.264, HEVC, AV1; absent codecs are not listed.
    pub codecs: Vec<CodecCaps>,
}

impl DeviceCaps {
    /// The limits of `codec`, if the device encodes it.
    pub fn codec(&self, codec: CodecId) -> Option<&CodecCaps> {
        self.codecs.iter().find(|c| c.codec == codec)
    }
}

fn set<'a>(values: impl IntoIterator<Item = &'a str>) -> CapabilityValue {
    CapabilityValue::Set(values.into_iter().map(str::to_owned).collect())
}

/// True when every codec of `device` has CBR with filler data: only then is
/// `cbr_padded` declared, and [`crate::plan::plan`] takes it on the same rule.
pub fn padded_everywhere(device: &DeviceCaps) -> bool {
    !device.codecs.is_empty() && device.codecs.iter().all(|c| c.cbr && c.cbr_padding)
}

/// The capabilities an adapter declares for `device` (`adapter.hardware`, system
/// memory input, no B-frames). Empty when the device encodes nothing. Width and
/// height are the range every declared codec supports, so a size in range works with
/// each of them; a rate-control mode or bitrate change is declared only when every
/// codec has it. `codec.video.fps.max` is the slowest codec's rate at the largest
/// common size, and is left out when no codec reports its throughput.
pub fn capability_set(device: &DeviceCaps) -> CapabilitySet {
    let codecs = &device.codecs;
    if codecs.is_empty() {
        return CapabilitySet::new();
    }
    let all = |f: fn(&CodecCaps) -> bool| codecs.iter().all(f);
    let min_width = codecs.iter().map(|c| c.width.0).max().unwrap_or(0);
    let max_width = codecs.iter().map(|c| c.width.1).min().unwrap_or(0);
    let min_height = codecs.iter().map(|c| c.height.0).max().unwrap_or(0);
    let max_height = codecs.iter().map(|c| c.height.1).min().unwrap_or(0);
    // Frame rate at the largest common size, from the slowest codec that reports it.
    let macroblocks = u64::from(max_width.div_ceil(16)) * u64::from(max_height.div_ceil(16));
    let fps_max = codecs
        .iter()
        .filter(|c| c.mb_per_sec > 0)
        .map(|c| c.mb_per_sec.checked_div(macroblocks).unwrap_or(0))
        .min()
        .map(|f| f.max(1));

    let mut profiles = Vec::new();
    let mut ten_bit = false;
    for c in codecs {
        for p in c.profiles.iter() {
            profiles.push(p.capability_name());
        }
        ten_bit |= c.ten_bit();
    }
    let cbr_padding = padded_everywhere(device);
    let mut rate_controls = Vec::new();
    if all(|c| c.cbr) {
        rate_controls.push("cbr");
    }
    if cbr_padding {
        rate_controls.push("cbr_padded");
    }
    if all(|c| c.vbr) {
        rate_controls.push("vbr");
    }
    if all(|c| c.quality) {
        rate_controls.push("constant_quality");
    }
    let mut reconfigure = vec![caps::RECONFIGURE_KEYFRAME];
    if all(|c| c.dynamic_bitrate) {
        reconfigure.insert(0, caps::RECONFIGURE_BITRATE);
    }

    let mut set_ = CapabilitySet::new()
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
        );
    if let Some(fps) = fps_max {
        set_ = set_.with(
            caps::VIDEO_FPS_MAX,
            CapabilityValue::Int(i64::try_from(fps).unwrap_or(i64::MAX)),
        );
    }
    set_
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::discrete_gpu;

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
        let set = capability_set(&discrete_gpu());
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
            values(&set, &caps::VIDEO_PROFILES),
            [
                "h264.baseline",
                "h264.high",
                "h264.main",
                "hevc.main",
                "hevc.main10"
            ]
        );
        assert_eq!(
            values(&set, &caps::VIDEO_RATE_CONTROL),
            ["cbr", "cbr_padded", "constant_quality", "vbr"]
        );
        assert_eq!(values(&set, &caps::RECONFIGURE), ["bitrate", "keyframe"]);
        assert_eq!(set.get(&caps::HARDWARE), Some(&CapabilityValue::Flag(true)));
    }

    #[test]
    fn cbr_padding_needs_every_codec() {
        let mut device = discrete_gpu();
        device.codecs[1].cbr_padding = false;
        let set = capability_set(&device);
        assert_eq!(
            values(&set, &caps::VIDEO_RATE_CONTROL),
            ["cbr", "constant_quality", "vbr"]
        );
    }

    #[test]
    fn h264_only_device_has_no_ten_bit() {
        let mut device = discrete_gpu();
        device.codecs.truncate(1);
        device.codecs[0].dynamic_bitrate = false;
        device.codecs[0].quality = false;
        device.codecs[0].mb_per_sec = 0;
        let set = capability_set(&device);
        assert_eq!(values(&set, &caps::VIDEO_BIT_DEPTH), ["8"]);
        assert_eq!(values(&set, &caps::INPUT_PIXEL_FORMAT), ["nv12"]);
        assert_eq!(values(&set, &caps::RECONFIGURE), ["keyframe"]);
        assert_eq!(
            values(&set, &caps::VIDEO_RATE_CONTROL),
            ["cbr", "cbr_padded", "vbr"]
        );
        assert_eq!(set.get(&caps::VIDEO_FPS_MAX), None);
    }

    #[test]
    fn no_codecs_declares_nothing() {
        assert!(capability_set(&DeviceCaps::default()).is_empty());
    }

    #[test]
    fn profile_sets() {
        let p = Profiles::H264_ALL.without(Profile::H264Baseline);
        assert!(p.contains(Profile::H264High) && !p.contains(Profile::H264Baseline));
        assert_eq!(
            Profiles::HEVC_ALL.iter().collect::<Vec<_>>(),
            [Profile::HevcMain, Profile::HevcMain10]
        );
        let mut c = discrete_gpu().codecs[0];
        assert!(c.usable());
        c.profiles = Profiles::NONE;
        assert!(!c.usable());
    }
}
