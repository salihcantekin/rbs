//! The QSV engine: one oneVPL hardware session (on Linux with a VA display on the
//! GPU's render node) and one encoder in it, fed from system-memory surfaces that
//! this engine allocates, one per slot.
//!
//! oneVPL sessions may be used from any thread as long as calls are not concurrent,
//! which `&mut self` ensures.

use std::ptr;

use rbs_codec::CreateError;
use rbs_codec_hw::copy::{DstPlane, copy_420};
use rbs_codec_hw::device::{CodecCaps, DeviceCaps, Profiles};
use rbs_codec_hw::plan::{Plan, Preset, Profile, Rc};
use rbs_codec_hw::session::{Engine, EngineError, Output};
use rbs_core::media::CodecId;
use rbs_media_format::h273::ColorCodes;

use crate::FRAMES_IN_FLIGHT;
use crate::loader::{Api, api};
use crate::params::{self, Params, align, annex_b, frame_info, video_param};
use crate::sys;

/// Longest wait for one picture, in milliseconds. A picture is waited for only when
/// it was submitted `FRAMES_IN_FLIGHT` frames earlier, at `flush` and in `Drop`.
const SYNC_WAIT_MS: u32 = 10_000;

/// Bytes reserved for each parameter set read back at creation.
const PARAMETER_SET_BYTES: usize = 1024;

/// Maps a failed oneVPL status to the engine's error.
fn engine_error(call: &'static str, s: sys::mfxStatus) -> EngineError {
    match s {
        sys::MFX_WRN_DEVICE_BUSY => EngineError::Busy,
        sys::MFX_ERR_DEVICE_LOST | sys::MFX_ERR_DEVICE_FAILED | sys::MFX_ERR_GPU_HANG => {
            EngineError::DeviceLost
        }
        other => EngineError::Failed(call, i64::from(other)),
    }
}

fn create_error(call: &'static str, s: sys::mfxStatus) -> CreateError {
    match s {
        sys::MFX_ERR_UNSUPPORTED
        | sys::MFX_ERR_INVALID_VIDEO_PARAM
        | sys::MFX_ERR_INCOMPATIBLE_VIDEO_PARAM
        | sys::MFX_WRN_PARTIAL_ACCELERATION => CreateError::Unsupported {
            what: format!("{call} refused the configuration (status {s})"),
        },
        sys::MFX_ERR_MEMORY_ALLOC => CreateError::ResourceExhausted,
        _ => CreateError::Device(format!("{call} failed with status {s}")),
    }
}

/// Why a device could not be opened.
#[derive(Debug)]
enum OpenError {
    /// No Intel hardware implementation: nothing to offer, for good.
    NoDevice,
    /// Opening failed now and may work later.
    Temporary(String),
}

/// The VA display the Linux runtime encodes through, on the render node the
/// dispatcher reported for the implementation.
#[cfg(target_os = "linux")]
struct Display {
    api: &'static Api,
    display: sys::va::VADisplay,
    /// Keeps the render node open while the display lives.
    _node: std::fs::File,
}

#[cfg(target_os = "linux")]
impl Display {
    fn open(api: &'static Api, render_node: u32) -> Result<Self, OpenError> {
        use std::os::fd::AsRawFd;
        let path = format!("/dev/dri/renderD{render_node}");
        let node = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => OpenError::NoDevice,
                _ => OpenError::Temporary(format!("{path}: {e}")),
            })?;
        // SAFETY: a valid, open DRM file descriptor; libva does not take ownership
        // of it, and `_node` keeps it open for the display's life.
        let display = unsafe { (api.va.get_display_drm)(node.as_raw_fd()) };
        if display.is_null() {
            return Err(OpenError::Temporary(format!("vaGetDisplayDRM({path})")));
        }
        let display = Display {
            api,
            display,
            _node: node,
        };
        let (mut major, mut minor) = (0, 0);
        // SAFETY: a display from `vaGetDisplayDRM` and valid out-pointers.
        let s = unsafe { (api.va.initialize)(display.display, &mut major, &mut minor) };
        if s != sys::va::VA_STATUS_SUCCESS {
            // `Drop` still calls `vaTerminate`, which frees the display.
            return Err(OpenError::Temporary(format!("vaInitialize failed ({s})")));
        }
        Ok(display)
    }
}

