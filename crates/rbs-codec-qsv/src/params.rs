//! Maps the shared [`Plan`] to oneVPL encoder parameters. Pure (no runtime calls),
//! so the mapping is unit-tested on every Linux and Windows build.

use core::ptr;

use rbs_codec_hw::plan::{Plan, Preset, Profile, Rc};
use rbs_core::media::CodecId;

use crate::sys;

/// Bitrate control fields of `mfxInfoMFX`. oneVPL stores rates in 16-bit kbit/s
/// and buffer sizes in 16-bit KB, all multiplied by `BRCParamMultiplier`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Brc {
    pub method: u16,
    pub multiplier: u16,
    pub initial_delay_kb: u16,
    pub buffer_kb: u16,
    pub target_kbps: u16,
    pub max_kbps: u16,
    pub icq: u16,
}

fn kbps(bps: u32) -> u64 {
    u64::from(bps).div_ceil(1000).max(1)
}

/// The bitrate control for `rc`. CBR and VBR get a one-second buffer at the peak
/// rate, half of it as the initial delay; constant quality is ICQ with
/// `ICQQuality = level`, where level 0 (best) becomes 1 (ICQ's best).
pub(crate) fn brc(rc: Rc) -> Brc {
    let (method, target, max) = match rc {
        Rc::Cbr { bitrate } => (sys::MFX_RATECONTROL_CBR, kbps(bitrate), kbps(bitrate)),
        Rc::Vbr { average, max } => (sys::MFX_RATECONTROL_VBR, kbps(average), kbps(max)),
        Rc::Quality { level } => {
            return Brc {
                method: sys::MFX_RATECONTROL_ICQ as u16,
                multiplier: 1,
                icq: u16::from(level.clamp(1, 51)),
                ..Brc::default()
            };
        }
    };
    // KB per second at the peak (8 kbit per KB).
    let buffer = max.div_ceil(8).max(1);
    let multiplier = max.max(buffer).div_ceil(u64::from(u16::MAX)).max(1);
    let scale = |v: u64| u16::try_from(v.div_ceil(multiplier)).unwrap_or(u16::MAX);
    Brc {
        method: method as u16,
        // A u32 bitrate in kbit/s over 65535 stays below 2^16.
        multiplier: u16::try_from(multiplier).unwrap_or(u16::MAX),
        initial_delay_kb: scale(buffer.div_ceil(2)),
        buffer_kb: scale(buffer),
        target_kbps: scale(target),
        max_kbps: scale(max),
        icq: 0,
    }
}

/// `x` rounded up to a multiple of `to`.
pub(crate) fn align(x: u32, to: u32) -> u32 {
    x.div_ceil(to) * to
}

/// Surface alignment: 16 for H.264, 32 for HEVC (its 32x32 coding blocks).
pub(crate) fn alignment(codec: CodecId) -> u32 {
    if codec == CodecId::Hevc { 32 } else { 16 }
}

/// oneVPL codec id.
pub(crate) fn codec_id(codec: CodecId) -> u32 {
    if codec == CodecId::Hevc {
        sys::MFX_CODEC_HEVC
    } else {
        sys::MFX_CODEC_AVC
    }
}

/// oneVPL profile.
pub(crate) fn profile(p: Profile) -> u16 {
    (match p {
        Profile::H264Baseline => sys::MFX_PROFILE_AVC_CONSTRAINED_BASELINE,
        Profile::H264Main => sys::MFX_PROFILE_AVC_MAIN,
        Profile::H264High => sys::MFX_PROFILE_AVC_HIGH,
        Profile::HevcMain => sys::MFX_PROFILE_HEVC_MAIN,
        Profile::HevcMain10 => sys::MFX_PROFILE_HEVC_MAIN10,
    }) as u16
}

/// Target usage: 1 is best quality, 7 best speed.
pub(crate) fn target_usage(p: Preset) -> u16 {
    match p {
        Preset::Quality => 2,
        Preset::Balanced => sys::MFX_TARGETUSAGE_BALANCED as u16,
        Preset::Speed => sys::MFX_TARGETUSAGE_BEST_SPEED as u16,
    }
}

