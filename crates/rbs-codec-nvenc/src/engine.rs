//! The NVENC engine: one CUDA context and one NVENC session in synchronous mode
//! (Linux has no asynchronous mode; one code path for both platforms), with
//! NVENC-allocated input buffers that system-memory frames are copied into.
//!
//! Every call that touches the session pushes the session's CUDA context on the
//! calling thread and pops it afterwards, so the session can move between threads
//! (it is used by one thread at a time; the port's `VideoEncoder: Send`).

use core::ffi::{c_char, c_int, c_void};
use std::ptr;

use rbs_codec::CreateError;
use rbs_core::media::{CodecId, PixelFormat, SystemFrame};

use rbs_codec_hw::copy::{DstPlane, copy_420, plane_sizes};
use rbs_codec_hw::device::{CodecCaps, DeviceCaps, Profiles};
use rbs_codec_hw::plan::{Plan, Preset, Profile, Rc};
use rbs_codec_hw::session::{Engine, EngineError, Output};

use crate::FRAMES_IN_FLIGHT;
use crate::loader::{Api, api};
use crate::sys::{self, cuda};

/// Returns the function pointer, or a typed error when the driver did not fill it.
fn f<T: Copy>(entry: Option<T>, name: &'static str) -> Result<T, EngineError> {
    entry.ok_or(EngineError::Failed(name, -1))
}

/// Maps an NVENC status to the engine's error.
fn status(call: &'static str, s: sys::NVENCSTATUS) -> Result<(), EngineError> {
    match s {
        sys::NV_ENC_SUCCESS => Ok(()),
        sys::NV_ENC_ERR_ENCODER_BUSY | sys::NV_ENC_ERR_LOCK_BUSY => Err(EngineError::Busy),
        sys::NV_ENC_ERR_DEVICE_NOT_EXIST => Err(EngineError::DeviceLost),
        other => Err(EngineError::Failed(call, i64::from(other))),
    }
}

fn create_error(call: &'static str, s: sys::NVENCSTATUS) -> CreateError {
    match s {
        // GeForce boards limit concurrent sessions; opening one more fails like this.
        sys::NV_ENC_ERR_OUT_OF_MEMORY | sys::NV_ENC_ERR_INCOMPATIBLE_CLIENT_KEY => {
            CreateError::ResourceExhausted
        }
        sys::NV_ENC_ERR_UNSUPPORTED_PARAM
        | sys::NV_ENC_ERR_INVALID_PARAM
        | sys::NV_ENC_ERR_UNSUPPORTED_DEVICE
        | sys::NV_ENC_ERR_NO_ENCODE_DEVICE => CreateError::Unsupported {
            what: format!("{call} refused the configuration (status {s})"),
        },
        _ => CreateError::Device(format!("{call} failed with status {s}")),
    }
}

/// The CUDA context current on this thread until dropped.
struct Current {
    pop: cuda::CtxPopCurrentFn,
    /// Not `Send`: the pop must run on the thread that pushed.
    _thread: std::marker::PhantomData<*const ()>,
}

impl Drop for Current {
    fn drop(&mut self) {
        let mut popped: cuda::CUcontext = ptr::null_mut();
        // SAFETY: a context was pushed on this thread by `Device::push`, which created
        // this guard; the guard is not `Send`, so this runs on the same thread.
        let _ = unsafe { (self.pop)(&mut popped) };
    }
}

/// A CUDA context and the NVENC session opened on it. Dropping destroys both.
struct Device {
    api: &'static Api,
    ctx: cuda::CUcontext,
    encoder: *mut c_void,
}

