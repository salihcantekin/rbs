//! The VA-API engine: one VA display on a DRM render node, one encode context,
//! and per slot an input surface (with an image to copy into) and a coded buffer.
//! Reconstructed pictures live in `slots + 1` further surfaces, so the reference of
//! a picture in flight is never overwritten.
//!
//! The engine writes the parameter sets itself (`crate::h264`, `crate::hevc`) and
//! hands them to the driver as a packed sequence header on every IDR picture; it
//! also writes the slice headers when the driver takes packed slice headers (else
//! the driver writes them from the slice parameters). Every picture is a P picture
//! referencing the previous one (for HEVC drivers that need it, a B picture whose
//! two lists hold that picture), or an IDR picture.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::OnceLock;
use std::time::Duration;

use core::ffi::{CStr, c_int, c_uint, c_void};

use rbs_codec::CreateError;
use rbs_codec_hw::copy::{DstPlane, copy_420};
use rbs_codec_hw::device::{CodecCaps, DeviceCaps, Profiles};
use rbs_codec_hw::plan::{Plan, Profile, Rc};
use rbs_codec_hw::session::{Engine, EngineError, Output};
use rbs_core::media::{CodecId, SystemFrame};

use crate::FRAMES_IN_FLIGHT;
use crate::h264::{self, LOG2_MAX_FRAME_NUM_MINUS4};
use crate::hevc::{self, Blocks, Features, Support, Tools};
use crate::loader::{Api, api};
use crate::sys;

/// Longest wait for one picture. A picture is waited for only when it was
/// submitted `FRAMES_IN_FLIGHT` frames earlier, at `flush` and in `Drop`.
const WAIT: Duration = Duration::from_secs(10);

/// Smallest picture side declared (VA drivers do not report a minimum).
const MIN_SIDE: u32 = 32;

/// Largest picture side declared when the driver does not report one.
const DEFAULT_MAX_SIDE: u32 = 4096;

/// The render node the probe chose, used by every session.
static NODE: OnceLock<PathBuf> = OnceLock::new();

fn ok(status: sys::VAStatus) -> bool {
    status == sys::VA_STATUS_SUCCESS as sys::VAStatus
}

/// An initialised VA display on a render node. Dropping terminates it and closes
/// the node.
struct Display {
    api: &'static Api,
    dpy: sys::VADisplay,
    // The DRM file descriptor must outlive the display.
    _node: File,
}

impl Display {
    fn open(api: &'static Api, path: &Path) -> Result<Self, String> {
        let node = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let fd = std::os::fd::AsRawFd::as_raw_fd(&node);
        // SAFETY: an open DRM file descriptor that outlives the display.
        let dpy = unsafe { (api.get_display_drm)(fd) };
        if dpy.is_null() {
            return Err(format!("{}: no VA display", path.display()));
        }
        // SAFETY: a display from `vaGetDisplayDRM`; a null callback turns libva's
        // informational messages on stderr off (errors still go there).
        unsafe {
            (api.set_info_callback)(dpy, None, ptr::null_mut());
        }
        let (mut major, mut minor) = (0, 0);
        // SAFETY: as above, with valid out-pointers.
        let status = unsafe { (api.initialize)(dpy, &mut major, &mut minor) };
        if !ok(status) {
            // SAFETY: `vaTerminate` also releases a display whose initialisation
            // failed.
            unsafe { (api.terminate)(dpy) };
            return Err(format!(
                "{}: vaInitialize: {}",
                path.display(),
                api.error(status)
            ));
        }
        Ok(Display {
            api,
            dpy,
            _node: node,
        })
    }

    /// The driver's vendor string.
    fn vendor(&self) -> String {
        // SAFETY: an initialised display; the string is owned by the driver.
        let s = unsafe { (self.api.query_vendor_string)(self.dpy) };
        if s.is_null() {
            return "VA-API".into();
        }
        // SAFETY: a NUL-terminated string that lives as long as the display.
        unsafe { CStr::from_ptr(s) }.to_string_lossy().into_owned()
    }

    fn profiles(&self) -> Vec<sys::VAProfile> {
        // SAFETY: an initialised display.
        let max = unsafe { (self.api.max_num_profiles)(self.dpy) }.clamp(0, 256);
        let mut list = vec![0 as sys::VAProfile; max as usize];
        let mut n: c_int = 0;
        // SAFETY: a list of `max` entries, the most the driver returns.
        let status =
            unsafe { (self.api.query_config_profiles)(self.dpy, list.as_mut_ptr(), &mut n) };
        if !ok(status) {
            return Vec::new();
        }
        list.truncate(n.clamp(0, max) as usize);
        list
    }

    fn entrypoints(&self, profile: sys::VAProfile) -> Vec<sys::VAEntrypoint> {
        // SAFETY: an initialised display.
        let max = unsafe { (self.api.max_num_entrypoints)(self.dpy) }.clamp(0, 256);
        let mut list = vec![0 as sys::VAEntrypoint; max as usize];
        let mut n: c_int = 0;
        // SAFETY: a list of `max` entries, the most the driver returns.
        let status = unsafe {
            (self.api.query_config_entrypoints)(self.dpy, profile, list.as_mut_ptr(), &mut n)
        };
        if !ok(status) {
            return Vec::new();
        }
        list.truncate(n.clamp(0, max) as usize);
        list
    }

    /// The driver's image format of `fourcc`, if it lists one.
    fn image_format(&self, fourcc: u32) -> Option<sys::VAImageFormat> {
        // SAFETY: an initialised display.
        let max = unsafe { (self.api.max_num_image_formats)(self.dpy) }.clamp(0, 1024);
        let mut list = vec![sys::VAImageFormat::default(); max as usize];
        let mut n: c_int = 0;
        // SAFETY: a list of `max` entries, the most the driver returns.
        let status = unsafe { (self.api.query_image_formats)(self.dpy, list.as_mut_ptr(), &mut n) };
        if !ok(status) {
            return None;
        }
        list.truncate(n.clamp(0, max) as usize);
        list.into_iter().find(|f| f.fourcc == fourcc)
    }

    /// The values of `types` for `profile` and `entrypoint`
    /// (`VA_ATTRIB_NOT_SUPPORTED` for an attribute the driver lacks).
    fn attributes<const N: usize>(
        &self,
        profile: sys::VAProfile,
        entrypoint: sys::VAEntrypoint,
        types: [sys::VAConfigAttribType; N],
    ) -> Option<[u32; N]> {
        let mut attribs = types.map(|type_| sys::VAConfigAttrib { type_, value: 0 });
        // SAFETY: an initialised display and `N` attributes.
        let status = unsafe {
            (self.api.get_config_attributes)(
                self.dpy,
                profile,
                entrypoint,
                attribs.as_mut_ptr(),
                N as c_int,
            )
        };
        ok(status).then(|| attribs.map(|a| a.value))
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        // SAFETY: an initialised display whose objects were destroyed before.
        unsafe { (self.api.terminate)(self.dpy) };
    }
}

/// How the driver encodes one profile.
#[derive(Debug, Clone, Copy)]
struct EncodeConfig {
    entrypoint: sys::VAEntrypoint,
    /// `VA_RT_FORMAT_*` of the input.
    rt_format: u32,
    /// `VA_RC_*` bits.
    rate_controls: u32,
    /// `VA_ENC_PACKED_HEADER_*` bits.
    packed_headers: u32,
    max_width: u32,
    max_height: u32,
    /// HEVC: `VAConfigAttribEncHEVCFeatures`, else `VA_ATTRIB_NOT_SUPPORTED`.
    hevc_features: u32,
    /// HEVC: `VAConfigAttribEncHEVCBlockSizes`, else `VA_ATTRIB_NOT_SUPPORTED`.
    hevc_block_sizes: u32,
    /// P pictures must be B pictures with both lists holding the reference
    /// (`VA_PREDICTION_DIRECTION_BI_NOT_EMPTY` with references in both lists).
    gpb: bool,
}