/// The frame description of `plan`'s input surfaces.
pub(crate) fn frame_info(plan: &Plan) -> sys::mfxFrameInfo {
    let a = alignment(plan.codec);
    let (fourcc, depth, shift) = if plan.ten_bit {
        // P010 keeps the 10 bits in the high bits of each 16-bit sample.
        (sys::MFX_FOURCC_P010, 10, 1)
    } else {
        (sys::MFX_FOURCC_NV12, 0, 0)
    };
    let side = |v: u32| u16::try_from(v).unwrap_or(u16::MAX);
    sys::mfxFrameInfo {
        BitDepthLuma: depth,
        BitDepthChroma: depth,
        Shift: shift,
        FourCC: fourcc,
        __bindgen_anon_1: sys::mfxFrameInfo__bindgen_ty_1 {
            __bindgen_anon_1: sys::mfxFrameInfo__bindgen_ty_1__bindgen_ty_1 {
                Width: side(align(plan.width, a)),
                Height: side(align(plan.height, a)),
                CropX: 0,
                CropY: 0,
                CropW: side(plan.width),
                CropH: side(plan.height),
            },
        },
        FrameRateExtN: plan.fps.0,
        FrameRateExtD: plan.fps.1,
        AspectRatioW: 1,
        AspectRatioH: 1,
        PicStruct: sys::MFX_PICSTRUCT_PROGRESSIVE as u16,
        ChromaFormat: sys::MFX_CHROMAFORMAT_YUV420 as u16,
        ..Default::default()
    }
}

/// The encoder parameters of `plan` with `async_depth` frames in flight and no
/// extension buffers.
pub(crate) fn video_param(plan: &Plan, async_depth: u16) -> sys::mfxVideoParam {
    let b = brc(plan.rc);
    let encode = sys::mfxInfoMFX__bindgen_ty_1__bindgen_ty_1 {
        TargetUsage: target_usage(plan.preset),
        // GOP lengths above 65535 frames are capped (an IDR at least every ~18
        // minutes at 60 fps).
        GopPicSize: u16::try_from(plan.gop).unwrap_or(u16::MAX),
        // P frames only: no reordering, one output per input in order.
        GopRefDist: 1,
        GopOptFlag: (sys::MFX_GOP_CLOSED | sys::MFX_GOP_STRICT) as u16,
        // Every I frame is an IDR: 0 for H.264, 1 for HEVC (oneVPL counts the
        // two differently).
        IdrInterval: u16::from(plan.codec == CodecId::Hevc),
        RateControlMethod: b.method,
        __bindgen_anon_1: sys::mfxInfoMFX__bindgen_ty_1__bindgen_ty_1__bindgen_ty_1 {
            InitialDelayInKB: b.initial_delay_kb,
        },
        BufferSizeInKB: b.buffer_kb,
        __bindgen_anon_2: if b.method == sys::MFX_RATECONTROL_ICQ as u16 {
            sys::mfxInfoMFX__bindgen_ty_1__bindgen_ty_1__bindgen_ty_2 { ICQQuality: b.icq }
        } else {
            sys::mfxInfoMFX__bindgen_ty_1__bindgen_ty_1__bindgen_ty_2 {
                TargetKbps: b.target_kbps,
            }
        },
        __bindgen_anon_3: sys::mfxInfoMFX__bindgen_ty_1__bindgen_ty_1__bindgen_ty_3 {
            MaxKbps: b.max_kbps,
        },
        NumSlice: 1,
        NumRefFrame: 1,
        EncodedOrder: 0,
    };
    let mfx = sys::mfxInfoMFX {
        BRCParamMultiplier: b.multiplier,
        FrameInfo: frame_info(plan),
        CodecId: codec_id(plan.codec),
        CodecProfile: profile(plan.profile),
        __bindgen_anon_1: sys::mfxInfoMFX__bindgen_ty_1 {
            __bindgen_anon_1: encode,
        },
        ..Default::default()
    };
    sys::mfxVideoParam {
        AsyncDepth: async_depth,
        __bindgen_anon_1: sys::mfxVideoParam__bindgen_ty_1 { mfx },
        IOPattern: sys::MFX_IOPATTERN_IN_SYSTEM_MEMORY as u16,
        ExtParam: ptr::null_mut(),
        NumExtParam: 0,
        ..Default::default()
    }
}

fn header<T>(id: u32) -> sys::mfxExtBuffer {
    sys::mfxExtBuffer {
        BufferId: id,
        BufferSz: u32::try_from(size_of::<T>()).unwrap_or(0),
    }
}