impl Device {
    /// Creates a context on CUDA device `ordinal` and opens an NVENC session on it.
    fn open(api: &'static Api, ordinal: c_int) -> Result<Self, CreateError> {
        let c = &api.cuda;
        let mut dev: cuda::CUdevice = 0;
        // SAFETY: valid out-pointer; `cuInit` succeeded when the API was loaded.
        let r = unsafe { (c.device_get)(&mut dev, ordinal) };
        if r != cuda::CUDA_SUCCESS {
            return Err(CreateError::Device(format!(
                "cuDeviceGet({ordinal}) failed ({r})"
            )));
        }
        let mut ctx: cuda::CUcontext = ptr::null_mut();
        // SAFETY: valid out-pointer and device. The new context becomes current on
        // this thread; it is popped right below.
        let r = unsafe { (c.ctx_create)(&mut ctx, cuda::CU_CTX_SCHED_BLOCKING_SYNC, dev) };
        if r != cuda::CUDA_SUCCESS {
            return Err(CreateError::Device(format!("cuCtxCreate failed ({r})")));
        }
        let mut popped: cuda::CUcontext = ptr::null_mut();
        // SAFETY: `cuCtxCreate` pushed `ctx` on this thread.
        let _ = unsafe { (c.ctx_pop)(&mut popped) };
        let mut device = Device {
            api,
            ctx,
            encoder: ptr::null_mut(),
        };
        let open = f(
            api.nvenc.0.nvEncOpenEncodeSessionEx,
            "nvEncOpenEncodeSessionEx",
        )
        .map_err(|_| CreateError::Device("nvEncOpenEncodeSessionEx missing".into()))?;
        let mut params = sys::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS {
            version: sys::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER,
            deviceType: sys::NV_ENC_DEVICE_TYPE_CUDA,
            device: ctx,
            apiVersion: sys::API_VERSION,
            ..Default::default()
        };
        let mut encoder: *mut c_void = ptr::null_mut();
        let _current = device
            .push()
            .map_err(|_| CreateError::Device("cuCtxPushCurrent".into()))?;
        // SAFETY: `params` is a valid, versioned parameter block naming a live CUDA
        // context; `encoder` is a valid out-pointer.
        let s = unsafe { open(&mut params, &mut encoder) };
        if s != sys::NV_ENC_SUCCESS || encoder.is_null() {
            return Err(create_error("nvEncOpenEncodeSessionEx", s));
        }
        device.encoder = encoder;
        Ok(device)
    }

    fn push(&self) -> Result<Current, EngineError> {
        // SAFETY: `ctx` is a live context owned by this device.
        let r = unsafe { (self.api.cuda.ctx_push)(self.ctx) };
        if r != cuda::CUDA_SUCCESS {
            return Err(EngineError::DeviceLost);
        }
        Ok(Current {
            pop: self.api.cuda.ctx_pop,
            _thread: std::marker::PhantomData,
        })
    }

    fn caps(&self, codec: sys::GUID, cap: sys::NV_ENC_CAPS) -> i64 {
        let Ok(get) = f(self.api.nvenc.0.nvEncGetEncodeCaps, "nvEncGetEncodeCaps") else {
            return 0;
        };
        let mut param = sys::NV_ENC_CAPS_PARAM {
            version: sys::NV_ENC_CAPS_PARAM_VER,
            capsToQuery: cap,
            ..Default::default()
        };
        let mut value: c_int = 0;
        // SAFETY: open session, valid parameter block and out-pointer.
        let s = unsafe { get(self.encoder, codec, &mut param, &mut value) };
        if s == sys::NV_ENC_SUCCESS {
            i64::from(value)
        } else {
            0
        }
    }

    fn codec_guids(&self) -> Vec<sys::GUID> {
        let funcs = &self.api.nvenc.0;
        let (Ok(count_fn), Ok(list_fn)) = (
            f(funcs.nvEncGetEncodeGUIDCount, "nvEncGetEncodeGUIDCount"),
            f(funcs.nvEncGetEncodeGUIDs, "nvEncGetEncodeGUIDs"),
        ) else {
            return Vec::new();
        };
        let mut count = 0u32;
        // SAFETY: open session, valid out-pointer.
        if unsafe { count_fn(self.encoder, &mut count) } != sys::NV_ENC_SUCCESS {
            return Vec::new();
        }
        let mut guids = vec![sys::GUID::default(); count.min(64) as usize];
        let mut filled = 0u32;
        // SAFETY: `guids` has room for `guids.len()` GUIDs, passed as the array size.
        let s = unsafe {
            list_fn(
                self.encoder,
                guids.as_mut_ptr(),
                guids.len() as u32,
                &mut filled,
            )
        };
        if s != sys::NV_ENC_SUCCESS {
            return Vec::new();
        }
        guids.truncate(filled as usize);
        guids
    }

    fn input_formats(&self, codec: sys::GUID) -> Vec<sys::NV_ENC_BUFFER_FORMAT> {
        let funcs = &self.api.nvenc.0;
        let (Ok(count_fn), Ok(list_fn)) = (
            f(funcs.nvEncGetInputFormatCount, "nvEncGetInputFormatCount"),
            f(funcs.nvEncGetInputFormats, "nvEncGetInputFormats"),
        ) else {
            return Vec::new();
        };
        let mut count = 0u32;
        // SAFETY: open session, valid out-pointer.
        if unsafe { count_fn(self.encoder, codec, &mut count) } != sys::NV_ENC_SUCCESS {
            return Vec::new();
        }
        let mut formats = vec![0; count.min(64) as usize];
        let mut filled = 0u32;
        // SAFETY: `formats` has room for `formats.len()` entries.
        let s = unsafe {
            list_fn(
                self.encoder,
                codec,
                formats.as_mut_ptr(),
                formats.len() as u32,
                &mut filled,
            )
        };
        if s != sys::NV_ENC_SUCCESS {
            return Vec::new();
        }
        formats.truncate(filled as usize);
        formats
    }