#[cfg(target_os = "linux")]
impl Drop for Display {
    fn drop(&mut self) {
        if !self.display.is_null() {
            // SAFETY: a display from `vaGetDisplayDRM` (initialised or not; terminating
            // frees it either way), terminated once, after the session that used it was
            // closed (`Device::drop`).
            let _ = unsafe { (self.api.va.terminate)(self.display) };
        }
    }
}

/// A dispatcher loader and one hardware session created from it. Dropping closes
/// the session, then the display, then unloads the loader.
struct Device {
    api: &'static Api,
    loader: sys::mfxLoader,
    session: sys::mfxSession,
    #[cfg(target_os = "linux")]
    _display: Option<Display>,
    name: String,
}

impl Device {
    fn open() -> Result<Self, OpenError> {
        let api = api().map_err(|_| OpenError::NoDevice)?;
        let v = &api.vpl;
        // SAFETY: `MFXLoad` has no preconditions.
        let loader = unsafe { (v.load)() };
        if loader.is_null() {
            return Err(OpenError::Temporary("MFXLoad returned no loader".into()));
        }
        let mut device = Device {
            api,
            loader,
            session: ptr::null_mut(),
            #[cfg(target_os = "linux")]
            _display: None,
            name: String::new(),
        };
        // SAFETY: a live loader.
        let config = unsafe { (v.create_config)(loader) };
        if config.is_null() {
            return Err(OpenError::Temporary("MFXCreateConfig failed".into()));
        }
        let mut value = sys::mfxVariant {
            Type: sys::MFX_VARIANT_TYPE_U32,
            Data: sys::mfxVariant_data {
                U32: sys::MFX_IMPL_TYPE_HARDWARE,
            },
            ..Default::default()
        };
        value.Version.Version = sys::MFX_VARIANT_VERSION;
        // SAFETY: a config of this loader, a NUL-terminated property name and a
        // versioned variant passed by value.
        let s = unsafe { (v.set_filter)(config, sys::FILTER_IMPL.as_ptr(), value) };
        if s != sys::MFX_ERR_NONE {
            return Err(OpenError::Temporary(format!(
                "MFXSetConfigFilterProperty failed ({s})"
            )));
        }

        // Implementation 0: the first hardware implementation the dispatcher found.
        let mut desc: sys::mfxHDL = ptr::null_mut();
        // SAFETY: a live loader and a valid out-pointer.
        let s = unsafe {
            (v.enum_implementations)(loader, 0, sys::MFX_IMPLCAPS_IMPLDESCSTRUCTURE, &mut desc)
        };
        if s != sys::MFX_ERR_NONE || desc.is_null() {
            return Err(OpenError::NoDevice);
        }
        // SAFETY: for `MFX_IMPLCAPS_IMPLDESCSTRUCTURE` the handle points at an
        // `mfxImplDescription` owned by the loader until released below.
        let impl_name = unsafe { c_string(&(*desc.cast::<sys::mfxImplDescription>()).ImplName) };
        // SAFETY: released once, as returned by `MFXEnumImplementations`.
        let _ = unsafe { (v.release_description)(loader, desc) };

        let mut render_node = intel_render_node().unwrap_or(128);
        let mut ext: sys::mfxHDL = ptr::null_mut();
        // SAFETY: as above. Older runtimes do not report extended device ids.
        let s = unsafe {
            (v.enum_implementations)(loader, 0, sys::MFX_IMPLCAPS_DEVICE_ID_EXTENDED, &mut ext)
        };
        let mut name = impl_name;
        if s == sys::MFX_ERR_NONE && !ext.is_null() {
            // SAFETY: for `MFX_IMPLCAPS_DEVICE_ID_EXTENDED` the handle points at an
            // `mfxExtendedDeviceId` owned by the loader until released below.
            let id = unsafe { &*ext.cast::<sys::mfxExtendedDeviceId>() };
            if id.DRMRenderNodeNum >= 128 {
                render_node = id.DRMRenderNodeNum;
            }
            let device_name = c_string(&id.DeviceName);
            if !device_name.is_empty() {
                name = device_name;
            }
            // SAFETY: released once.
            let _ = unsafe { (v.release_description)(loader, ext) };
        }
        #[cfg(not(target_os = "linux"))]
        let _ = render_node;
        device.name = if name.is_empty() {
            "Intel Quick Sync Video".into()
        } else {
            name
        };

        let mut session: sys::mfxSession = ptr::null_mut();
        // SAFETY: a live loader, an implementation index it reported and a valid
        // out-pointer.
        let s = unsafe { (v.create_session)(loader, 0, &mut session) };
        if s != sys::MFX_ERR_NONE || session.is_null() {
            return Err(match s {
                sys::MFX_ERR_NOT_FOUND | sys::MFX_ERR_UNSUPPORTED => OpenError::NoDevice,
                _ => OpenError::Temporary(format!("MFXCreateSession failed ({s})")),
            });
        }
        device.session = session;

        #[cfg(target_os = "linux")]
        {
            let display = Display::open(api, render_node)?;
            // SAFETY: a live session without components yet, and an initialised VA
            // display that outlives the session (`Device` field order).
            let s = unsafe {
                (v.set_handle)(device.session, sys::MFX_HANDLE_VA_DISPLAY, display.display)
            };
            device._display = Some(display);
            if s != sys::MFX_ERR_NONE {
                return Err(OpenError::Temporary(format!(
                    "MFXVideoCORE_SetHandle failed ({s})"
                )));
            }
        }
        Ok(device)
    }

