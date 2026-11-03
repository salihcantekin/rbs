//! macOS implementation: the factory, probing and the session.
//!
//! This is the adapter's low-level module (principle P11): every `unsafe` block states
//! its invariant.

mod cf;
mod session;

use std::sync::OnceLock;

use objc2_core_foundation::{CFArray, CFBoolean, CFDictionary, CFNumber, CFRetained, CFString};
use objc2_core_media::{kCMVideoCodecType_H264, kCMVideoCodecType_HEVC};
use objc2_video_toolbox::{
    VTCopyVideoEncoderList, kVTVideoEncoderList_CodecType,
    kVTVideoEncoderList_IsHardwareAccelerated,
};
use rbs_codec::{CreateError, VideoEncoder, VideoEncoderConfig, VideoEncoderFactory, caps};
use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::media::CodecId;
use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};

pub use session::VideoToolboxSession;

/// Smallest and largest picture side the adapter declares. Apple's encoders take
/// larger pictures on some machines; 4096 is what every Apple silicon media engine
/// supports for both codecs.
const MIN_SIDE: i64 = 64;
const MAX_SIDE: i64 = 4096;

/// One encoder from `VTCopyVideoEncoderList`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EncoderInfo {
    codec: CodecId,
    hardware: bool,
}

/// Factory of the VideoToolbox adapter. Cheap to create: it does nothing until
/// probed or asked for a session.
#[derive(Debug, Default)]
pub struct VideoToolboxFactory {
    software: bool,
    /// The system's encoder list, read once per process: it does not change while the
    /// process runs, and the first read loads encoder plug-ins (too slow to repeat
    /// within the probe budget).
    encoders: OnceLock<Vec<EncoderInfo>>,
}

impl VideoToolboxFactory {
    /// The hardware-only adapter (`org.rbs.codec.videotoolbox`).
    pub fn new() -> Self {
        Self::default()
    }

    /// The adapter that also offers Apple's software encoders
    /// (`org.rbs.codec.videotoolbox-software`).
    pub fn with_software() -> Self {
        Self {
            software: true,
            encoders: OnceLock::new(),
        }
    }

    /// Creates a session with its concrete type, for callers that want its counters.
    pub fn create_session(
        &self,
        config: &VideoEncoderConfig,
    ) -> Result<VideoToolboxSession, CreateError> {
        if !self.codecs().contains(&config.codec) {
            return Err(CreateError::Unsupported {
                what: format!(
                    "no {} VideoToolbox encoder for {}",
                    if self.software { "" } else { "hardware" },
                    config.codec
                ),
            });
        }
        VideoToolboxSession::new(config, !self.software)
    }

    fn encoders(&self) -> &[EncoderInfo] {
        self.encoders.get_or_init(read_encoder_list)
    }

    /// Codecs this adapter offers, in a fixed order.
    fn codecs(&self) -> Vec<CodecId> {
        let list = self.encoders();
        [CodecId::H264, CodecId::Hevc]
            .into_iter()
            .filter(|c| {
                list.iter()
                    .any(|e| e.codec == *c && (self.software || e.hardware))
            })
            .collect()
    }
}