fn supported(v: u32) -> bool {
    v != sys::VA_ATTRIB_NOT_SUPPORTED
}

/// The HEVC coding tools of a driver's features attribute, or FFmpeg's defaults
/// when it reports none; `None` when it requires a tool the adapter does not write.
fn hevc_tools(features: u32, constant_qp: bool) -> Option<Tools> {
    if !supported(features) {
        return Some(Tools::default_for(constant_qp));
    }
    let f = sys::VAConfigAttribValEncHEVCFeatures { value: features };
    // SAFETY: both views of the union are the same plain 32-bit integer.
    let b = unsafe { f.bits };
    let s = |v: u32| match v {
        sys::VA_FEATURE_NOT_SUPPORTED => Support::No,
        sys::VA_FEATURE_REQUIRED => Support::Required,
        _ => Support::Yes,
    };
    Tools::from_features(
        &Features {
            separate_colour_planes: s(b.separate_colour_planes()),
            scaling_lists: s(b.scaling_lists()),
            amp: s(b.amp()),
            sao: s(b.sao()),
            pcm: s(b.pcm()),
            temporal_mvp: s(b.temporal_mvp()),
            strong_intra_smoothing: s(b.strong_intra_smoothing()),
            dependent_slices: s(b.dependent_slices()),
            sign_data_hiding: s(b.sign_data_hiding()),
            constrained_intra_pred: s(b.constrained_intra_pred()),
            transform_skip: s(b.transform_skip()),
            cu_qp_delta: s(b.cu_qp_delta()),
            weighted_prediction: s(b.weighted_prediction()),
            transquant_bypass: s(b.transquant_bypass()),
            deblocking_filter_disable: s(b.deblocking_filter_disable()),
        },
        constant_qp,
    )
}

/// The HEVC block sizes of a driver's attribute, or FFmpeg's defaults when it
/// reports none or sizes outside Main / Main 10.
fn hevc_blocks(block_sizes: u32) -> Blocks {
    if !supported(block_sizes) {
        return Blocks::DEFAULT;
    }
    let a = sys::VAConfigAttribValEncHEVCBlockSizes { value: block_sizes };
    // SAFETY: both views of the union are the same plain 32-bit integer.
    let b = unsafe { a.bits };
    // Every field is at most 4 bits wide.
    let f = |v: u32| (v & 0xf) as u8;
    Blocks::from_attribute(
        f(b.log2_max_coding_tree_block_size_minus3()),
        f(b.log2_min_luma_coding_block_size_minus3()),
        f(b.log2_min_luma_transform_block_size_minus2()),
        f(b.log2_max_luma_transform_block_size_minus2()),
        f(b.max_max_transform_hierarchy_depth_inter()),
        f(b.max_max_transform_hierarchy_depth_intra()),
    )
    .unwrap_or(Blocks::DEFAULT)
}

const fn va_profile(profile: Profile) -> sys::VAProfile {
    match profile {
        Profile::H264Baseline => sys::VAProfileH264ConstrainedBaseline,
        Profile::H264Main => sys::VAProfileH264Main,
        Profile::H264High => sys::VAProfileH264High,
        Profile::HevcMain => sys::VAProfileHEVCMain,
        Profile::HevcMain10 => sys::VAProfileHEVCMain10,
    }
}

/// How `display` encodes `profile`, or `None` when it cannot do so the way this
/// engine needs: 4:2:0 input (10-bit for Main 10), at least one reference, P (or
/// generalised B) prediction from the previous picture, a packed sequence header,
/// some rate control and, for HEVC, no coding tool required that the adapter does
/// not write. The full-power entry point is preferred over low power.
fn encode_config(display: &Display, profile: Profile) -> Option<EncodeConfig> {
    let hevc = matches!(profile, Profile::HevcMain | Profile::HevcMain10);
    let rt_format = if profile == Profile::HevcMain10 {
        sys::VA_RT_FORMAT_YUV420_10
    } else {
        sys::VA_RT_FORMAT_YUV420
    };
    // P010 input needs a P010 image to copy into (a derived image is not known
    // before a surface exists, so the driver must list the format).
    if profile == Profile::HevcMain10 && display.image_format(sys::VA_FOURCC_P010).is_none() {
        return None;
    }
    let profile = va_profile(profile);
    if !display.profiles().contains(&profile) {
        return None;
    }
    let entrypoints = display.entrypoints(profile);
    [sys::VAEntrypointEncSlice, sys::VAEntrypointEncSliceLP]
        .into_iter()
        .filter(|e| entrypoints.contains(e))
        .find_map(|entrypoint| {
            let [rt, rc, packed, refs, width, height, prediction] = display.attributes(
                profile,
                entrypoint,
                [
                    sys::VAConfigAttribRTFormat,
                    sys::VAConfigAttribRateControl,
                    sys::VAConfigAttribEncPackedHeaders,
                    sys::VAConfigAttribEncMaxRefFrames,
                    sys::VAConfigAttribMaxPictureWidth,
                    sys::VAConfigAttribMaxPictureHeight,
                    sys::VAConfigAttribPredictionDirection,
                ],
            )?;
            let [hevc_features, hevc_block_sizes] = if hevc {
                display.attributes(
                    profile,
                    entrypoint,
                    [
                        sys::VAConfigAttribEncHEVCFeatures,
                        sys::VAConfigAttribEncHEVCBlockSizes,
                    ],
                )?
            } else {
                [sys::VA_ATTRIB_NOT_SUPPORTED; 2]
            };
            let (l0, l1) = if supported(refs) {
                (refs & 0xffff, refs >> 16 & 0xffff)
            } else {
                (1, 0)
            };
            // Without the attribute the driver has no prediction constraints.
            let previous =
                !supported(prediction) || prediction & sys::VA_PREDICTION_DIRECTION_PREVIOUS != 0;
            let bi_not_empty = supported(prediction)
                && prediction & sys::VA_PREDICTION_DIRECTION_BI_NOT_EMPTY != 0;
            // H.264 pictures stay P pictures (FFmpeg converts HEVC only).
            let gpb = hevc && bi_not_empty;
            let usable = supported(rt)
                && rt & rt_format != 0
                && supported(packed)
                && packed & sys::VA_ENC_PACKED_HEADER_SEQUENCE != 0
                && supported(rc)
                && rc & (sys::VA_RC_CBR | sys::VA_RC_VBR | sys::VA_RC_CQP) != 0
                && l0 >= 1
                && previous
                && (!gpb || l1 >= 1)
                && (!hevc || hevc_tools(hevc_features, false).is_some());
            let side = |v: u32| {
                if supported(v) && v > 0 {
                    v
                } else {
                    DEFAULT_MAX_SIDE
                }
            };
            usable.then_some(EncodeConfig {
                entrypoint,
                rt_format,
                rate_controls: rc,
                packed_headers: packed,
                max_width: side(width),
                max_height: side(height),
                hevc_features,
                hevc_block_sizes,
                gpb,
            })
        })
}

/// What the `codec` encoder of `display` can do, or `None` when it is absent.
fn codec_caps(display: &Display, codec: CodecId) -> Option<CodecCaps> {
    let candidates: &[Profile] = match codec {
        CodecId::H264 => &[Profile::H264Baseline, Profile::H264Main, Profile::H264High],
        CodecId::Hevc => &[Profile::HevcMain, Profile::HevcMain10],
        _ => &[],
    };
    // What every declared profile can do: the common rate controls and sizes.
    let mut profiles = Profiles::NONE;
    let mut common: Option<EncodeConfig> = None;
    for &profile in candidates {
        if let Some(c) = encode_config(display, profile) {
            profiles = profiles.with(profile);
            common = Some(match common {
                None => c,
                Some(a) => EncodeConfig {
                    rate_controls: a.rate_controls & c.rate_controls,
                    max_width: a.max_width.min(c.max_width),
                    max_height: a.max_height.min(c.max_height),
                    ..a
                },
            });
        }
    }
    let c = common?;
    let cbr = c.rate_controls & sys::VA_RC_CBR != 0;
    let vbr = c.rate_controls & sys::VA_RC_VBR != 0;
    let caps = CodecCaps {
        codec,
        width: (MIN_SIDE, c.max_width & !1),
        height: (MIN_SIDE, c.max_height & !1),
        mb_per_sec: 0,
        profiles,
        cbr,
        vbr,
        quality: c.rate_controls & sys::VA_RC_CQP != 0,
        // A new rate-control parameter buffer is taken with any picture.
        dynamic_bitrate: cbr || vbr,
        // The adapter writes its own headers and writes no filler NAL units yet.
        cbr_padding: false,
    };
    caps.usable().then_some(caps)
}