    fn name(&self, ordinal: c_int) -> String {
        let c = &self.api.cuda;
        let mut dev: cuda::CUdevice = 0;
        let mut name = [0 as c_char; 128];
        // SAFETY: valid out-pointers; `name` has the length passed.
        let ok = unsafe {
            (c.device_get)(&mut dev, ordinal) == cuda::CUDA_SUCCESS
                && (c.device_get_name)(name.as_mut_ptr(), name.len() as c_int - 1, dev)
                    == cuda::CUDA_SUCCESS
        };
        if !ok {
            return format!("CUDA device {ordinal}");
        }
        let bytes: Vec<u8> = name
            .iter()
            .take_while(|c| **c != 0)
            .map(|c| *c as u8)
            .collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        if !self.encoder.is_null() {
            if let (Ok(_current), Some(destroy)) =
                (self.push(), self.api.nvenc.0.nvEncDestroyEncoder)
            {
                // SAFETY: the session is open and owned by this device; every buffer
                // of it was destroyed by its owner before (field order of `NvencEngine`).
                let _ = unsafe { destroy(self.encoder) };
            }
        }
        // SAFETY: the context was created by `Device::open`, is not current on any
        // thread (every push is popped) and is destroyed exactly once.
        let _ = unsafe { (self.api.cuda.ctx_destroy)(self.ctx) };
    }
}

fn codec_guid(codec: CodecId) -> Option<sys::GUID> {
    match codec {
        CodecId::H264 => Some(sys::NV_ENC_CODEC_H264_GUID),
        CodecId::Hevc => Some(sys::NV_ENC_CODEC_HEVC_GUID),
        _ => None,
    }
}

/// Reads what CUDA device `ordinal` encodes. `Ok` with no codecs when the driver is
/// missing or the device has no NVENC; `Err` only when probing itself failed.
pub(crate) fn probe(ordinal: c_int) -> Result<DeviceCaps, String> {
    let Ok(api) = api() else {
        return Ok(DeviceCaps::default());
    };
    let mut count: c_int = 0;
    // SAFETY: valid out-pointer; `cuInit` succeeded.
    let r = unsafe { (api.cuda.device_get_count)(&mut count) };
    if r != cuda::CUDA_SUCCESS || count <= ordinal {
        return Ok(DeviceCaps::default());
    }
    let device = match Device::open(api, ordinal) {
        Ok(d) => d,
        // No encoder on this board: nothing to offer, for good.
        Err(CreateError::Unsupported { .. }) => return Ok(DeviceCaps::default()),
        // No free session or context right now: the caller probes again later.
        Err(e) => return Err(e.to_string()),
    };
    let _current = device
        .push()
        .map_err(|_| "cuCtxPushCurrent failed".to_string())?;
    let guids = device.codec_guids();
    let mut codecs = Vec::new();
    for codec in [CodecId::H264, CodecId::Hevc] {
        let Some(guid) = codec_guid(codec) else {
            continue;
        };
        if !guids.iter().any(|g| sys::same_guid(g, &guid)) {
            continue;
        }
        let formats = device.input_formats(guid);
        if !formats.contains(&sys::NV_ENC_BUFFER_FORMAT_NV12) {
            continue;
        }
        let cap = |c| device.caps(guid, c);
        let side =
            |v: i64, fallback: u32| u32::try_from(v).ok().filter(|v| *v > 0).unwrap_or(fallback);
        let modes = cap(sys::NV_ENC_CAPS_SUPPORTED_RATECONTROL_MODES);
        codecs.push(CodecCaps {
            codec,
            // Older drivers do not report minimums; 146x50 (H.264) and 130x34 (HEVC)
            // are the documented NVENC limits, rounded up to even.
            width: (
                side(
                    cap(sys::NV_ENC_CAPS_WIDTH_MIN),
                    if codec == CodecId::H264 { 146 } else { 130 },
                ),
                side(cap(sys::NV_ENC_CAPS_WIDTH_MAX), 0),
            ),
            height: (
                side(
                    cap(sys::NV_ENC_CAPS_HEIGHT_MIN),
                    if codec == CodecId::H264 { 50 } else { 34 },
                ),
                side(cap(sys::NV_ENC_CAPS_HEIGHT_MAX), 0),
            ),
            mb_per_sec: u64::try_from(cap(sys::NV_ENC_CAPS_MB_PER_SEC_MAX)).unwrap_or(0),
            profiles: if codec == CodecId::H264 {
                Profiles::H264_ALL
            } else if cap(sys::NV_ENC_CAPS_SUPPORT_10BIT_ENCODE) != 0
                && formats.contains(&sys::NV_ENC_BUFFER_FORMAT_YUV420_10BIT)
            {
                Profiles::HEVC_ALL
            } else {
                Profiles::HEVC_MAIN
            },
            // A zero mask means the driver does not report modes; every NVENC
            // generation has CBR and VBR. Constant quality is VBR with a target.
            cbr: modes == 0 || modes & i64::from(sys::NV_ENC_PARAMS_RC_CBR) != 0,
            vbr: modes == 0 || modes & i64::from(sys::NV_ENC_PARAMS_RC_VBR) != 0,
            quality: modes == 0 || modes & i64::from(sys::NV_ENC_PARAMS_RC_VBR) != 0,
            dynamic_bitrate: cap(sys::NV_ENC_CAPS_SUPPORT_DYN_BITRATE_CHANGE) != 0,
        });
    }
    codecs.retain(|c| c.usable());
    Ok(DeviceCaps {
        name: device.name(ordinal),
        codecs,
    })
}