    /// True when the runtime accepts `plan` with the fields that shape the stream
    /// (codec, profile, input format, size, rate-control method, no reordering)
    /// unchanged. Other fields it may adjust.
    fn accepts(&self, plan: &Plan) -> bool {
        let mut input = video_param(plan, 1);
        let mut output = input;
        // SAFETY: a live session; both parameter blocks are valid and have no
        // extension buffers.
        let s = unsafe { (self.api.vpl.query)(self.session, &mut input, &mut output) };
        match s {
            sys::MFX_ERR_NONE => true,
            sys::MFX_WRN_INCOMPATIBLE_VIDEO_PARAM => same_stream(&input, &output),
            _ => false,
        }
    }
}

/// True when `b` keeps the fields of `a` that shape the stream: codec, profile,
/// input format, sizes, rate-control method and GOP reference distance. Both must be
/// encoder parameters.
fn same_stream(a: &sys::mfxVideoParam, b: &sys::mfxVideoParam) -> bool {
    // SAFETY: both blocks are encoder parameters (built by `video_param`, or written
    // back by `Query` or `GetVideoParam` in the same layout), so `mfx` and its encoder
    // and size members are the initialised union members.
    let (a, b) = unsafe { (a.__bindgen_anon_1.mfx, b.__bindgen_anon_1.mfx) };
    // SAFETY: as above.
    let (ae, be) = unsafe {
        (
            a.__bindgen_anon_1.__bindgen_anon_1,
            b.__bindgen_anon_1.__bindgen_anon_1,
        )
    };
    // SAFETY: as above.
    let (asz, bsz) = unsafe {
        (
            a.FrameInfo.__bindgen_anon_1.__bindgen_anon_1,
            b.FrameInfo.__bindgen_anon_1.__bindgen_anon_1,
        )
    };
    a.CodecId == b.CodecId
        && a.CodecProfile == b.CodecProfile
        && a.FrameInfo.FourCC == b.FrameInfo.FourCC
        && ae.RateControlMethod == be.RateControlMethod
        && ae.GopRefDist == be.GopRefDist
        && (asz.Width, asz.Height, asz.CropW, asz.CropH)
            == (bsz.Width, bsz.Height, bsz.CropW, bsz.CropH)
}

impl Drop for Device {
    fn drop(&mut self) {
        let v = &self.api.vpl;
        if !self.session.is_null() {
            // SAFETY: the session is open, its encoder was closed by its owner
            // before (`QsvEngine::drop`), and it is closed once.
            let _ = unsafe { (v.close)(self.session) };
        }
        // Fields drop only after this body; terminate the display now, between the
        // session that used it and the loader.
        #[cfg(target_os = "linux")]
        drop(self._display.take());
        // SAFETY: the loader came from `MFXLoad`, its session is closed, and it is
        // unloaded once.
        unsafe { (v.unload)(self.loader) };
    }
}