/// The DRM render nodes, in order.
fn render_nodes() -> Vec<PathBuf> {
    let Ok(dir) = std::fs::read_dir("/dev/dri") else {
        return Vec::new();
    };
    let mut nodes: Vec<PathBuf> = dir
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("renderD"))
        })
        .collect();
    nodes.sort();
    nodes
}

/// Reads what the first render node with a VA-API H.264 or HEVC encoder does. `Ok`
/// with no codecs when libva is missing or no node has an encoder.
pub(crate) fn probe() -> Result<DeviceCaps, String> {
    let Ok(api) = api() else {
        return Ok(DeviceCaps::default());
    };
    for node in render_nodes() {
        let Ok(display) = Display::open(api, &node) else {
            continue;
        };
        let codecs: Vec<CodecCaps> = [CodecId::H264, CodecId::Hevc]
            .into_iter()
            .filter_map(|c| codec_caps(&display, c))
            .collect();
        if !codecs.is_empty() {
            let name = format!("{} ({})", display.vendor(), node.display());
            let _ = NODE.set(node);
            return Ok(DeviceCaps { name, codecs });
        }
    }
    Ok(DeviceCaps::default())
}

fn create_error(api: &Api, call: &str, status: sys::VAStatus) -> CreateError {
    let unsupported = [
        sys::VA_STATUS_ERROR_UNSUPPORTED_PROFILE,
        sys::VA_STATUS_ERROR_UNSUPPORTED_ENTRYPOINT,
        sys::VA_STATUS_ERROR_UNSUPPORTED_RT_FORMAT,
        sys::VA_STATUS_ERROR_ATTR_NOT_SUPPORTED,
        sys::VA_STATUS_ERROR_RESOLUTION_NOT_SUPPORTED,
        sys::VA_STATUS_ERROR_INVALID_VALUE,
    ];
    if unsupported.iter().any(|&s| s as sys::VAStatus == status) {
        CreateError::Unsupported {
            what: format!("{call}: {}", api.error(status)),
        }
    } else if status == sys::VA_STATUS_ERROR_ALLOCATION_FAILED as sys::VAStatus {
        CreateError::ResourceExhausted
    } else {
        CreateError::Device(format!("{call}: {}", api.error(status)))
    }
}

fn failed(call: &'static str, status: sys::VAStatus) -> EngineError {
    EngineError::Failed(call, i64::from(status))
}

/// One slot: the input surface, the image its pixels are written through and the
/// coded buffer of its picture.
struct Slot {
    surface: sys::VASurfaceID,
    image: sys::VAImage,
    /// The image is the surface's own memory (`vaDeriveImage`); otherwise it is
    /// copied into the surface with `vaPutImage`.
    derived: bool,
    coded: sys::VABufferID,
    token: u64,
    idr: bool,
}

/// The parameter sets of a session's codec.
enum Stream {
    H264(h264::Sequence),
    Hevc(hevc::Sequence),
}

impl Stream {
    /// The parameter sets, Annex B.
    fn headers(&self) -> Vec<u8> {
        match self {
            Stream::H264(s) => s.headers(),
            Stream::Hevc(s) => s.headers(),
        }
    }

    /// The coded (aligned) picture size of the surfaces.
    fn coded_size(&self) -> (u32, u32) {
        match self {
            Stream::H264(s) => (s.width_mbs * 16, s.height_mbs * 16),
            Stream::Hevc(s) => (s.width, s.height),
        }
    }

    fn set_peak(&mut self, peak_bps: u32) {
        match self {
            Stream::H264(s) => s.peak_bps = peak_bps,
            Stream::Hevc(s) => s.peak_bps = peak_bps,
        }
    }

    fn fps(&self) -> (u32, u32) {
        match self {
            Stream::H264(s) => s.fps,
            Stream::Hevc(s) => s.fps,
        }
    }
}

/// One VA-API H.264 or HEVC encoder.
pub(crate) struct VaapiEngine {
    slots: Vec<Slot>,
    /// Reconstructed pictures, `slots + 1`.
    recon: Vec<sys::VASurfaceID>,
    /// Every surface (inputs, then reconstructed), for destruction.
    surfaces: Vec<sys::VASurfaceID>,
    config: sys::VAConfigID,
    context: sys::VAContextID,
    stream: Stream,
    /// The coded size of the surfaces (macroblock- or coding-block-aligned).
    size: (u32, u32),
    /// `VA_FOURCC_NV12` or `VA_FOURCC_P010`.
    fourcc: u32,
    /// The parameter sets (H.264: SPS and PPS; HEVC: VPS, SPS and PPS), Annex B.
    headers: Vec<u8>,
    rc: Rc,
    /// The rate control changed: send it with the next picture.
    rc_changed: bool,
    /// The driver takes packed slice headers: the adapter writes them.
    packed_slice: bool,
    gop: u32,
    /// Pictures encoded, for the reconstructed surface ring.
    pictures: u64,
    /// Pictures since the last IDR picture (the H.264 POC is twice this, the HEVC
    /// POC this).
    since_idr: u32,
    frame_num: u32,
    idr_pic_id: u16,
    /// The previous picture's reconstructed surface and frame number.
    reference: Option<(sys::VASurfaceID, u32, u32)>,
    rows: [usize; 2],
    // Declared last: destroyed after every object above (see `Drop`).
    display: Display,
}

// SAFETY: the VA objects belong to this engine's own display, which libva allows to
// be used from any thread as long as calls are not concurrent; `&mut self` ensures
// that.
unsafe impl Send for VaapiEngine {}