/// Fills `rc` from the plan's rate control.
fn apply_rc(rc: &mut sys::NV_ENC_RC_PARAMS, plan: Rc) {
    rc.version = sys::NV_ENC_RC_PARAMS_VER;
    match plan {
        Rc::Cbr { bitrate } => {
            rc.rateControlMode = sys::NV_ENC_PARAMS_RC_CBR;
            rc.averageBitRate = bitrate;
            rc.maxBitRate = bitrate;
            rc.vbvBufferSize = bitrate;
            rc.vbvInitialDelay = bitrate;
            rc.targetQuality = 0;
        }
        Rc::Vbr { average, max } => {
            rc.rateControlMode = sys::NV_ENC_PARAMS_RC_VBR;
            rc.averageBitRate = average;
            rc.maxBitRate = max;
            rc.vbvBufferSize = max;
            rc.vbvInitialDelay = max;
            rc.targetQuality = 0;
        }
        Rc::Quality { level } => {
            rc.rateControlMode = sys::NV_ENC_PARAMS_RC_VBR;
            rc.averageBitRate = 0;
            rc.maxBitRate = 0;
            rc.vbvBufferSize = 0;
            rc.vbvInitialDelay = 0;
            // NVENC reads 0 as "automatic", so level 0 (best) becomes 1.
            rc.targetQuality = level.max(1);
            rc.targetQualityLSB = 0;
        }
    }
    // No look-ahead: it delays output and would make `encode` return
    // NV_ENC_ERR_NEED_MORE_INPUT.
    rc.set_enableLookahead(0);
}

fn apply_vui(vui: &mut sys::NV_ENC_CONFIG_H264_VUI_PARAMETERS, plan: &Plan) {
    vui.videoSignalTypePresentFlag = 1;
    vui.videoFormat = sys::NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
    vui.videoFullRangeFlag = u32::from(plan.color.full_range);
    vui.colourDescriptionPresentFlag = 1;
    vui.colourPrimaries = u32::from(plan.color.primaries);
    vui.transferCharacteristics = u32::from(plan.color.transfer);
    vui.colourMatrix = u32::from(plan.color.matrix);
}

/// One NVENC session with its buffers.
pub(crate) struct NvencEngine {
    /// Input buffers (system memory, NVENC-allocated), one per slot.
    inputs: [sys::NV_ENC_INPUT_PTR; FRAMES_IN_FLIGHT],
    /// Bitstream buffers, one per slot.
    outputs: [sys::NV_ENC_OUTPUT_PTR; FRAMES_IN_FLIGHT],
    /// Pitch the last lock of each input buffer reported.
    pitches: [u32; FRAMES_IN_FLIGHT],
    /// Slots encoded but not yet read back.
    pending: [bool; FRAMES_IN_FLIGHT],
    /// Token of each slot's last encode, so `Drop` reads pending slots back in
    /// submission order (NVENC requires bitstreams to be locked in that order).
    tokens: [u64; FRAMES_IN_FLIGHT],
    buffer_format: sys::NV_ENC_BUFFER_FORMAT,
    pixel_format: PixelFormat,
    width: u32,
    height: u32,
    /// Initialisation parameters, kept for reconfiguration; `encodeConfig` points at
    /// `config` (both boxed, so the pointer stays valid when the engine moves).
    init: Box<sys::NV_ENC_INITIALIZE_PARAMS>,
    config: Box<sys::NV_ENC_CONFIG>,
    /// Declared last: dropped after the buffers above are destroyed in `Drop`.
    device: Device,
}

// SAFETY: the raw pointers are NVENC handles owned by this engine. NVENC sessions may
// be used from any thread as long as calls are not concurrent (`&mut self` ensures
// that) and the session's CUDA context is current, which every call arranges by
// pushing it (`Device::push`).
unsafe impl Send for NvencEngine {}