/// Encoder parameters with their extension buffers (VUI colour description and the
/// reset option), boxed so the pointers between them stay valid.
pub(crate) struct Params {
    pub param: sys::mfxVideoParam,
    signal: sys::mfxExtVideoSignalInfo,
    reset: sys::mfxExtEncoderResetOption,
    ext: [*mut sys::mfxExtBuffer; 2],
}

impl Params {
    /// The parameters of `plan` with the VUI colour description.
    pub(crate) fn new(plan: &Plan, async_depth: u16) -> Box<Params> {
        let signal = sys::mfxExtVideoSignalInfo {
            Header: header::<sys::mfxExtVideoSignalInfo>(sys::MFX_EXTBUFF_VIDEO_SIGNAL_INFO),
            // 5: unspecified video format.
            VideoFormat: 5,
            VideoFullRange: u16::from(plan.color.full_range),
            ColourDescriptionPresent: 1,
            ColourPrimaries: u16::from(plan.color.primaries),
            TransferCharacteristics: u16::from(plan.color.transfer),
            MatrixCoefficients: u16::from(plan.color.matrix),
        };
        let reset = sys::mfxExtEncoderResetOption {
            Header: header::<sys::mfxExtEncoderResetOption>(sys::MFX_EXTBUFF_ENCODER_RESET_OPTION),
            StartNewSequence: sys::MFX_CODINGOPTION_OFF as u16,
            ..Default::default()
        };
        let mut params = Box::new(Params {
            param: video_param(plan, async_depth),
            signal,
            reset,
            ext: [ptr::null_mut(); 2],
        });
        params.link(false);
        params
    }

    /// Points `param` at the extension buffers of this (boxed, so not moving)
    /// value; with `reset`, also at the reset option (only valid for `Reset`).
    pub(crate) fn link(&mut self, reset: bool) {
        self.ext[0] = (&raw mut self.signal).cast();
        self.ext[1] = (&raw mut self.reset).cast();
        self.param.ExtParam = self.ext.as_mut_ptr();
        self.param.NumExtParam = if reset { 2 } else { 1 };
    }

    /// A copy of these parameters with the rate control `rc`, linked for `Reset`.
    pub(crate) fn with_rc(&self, rc: Rc) -> Box<Params> {
        let mut next = Box::new(Params {
            param: self.param,
            signal: self.signal,
            reset: self.reset,
            ext: [ptr::null_mut(); 2],
        });
        let b = brc(rc);
        // SAFETY: `param` was built by `video_param` for encoding, so `mfx` and its
        // encoder member are the initialised union members.
        let mfx = unsafe { &mut next.param.__bindgen_anon_1.mfx };
        mfx.BRCParamMultiplier = b.multiplier;
        // SAFETY: as above.
        let encode = unsafe { &mut mfx.__bindgen_anon_1.__bindgen_anon_1 };
        encode.RateControlMethod = b.method;
        encode.__bindgen_anon_1.InitialDelayInKB = b.initial_delay_kb;
        encode.BufferSizeInKB = b.buffer_kb;
        encode.__bindgen_anon_2.TargetKbps = b.target_kbps;
        encode.__bindgen_anon_3.MaxKbps = b.max_kbps;
        next.link(true);
        next
    }
}