impl VaapiEngine {
    /// Opens an encoder for `plan` on the probed render node. Returns the engine and
    /// the Annex B parameter sets (extradata).
    pub(crate) fn open(plan: &Plan) -> Result<(Self, Vec<u8>), CreateError> {
        let api = api().map_err(|e| CreateError::Unsupported { what: e.into() })?;
        let hevc = match plan.codec {
            CodecId::H264 if !plan.ten_bit => false,
            CodecId::Hevc => true,
            _ => {
                return Err(CreateError::Unsupported {
                    what: format!(
                        "{} ({}) with VA-API (8-bit H.264 and HEVC only)",
                        plan.codec,
                        plan.pixel_format()
                    ),
                });
            }
        };
        let node = NODE.get().ok_or_else(|| CreateError::Unsupported {
            what: "no VA-API encoder".into(),
        })?;
        let display = Display::open(api, node).map_err(CreateError::Device)?;
        let encode =
            encode_config(&display, plan.profile).ok_or_else(|| CreateError::Unsupported {
                what: format!("VA-API profile {}", plan.profile.capability_name()),
            })?;
        let rc_mode = match plan.rc {
            Rc::Cbr { .. } => sys::VA_RC_CBR,
            Rc::Vbr { .. } => sys::VA_RC_VBR,
            Rc::Quality { .. } => sys::VA_RC_CQP,
        };
        if encode.rate_controls & rc_mode == 0 {
            return Err(CreateError::Unsupported {
                what: "rate-control mode on this VA-API driver".into(),
            });
        }
        let stream =
            if hevc {
                let tools = hevc_tools(encode.hevc_features, rc_mode == sys::VA_RC_CQP)
                    .ok_or_else(|| CreateError::Unsupported {
                        what: "an HEVC coding tool the VA-API driver requires".into(),
                    })?;
                Stream::Hevc(hevc::Sequence::new(
                    plan,
                    hevc_blocks(encode.hevc_block_sizes),
                    tools,
                    encode.gpb,
                ))
            } else {
                Stream::H264(h264::Sequence::new(plan))
            };
        let (width, height) = stream.coded_size();
        let fourcc = if plan.ten_bit {
            sys::VA_FOURCC_P010
        } else {
            sys::VA_FOURCC_NV12
        };
        let mut engine = VaapiEngine {
            slots: Vec::with_capacity(FRAMES_IN_FLIGHT),
            recon: Vec::with_capacity(FRAMES_IN_FLIGHT + 1),
            surfaces: Vec::new(),
            config: sys::VA_INVALID_ID,
            context: sys::VA_INVALID_ID,
            headers: stream.headers(),
            stream,
            size: (width, height),
            fourcc,
            rc: plan.rc,
            rc_changed: false,
            packed_slice: encode.packed_headers & sys::VA_ENC_PACKED_HEADER_SLICE != 0,
            gop: plan.gop.max(1),
            pictures: 0,
            since_idr: 0,
            frame_num: 0,
            idr_pic_id: 0,
            reference: None,
            rows: [plan.height as usize, plan.height.div_ceil(2) as usize],
            display,
        };
        let dpy = engine.display.dpy;
        let mut attribs = [
            sys::VAConfigAttrib {
                type_: sys::VAConfigAttribRTFormat,
                value: encode.rt_format,
            },
            sys::VAConfigAttrib {
                type_: sys::VAConfigAttribRateControl,
                value: rc_mode,
            },
            sys::VAConfigAttrib {
                type_: sys::VAConfigAttribEncPackedHeaders,
                value: sys::VA_ENC_PACKED_HEADER_SEQUENCE
                    | (encode.packed_headers & sys::VA_ENC_PACKED_HEADER_SLICE),
            },
        ];
        // SAFETY: an initialised display, three attributes and a valid out-pointer.
        let status = unsafe {
            (api.create_config)(
                dpy,
                va_profile(plan.profile),
                encode.entrypoint,
                attribs.as_mut_ptr(),
                attribs.len() as c_int,
                &mut engine.config,
            )
        };
        if !ok(status) {
            engine.config = sys::VA_INVALID_ID;
            return Err(create_error(api, "vaCreateConfig", status));
        }
        let count = 2 * FRAMES_IN_FLIGHT + 1;
        let mut surfaces = vec![sys::VA_INVALID_SURFACE; count];
        let mut format = sys::VASurfaceAttrib {
            type_: sys::VASurfaceAttribPixelFormat,
            flags: sys::VA_SURFACE_ATTRIB_SETTABLE,
            value: sys::VAGenericValue {
                type_: sys::VAGenericValueTypeInteger,
                value: sys::_VAGenericValue__bindgen_ty_1 { i: fourcc as i32 },
            },
        };
        // SAFETY: an initialised display, room for `count` ids and one attribute.
        let status = unsafe {
            (api.create_surfaces)(
                dpy,
                encode.rt_format,
                width,
                height,
                surfaces.as_mut_ptr(),
                count as c_uint,
                &mut format,
                1,
            )
        };
        if !ok(status) {
            return Err(create_error(api, "vaCreateSurfaces", status));
        }
        engine.surfaces = surfaces;
        // SAFETY: an initialised display, a live config and `count` live surfaces.
        let status = unsafe {
            (api.create_context)(
                dpy,
                engine.config,
                width as c_int,
                height as c_int,
                sys::VA_PROGRESSIVE as c_int,
                engine.surfaces.as_mut_ptr(),
                count as c_int,
                &mut engine.context,
            )
        };
        if !ok(status) {
            engine.context = sys::VA_INVALID_ID;
            return Err(create_error(api, "vaCreateContext", status));
        }
        engine.recon = engine.surfaces[FRAMES_IN_FLIGHT..].to_vec();
        // Room for a picture: the raw frame plus headers (an encoder never needs more
        // than its PCM size; 10-bit samples take two bytes here, more than enough).
        let bytes_per_sample = if plan.ten_bit { 2 } else { 1 };
        let coded_size = width * height * 3 / 2 * bytes_per_sample + (64 << 10);
        let image_format = engine.image_format();
        for i in 0..FRAMES_IN_FLIGHT {
            let surface = engine.surfaces[i];
            let mut coded = sys::VA_INVALID_ID;
            // SAFETY: an initialised display, a live context and a valid
            // out-pointer; no initial data.
            let status = unsafe {
                (api.create_buffer)(
                    dpy,
                    engine.context,
                    sys::VAEncCodedBufferType,
                    coded_size,
                    1,
                    ptr::null_mut(),
                    &mut coded,
                )
            };
            if !ok(status) {
                return Err(create_error(api, "vaCreateBuffer(coded)", status));
            }
            let (image, derived) = match engine.image(surface, image_format) {
                Ok(v) => v,
                Err(e) => {
                    // SAFETY: the buffer just created, not in use.
                    unsafe { (api.destroy_buffer)(dpy, coded) };
                    return Err(e);
                }
            };
            if let Err(e) = engine.zero(&image) {
                // SAFETY: objects just created, not in use.
                unsafe {
                    (api.destroy_image)(dpy, image.image_id);
                    (api.destroy_buffer)(dpy, coded);
                }
                return Err(e);
            }
            engine.slots.push(Slot {
                surface,
                image,
                derived,
                coded,
                token: 0,
                idr: false,
            });
        }
        let extradata = engine.headers.clone();
        Ok((engine, extradata))
    }

    /// The driver's image format of the input fourcc, if it lists one.
    fn image_format(&self) -> Option<sys::VAImageFormat> {
        self.display.image_format(self.fourcc)
    }

    /// An NV12 or P010 image for `surface`: the surface's own memory when the
    /// driver can derive it in that format, else a separate image put into the
    /// surface after each copy.
    fn image(
        &self,
        surface: sys::VASurfaceID,
        format: Option<sys::VAImageFormat>,
    ) -> Result<(sys::VAImage, bool), CreateError> {
        let api = self.display.api;
        let dpy = self.display.dpy;
        let mut image = sys::VAImage {
            image_id: sys::VA_INVALID_ID,
            buf: sys::VA_INVALID_ID,
            ..Default::default()
        };
        // SAFETY: an initialised display, a live surface and a valid out-pointer.
        let status = unsafe { (api.derive_image)(dpy, surface, &mut image) };
        if ok(status) {
            if image.format.fourcc == self.fourcc && image.num_planes >= 2 {
                return Ok((image, true));
            }
            // SAFETY: the image just derived.
            unsafe { (api.destroy_image)(dpy, image.image_id) };
        }
        let mut format = format.ok_or_else(|| CreateError::Unsupported {
            what: "the VA-API driver has no image format for the input".into(),
        })?;
        let mut image = sys::VAImage {
            image_id: sys::VA_INVALID_ID,
            buf: sys::VA_INVALID_ID,
            ..Default::default()
        };
        // SAFETY: an initialised display, a format from the driver's list and a
        // valid out-pointer.
        let status = unsafe {
            (api.create_image)(
                dpy,
                &mut format,
                self.size.0 as c_int,
                self.size.1 as c_int,
                &mut image,
            )
        };
        if !ok(status) {
            return Err(create_error(api, "vaCreateImage", status));
        }
        if image.num_planes < 2 {
            // SAFETY: the image just created.
            unsafe { (api.destroy_image)(dpy, image.image_id) };
            return Err(CreateError::Device("VA 4:2:0 image with one plane".into()));
        }
        Ok((image, false))
    }