impl NvencEngine {
    /// Opens a session for `plan` on CUDA device `ordinal`. Returns the engine and the
    /// Annex B parameter sets (extradata).
    pub(crate) fn open(ordinal: c_int, plan: &Plan) -> Result<(Self, Vec<u8>), CreateError> {
        let api = api().map_err(|e| CreateError::Unsupported { what: e.into() })?;
        let device = Device::open(api, ordinal)?;
        let funcs = &api.nvenc.0;
        let missing = |name: &str| CreateError::Device(format!("{name} missing"));
        let codec = codec_guid(plan.codec).ok_or_else(|| CreateError::Unsupported {
            what: format!("codec {}", plan.codec),
        })?;
        // P1 (fastest) ..= P7 (best).
        let preset = sys::NV_ENC_PRESET_GUIDS[match plan.preset {
            Preset::Speed => 1,
            Preset::Balanced => 3,
            Preset::Quality => 4,
        }];
        let tuning = if plan.low_latency {
            sys::NV_ENC_TUNING_INFO_LOW_LATENCY
        } else {
            sys::NV_ENC_TUNING_INFO_HIGH_QUALITY
        };
        let buffer_format = if plan.ten_bit {
            sys::NV_ENC_BUFFER_FORMAT_YUV420_10BIT
        } else {
            sys::NV_ENC_BUFFER_FORMAT_NV12
        };

        let _current = device
            .push()
            .map_err(|_| CreateError::Device("cuCtxPushCurrent".into()))?;
        let preset_config = f(funcs.nvEncGetEncodePresetConfigEx, "")
            .map_err(|_| missing("nvEncGetEncodePresetConfigEx"))?;
        let mut presets = Box::new(sys::NV_ENC_PRESET_CONFIG {
            version: sys::NV_ENC_PRESET_CONFIG_VER,
            ..Default::default()
        });
        presets.presetCfg.version = sys::NV_ENC_CONFIG_VER;
        // SAFETY: open session; `presets` is a versioned block the call fills.
        let s = unsafe { preset_config(device.encoder, codec, preset, tuning, &mut *presets) };
        if s != sys::NV_ENC_SUCCESS {
            return Err(create_error("nvEncGetEncodePresetConfigEx", s));
        }
        let mut config = Box::new(presets.presetCfg);
        config.version = sys::NV_ENC_CONFIG_VER;
        config.profileGUID = match plan.profile {
            Profile::H264Baseline => sys::NV_ENC_H264_PROFILE_BASELINE_GUID,
            Profile::H264Main => sys::NV_ENC_H264_PROFILE_MAIN_GUID,
            Profile::H264High => sys::NV_ENC_H264_PROFILE_HIGH_GUID,
            Profile::HevcMain => sys::NV_ENC_HEVC_PROFILE_MAIN_GUID,
            Profile::HevcMain10 => sys::NV_ENC_HEVC_PROFILE_MAIN10_GUID,
        };
        config.gopLength = plan.gop;
        // P frames only: no reordering, one output per input in order.
        config.frameIntervalP = 1;
        apply_rc(&mut config.rcParams, plan.rc);
        match plan.codec {
            CodecId::H264 => {
                // SAFETY: the preset filled the H.264 member of the union for the
                // H.264 codec GUID; it is the only member used.
                let h264 = unsafe { &mut config.encodeCodecConfig.h264Config };
                h264.idrPeriod = plan.gop;
                // Parameter sets in band with every IDR, so any IDR starts a decodable
                // stream (Annex B consumers such as MPEG-TS need them).
                h264.set_repeatSPSPPS(1);
                h264.set_outputAUD(0);
                h264.chromaFormatIDC = 1;
                if plan.profile == Profile::H264Baseline {
                    h264.entropyCodingMode = sys::NV_ENC_H264_ENTROPY_CODING_MODE_CAVLC;
                }
                apply_vui(&mut h264.h264VUIParameters, plan);
            }
            _ => {
                // SAFETY: as above, for HEVC.
                let hevc = unsafe { &mut config.encodeCodecConfig.hevcConfig };
                hevc.idrPeriod = plan.gop;
                hevc.set_repeatSPSPPS(1);
                hevc.set_outputAUD(0);
                hevc.set_chromaFormatIDC(1);
                hevc.set_pixelBitDepthMinus8(if plan.ten_bit { 2 } else { 0 });
                apply_vui(&mut hevc.hevcVUIParameters, plan);
            }
        }

        let mut init = Box::new(sys::NV_ENC_INITIALIZE_PARAMS {
            version: sys::NV_ENC_INITIALIZE_PARAMS_VER,
            encodeGUID: codec,
            presetGUID: preset,
            encodeWidth: plan.width,
            encodeHeight: plan.height,
            darWidth: plan.width,
            darHeight: plan.height,
            frameRateNum: plan.fps.0,
            frameRateDen: plan.fps.1,
            enableEncodeAsync: 0,
            enablePTD: 1,
            maxEncodeWidth: plan.width,
            maxEncodeHeight: plan.height,
            tuningInfo: tuning,
            bufferFormat: buffer_format,
            ..Default::default()
        });
        init.encodeConfig = &mut *config;
        let initialize =
            f(funcs.nvEncInitializeEncoder, "").map_err(|_| missing("nvEncInitializeEncoder"))?;
        // SAFETY: open session; `init` is versioned and points at a live `config`.
        let s = unsafe { initialize(device.encoder, &mut *init) };
        if s != sys::NV_ENC_SUCCESS {
            return Err(create_error("nvEncInitializeEncoder", s));
        }

        let mut engine = NvencEngine {
            inputs: [ptr::null_mut(); FRAMES_IN_FLIGHT],
            outputs: [ptr::null_mut(); FRAMES_IN_FLIGHT],
            pitches: [0; FRAMES_IN_FLIGHT],
            pending: [false; FRAMES_IN_FLIGHT],
            tokens: [0; FRAMES_IN_FLIGHT],
            buffer_format,
            pixel_format: if plan.ten_bit {
                PixelFormat::P010
            } else {
                PixelFormat::Nv12
            },
            width: plan.width,
            height: plan.height,
            init,
            config,
            device,
        };
        let create_input =
            f(funcs.nvEncCreateInputBuffer, "").map_err(|_| missing("nvEncCreateInputBuffer"))?;
        let create_output = f(funcs.nvEncCreateBitstreamBuffer, "")
            .map_err(|_| missing("nvEncCreateBitstreamBuffer"))?;
        for slot in 0..FRAMES_IN_FLIGHT {
            let mut input = sys::NV_ENC_CREATE_INPUT_BUFFER {
                version: sys::NV_ENC_CREATE_INPUT_BUFFER_VER,
                width: plan.width,
                height: plan.height,
                bufferFmt: buffer_format,
                ..Default::default()
            };
            // SAFETY: initialised session; versioned block the call fills. On error
            // the engine's `Drop` destroys the buffers created so far.
            let s = unsafe { create_input(engine.device.encoder, &mut input) };
            if s != sys::NV_ENC_SUCCESS {
                return Err(create_error("nvEncCreateInputBuffer", s));
            }
            engine.inputs[slot] = input.inputBuffer;
            let mut output = sys::NV_ENC_CREATE_BITSTREAM_BUFFER {
                version: sys::NV_ENC_CREATE_BITSTREAM_BUFFER_VER,
                ..Default::default()
            };
            // SAFETY: as above.
            let s = unsafe { create_output(engine.device.encoder, &mut output) };
            if s != sys::NV_ENC_SUCCESS {
                return Err(create_error("nvEncCreateBitstreamBuffer", s));
            }
            engine.outputs[slot] = output.bitstreamBuffer;
        }

        let get_params =
            f(funcs.nvEncGetSequenceParams, "").map_err(|_| missing("nvEncGetSequenceParams"))?;
        let mut extradata = vec![0u8; 1024];
        let mut size = 0u32;
        let mut payload = sys::NV_ENC_SEQUENCE_PARAM_PAYLOAD {
            version: sys::NV_ENC_SEQUENCE_PARAM_PAYLOAD_VER,
            inBufferSize: extradata.len() as u32,
            spsppsBuffer: extradata.as_mut_ptr().cast(),
            outSPSPPSPayloadSize: &mut size,
            ..Default::default()
        };
        // SAFETY: initialised session; the buffer and size pointers are valid for the
        // sizes given.
        let s = unsafe { get_params(engine.device.encoder, &mut payload) };
        if s != sys::NV_ENC_SUCCESS {
            return Err(create_error("nvEncGetSequenceParams", s));
        }
        extradata.truncate((size as usize).min(1024));
        if extradata.is_empty() {
            return Err(CreateError::Device(
                "NVENC returned no parameter sets".into(),
            ));
        }
        drop(_current);
        Ok((engine, extradata))
    }