fn read_encoder_list() -> Vec<EncoderInfo> {
    let mut raw: *const CFArray = std::ptr::null();
    // SAFETY: `raw` is a valid out pointer; on success it receives a +1 reference to a
    // CFArray (the "Copy" rule), which `from_raw` adopts below.
    let status = unsafe { VTCopyVideoEncoderList(None, std::ptr::NonNull::from(&mut raw)) };
    let Some(raw) = std::ptr::NonNull::new(raw.cast_mut()) else {
        return Vec::new();
    };
    // SAFETY: a non-null result of a Copy function is an owned reference to a CFArray.
    let list: CFRetained<CFArray> = unsafe { CFRetained::from_raw(raw) };
    if status != 0 {
        return Vec::new();
    }
    // SAFETY: the encoder list is an array of dictionaries with CFString keys.
    let list: &CFArray<CFDictionary<CFString, objc2_core_foundation::CFType>> =
        unsafe { list.cast_unchecked() };
    let mut out = Vec::new();
    for entry in list.iter() {
        // SAFETY: the keys are CoreFoundation constants that live for the process.
        let (codec_key, hw_key) = unsafe {
            (
                kVTVideoEncoderList_CodecType,
                kVTVideoEncoderList_IsHardwareAccelerated,
            )
        };
        let Some(codec) = entry
            .get(codec_key)
            .and_then(|v| v.downcast_ref::<CFNumber>().and_then(|n| n.as_i64()))
        else {
            continue;
        };
        let codec = match u32::try_from(codec) {
            Ok(c) if c == kCMVideoCodecType_H264 => CodecId::H264,
            Ok(c) if c == kCMVideoCodecType_HEVC => CodecId::Hevc,
            _ => continue,
        };
        let hardware = entry
            .get(hw_key)
            .and_then(|v| v.downcast_ref::<CFBoolean>().map(CFBoolean::as_bool))
            .unwrap_or(false);
        out.push(EncoderInfo { codec, hardware });
    }
    out
}

fn set(values: &[&str]) -> CapabilityValue {
    CapabilityValue::Set(values.iter().map(|v| (*v).to_owned()).collect())
}

impl AdapterFactory for VideoToolboxFactory {
    fn id(&self) -> AdapterId {
        AdapterId::new(if self.software {
            crate::SOFTWARE_ADAPTER_ID
        } else {
            crate::ADAPTER_ID
        })
    }

    fn display_name(&self) -> &str {
        if self.software {
            "VideoToolbox (hardware or software)"
        } else {
            "VideoToolbox (hardware)"
        }
    }

    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        let codecs = self.codecs();
        if codecs.is_empty() {
            return Ok(CapabilitySet::new());
        }
        let hevc = codecs.contains(&CodecId::Hevc);
        let names: Vec<&str> = codecs.iter().map(|c| c.as_str()).collect();
        let mut profiles = Vec::new();
        if codecs.contains(&CodecId::H264) {
            profiles.extend(["h264.baseline", "h264.main", "h264.high"]);
        }
        if hevc {
            profiles.extend(["hevc.main", "hevc.main10"]);
        }
        let mut rate_controls = vec!["cbr", "vbr"];
        if !self.software {
            // Constant quality (`kVTCompressionPropertyKey_Quality` without a bitrate) is
            // an Apple silicon media engine feature.
            rate_controls.push("constant_quality");
        }
        let side = CapabilityValue::IntRange {
            min: MIN_SIDE,
            max: MAX_SIDE,
        };
        Ok(CapabilitySet::new()
            .with(caps::HARDWARE, CapabilityValue::Flag(!self.software))
            .with(caps::VIDEO_ENCODE, set(&names))
            .with(caps::VIDEO_PROFILES, set(&profiles))
            .with(
                caps::VIDEO_BIT_DEPTH,
                set(if hevc { &["8", "10"] } else { &["8"] }),
            )
            .with(caps::VIDEO_WIDTH, side.clone())
            .with(caps::VIDEO_HEIGHT, side)
            .with(caps::VIDEO_FPS_MAX, CapabilityValue::Int(60))
            .with(caps::VIDEO_RATE_CONTROL, set(&rate_controls))
            // Frame reordering is left off until its depth is measured on hardware.
            .with(caps::VIDEO_B_FRAMES_MAX, CapabilityValue::Int(0))
            .with(
                caps::RECONFIGURE,
                set(&[caps::RECONFIGURE_BITRATE, caps::RECONFIGURE_KEYFRAME]),
            )
            .with(caps::INPUT_MEMORY, set(&["metal", "system"]))
            .with(
                caps::INPUT_PIXEL_FORMAT,
                set(if hevc { &["nv12", "p010"] } else { &["nv12"] }),
            ))
    }
}

impl VideoEncoderFactory for VideoToolboxFactory {
    fn create(&self, config: &VideoEncoderConfig) -> Result<Box<dyn VideoEncoder>, CreateError> {
        Ok(Box::new(self.create_session(config)?))
    }
}