    /// Zeroes `image`, so the rows and columns between the picture and the
    /// macroblock-aligned surface (never written by `upload`) are constant
    /// rather than stale memory.
    fn zero(&self, image: &sys::VAImage) -> Result<(), CreateError> {
        let api = self.display.api;
        let dpy = self.display.dpy;
        let mut base: *mut c_void = ptr::null_mut();
        // SAFETY: a live image buffer not in use; a valid out-pointer.
        let status = unsafe { (api.map_buffer)(dpy, image.buf, &mut base) };
        if !ok(status) {
            return Err(create_error(api, "vaMapBuffer(image)", status));
        }
        if !base.is_null() {
            // SAFETY: the mapped image holds `data_size` bytes.
            unsafe { ptr::write_bytes(base.cast::<u8>(), 0, image.data_size as usize) };
        }
        // SAFETY: the buffer mapped above.
        let status = unsafe { (api.unmap_buffer)(dpy, image.buf) };
        if !ok(status) {
            return Err(create_error(api, "vaUnmapBuffer(image)", status));
        }
        Ok(())
    }

    /// Creates a parameter buffer holding `data`.
    fn buffer<T>(
        &self,
        type_: sys::VABufferType,
        data: &T,
    ) -> Result<sys::VABufferID, EngineError> {
        self.raw_buffer(type_, ptr::from_ref(data).cast(), size_of::<T>())
    }

    fn raw_buffer(
        &self,
        type_: sys::VABufferType,
        data: *const c_void,
        size: usize,
    ) -> Result<sys::VABufferID, EngineError> {
        let mut id = sys::VA_INVALID_ID;
        let size = c_uint::try_from(size).map_err(|_| EngineError::Broken("VA buffer size"))?;
        // SAFETY: a live context; libva copies `size` bytes from `data`, which
        // points to that many readable bytes, and never writes through it.
        let status = unsafe {
            (self.display.api.create_buffer)(
                self.display.dpy,
                self.context,
                type_,
                size,
                1,
                data.cast_mut(),
                &mut id,
            )
        };
        if !ok(status) {
            return Err(failed("vaCreateBuffer", status));
        }
        Ok(id)
    }

    /// A misc parameter buffer: the type word followed by `data`.
    fn misc<T: Copy>(
        &self,
        type_: sys::VAEncMiscParameterType,
        data: &T,
    ) -> Result<sys::VABufferID, EngineError> {
        // u32 words keep the payload aligned like the C struct.
        let words = 1 + size_of::<T>().div_ceil(4);
        let mut buf = vec![0u32; words];
        buf[0] = type_;
        // SAFETY: `buf` has room for `T` after the first word; `T` is a plain C
        // struct (Copy) and both regions are 4-byte aligned.
        unsafe {
            ptr::copy_nonoverlapping(
                ptr::from_ref(data).cast::<u8>(),
                buf.as_mut_ptr().add(1).cast::<u8>(),
                size_of::<T>(),
            );
        }
        self.raw_buffer(
            sys::VAEncMiscParameterBufferType,
            buf.as_ptr().cast(),
            4 + size_of::<T>(),
        )
    }

    /// Rate-control and HRD parameters of the current rate control (none for
    /// constant QP).
    fn rate_buffers(&self, out: &mut Vec<sys::VABufferID>) -> Result<(), EngineError> {
        let (average, peak) = match self.rc {
            Rc::Cbr { bitrate } => (bitrate, bitrate),
            Rc::Vbr { average, max } => (average, max),
            Rc::Quality { .. } => return Ok(()),
        };
        let mut rc = sys::VAEncMiscParameterRateControl {
            bits_per_second: peak,
            target_percentage: u32::try_from(u64::from(average) * 100 / u64::from(peak.max(1)))
                .unwrap_or(100)
                .clamp(1, 100),
            window_size: 1000,
            ..Default::default()
        };
        // A skipped picture would be an empty packet, which ends the session.
        // SAFETY: writing the bitfield view of a plain-integer union.
        unsafe { rc.rc_flags.bits.set_disable_frame_skip(1) };
        out.push(self.misc(sys::VAEncMiscParameterTypeRateControl, &rc)?);
        let hrd = sys::VAEncMiscParameterHRD {
            buffer_size: peak,
            initial_buffer_fullness: peak / 4 * 3,
            ..Default::default()
        };
        out.push(self.misc(sys::VAEncMiscParameterTypeHRD, &hrd)?);
        Ok(())
    }

    fn h264_sequence_params(&self, s: &h264::Sequence) -> sys::VAEncSequenceParameterBufferH264 {
        let mut p = sys::VAEncSequenceParameterBufferH264 {
            seq_parameter_set_id: 0,
            level_idc: s.level_idc,
            intra_period: self.gop,
            intra_idr_period: self.gop,
            ip_period: 1,
            bits_per_second: s.peak_bps,
            max_num_ref_frames: 1,
            picture_width_in_mbs: s.width_mbs as u16,
            picture_height_in_mbs: s.height_mbs as u16,
            frame_cropping_flag: u8::from(s.crop != (0, 0)),
            frame_crop_right_offset: s.crop.0,
            frame_crop_bottom_offset: s.crop.1,
            vui_parameters_present_flag: 1,
            aspect_ratio_idc: 1,
            ..Default::default()
        };
        (p.num_units_in_tick, p.time_scale) = s.timing();
        // SAFETY: writing the bitfield view of a plain-integer union.
        unsafe {
            let f = &mut p.seq_fields.bits;
            f.set_chroma_format_idc(1);
            f.set_frame_mbs_only_flag(1);
            f.set_direct_8x8_inference_flag(1);
            f.set_log2_max_frame_num_minus4(LOG2_MAX_FRAME_NUM_MINUS4);
            f.set_pic_order_cnt_type(2);
            let v = &mut p.vui_fields.bits;
            v.set_aspect_ratio_info_present_flag(1);
            v.set_timing_info_present_flag(1);
            v.set_bitstream_restriction_flag(1);
            v.set_log2_max_mv_length_horizontal(15);
            v.set_log2_max_mv_length_vertical(15);
            v.set_fixed_frame_rate_flag(1);
            v.set_motion_vectors_over_pic_boundaries_flag(1);
        }
        p
    }

    fn hevc_sequence_params(&self, s: &hevc::Sequence) -> sys::VAEncSequenceParameterBufferHEVC {
        let b = &s.blocks;
        let t = &s.tools;
        let mut p = sys::VAEncSequenceParameterBufferHEVC {
            general_profile_idc: s.profile_idc(),
            general_level_idc: s.level_idc,
            general_tier_flag: 0,
            intra_period: self.gop,
            intra_idr_period: self.gop,
            ip_period: 1,
            bits_per_second: s.peak_bps,
            // Bounded by the driver's maximum picture size (at most 16 bits).
            pic_width_in_luma_samples: u16::try_from(s.width).unwrap_or(u16::MAX),
            pic_height_in_luma_samples: u16::try_from(s.height).unwrap_or(u16::MAX),
            log2_min_luma_coding_block_size_minus3: b.log2_min_cb - 3,
            log2_diff_max_min_luma_coding_block_size: b.log2_ctb - b.log2_min_cb,
            log2_min_transform_block_size_minus2: b.log2_min_tb - 2,
            log2_diff_max_min_transform_block_size: b.log2_max_tb - b.log2_min_tb,
            max_transform_hierarchy_depth_inter: b.depth_inter,
            max_transform_hierarchy_depth_intra: b.depth_intra,
            vui_parameters_present_flag: 1,
            aspect_ratio_idc: 1,
            ..Default::default()
        };
        (p.vui_num_units_in_tick, p.vui_time_scale) = s.timing();
        // SAFETY: writing the bitfield view of a plain-integer union.
        unsafe {
            let f = &mut p.seq_fields.bits;
            f.set_chroma_format_idc(1);
            f.set_bit_depth_luma_minus8(s.bit_depth_minus8());
            f.set_bit_depth_chroma_minus8(s.bit_depth_minus8());
            f.set_amp_enabled_flag(u32::from(t.amp));
            f.set_sample_adaptive_offset_enabled_flag(u32::from(t.sao));
            f.set_sps_temporal_mvp_enabled_flag(u32::from(t.temporal_mvp));
            f.set_strong_intra_smoothing_enabled_flag(u32::from(t.strong_intra_smoothing));
            let v = &mut p.vui_fields.bits;
            v.set_aspect_ratio_info_present_flag(1);
            v.set_vui_timing_info_present_flag(1);
            v.set_bitstream_restriction_flag(1);
            v.set_motion_vectors_over_pic_boundaries_flag(1);
            v.set_restricted_ref_pic_lists_flag(1);
            v.set_log2_max_mv_length_horizontal(15);
            v.set_log2_max_mv_length_vertical(15);
        }
        p
    }