    fn encoder(&self) -> *mut c_void {
        self.device.encoder
    }

    /// Bytes per row and rows of each plane.
    fn planes(&self) -> [(usize, usize); 2] {
        plane_sizes(self.pixel_format, self.width, self.height).unwrap_or_default()
    }

    /// Reads back and unlocks the bitstream of `slot`.
    fn lock_output(&mut self, slot: usize, wait: bool) -> Result<Option<Output>, EngineError> {
        let funcs = &self.device.api.nvenc.0;
        let lock = f(funcs.nvEncLockBitstream, "nvEncLockBitstream")?;
        let unlock = f(funcs.nvEncUnlockBitstream, "nvEncUnlockBitstream")?;
        let mut params = sys::NV_ENC_LOCK_BITSTREAM {
            version: sys::NV_ENC_LOCK_BITSTREAM_VER,
            outputBitstream: self.outputs[slot],
            ..Default::default()
        };
        // DIAG (temporary): RBS_NVENC_DIAG_BLOCK=1 never uses doNotWait.
        let block = std::env::var("RBS_NVENC_DIAG_BLOCK").is_ok_and(|v| v == "1");
        params.set_doNotWait(u32::from(!wait && !block));
        eprintln!("[nvenc-diag] lock slot {slot} wait {wait} block {block}");
        // SAFETY: the bitstream buffer belongs to this session and has a picture
        // queued (`pending`).
        let s = unsafe { lock(self.encoder(), &mut params) };
        if s == sys::NV_ENC_ERR_LOCK_BUSY && !wait {
            return Ok(None);
        }
        status("nvEncLockBitstream", s)?;
        let len = params.bitstreamSizeInBytes as usize;
        eprintln!("[nvenc-diag] lock -> {s} len {len} ptr {:?} hwstatus {}", params.bitstreamBufferPtr, params.hwEncodeStatus);
        let data = if len == 0 || params.bitstreamBufferPtr.is_null() {
            Vec::new()
        } else {
            // SAFETY: while locked, `bitstreamBufferPtr` points at
            // `bitstreamSizeInBytes` bytes of encoded data owned by NVENC.
            unsafe { std::slice::from_raw_parts(params.bitstreamBufferPtr.cast::<u8>(), len) }
                .to_vec()
        };
        let keyframe = params.pictureType == sys::NV_ENC_PIC_TYPE_IDR;
        let token = params.outputTimeStamp;
        // SAFETY: the buffer was locked above.
        let s = unsafe { unlock(self.encoder(), self.outputs[slot]) };
        self.pending[slot] = false;
        status("nvEncUnlockBitstream", s)?;
        if data.is_empty() {
            return Err(EngineError::Broken("NVENC returned an empty picture"));
        }
        Ok(Some(Output {
            data,
            keyframe,
            token,
        }))
    }