/// Splits parameter-set buffers into Annex B: each must start with a start code,
/// which is added when it is missing. Empty buffers are skipped.
pub(crate) fn annex_b(sets: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(sets.iter().map(|s| s.len() + 4).sum());
    for set in sets.iter().filter(|s| !s.is_empty()) {
        if !(set.starts_with(&[0, 0, 1]) || set.starts_with(&[0, 0, 0, 1])) {
            out.extend_from_slice(&[0, 0, 0, 1]);
        }
        out.extend_from_slice(set);
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
mod tests {
    use super::*;
    use rbs_codec_hw::plan::Preset;
    use rbs_core::media::ColorSpace;
    use rbs_media_format::h273::ColorCodes;

    fn plan(codec: CodecId, rc: Rc) -> Plan {
        Plan {
            codec,
            width: 1918,
            height: 1080,
            ten_bit: codec == CodecId::Hevc,
            fps: (60000, 1001),
            preset: Preset::Balanced,
            low_latency: false,
            profile: if codec == CodecId::Hevc {
                Profile::HevcMain10
            } else {
                Profile::H264High
            },
            rc,
            gop: 120,
            color: ColorCodes::of(&ColorSpace::BT709_LIMITED).unwrap(),
        }
    }

    #[test]
    fn bitrates_fit_sixteen_bits() {
        let b = brc(Rc::Cbr { bitrate: 6_000_000 });
        assert_eq!(
            (
                b.multiplier,
                b.target_kbps,
                b.max_kbps,
                b.buffer_kb,
                b.initial_delay_kb
            ),
            (1, 6000, 6000, 750, 375)
        );
        let b = brc(Rc::Vbr {
            average: 100_000_000,
            max: 200_000_000,
        });
        assert_eq!(b.multiplier, 4);
        assert_eq!((b.target_kbps, b.max_kbps), (25_000, 50_000));
        assert!(u32::from(b.max_kbps) * u32::from(b.multiplier) >= 200_000);
        let b = brc(Rc::Cbr { bitrate: u32::MAX });
        assert!(u64::from(b.max_kbps) * u64::from(b.multiplier) >= u64::from(u32::MAX) / 1000);
        let b = brc(Rc::Quality { level: 0 });
        assert_eq!((b.method, b.icq), (sys::MFX_RATECONTROL_ICQ as u16, 1));
        assert_eq!(brc(Rc::Quality { level: 51 }).icq, 51);
    }

    #[test]
    fn hevc_main10_parameters() {
        let p = plan(CodecId::Hevc, Rc::Cbr { bitrate: 6_000_000 });
        let v = video_param(&p, 4);
        assert_eq!(v.AsyncDepth, 4);
        // SAFETY: built for encoding above.
        let mfx = unsafe { v.__bindgen_anon_1.mfx };
        assert_eq!(mfx.CodecId, sys::MFX_CODEC_HEVC);
        assert_eq!(mfx.CodecProfile, sys::MFX_PROFILE_HEVC_MAIN10 as u16);
        assert_eq!(mfx.FrameInfo.FourCC, sys::MFX_FOURCC_P010);
        assert_eq!(mfx.FrameInfo.Shift, 1);
        // SAFETY: as above.
        let size = unsafe { mfx.FrameInfo.__bindgen_anon_1.__bindgen_anon_1 };
        assert_eq!(
            (size.Width, size.Height, size.CropW, size.CropH),
            (1920, 1088, 1918, 1080)
        );
        // SAFETY: as above.
        let e = unsafe { mfx.__bindgen_anon_1.__bindgen_anon_1 };
        assert_eq!((e.GopPicSize, e.GopRefDist, e.IdrInterval), (120, 1, 1));
        assert_eq!(
            (mfx.FrameInfo.FrameRateExtN, mfx.FrameInfo.FrameRateExtD),
            (60000, 1001)
        );
    }

    #[test]
    fn rate_change_keeps_the_rest() {
        let p = plan(CodecId::H264, Rc::Cbr { bitrate: 6_000_000 });
        let params = Params::new(&p, 4);
        assert_eq!(params.param.NumExtParam, 1);
        let next = params.with_rc(Rc::Cbr { bitrate: 3_000_000 });
        assert_eq!(next.param.NumExtParam, 2);
        assert_eq!(next.param.ExtParam, next.ext.as_ptr().cast_mut());
        // SAFETY: built for encoding.
        let (a, b) = unsafe {
            (
                params
                    .param
                    .__bindgen_anon_1
                    .mfx
                    .__bindgen_anon_1
                    .__bindgen_anon_1,
                next.param
                    .__bindgen_anon_1
                    .mfx
                    .__bindgen_anon_1
                    .__bindgen_anon_1,
            )
        };
        // SAFETY: CBR fills `TargetKbps`.
        let rates = unsafe { (a.__bindgen_anon_2.TargetKbps, b.__bindgen_anon_2.TargetKbps) };
        assert_eq!(rates, (6000, 3000));
        assert_eq!(a.GopPicSize, b.GopPicSize);
        assert_eq!(a.IdrInterval, 0);
    }

    #[test]
    fn parameter_sets_become_annex_b() {
        assert_eq!(
            annex_b(&[&[0, 0, 0, 1, 0x67, 1], &[0x68, 2], &[]]),
            [0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 2]
        );
    }
}