    fn picture(surface: sys::VASurfaceID, frame_num: u32, since_idr: u32) -> sys::VAPictureH264 {
        let poc = i32::try_from(u64::from(since_idr) * 2).unwrap_or(i32::MAX);
        sys::VAPictureH264 {
            picture_id: surface,
            frame_idx: frame_num,
            flags: 0,
            TopFieldOrderCnt: poc,
            BottomFieldOrderCnt: poc,
            ..Default::default()
        }
    }

    const INVALID_PICTURE: sys::VAPictureH264 = sys::VAPictureH264 {
        picture_id: sys::VA_INVALID_SURFACE,
        frame_idx: 0,
        flags: sys::VA_PICTURE_H264_INVALID,
        TopFieldOrderCnt: 0,
        BottomFieldOrderCnt: 0,
        va_reserved: [0; 4],
    };

    const INVALID_HEVC_PICTURE: sys::VAPictureHEVC = sys::VAPictureHEVC {
        picture_id: sys::VA_INVALID_SURFACE,
        pic_order_cnt: 0,
        flags: sys::VA_PICTURE_HEVC_INVALID,
        va_reserved: [0; 4],
    };

    /// Renders the buffers of one picture into `slot`'s input surface.
    fn render(&mut self, slot: usize, idr: bool) -> Result<(), EngineError> {
        let recon_index = (self.pictures % self.recon.len() as u64) as usize;
        let recon = self.recon[recon_index];
        let reference = if idr { None } else { self.reference };
        if !idr && reference.is_none() {
            return Err(EngineError::Broken("P picture without a reference"));
        }
        // At most 12: sequence, packed sequence (2), frame rate, rate control, HRD,
        // picture, packed slice (2), slice.
        let mut buffers = Vec::with_capacity(12);
        let result = self.build(slot, idr, recon, reference, &mut buffers);
        let result = result.and_then(|()| self.submit(slot, &mut buffers));
        for id in buffers {
            // SAFETY: parameter buffers created above; the driver copied their
            // contents during `vaRenderPicture`.
            unsafe { (self.display.api.destroy_buffer)(self.display.dpy, id) };
        }
        result?;
        let frame_num = self.frame_num;
        self.reference = Some((recon, frame_num, self.since_idr));
        self.pictures += 1;
        Ok(())
    }

    fn build(
        &mut self,
        slot: usize,
        idr: bool,
        recon: sys::VASurfaceID,
        reference: Option<(sys::VASurfaceID, u32, u32)>,
        buffers: &mut Vec<sys::VABufferID>,
    ) -> Result<(), EngineError> {
        if idr {
            let seq = match &self.stream {
                Stream::H264(s) => {
                    let p = self.h264_sequence_params(s);
                    self.buffer(sys::VAEncSequenceParameterBufferType, &p)?
                }
                Stream::Hevc(s) => {
                    let p = self.hevc_sequence_params(s);
                    self.buffer(sys::VAEncSequenceParameterBufferType, &p)?
                }
            };
            buffers.push(seq);
            let bits = u32::try_from(self.headers.len() * 8)
                .map_err(|_| EngineError::Broken("parameter sets too long"))?;
            let packed = sys::VAEncPackedHeaderParameterBuffer {
                type_: sys::VAEncPackedHeaderSequence,
                bit_length: bits,
                has_emulation_bytes: 1,
                ..Default::default()
            };
            buffers.push(self.buffer(sys::VAEncPackedHeaderParameterBufferType, &packed)?);
            buffers.push(self.raw_buffer(
                sys::VAEncPackedHeaderDataBufferType,
                self.headers.as_ptr().cast(),
                self.headers.len(),
            )?);
            let (num, den) = self.stream.fps();
            let framerate = if num <= 0xffff && den <= 0xffff {
                num | (den << 16)
            } else {
                num.div_ceil(den.max(1))
            };
            let fr = sys::VAEncMiscParameterFrameRate {
                framerate,
                ..Default::default()
            };
            buffers.push(self.misc(sys::VAEncMiscParameterTypeFrameRate, &fr)?);
        }
        if idr || self.rc_changed {
            self.rate_buffers(buffers)?;
            self.rc_changed = false;
        }
        match &self.stream {
            Stream::H264(s) => self.h264_picture(s, slot, idr, recon, reference, buffers),
            Stream::Hevc(s) => self.hevc_picture(s, slot, idr, recon, reference, buffers),
        }
    }

    /// The picture, packed slice header and slice buffers of an H.264 picture.
    fn h264_picture(
        &self,
        seq: &h264::Sequence,
        slot: usize,
        idr: bool,
        recon: sys::VASurfaceID,
        reference: Option<(sys::VASurfaceID, u32, u32)>,
        buffers: &mut Vec<sys::VABufferID>,
    ) -> Result<(), EngineError> {
        let mut pic = sys::VAEncPictureParameterBufferH264 {
            CurrPic: Self::picture(recon, self.frame_num, self.since_idr),
            ReferenceFrames: [Self::INVALID_PICTURE; 16],
            coded_buf: self.slots[slot].coded,
            frame_num: self.frame_num as u16,
            pic_init_qp: seq.init_qp,
            ..Default::default()
        };
        let ref_picture = reference.map(|(surface, frame_num, since_idr)| {
            let mut p = Self::picture(surface, frame_num, since_idr);
            p.flags = sys::VA_PICTURE_H264_SHORT_TERM_REFERENCE;
            p
        });
        if let Some(r) = ref_picture {
            pic.ReferenceFrames[0] = r;
        }
        // SAFETY: writing the bitfield view of a plain-integer union.
        unsafe {
            let f = &mut pic.pic_fields.bits;
            f.set_idr_pic_flag(u32::from(idr));
            f.set_reference_pic_flag(1);
            f.set_entropy_coding_mode_flag(u32::from(seq.cabac));
            f.set_transform_8x8_mode_flag(u32::from(seq.transform_8x8));
            f.set_deblocking_filter_control_present_flag(1);
        }
        buffers.push(self.buffer(sys::VAEncPictureParameterBufferType, &pic)?);
        let mut slice = sys::VAEncSliceParameterBufferH264 {
            macroblock_address: 0,
            num_macroblocks: seq.width_mbs * seq.height_mbs,
            macroblock_info: sys::VA_INVALID_ID,
            // H.264 slice_type: 2 = I, 0 = P.
            slice_type: if idr { 2 } else { 0 },
            idr_pic_id: self.idr_pic_id,
            RefPicList0: [Self::INVALID_PICTURE; 32],
            RefPicList1: [Self::INVALID_PICTURE; 32],
            ..Default::default()
        };
        if let Some(r) = ref_picture {
            slice.RefPicList0[0] = r;
        }
        if self.packed_slice {
            let (header, bits) = seq.slice_header(idr, self.frame_num, self.idr_pic_id);
            let packed = sys::VAEncPackedHeaderParameterBuffer {
                type_: sys::VAEncPackedHeaderSlice,
                bit_length: bits,
                // Written without emulation prevention; the driver adds it.
                has_emulation_bytes: 0,
                ..Default::default()
            };
            buffers.push(self.buffer(sys::VAEncPackedHeaderParameterBufferType, &packed)?);
            buffers.push(self.raw_buffer(
                sys::VAEncPackedHeaderDataBufferType,
                header.as_ptr().cast(),
                header.len(),
            )?);
        }
        buffers.push(self.buffer(sys::VAEncSliceParameterBufferType, &slice)?);
        Ok(())
    }