    fn send_eos(&mut self) -> Result<(), EngineError> {
        let encode = f(
            self.device.api.nvenc.0.nvEncEncodePicture,
            "nvEncEncodePicture",
        )?;
        let mut pic = sys::NV_ENC_PIC_PARAMS {
            version: sys::NV_ENC_PIC_PARAMS_VER,
            encodePicFlags: sys::NV_ENC_PIC_FLAG_EOS,
            ..Default::default()
        };
        // SAFETY: an end-of-stream picture carries no buffers.
        status("nvEncEncodePicture", unsafe {
            encode(self.encoder(), &mut pic)
        })
    }
}

impl Engine for NvencEngine {
    fn slots(&self) -> usize {
        FRAMES_IN_FLIGHT
    }

    fn upload(&mut self, slot: usize, frame: &SystemFrame) -> Result<(), EngineError> {
        let funcs = &self.device.api.nvenc.0;
        let lock = f(funcs.nvEncLockInputBuffer, "nvEncLockInputBuffer")?;
        let unlock = f(funcs.nvEncUnlockInputBuffer, "nvEncUnlockInputBuffer")?;
        let planes = self.planes();
        let _current = self.device.push()?;
        let mut params = sys::NV_ENC_LOCK_INPUT_BUFFER {
            version: sys::NV_ENC_LOCK_INPUT_BUFFER_VER,
            inputBuffer: self.inputs[slot],
            ..Default::default()
        };
        // SAFETY: the input buffer belongs to this session and is not queued (the
        // session never uploads into a slot in flight).
        status("nvEncLockInputBuffer", unsafe {
            lock(self.encoder(), &mut params)
        })?;
        let base = params.bufferDataPtr.cast::<u8>();
        let pitch = params.pitch as usize;
        let [(_, luma_rows), (_, chroma_rows)] = planes;
        let copied = !base.is_null() && {
            // The chroma plane follows `height` rows of luma at the same pitch.
            let luma_len = pitch * self.height as usize;
            // SAFETY: while locked, the buffer holds `height` luma rows and then the
            // chroma rows, `pitch` bytes each, and nothing else refers to it; the
            // two slices do not overlap. `luma_rows == height` for 4:2:0.
            let (luma, chroma) = unsafe {
                (
                    std::slice::from_raw_parts_mut(base, pitch * luma_rows),
                    std::slice::from_raw_parts_mut(base.add(luma_len), pitch * chroma_rows),
                )
            };
            copy_420(
                frame,
                [
                    DstPlane { data: luma, pitch },
                    DstPlane {
                        data: chroma,
                        pitch,
                    },
                ],
            )
        };
        // SAFETY: locked above.
        let s = unsafe { unlock(self.encoder(), self.inputs[slot]) };
        status("nvEncUnlockInputBuffer", s)?;
        if !copied {
            return Err(EngineError::Broken("frame planes do not match the session"));
        }
        self.pitches[slot] = params.pitch;
        Ok(())
    }