/// The first DRM render node of an Intel GPU (PCI vendor 0x8086), for runtimes that
/// do not report theirs. `None` elsewhere or when sysfs has no answer.
fn intel_render_node() -> Option<u32> {
    #[cfg(target_os = "linux")]
    {
        (128..192).find(|n| {
            std::fs::read_to_string(format!("/sys/class/drm/renderD{n}/device/vendor"))
                .is_ok_and(|v| v.trim() == "0x8086")
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// The text of a fixed-size, NUL-terminated C string field.
fn c_string(chars: &[core::ffi::c_char]) -> String {
    let bytes: Vec<u8> = chars
        .iter()
        .take_while(|c| **c != 0)
        .map(|c| *c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).trim().to_owned()
}

/// A plan the probe queries the runtime with.
fn probe_plan(codec: CodecId, profile: Profile, rc: Rc, width: u32, height: u32) -> Plan {
    Plan {
        codec,
        width,
        height,
        ten_bit: profile == Profile::HevcMain10,
        fps: (30, 1),
        preset: Preset::Balanced,
        low_latency: false,
        profile,
        rc,
        cbr_padding: false,
        gop: 60,
        color: ColorCodes {
            primaries: 1,
            transfer: 1,
            matrix: 1,
            full_range: false,
        },
    }
}

/// Reads what the first Intel hardware implementation encodes. `Ok` with no codecs
/// when the libraries are missing or there is no Intel GPU; `Err` only when probing
/// itself failed now and may work later.
pub(crate) fn probe() -> Result<DeviceCaps, String> {
    let device = match Device::open() {
        Ok(d) => d,
        Err(OpenError::NoDevice) => return Ok(DeviceCaps::default()),
        Err(OpenError::Temporary(e)) => return Err(e),
    };
    const CBR: Rc = Rc::Cbr { bitrate: 4_000_000 };
    let mut codecs = Vec::new();
    for (codec, candidates) in [
        (
            CodecId::H264,
            &[Profile::H264Baseline, Profile::H264Main, Profile::H264High][..],
        ),
        (CodecId::Hevc, &[Profile::HevcMain, Profile::HevcMain10][..]),
    ] {
        let mut profiles = Profiles::NONE;
        for p in candidates {
            if device.accepts(&probe_plan(codec, *p, CBR, 1280, 720)) {
                profiles = profiles.with(*p);
            }
        }
        // 8-bit profile the other queries use.
        let Some(base) = profiles.iter().find(|p| *p != Profile::HevcMain10) else {
            continue;
        };
        let at = |rc: Rc, w: u32, h: u32| device.accepts(&probe_plan(codec, base, rc, w, h));
        let max = [
            (8192, 8192),
            (8192, 4320),
            (4096, 4096),
            (4096, 2304),
            (3840, 2160),
            (1920, 1080),
        ]
        .into_iter()
        .find(|(w, h)| at(CBR, *w, *h));
        let min = [(32, 32), (64, 64), (128, 128), (256, 144)]
            .into_iter()
            .find(|(w, h)| at(CBR, *w, *h));
        let (Some(max), Some(min)) = (max, min) else {
            continue;
        };
        let vbr = at(
            Rc::Vbr {
                average: 4_000_000,
                max: 8_000_000,
            },
            1280,
            720,
        );
        codecs.push(CodecCaps {
            codec,
            width: (min.0, max.0),
            height: (min.1, max.1),
            // oneVPL does not report throughput.
            mb_per_sec: 0,
            profiles,
            cbr: true,
            vbr,
            quality: at(Rc::Quality { level: 23 }, 1280, 720),
            // `MFXVideoENCODE_Reset` changes CBR and VBR rates in place.
            dynamic_bitrate: true,
            // Filler data needs HRD conformance, which puts the rate in the SPS (a
            // rate change would then need a new sequence); not mapped yet.
            cbr_padding: false,
        });
    }
    codecs.retain(CodecCaps::usable);
    Ok(DeviceCaps {
        name: device.name.clone(),
        codecs,
    })
}

/// One slot: an input surface over its own buffer, and a bitstream over its own
/// buffer. Boxed, so the pointers the runtime holds stay valid when the engine
/// moves.
struct Slot {
    surface: Box<sys::mfxFrameSurface1>,
    /// Pixels of `surface` (luma, then chroma at `pitch * aligned height`).
    pixels: Box<[u8]>,
    bitstream: Box<sys::mfxBitstream>,
    /// Bytes of `bitstream`.
    data: Box<[u8]>,
    /// Frame-type control for forced IDR pictures; read by the runtime until the
    /// picture is synced.
    ctrl: Box<sys::mfxEncodeCtrl>,
    sync: sys::mfxSyncPoint,
    /// Encoded but not yet read back.
    pending: bool,
    /// `pending` and the sync point already completed.
    synced: bool,
    /// Token of the slot's last encode (submission order for `Drop`).
    token: u64,
}

/// One QSV encoder with its surfaces and bitstreams.
pub(crate) struct QsvEngine {
    /// Declared first: the device (session, display, loader) drops after `Drop`
    /// closed the encoder, and before the buffers below are freed.
    device: Device,
    slots: Vec<Slot>,
    params: Box<Params>,
    pitch: usize,
    /// Rows of luma in a surface (the aligned height).
    luma_rows: usize,
    /// Bytes of one input surface.
    raw_bytes: usize,
    /// Bytes of each slot's bitstream.
    bitstream_bytes: usize,
}

// SAFETY: the raw pointers are oneVPL handles and buffers owned by this engine.
// oneVPL sessions may be used from any thread as long as calls are not concurrent,
// which `&mut self` ensures; the VA display is only used through the session.
unsafe impl Send for QsvEngine {}

impl QsvEngine {
    /// Opens an encoder for `plan`. Returns the engine and the Annex B parameter
    /// sets (extradata).
    pub(crate) fn open(plan: &Plan) -> Result<(Self, Vec<u8>), CreateError> {
        let device = Device::open().map_err(|e| match e {
            OpenError::NoDevice => CreateError::Unsupported {
                what: "no Intel Quick Sync Video device".into(),
            },
            OpenError::Temporary(_) => CreateError::ResourceExhausted,
        })?;
        let v = &device.api.vpl;
        let mut params = Params::new(plan, FRAMES_IN_FLIGHT as u16);
        // SAFETY: a live session; `params` is boxed and its extension pointers
        // point into itself.
        let s = unsafe { (v.init)(device.session, &mut params.param) };
        if s < sys::MFX_ERR_NONE || s == sys::MFX_WRN_PARTIAL_ACCELERATION {
            return Err(create_error("MFXVideoENCODE_Init", s));
        }

        // Read back what the encoder runs with, and its parameter sets.
        let mut sps = vec![0u8; PARAMETER_SET_BYTES];
        let mut pps = vec![0u8; PARAMETER_SET_BYTES];
        let mut vps = vec![0u8; PARAMETER_SET_BYTES];
        let mut sps_pps = sys::mfxExtCodingOptionSPSPPS {
            Header: sys::mfxExtBuffer {
                BufferId: sys::MFX_EXTBUFF_CODING_OPTION_SPSPPS,
                BufferSz: size_of::<sys::mfxExtCodingOptionSPSPPS>() as u32,
            },
            SPSBuffer: sps.as_mut_ptr(),
            PPSBuffer: pps.as_mut_ptr(),
            SPSBufSize: PARAMETER_SET_BYTES as u16,
            PPSBufSize: PARAMETER_SET_BYTES as u16,
            ..Default::default()
        };
        let mut vps_ext = sys::mfxExtCodingOptionVPS {
            Header: sys::mfxExtBuffer {
                BufferId: sys::MFX_EXTBUFF_CODING_OPTION_VPS,
                BufferSz: size_of::<sys::mfxExtCodingOptionVPS>() as u32,
            },
            __bindgen_anon_1: sys::mfxExtCodingOptionVPS__bindgen_ty_1 {
                VPSBuffer: vps.as_mut_ptr(),
            },
            VPSBufSize: PARAMETER_SET_BYTES as u16,
            ..Default::default()
        };
        let mut ext: [*mut sys::mfxExtBuffer; 2] =
            [(&raw mut sps_pps).cast(), (&raw mut vps_ext).cast()];
        let mut actual = sys::mfxVideoParam {
            ExtParam: ext.as_mut_ptr(),
            NumExtParam: if plan.codec == CodecId::Hevc { 2 } else { 1 },
            ..Default::default()
        };
        // SAFETY: an initialised encoder; `actual` points at live extension buffers
        // whose data pointers have the capacities given.
        let s = unsafe { (v.get_video_param)(device.session, &mut actual) };
        if s < sys::MFX_ERR_NONE {
            // SAFETY: the encoder was initialised above and is closed once.
            let _ = unsafe { (v.encode_close)(device.session) };
            return Err(create_error("MFXVideoENCODE_GetVideoParam", s));
        }
        let take = |buf: &[u8], len: u16| buf[..usize::from(len).min(buf.len())].to_vec();
        let sps = take(&sps, sps_pps.SPSBufSize);
        let pps = take(&pps, sps_pps.PPSBufSize);
        let vps = if plan.codec == CodecId::Hevc {
            take(&vps, vps_ext.VPSBufSize)
        } else {
            Vec::new()
        };
        let extradata = annex_b(&[&vps, &sps, &pps]);
        if !same_stream(&params.param, &actual) {
            // `Init` corrected a field that shapes the stream: the session would not
            // be what was asked for.
            // SAFETY: the encoder was initialised above and is closed once.
            let _ = unsafe { (v.encode_close)(device.session) };
            return Err(CreateError::Unsupported {
                what: "the QSV runtime changed the profile, format, size or rate control".into(),
            });
        }
        // SAFETY: `GetVideoParam` filled the encoder parameters.
        let (mfx, buffer_kb) = unsafe {
            let mfx = actual.__bindgen_anon_1.mfx;
            (mfx, mfx.__bindgen_anon_1.__bindgen_anon_1.BufferSizeInKB)
        };

        // Surfaces as the encoder describes them (sizes aligned by `frame_info`).
        let info = if mfx.FrameInfo.FourCC == 0 {
            frame_info(plan)
        } else {
            mfx.FrameInfo
        };
        // SAFETY: the width/height member of the frame-info union is the one used.
        let size = unsafe { info.__bindgen_anon_1.__bindgen_anon_1 };
        let bytes_per_sample = if plan.ten_bit { 2 } else { 1 };
        let pitch = align(u32::from(size.Width) * bytes_per_sample, 64) as usize;
        let luma_rows = usize::from(size.Height);
        let surface_bytes = pitch * luma_rows * 3 / 2;
        let bitstream_bytes =
            (usize::from(buffer_kb) * usize::from(mfx.BRCParamMultiplier.max(1)) * 1000)
                .max(params::bitstream_bytes(plan.rc, surface_bytes));

        let mut engine = QsvEngine {
            device,
            slots: Vec::with_capacity(FRAMES_IN_FLIGHT),
            params,
            pitch,
            luma_rows,
            raw_bytes: surface_bytes,
            bitstream_bytes,
        };
        if extradata.is_empty() {
            return Err(CreateError::Device(
                "the QSV runtime returned no parameter sets".into(),
            ));
        }
        for _ in 0..FRAMES_IN_FLIGHT {
            let mut pixels = vec![0u8; surface_bytes].into_boxed_slice();
            let base = pixels.as_mut_ptr();
            let mut surface = Box::new(sys::mfxFrameSurface1 {
                Info: info,
                ..Default::default()
            });
            surface.Data.__bindgen_anon_2 = sys::mfxFrameData__bindgen_ty_2 {
                Pitch: pitch as u16,
            };
            surface.Data.__bindgen_anon_3 = sys::mfxFrameData__bindgen_ty_3 { Y: base };
            surface.Data.__bindgen_anon_4 = sys::mfxFrameData__bindgen_ty_4 {
                // SAFETY: `pixels` holds `pitch * luma_rows * 3 / 2` bytes, so the
                // chroma plane starts inside it.
                UV: unsafe { base.add(pitch * luma_rows) },
            };
            let mut data = vec![0u8; bitstream_bytes].into_boxed_slice();
            let bitstream = Box::new(sys::mfxBitstream {
                Data: data.as_mut_ptr(),
                MaxLength: u32::try_from(data.len()).unwrap_or(u32::MAX),
                ..Default::default()
            });
            engine.slots.push(Slot {
                surface,
                pixels,
                bitstream,
                data,
                ctrl: Box::default(),
                sync: ptr::null_mut(),
                pending: false,
                synced: false,
                token: 0,
            });
        }
        Ok((engine, extradata))
    }

    fn session(&self) -> sys::mfxSession {
        self.device.session
    }

    /// Waits for (or with `wait == false` polls) the picture of `slot`. `Ok(false)`
    /// when it is not finished yet.
    fn sync(&mut self, slot: usize, wait: bool) -> Result<bool, EngineError> {
        let s = &mut self.slots[slot];
        if s.synced {
            return Ok(true);
        }
        let timeout = if wait { SYNC_WAIT_MS } else { 0 };
        // SAFETY: a live session and the sync point its last `EncodeFrameAsync`
        // returned for this slot, not yet completed.
        let status = unsafe { (self.device.api.vpl.sync)(self.device.session, s.sync, timeout) };
        match status {
            sys::MFX_ERR_NONE => {
                s.synced = true;
                Ok(true)
            }
            sys::MFX_WRN_IN_EXECUTION if !wait => Ok(false),
            sys::MFX_WRN_IN_EXECUTION => Err(EngineError::Broken(
                "QSV did not finish a picture within 10 s",
            )),
            other => Err(engine_error("MFXVideoCORE_SyncOperation", other)),
        }
    }
}

impl Engine for QsvEngine {
    fn slots(&self) -> usize {
        FRAMES_IN_FLIGHT
    }

    fn upload(
        &mut self,
        slot: usize,
        frame: &rbs_core::media::SystemFrame,
    ) -> Result<(), EngineError> {
        let (pitch, luma_rows) = (self.pitch, self.luma_rows);
        let s = &mut self.slots[slot];
        // The runtime updates `Locked` from its own threads: read it atomically.
        // SAFETY: the field is a live, aligned `u16` inside the boxed surface, and the
        // runtime only accesses it atomically as well.
        let locked = unsafe {
            std::sync::atomic::AtomicU16::from_ptr(&raw mut s.surface.Data.Locked)
                .load(std::sync::atomic::Ordering::Acquire)
        };
        if locked != 0 {
            // The runtime still reads this surface.
            return Err(EngineError::Busy);
        }
        let (luma, chroma) = s.pixels.split_at_mut(pitch * luma_rows);
        if !copy_420(
            frame,
            [
                DstPlane { data: luma, pitch },
                DstPlane {
                    data: chroma,
                    pitch,
                },
            ],
        ) {
            return Err(EngineError::Broken("frame planes do not match the session"));
        }
        Ok(())
    }

    fn encode(&mut self, slot: usize, token: u64, idr: bool) -> Result<(), EngineError> {
        let session = self.session();
        let encode_frame = self.device.api.vpl.encode_frame;
        let s = &mut self.slots[slot];
        s.surface.Data.TimeStamp = token;
        s.bitstream.DataOffset = 0;
        s.bitstream.DataLength = 0;
        let ctrl = if idr {
            *s.ctrl = sys::mfxEncodeCtrl {
                FrameType: (sys::MFX_FRAMETYPE_I | sys::MFX_FRAMETYPE_REF | sys::MFX_FRAMETYPE_IDR)
                    as u16,
                ..Default::default()
            };
            &raw mut *s.ctrl
        } else {
            ptr::null_mut()
        };
        let mut sync: sys::mfxSyncPoint = ptr::null_mut();
        // SAFETY: an initialised encoder; the surface, bitstream and control are
        // boxed, owned by this slot and not in use by the runtime (the session never
        // encodes into a slot in flight), and stay alive until the picture is synced
        // (`Drop` syncs pending slots before closing the encoder).
        let status = unsafe {
            encode_frame(
                session,
                ctrl,
                &raw mut *s.surface,
                &raw mut *s.bitstream,
                &mut sync,
            )
        };
        match status {
            sys::MFX_WRN_DEVICE_BUSY => Err(EngineError::Busy),
            sys::MFX_ERR_MORE_DATA => Err(EngineError::Broken(
                "QSV buffered a frame (reordering or look-ahead)",
            )),
            s_ if s_ >= sys::MFX_ERR_NONE && !sync.is_null() => {
                s.sync = sync;
                s.pending = true;
                s.synced = false;
                s.token = token;
                Ok(())
            }
            s_ if s_ >= sys::MFX_ERR_NONE => Err(EngineError::Broken(
                "QSV accepted a frame without a sync point",
            )),
            other => Err(engine_error("MFXVideoENCODE_EncodeFrameAsync", other)),
        }
    }

    fn output(&mut self, slot: usize, wait: bool) -> Result<Option<Output>, EngineError> {
        if !self.slots[slot].pending {
            return Err(EngineError::Broken("no picture queued in this slot"));
        }
        if !self.sync(slot, wait)? {
            return Ok(None);
        }
        let s = &mut self.slots[slot];
        s.pending = false;
        s.synced = false;
        let start = s.bitstream.DataOffset as usize;
        let len = s.bitstream.DataLength as usize;
        let data = s
            .data
            .get(start..start.saturating_add(len))
            .ok_or(EngineError::Broken("QSV bitstream outside its buffer"))?
            .to_vec();
        if data.is_empty() {
            return Err(EngineError::Broken("QSV returned an empty picture"));
        }
        Ok(Some(Output {
            data,
            keyframe: u32::from(s.bitstream.FrameType) & sys::MFX_FRAMETYPE_IDR != 0,
            token: s.bitstream.TimeStamp,
        }))
    }

    fn end_of_stream(&mut self) -> Result<(), EngineError> {
        // The encoder holds no frames back (no B-frames, no look-ahead): every
        // queued picture finishes without an end-of-stream call.
        Ok(())
    }

    fn set_rate_control(&mut self, rc: Rc) -> Result<(), EngineError> {
        // Let queued pictures finish first: they are read back later as usual.
        for slot in 0..self.slots.len() {
            if self.slots[slot].pending {
                self.sync(slot, true)?;
            }
        }
        // The runtime checks bitstreams against its buffer for the new rate: grow
        // them first. No picture is in flight (all synced above); a synced picture not
        // read yet moves with its bytes.
        let needed = params::bitstream_bytes(rc, self.raw_bytes);
        if needed > self.bitstream_bytes {
            for s in &mut self.slots {
                let mut data = vec![0u8; needed].into_boxed_slice();
                let used = s.bitstream.DataOffset as usize + s.bitstream.DataLength as usize;
                if let (Some(to), Some(from)) = (data.get_mut(..used), s.data.get(..used)) {
                    to.copy_from_slice(from);
                }
                s.bitstream.Data = data.as_mut_ptr();
                s.bitstream.MaxLength = u32::try_from(data.len()).unwrap_or(u32::MAX);
                s.data = data;
            }
            self.bitstream_bytes = needed;
        }
        let mut next = self.params.with_rc(rc);
        // SAFETY: an initialised encoder with no picture in flight; `next` is
        // boxed and its extension pointers point into itself.
        let s = unsafe { (self.device.api.vpl.reset)(self.session(), &mut next.param) };
        match s {
            sys::MFX_ERR_INVALID_VIDEO_PARAM
            | sys::MFX_ERR_INCOMPATIBLE_VIDEO_PARAM
            | sys::MFX_ERR_UNSUPPORTED => Err(EngineError::Unsupported),
            s_ if s_ < sys::MFX_ERR_NONE => Err(engine_error("MFXVideoENCODE_Reset", s_)),
            _ => {
                next.link(false);
                self.params = next;
                Ok(())
            }
        }
    }
}

impl Drop for QsvEngine {
    fn drop(&mut self) {
        // Let queued pictures finish before their surfaces and bitstreams go away.
        for slot in 0..self.slots.len() {
            if self.slots[slot].pending {
                let _ = self.sync(slot, true);
            }
        }
        // SAFETY: the encoder was initialised by `open` and is closed once; no
        // operation of it is in flight any more.
        let _ = unsafe { (self.device.api.vpl.encode_close)(self.session()) };
        // `device` drops next (session, display, loader), then the buffers.
    }
}