    /// The picture, packed slice header and slice buffers of an HEVC picture.
    fn hevc_picture(
        &self,
        seq: &hevc::Sequence,
        slot: usize,
        idr: bool,
        recon: sys::VASurfaceID,
        reference: Option<(sys::VASurfaceID, u32, u32)>,
        buffers: &mut Vec<sys::VABufferID>,
    ) -> Result<(), EngineError> {
        let poc = |since_idr: u32| i32::try_from(since_idr).unwrap_or(i32::MAX);
        let t = &seq.tools;
        let mut pic = sys::VAEncPictureParameterBufferHEVC {
            decoded_curr_pic: sys::VAPictureHEVC {
                picture_id: recon,
                pic_order_cnt: poc(self.since_idr),
                flags: 0,
                ..Default::default()
            },
            reference_frames: [Self::INVALID_HEVC_PICTURE; 15],
            coded_buf: self.slots[slot].coded,
            // 0xff unless this slice uses temporal MV prediction (never on IDR).
            collocated_ref_pic_index: if t.temporal_mvp && !idr { 0 } else { 0xff },
            pic_init_qp: seq.init_qp,
            diff_cu_qp_delta_depth: seq.cu_qp_delta_depth(),
            nal_unit_type: hevc::Sequence::nal_unit_type(idr),
            ..Default::default()
        };
        // The previous picture, before this one in output order.
        let ref_picture = reference.map(|(surface, _, since_idr)| sys::VAPictureHEVC {
            picture_id: surface,
            pic_order_cnt: poc(since_idr),
            flags: sys::VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE,
            ..Default::default()
        });
        if let Some(r) = ref_picture {
            pic.reference_frames[0] = r;
        }
        // SAFETY: writing the bitfield view of a plain-integer union.
        unsafe {
            let f = &mut pic.pic_fields.bits;
            f.set_idr_pic_flag(u32::from(idr));
            // 1 = I, 2 = P (also when coded as generalised B slices).
            f.set_coding_type(if idr { 1 } else { 2 });
            f.set_reference_pic_flag(1);
            f.set_sign_data_hiding_enabled_flag(u32::from(t.sign_data_hiding));
            f.set_constrained_intra_pred_flag(u32::from(t.constrained_intra_pred));
            f.set_transform_skip_enabled_flag(u32::from(t.transform_skip));
            f.set_cu_qp_delta_enabled_flag(u32::from(t.cu_qp_delta));
            f.set_transquant_bypass_enabled_flag(u32::from(t.transquant_bypass));
            f.set_pps_loop_filter_across_slices_enabled_flag(1);
        }
        buffers.push(self.buffer(sys::VAEncPictureParameterBufferType, &pic)?);
        let slice_type = seq.slice_type(idr);
        let mut slice = sys::VAEncSliceParameterBufferHEVC {
            slice_segment_address: 0,
            num_ctu_in_slice: seq.ctus(),
            slice_type,
            ref_pic_list0: [Self::INVALID_HEVC_PICTURE; 15],
            ref_pic_list1: [Self::INVALID_HEVC_PICTURE; 15],
            max_num_merge_cand: 5,
            ..Default::default()
        };
        if let Some(r) = ref_picture {
            slice.ref_pic_list0[0] = r;
            if slice_type == 0 {
                slice.ref_pic_list1[0] = r;
            }
        }
        // SAFETY: writing the bitfield view of a plain-integer union.
        unsafe {
            let f = &mut slice.slice_fields.bits;
            f.set_last_slice_of_pic_flag(1);
            f.set_slice_temporal_mvp_enabled_flag(u32::from(t.temporal_mvp && !idr));
            f.set_slice_sao_luma_flag(u32::from(t.sao));
            f.set_slice_sao_chroma_flag(u32::from(t.sao));
            f.set_collocated_from_l0_flag(1);
        }
        if self.packed_slice {
            let header = seq.slice_header(idr, self.since_idr);
            let bits = u32::try_from(header.len() * 8)
                .map_err(|_| EngineError::Broken("slice header too long"))?;
            let packed = sys::VAEncPackedHeaderParameterBuffer {
                type_: sys::VAEncPackedHeaderSlice,
                bit_length: bits,
                // Byte-aligned, with emulation prevention (as FFmpeg sends it).
                has_emulation_bytes: 1,
                ..Default::default()
            };
            buffers.push(self.buffer(sys::VAEncPackedHeaderParameterBufferType, &packed)?);
            buffers.push(self.raw_buffer(
                sys::VAEncPackedHeaderDataBufferType,
                header.as_ptr().cast(),
                header.len(),
            )?);
        }
        buffers.push(self.buffer(sys::VAEncSliceParameterBufferType, &slice)?);
        Ok(())
    }

    fn submit(&self, slot: usize, buffers: &mut [sys::VABufferID]) -> Result<(), EngineError> {
        let api = self.display.api;
        let dpy = self.display.dpy;
        // SAFETY: a live context and the slot's live input surface.
        let status = unsafe { (api.begin_picture)(dpy, self.context, self.slots[slot].surface) };
        if !ok(status) {
            return Err(failed("vaBeginPicture", status));
        }
        // SAFETY: live parameter buffers of this context.
        let status = unsafe {
            (api.render_picture)(
                dpy,
                self.context,
                buffers.as_mut_ptr(),
                buffers.len() as c_int,
            )
        };
        // SAFETY: a picture begun above. It is ended also after a failed render (a
        // driver may then encode a partial picture; the session ends on the error).
        let end = unsafe { (api.end_picture)(dpy, self.context) };
        if !ok(status) {
            return Err(failed("vaRenderPicture", status));
        }
        if !ok(end) {
            return Err(failed("vaEndPicture", end));
        }
        Ok(())
    }

    /// Waits for the picture of `slot` (or checks it with `wait == false`):
    /// `Ok(false)` while it is not finished.
    fn sync(&self, slot: usize, wait: bool) -> Result<bool, EngineError> {
        let api = self.display.api;
        let dpy = self.display.dpy;
        let surface = self.slots[slot].surface;
        let timeout = if wait { WAIT.as_nanos() as u64 } else { 0 };
        let timed_out = || {
            if wait {
                Err(EngineError::Broken(
                    "VA-API did not finish a picture within 10 s",
                ))
            } else {
                Ok(false)
            }
        };
        // The coded buffer is what the output needs: its completion also covers a
        // bitstream write-back that may finish after the input surface is idle.
        if let Some(sync_buffer) = api.sync_buffer {
            // SAFETY: a live coded buffer of this display.
            let status = unsafe { sync_buffer(dpy, self.slots[slot].coded, timeout) };
            if ok(status) {
                return Ok(true);
            }
            if status == sys::VA_STATUS_ERROR_TIMEDOUT as sys::VAStatus {
                return timed_out();
            }
            if status != sys::VA_STATUS_ERROR_UNIMPLEMENTED as sys::VAStatus {
                return Err(failed("vaSyncBuffer", status));
            }
        }
        if let Some(sync2) = api.sync_surface2 {
            // SAFETY: a live surface of this display.
            let status = unsafe { sync2(dpy, surface, timeout) };
            if ok(status) {
                return Ok(true);
            }
            if status == sys::VA_STATUS_ERROR_TIMEDOUT as sys::VAStatus {
                return timed_out();
            }
            if status != sys::VA_STATUS_ERROR_UNIMPLEMENTED as sys::VAStatus {
                return Err(failed("vaSyncSurface2", status));
            }
        }
        if !wait {
            let mut state: sys::VASurfaceStatus = 0;
            // SAFETY: a live surface and a valid out-pointer.
            let status = unsafe { (api.query_surface_status)(dpy, surface, &mut state) };
            if !ok(status) {
                return Err(failed("vaQuerySurfaceStatus", status));
            }
            if state & sys::VASurfaceRendering != 0 {
                return Ok(false);
            }
        }
        // SAFETY: a live surface of this display.
        let status = unsafe { (api.sync_surface)(dpy, surface) };
        if !ok(status) {
            return Err(failed("vaSyncSurface", status));
        }
        Ok(true)
    }