    fn encode(&mut self, slot: usize, token: u64, idr: bool) -> Result<(), EngineError> {
        let encode = f(
            self.device.api.nvenc.0.nvEncEncodePicture,
            "nvEncEncodePicture",
        )?;
        let _current = self.device.push()?;
        let mut pic = sys::NV_ENC_PIC_PARAMS {
            version: sys::NV_ENC_PIC_PARAMS_VER,
            inputWidth: self.width,
            inputHeight: self.height,
            inputPitch: self.pitches[slot],
            encodePicFlags: if idr {
                sys::NV_ENC_PIC_FLAG_FORCEIDR | sys::NV_ENC_PIC_FLAG_OUTPUT_SPSPPS
            } else {
                0
            },
            frameIdx: token as u32,
            inputTimeStamp: token,
            inputBuffer: self.inputs[slot],
            outputBitstream: self.outputs[slot],
            bufferFmt: self.buffer_format,
            pictureStruct: sys::NV_ENC_PIC_STRUCT_FRAME,
            ..Default::default()
        };
        // SAFETY: both buffers belong to this session; the input holds the uploaded
        // frame and neither is queued.
        let s = unsafe { encode(self.encoder(), &mut pic) };
        if s == sys::NV_ENC_ERR_NEED_MORE_INPUT {
            // Only happens when NVENC reorders or looks ahead, which the session
            // configuration rules out. The picture is still queued in this slot's
            // bitstream; `Drop` drains it after end-of-stream.
            self.pending[slot] = true;
            self.tokens[slot] = token;
            return Err(EngineError::Broken("NVENC buffered a frame (reordering)"));
        }
        status("nvEncEncodePicture", s)?;
        self.pending[slot] = true;
        self.tokens[slot] = token;
        Ok(())
    }

    fn output(&mut self, slot: usize, wait: bool) -> Result<Option<Output>, EngineError> {
        let _current = self.device.push()?;
        self.lock_output(slot, wait)
    }

    fn end_of_stream(&mut self) -> Result<(), EngineError> {
        let _current = self.device.push()?;
        self.send_eos()
    }

    fn set_rate_control(&mut self, rc: Rc) -> Result<(), EngineError> {
        let reconfigure = f(
            self.device.api.nvenc.0.nvEncReconfigureEncoder,
            "nvEncReconfigureEncoder",
        )?;
        let _current = self.device.push()?;
        let mut config = Box::new(*self.config);
        apply_rc(&mut config.rcParams, rc);
        let mut params = Box::new(sys::NV_ENC_RECONFIGURE_PARAMS {
            version: sys::NV_ENC_RECONFIGURE_PARAMS_VER,
            reInitEncodeParams: *self.init,
            ..Default::default()
        });
        params.reInitEncodeParams.encodeConfig = &mut *config;
        // SAFETY: initialised session; `params` is versioned and points at a live
        // configuration that differs from the current one only in rate control.
        let s = unsafe { reconfigure(self.encoder(), &mut *params) };
        if matches!(
            s,
            sys::NV_ENC_ERR_UNSUPPORTED_PARAM | sys::NV_ENC_ERR_INVALID_PARAM
        ) {
            // The device refused the new rate; the session keeps the old one.
            return Err(EngineError::Unsupported);
        }
        status("nvEncReconfigureEncoder", s)?;
        *self.config = *config;
        self.init.encodeConfig = &mut *self.config;
        Ok(())
    }
}

impl Drop for NvencEngine {
    fn drop(&mut self) {
        let Ok(_current) = self.device.push() else {
            return;
        };
        // Let queued pictures finish before their buffers go away.
        if self.pending.iter().any(|p| *p) && self.send_eos().is_ok() {
            // Oldest first: at most `FRAMES_IN_FLIGHT` selection passes, no allocation.
            while let Some(slot) = (0..FRAMES_IN_FLIGHT)
                .filter(|s| self.pending[*s])
                .min_by_key(|s| self.tokens[*s])
            {
                if self.lock_output(slot, true).is_err() {
                    // `lock_output` clears `pending` only once the lock succeeded.
                    self.pending[slot] = false;
                }
            }
        }
        let funcs = &self.device.api.nvenc.0;
        for input in self.inputs.iter().filter(|p| !p.is_null()) {
            if let Some(destroy) = funcs.nvEncDestroyInputBuffer {
                // SAFETY: created by this session, no longer queued, destroyed once.
                let _ = unsafe { destroy(self.device.encoder, *input) };
            }
        }
        for output in self.outputs.iter().filter(|p| !p.is_null()) {
            if let Some(destroy) = funcs.nvEncDestroyBitstreamBuffer {
                // SAFETY: as above.
                let _ = unsafe { destroy(self.device.encoder, *output) };
            }
        }
        // `device` drops after this, destroying the session and the context.
    }
}
