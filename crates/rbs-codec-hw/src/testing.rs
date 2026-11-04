//! Test devices, shared by the tests of this crate and of the adapters (feature
//! `testing`).
use rbs_core::media::CodecId;

use crate::device::{CodecCaps, DeviceCaps, Profiles};

/// A device like a current discrete GPU: H.264 to 4096 (8 bits), HEVC to 8192
/// with Main 10, every rate-control mode, CBR padding and bitrate changes.
pub fn discrete_gpu() -> DeviceCaps {
    DeviceCaps {
        name: "Test GPU".into(),
        codecs: vec![
            CodecCaps {
                codec: CodecId::H264,
                width: (145, 4096),
                height: (49, 4096),
                mb_per_sec: 4096 / 16 * 4096 / 16 * 60,
                profiles: Profiles::H264_ALL,
                cbr: true,
                vbr: true,
                quality: true,
                dynamic_bitrate: true,
                cbr_padding: true,
            },
            CodecCaps {
                codec: CodecId::Hevc,
                width: (129, 8192),
                height: (33, 8192),
                mb_per_sec: 8192 / 16 * 8192 / 16 * 30,
                profiles: Profiles::HEVC_ALL,
                cbr: true,
                vbr: true,
                quality: true,
                dynamic_bitrate: true,
                cbr_padding: true,
            },
        ],
    }
}