    /// The bytes of `slot`'s coded buffer.
    fn coded(&self, slot: usize) -> Result<Vec<u8>, EngineError> {
        let api = self.display.api;
        let dpy = self.display.dpy;
        let coded = self.slots[slot].coded;
        let mut segment: *mut c_void = ptr::null_mut();
        // SAFETY: a live coded buffer whose picture finished; a valid out-pointer.
        let status = unsafe { (api.map_buffer)(dpy, coded, &mut segment) };
        if !ok(status) {
            return Err(failed("vaMapBuffer", status));
        }
        let mut data = Vec::new();
        let mut seg = segment.cast::<sys::VACodedBufferSegment>();
        let mut segments = 0;
        let mut overflow = false;
        while !seg.is_null() && segments < 1024 {
            // SAFETY: a segment of the mapped coded buffer list.
            let s = unsafe { &*seg };
            overflow |= s.status
                & (sys::VA_CODED_BUF_STATUS_SLICE_OVERFLOW_MASK
                    | sys::VA_CODED_BUF_STATUS_FRAME_SIZE_OVERFLOW)
                != 0;
            if !s.buf.is_null() && s.size > 0 {
                // SAFETY: `size` bytes at `buf`, mapped until `vaUnmapBuffer`.
                data.extend_from_slice(unsafe {
                    std::slice::from_raw_parts(s.buf.cast::<u8>(), s.size as usize)
                });
            }
            seg = s.next.cast();
            segments += 1;
        }
        // SAFETY: the buffer mapped above.
        let status = unsafe { (api.unmap_buffer)(dpy, coded) };
        if !ok(status) {
            return Err(failed("vaUnmapBuffer", status));
        }
        if overflow {
            return Err(EngineError::Broken(
                "VA-API picture overflowed its coded buffer",
            ));
        }
        if data.is_empty() {
            return Err(EngineError::Broken("VA-API returned an empty picture"));
        }
        Ok(data)
    }
}

impl Engine for VaapiEngine {
    fn slots(&self) -> usize {
        FRAMES_IN_FLIGHT
    }

    fn upload(&mut self, slot: usize, frame: &SystemFrame) -> Result<(), EngineError> {
        let api = self.display.api;
        let dpy = self.display.dpy;
        let s = &self.slots[slot];
        let image = s.image;
        let mut base: *mut c_void = ptr::null_mut();
        // SAFETY: the slot's live image buffer; the surface is not in use (the
        // session reuses a slot only after its output was read).
        let status = unsafe { (api.map_buffer)(dpy, image.buf, &mut base) };
        if !ok(status) {
            return Err(failed("vaMapBuffer(image)", status));
        }
        let size = image.data_size as usize;
        let plane = |i: usize| {
            let offset = image.offsets[i] as usize;
            let pitch = image.pitches[i] as usize;
            let len = pitch.checked_mul(self.rows[i])?;
            (offset.checked_add(len)? <= size && pitch > 0).then_some((offset, pitch, len))
        };
        let copied = match (plane(0), plane(1)) {
            (Some((y_off, y_pitch, y_len)), Some((uv_off, uv_pitch, uv_len)))
                if !base.is_null() && (y_off + y_len <= uv_off || uv_off + uv_len <= y_off) =>
            {
                let base = base.cast::<u8>();
                // SAFETY: two disjoint ranges (checked above) inside the mapped
                // image of `data_size` bytes.
                let (luma, chroma) = unsafe {
                    (
                        std::slice::from_raw_parts_mut(base.add(y_off), y_len),
                        std::slice::from_raw_parts_mut(base.add(uv_off), uv_len),
                    )
                };
                Some(copy_420(
                    frame,
                    [
                        DstPlane {
                            data: luma,
                            pitch: y_pitch,
                        },
                        DstPlane {
                            data: chroma,
                            pitch: uv_pitch,
                        },
                    ],
                ))
            }
            _ => None,
        };
        // SAFETY: the buffer mapped above.
        let status = unsafe { (api.unmap_buffer)(dpy, image.buf) };
        match copied {
            None => {
                return Err(EngineError::Broken(
                    "VA image planes do not fit the picture",
                ));
            }
            Some(false) => {
                return Err(EngineError::Broken("frame planes do not match the session"));
            }
            Some(true) => {}
        }
        if !ok(status) {
            return Err(failed("vaUnmapBuffer(image)", status));
        }
        if !s.derived {
            // The whole aligned image, so its padding (zeroed at open) is put too.
            let (w, h) = self.size;
            // SAFETY: a live surface and image of this display, both `w` x `h`.
            let status =
                unsafe { (api.put_image)(dpy, s.surface, image.image_id, 0, 0, w, h, 0, 0, w, h) };
            if !ok(status) {
                return Err(failed("vaPutImage", status));
            }
        }
        Ok(())
    }

    fn encode(&mut self, slot: usize, token: u64, idr: bool) -> Result<(), EngineError> {
        let idr = idr || self.reference.is_none() || self.since_idr + 1 >= self.gop;
        let (since_idr, frame_num, idr_pic_id) = (self.since_idr, self.frame_num, self.idr_pic_id);
        if idr {
            self.since_idr = 0;
            self.frame_num = 0;
            if self.pictures > 0 {
                self.idr_pic_id = self.idr_pic_id.wrapping_add(1);
            }
        } else {
            self.since_idr += 1;
            self.frame_num = (self.frame_num + 1) % (1 << (LOG2_MAX_FRAME_NUM_MINUS4 + 4));
        }
        if let Err(e) = self.render(slot, idr) {
            (self.since_idr, self.frame_num, self.idr_pic_id) = (since_idr, frame_num, idr_pic_id);
            return Err(e);
        }
        let s = &mut self.slots[slot];
        s.token = token;
        s.idr = idr;
        Ok(())
    }

    fn output(&mut self, slot: usize, wait: bool) -> Result<Option<Output>, EngineError> {
        if !self.sync(slot, wait)? {
            return Ok(None);
        }
        let data = self.coded(slot)?;
        let s = &self.slots[slot];
        Ok(Some(Output {
            data,
            keyframe: s.idr,
            token: s.token,
        }))
    }

    fn end_of_stream(&mut self) -> Result<(), EngineError> {
        // Every picture is encoded on its own; nothing is held back.
        Ok(())
    }

    fn set_rate_control(&mut self, rc: Rc) -> Result<(), EngineError> {
        self.rc = rc;
        // The sequence parameters of the next IDR carry the new peak too.
        // `level_idc` stays: a peak above the level's limit is not refused.
        self.stream.set_peak(match rc {
            Rc::Cbr { bitrate } => bitrate,
            Rc::Vbr { max, .. } => max,
            Rc::Quality { .. } => 0,
        });
        self.rc_changed = true;
        Ok(())
    }
}

impl Drop for VaapiEngine {
    fn drop(&mut self) {
        let api = self.display.api;
        let dpy = self.display.dpy;
        // Pictures still encoding finish before their objects go.
        for s in &self.slots {
            // SAFETY: a live surface of this display; bounded when libva allows.
            unsafe {
                match api.sync_surface2 {
                    Some(sync2) => sync2(dpy, s.surface, WAIT.as_nanos() as u64),
                    None => (api.sync_surface)(dpy, s.surface),
                }
            };
        }
        for s in &self.slots {
            // SAFETY: objects created by this engine, no longer in use.
            unsafe {
                (api.destroy_image)(dpy, s.image.image_id);
                (api.destroy_buffer)(dpy, s.coded);
            }
        }
        if self.context != sys::VA_INVALID_ID {
            // SAFETY: as above.
            unsafe { (api.destroy_context)(dpy, self.context) };
        }
        if !self.surfaces.is_empty() {
            // SAFETY: as above.
            unsafe {
                (api.destroy_surfaces)(
                    dpy,
                    self.surfaces.as_mut_ptr(),
                    self.surfaces.len() as c_int,
                )
            };
        }
        if self.config != sys::VA_INVALID_ID {
            // SAFETY: as above.
            unsafe { (api.destroy_config)(dpy, self.config) };
        }
        // `display` terminates after this.
    }
}
