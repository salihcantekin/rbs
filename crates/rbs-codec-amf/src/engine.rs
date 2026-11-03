//! The AMF engine: one AMF context (D3D11 on Windows, Vulkan on Linux) and one
//! encoder component in it, fed from host-memory surfaces that this engine
//! allocates, one per slot.
//!
//! AMF keeps its own queue: `SubmitInput` takes a surface and `QueryOutput` returns
//! finished pictures in input order (no B-frames). The engine uses the slot only for
//! its surface; `output` returns the next picture of the queue and the session
//! checks its token. AMF objects may be used from any thread as long as calls are
//! not concurrent (`&mut self`).

use std::ptr::{self, NonNull};
use std::time::{Duration, Instant};

use rbs_codec::CreateError;
use rbs_codec_hw::copy::{DstPlane, copy_420};
use rbs_codec_hw::device::{CodecCaps, DeviceCaps, Profiles};
use rbs_codec_hw::plan::{Plan, Rc};
use rbs_codec_hw::session::{Engine, EngineError, Output};
use rbs_core::media::{CodecId, SystemFrame};

use crate::FRAMES_IN_FLIGHT;
use crate::loader::{Api, api};
use crate::params::{self, Value, names};
use crate::sys::{self, Name, Names};

/// Longest wait for one picture. A picture is waited for only when it was
/// submitted `FRAMES_IN_FLIGHT` frames earlier, at `flush` and in `Drop`.
const WAIT: Duration = Duration::from_secs(10);

/// Longest wait for AMF to release an input surface whose output was read.
const RELEASE_WAIT: Duration = Duration::from_millis(100);

/// Calls method `$m` of the AMF interface `$p`: `Ok(result)`, or `Err(name)` when
/// the vtable has no such entry. Must be used inside `unsafe` with a live `$p`.
macro_rules! vcall {
    ($p:expr, $m:ident $(, $a:expr)* $(,)?) => {{
        let p = $p;
        match (*(*p).pVtbl).$m {
            Some(f) => Ok(f(p $(, $a)*)),
            None => Err(stringify!($m)),
        }
    }};
}

/// An AMF interface whose reference this crate owns.
trait Interface {
    /// Releases one reference.
    ///
    /// # Safety
    ///
    /// `this` must be a live interface with a reference owned by the caller.
    unsafe fn release(this: *mut Self);
}

macro_rules! interface {
    ($($t:ident),*) => {$(
        impl Interface for sys::$t {
            unsafe fn release(this: *mut Self) {
                // SAFETY: the caller owns a reference of the live interface `this`.
                let _ = unsafe { vcall!(this, Release) };
            }
        }
    )*};
}
interface!(
    AMFContext,
    AMFContext1,
    AMFComponent,
    AMFSurface,
    AMFData,
    AMFBuffer,
    AMFCaps,
    AMFIOCaps
);

/// One owned reference to an AMF interface, released on drop.
struct Com<T: Interface>(NonNull<T>);

impl<T: Interface> Com<T> {
    /// Takes ownership of the reference `p` (from a `Create`, `Alloc`, `Query` or
    /// `Get` call that returned a referenced interface). `None` for null.
    fn new(p: *mut T) -> Option<Self> {
        NonNull::new(p).map(Com)
    }

    fn ptr(&self) -> *mut T {
        self.0.as_ptr()
    }
}

impl<T: Interface> Drop for Com<T> {
    fn drop(&mut self) {
        // SAFETY: this value owns one reference of the live interface.
        unsafe { T::release(self.0.as_ptr()) };
    }
}

fn int(v: i64) -> sys::AMFVariantStruct {
    sys::AMFVariantStruct {
        type_: sys::AMF_VARIANT_INT64,
        __bindgen_anon_1: sys::AMFVariantStruct__bindgen_ty_1 { int64Value: v },
    }
}

fn variant(v: Value) -> sys::AMFVariantStruct {
    let value = match v {
        Value::Int(i) => return int(i),
        Value::Bool(b) => {
            return sys::AMFVariantStruct {
                type_: sys::AMF_VARIANT_BOOL,
                __bindgen_anon_1: sys::AMFVariantStruct__bindgen_ty_1 {
                    boolValue: u8::from(b),
                },
            };
        }
        Value::Size(width, height) => (
            sys::AMF_VARIANT_SIZE,
            sys::AMFVariantStruct__bindgen_ty_1 {
                sizeValue: sys::AMFSize { width, height },
            },
        ),
        Value::Rate(num, den) => (
            sys::AMF_VARIANT_RATE,
            sys::AMFVariantStruct__bindgen_ty_1 {
                rateValue: sys::AMFRate { num, den },
            },
        ),
    };
    sys::AMFVariantStruct {
        type_: value.0,
        __bindgen_anon_1: value.1,
    }
}

/// Sets property `$name` (a [`Name`]) of interface `$p` to the variant `$v`; the
/// `AMF_RESULT`, or `Err` for a missing vtable entry. Inside `unsafe`.
macro_rules! set_property {
    ($p:expr, $name:expr, $v:expr) => {
        vcall!($p, SetProperty, $name.as_ptr(), $v)
    };
}

/// Reads an integer property of interface `$p`; `None` when it is missing or not
/// an integer. Inside `unsafe`.
macro_rules! int_property {
    ($p:expr, $name:expr) => {{
        let mut v = sys::AMFVariantStruct::default();
        match vcall!($p, GetProperty, $name.as_ptr(), &mut v) {
            Ok(sys::AMF_OK) if v.type_ == sys::AMF_VARIANT_INT64 => {
                Some(v.__bindgen_anon_1.int64Value)
            }
            _ => None,
        }
    }};
}

/// Maps a failed AMF result to the engine's error.
fn engine_error(call: &'static str, r: sys::AMF_RESULT) -> EngineError {
    match r {
        sys::AMF_INPUT_FULL => EngineError::Busy,
        sys::AMF_NO_DEVICE | sys::AMF_DIRECTX_FAILED | sys::AMF_VULKAN_FAILED => {
            EngineError::DeviceLost
        }
        other => EngineError::Failed(call, i64::from(other)),
    }
}

fn create_error(call: &str, r: sys::AMF_RESULT) -> CreateError {
    match r {
        sys::AMF_NOT_SUPPORTED
        | sys::AMF_INVALID_ARG
        | sys::AMF_OUT_OF_RANGE
        | sys::AMF_INVALID_FORMAT
        | sys::AMF_INVALID_RESOLUTION
        | sys::AMF_CODEC_NOT_SUPPORTED
        | sys::AMF_SURFACE_FORMAT_NOT_SUPPORTED
        | sys::AMF_ENCODER_NOT_PRESENT => CreateError::Unsupported {
            what: format!("{call} refused the configuration (result {r})"),
        },
        // No AMD device behind the context (for example a leftover runtime on a
        // machine without an AMD GPU, or one that is not the default adapter):
        // definitive, so the probe caches an empty set.
        sys::AMF_NO_DEVICE | sys::AMF_DIRECTX_FAILED | sys::AMF_VULKAN_FAILED => {
            CreateError::Unsupported {
                what: format!("{call}: no AMD device (result {r})"),
            }
        }
        sys::AMF_OUT_OF_MEMORY => CreateError::ResourceExhausted,
        _ => CreateError::Device(format!("{call} failed with result {r}")),
    }
}

fn missing(name: &'static str) -> CreateError {
    CreateError::Device(format!("AMF vtable entry {name} missing"))
}

/// An AMF context on the GPU (D3D11 on Windows, Vulkan on Linux). Dropping
/// terminates and releases it.
struct Context {
    context: Com<sys::AMFContext>,
}

impl Context {
    fn open(api: &'static Api) -> Result<Self, CreateError> {
        let mut raw: *mut sys::AMFContext = ptr::null_mut();
        // SAFETY: the runtime's live factory and a valid out-pointer.
        let r = unsafe { vcall!(api.factory, CreateContext, &mut raw) }.map_err(missing)?;
        let context = Com::new(raw)
            .filter(|_| r == sys::AMF_OK)
            .ok_or_else(|| create_error("CreateContext", r))?;
        let context = Context { context };
        #[cfg(target_os = "windows")]
        {
            // SAFETY: a live, uninitialised context; null asks AMF to create its own
            // D3D11 device on the default adapter.
            let r = unsafe {
                vcall!(
                    context.context.ptr(),
                    InitDX11,
                    ptr::null_mut(),
                    sys::AMF_DX11_0
                )
            }
            .map_err(missing)?;
            if r != sys::AMF_OK {
                return Err(create_error("InitDX11", r));
            }
        }
        #[cfg(target_os = "linux")]
        {
            let mut raw1: *mut core::ffi::c_void = ptr::null_mut();
            // SAFETY: a live context, a valid IID and out-pointer.
            let r = unsafe {
                vcall!(
                    context.context.ptr(),
                    QueryInterface,
                    &sys::IID_AMF_CONTEXT1,
                    &mut raw1
                )
            }
            .map_err(missing)?;
            let context1 = Com::new(raw1.cast::<sys::AMFContext1>())
                .filter(|_| r == sys::AMF_OK)
                .ok_or_else(|| create_error("QueryInterface(AMFContext1)", r))?;
            // SAFETY: a live, uninitialised context; null asks AMF to create its own
            // Vulkan device.
            let r =
                unsafe { vcall!(context1.ptr(), InitVulkan, ptr::null_mut()) }.map_err(missing)?;
            if r != sys::AMF_OK {
                return Err(create_error("InitVulkan", r));
            }
        }
        Ok(context)
    }

    fn ptr(&self) -> *mut sys::AMFContext {
        self.context.ptr()
    }

    /// Creates the encoder component of `names`.
    fn component(
        &self,
        api: &'static Api,
        names: &Names,
    ) -> Result<Com<sys::AMFComponent>, CreateError> {
        let mut raw: *mut sys::AMFComponent = ptr::null_mut();
        // SAFETY: the live factory, a live context, a NUL-terminated wide id and a
        // valid out-pointer.
        let r = unsafe {
            vcall!(
                api.factory,
                CreateComponent,
                self.ptr(),
                names.component.as_ptr(),
                &mut raw
            )
        }
        .map_err(missing)?;
        Com::new(raw)
            .filter(|_| r == sys::AMF_OK)
            .ok_or_else(|| create_error("CreateComponent", r))
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        // SAFETY: a live context; its components were terminated and released
        // before (`Encoder` drops its component first).
        let _ = unsafe { vcall!(self.ptr(), Terminate) };
        // `context` releases the last reference after this.
    }
}

/// What the encoder of `codec` can do, or `None` when it is absent.
fn codec_caps(api: &'static Api, context: &Context, codec: CodecId) -> Option<CodecCaps> {
    let n = names(codec);
    let component = context.component(api, n).ok()?;
    let mut raw: *mut sys::AMFCaps = ptr::null_mut();
    // SAFETY: a live component and a valid out-pointer.
    let r = unsafe { vcall!(component.ptr(), GetCaps, &mut raw) }.ok()?;
    let caps = Com::new(raw).filter(|_| r == sys::AMF_OK)?;
    // SAFETY: live caps.
    let max_profile = unsafe { int_property!(caps.ptr(), n.max_profile_cap) };
    // SAFETY: live caps.
    let throughput = unsafe { int_property!(caps.ptr(), n.throughput_cap) };
    let mut raw: *mut sys::AMFIOCaps = ptr::null_mut();
    // SAFETY: live caps and a valid out-pointer.
    let r = unsafe { vcall!(caps.ptr(), GetInputCaps, &mut raw) }.ok()?;
    let input = Com::new(raw).filter(|_| r == sys::AMF_OK)?;
    let (mut min_w, mut max_w, mut min_h, mut max_h) = (0i32, 0i32, 0i32, 0i32);
    // SAFETY: live input caps and valid out-pointers.
    unsafe {
        vcall!(input.ptr(), GetWidthRange, &mut min_w, &mut max_w).ok()?;
        vcall!(input.ptr(), GetHeightRange, &mut min_h, &mut max_h).ok()?;
    }
    // SAFETY: live input caps.
    let formats = unsafe { vcall!(input.ptr(), GetNumOfFormats) }.unwrap_or(0);
    let mut nv12 = false;
    let mut p010 = false;
    for i in 0..formats.min(64) {
        let mut format: sys::AMF_SURFACE_FORMAT = 0;
        let mut native: sys::amf_bool = 0;
        // SAFETY: an index below the count and valid out-pointers.
        if unsafe { vcall!(input.ptr(), GetFormatAt, i, &mut format, &mut native) }
            == Ok(sys::AMF_OK)
        {
            nv12 |= format == sys::AMF_SURFACE_NV12;
            p010 |= format == sys::AMF_SURFACE_P010;
        }
    }
    if !nv12 {
        return None;
    }
    let profiles = if codec == CodecId::H264 {
        Profiles::H264_ALL
    } else if p010
        && max_profile.is_some_and(|p| p >= i64::from(sys::AMF_VIDEO_ENCODER_HEVC_PROFILE_MAIN_10))
    {
        Profiles::HEVC_ALL
    } else {
        Profiles::HEVC_MAIN
    };
    // Even sides only: round the minimum up and the maximum down.
    let even_up = |v: i32| u32::try_from(v).unwrap_or(0).next_multiple_of(2);
    let even_down = |v: i32| u32::try_from(v).unwrap_or(0) & !1;
    let c = CodecCaps {
        codec,
        width: (even_up(min_w), even_down(max_w)),
        height: (even_up(min_h), even_down(max_h)),
        mb_per_sec: throughput.and_then(|t| u64::try_from(t).ok()).unwrap_or(0),
        profiles,
        // Every AMF encoder has CBR, peak-constrained VBR and constant QP, and takes
        // rate changes while running.
        cbr: true,
        vbr: true,
        quality: true,
        dynamic_bitrate: true,
    };
    c.usable().then_some(c)
}

/// Reads what the AMF encoders of the default GPU do. `Ok` with no codecs when the
/// runtime is missing or too old or there is no AMD GPU; `Err` only when probing
/// itself failed now and may work later.
pub(crate) fn probe() -> Result<DeviceCaps, String> {
    let Ok(api) = api() else {
        return Ok(DeviceCaps::default());
    };
    let context = match Context::open(api) {
        Ok(c) => c,
        Err(CreateError::Unsupported { .. }) => return Ok(DeviceCaps::default()),
        Err(e) => return Err(e.to_string()),
    };
    let codecs = [CodecId::H264, CodecId::Hevc]
        .into_iter()
        .filter_map(|c| codec_caps(api, &context, c))
        .collect();
    Ok(DeviceCaps {
        name: "AMD AMF".into(),
        codecs,
    })
}

/// One AMF encoder with its surfaces.
pub(crate) struct AmfEngine {
    /// One host-memory surface per slot.
    surfaces: Vec<Surface>,
    /// Declared after `surfaces` and before `context`: dropped (terminated) after
    /// the surfaces are released and before the context goes.
    component: Option<Com<sys::AMFComponent>>,
    context: Context,
    names: &'static Names,
    codec: CodecId,
    /// Frames between engine-forced IDR pictures.
    gop: u32,
    /// Frames since the last IDR picture.
    since_idr: u32,
    /// 100-ns ticks per frame, for surface timestamps.
    duration: i64,
    /// `Drain` was called: the next input needs a `Flush` first.
    drained: bool,
    /// `Drain` returned `INPUT_FULL`: retried while waiting for output.
    drain_pending: bool,
    /// Rows of the luma and chroma planes.
    rows: [usize; 2],
}

/// An input surface and its reference count when AMF does not hold it.
struct Surface {
    surface: Com<sys::AMFSurface>,
    idle: i64,
}

/// The current reference count of `surface` (AMF has no getter: `Acquire` returns
/// the count after adding one, which is released right away).
fn references(surface: *mut sys::AMFSurface) -> Result<i64, &'static str> {
    // SAFETY: a live surface owned by this engine; the extra reference is released
    // right away.
    unsafe {
        let n = vcall!(surface, Acquire)?;
        let _ = vcall!(surface, Release);
        #[allow(clippy::useless_conversion)] // `amf_long` is 32 bits on Windows.
        Ok(i64::from(n) - 1)
    }
}

// SAFETY: the raw pointers are AMF interfaces owned by this engine. AMF objects may
// be used from any thread as long as calls are not concurrent, which `&mut self`
// ensures.
unsafe impl Send for AmfEngine {}

impl AmfEngine {
    /// Opens an encoder for `plan`. Returns the engine and the Annex B parameter
    /// sets (extradata).
    pub(crate) fn open(plan: &Plan) -> Result<(Self, Vec<u8>), CreateError> {
        let api = api().map_err(|e| CreateError::Unsupported { what: e.into() })?;
        let properties =
            params::properties(plan).map_err(|what| CreateError::Unsupported { what })?;
        let n = names(plan.codec);
        let context = Context::open(api)?;
        let component = context.component(api, n)?;
        // From here every error path drops the engine, which terminates the
        // component before releasing it.
        let mut engine = AmfEngine {
            surfaces: Vec::with_capacity(FRAMES_IN_FLIGHT),
            component: Some(component),
            context,
            names: n,
            codec: plan.codec,
            gop: plan.gop.max(1),
            since_idr: 0,
            duration: params::frame_duration(plan.fps),
            drained: false,
            drain_pending: false,
            rows: [plan.height as usize, plan.height.div_ceil(2) as usize],
        };
        let component = engine.component();
        for (name, value) in &properties {
            // SAFETY: a live, uninitialised component and a NUL-terminated name.
            let r = unsafe { set_property!(component, name, variant(*value)) }.map_err(missing)?;
            if r != sys::AMF_OK && !params::optional(name) {
                return Err(create_error(
                    &format!("SetProperty({})", wide_name(name)),
                    r,
                ));
            }
        }
        if plan.codec == CodecId::H264 {
            // Without B-frames the output is in input order; an encoder that
            // refused the setting must still report 0 (or nothing: no B-frames).
            // SAFETY: a live component and a NUL-terminated name.
            let pattern = unsafe { int_property!(component, sys::H264_B_PIC_PATTERN) };
            if pattern.is_some_and(|p| p != 0) {
                return Err(CreateError::Unsupported {
                    what: "the AMF H.264 encoder keeps B-frames on".into(),
                });
            }
        }
        let format = if plan.ten_bit {
            sys::AMF_SURFACE_P010
        } else {
            sys::AMF_SURFACE_NV12
        };
        let side = |v: u32| i32::try_from(v).unwrap_or(i32::MAX);
        // SAFETY: a live component with its properties set.
        let r = unsafe { vcall!(component, Init, format, side(plan.width), side(plan.height)) }
            .map_err(missing)?;
        if r != sys::AMF_OK {
            return Err(create_error("Init", r));
        }
        let extradata = engine.extradata()?;
        for _ in 0..FRAMES_IN_FLIGHT {
            let mut raw: *mut sys::AMFSurface = ptr::null_mut();
            // SAFETY: a live context and a valid out-pointer.
            let r = unsafe {
                vcall!(
                    engine.context.ptr(),
                    AllocSurface,
                    sys::AMF_MEMORY_HOST,
                    format,
                    side(plan.width),
                    side(plan.height),
                    &mut raw
                )
            }
            .map_err(missing)?;
            let surface = Com::new(raw)
                .filter(|_| r == sys::AMF_OK)
                .ok_or_else(|| create_error("AllocSurface", r))?;
            // The references of a surface nobody else uses: ours plus any the
            // runtime keeps for its own bookkeeping.
            let idle = references(surface.ptr())
                .map_err(|_| CreateError::Device("AMF surface without Acquire".into()))?;
            engine.surfaces.push(Surface { surface, idle });
        }
        Ok((engine, extradata))
    }

    /// Sends a pending `Drain`. `INPUT_FULL` keeps it pending (retried while
    /// output is read; the queued pictures come out without it).
    fn try_drain(&mut self) -> Result<(), EngineError> {
        if !self.drain_pending {
            return Ok(());
        }
        // SAFETY: an initialised component.
        let r =
            unsafe { vcall!(self.component(), Drain) }.map_err(|m| EngineError::Failed(m, -1))?;
        match r {
            sys::AMF_OK => {
                self.drain_pending = false;
                Ok(())
            }
            sys::AMF_INPUT_FULL => Ok(()),
            other => Err(engine_error("Drain", other)),
        }
    }

    fn component(&self) -> *mut sys::AMFComponent {
        // `component` is `Some` from `open` until `Drop`.
        self.component.as_ref().map_or(ptr::null_mut(), Com::ptr)
    }

    /// The parameter sets the encoder reports after `Init`, as Annex B.
    fn extradata(&self) -> Result<Vec<u8>, CreateError> {
        let mut v = sys::AMFVariantStruct::default();
        // SAFETY: an initialised component, a NUL-terminated name and a valid
        // out-variant.
        let r = unsafe {
            vcall!(
                self.component(),
                GetProperty,
                self.names.extradata.as_ptr(),
                &mut v
            )
        }
        .map_err(missing)?;
        if r != sys::AMF_OK || v.type_ != sys::AMF_VARIANT_INTERFACE {
            return Err(CreateError::Device("AMF returned no parameter sets".into()));
        }
        // SAFETY: the variant's type (checked above) says this member is set.
        let interface = unsafe { v.__bindgen_anon_1.pInterface };
        if interface.is_null() {
            return Err(CreateError::Device("AMF returned no parameter sets".into()));
        }
        // The variant holds a reference to the interface: released below.
        struct Held(*mut sys::AMFInterface);
        impl Drop for Held {
            fn drop(&mut self) {
                // SAFETY: the reference `GetProperty` added for the variant.
                let _ = unsafe { vcall!(self.0, Release) };
            }
        }
        let held = Held(interface);
        let mut raw: *mut core::ffi::c_void = ptr::null_mut();
        // SAFETY: a live interface, a valid IID and out-pointer.
        let r = unsafe { vcall!(held.0, QueryInterface, &sys::IID_AMF_BUFFER, &mut raw) }
            .map_err(missing)?;
        let buffer = Com::new(raw.cast::<sys::AMFBuffer>())
            .filter(|_| r == sys::AMF_OK)
            .ok_or_else(|| CreateError::Device("AMF parameter sets are not a buffer".into()))?;
        let bytes = buffer_bytes(&buffer).map_err(|e| CreateError::Device(e.into()))?;
        if bytes.is_empty() {
            return Err(CreateError::Device("AMF returned no parameter sets".into()));
        }
        Ok(bytes)
    }

    fn set_surface(
        &self,
        surface: *mut sys::AMFSurface,
        name: Name,
        value: Value,
    ) -> Result<(), EngineError> {
        // SAFETY: a live surface owned by this engine and a NUL-terminated name.
        let r = unsafe { set_property!(surface, name, variant(value)) }
            .map_err(|m| EngineError::Failed(m, -1))?;
        if r != sys::AMF_OK {
            return Err(EngineError::Failed("SetProperty", i64::from(r)));
        }
        Ok(())
    }

    /// Marks `surface` as a forced IDR picture with parameter sets, or clears that.
    fn mark_idr(&self, surface: *mut sys::AMFSurface, idr: bool) -> Result<(), EngineError> {
        let picture_type = if idr {
            // 2 is IDR for both encoders.
            i64::from(sys::AMF_VIDEO_ENCODER_PICTURE_TYPE_IDR)
        } else {
            i64::from(sys::AMF_VIDEO_ENCODER_PICTURE_TYPE_NONE)
        };
        self.set_surface(
            surface,
            self.names.force_picture_type,
            Value::Int(picture_type),
        )?;
        if self.codec == CodecId::Hevc {
            self.set_surface(surface, sys::HEVC_INSERT_HEADER, Value::Bool(idr))
        } else {
            self.set_surface(surface, sys::H264_INSERT_SPS, Value::Bool(idr))?;
            self.set_surface(surface, sys::H264_INSERT_PPS, Value::Bool(idr))
        }
    }
}

/// The bytes of an AMF buffer.
fn buffer_bytes(buffer: &Com<sys::AMFBuffer>) -> Result<Vec<u8>, &'static str> {
    // SAFETY: a live buffer.
    let (data, size) = unsafe {
        (
            vcall!(buffer.ptr(), GetNative)?,
            vcall!(buffer.ptr(), GetSize)?,
        )
    };
    if data.is_null() || size == 0 {
        return Ok(Vec::new());
    }
    // SAFETY: a host buffer of `size` bytes at `data`, alive while `buffer` holds
    // its reference; copied out before that is released.
    Ok(unsafe { std::slice::from_raw_parts(data.cast::<u8>(), size) }.to_vec())
}

/// A wide name as text, for error messages.
fn wide_name(name: Name) -> String {
    name.iter()
        .take_while(|c| **c != 0)
        .map(|c| char::from(u8::try_from(*c).unwrap_or(b'?')))
        .collect()
}

impl Engine for AmfEngine {
    fn slots(&self) -> usize {
        FRAMES_IN_FLIGHT
    }

    fn upload(&mut self, slot: usize, frame: &SystemFrame) -> Result<(), EngineError> {
        let surface = self.surfaces[slot].surface.ptr();
        let idle = self.surfaces[slot].idle;
        // The session reuses a slot only after its output was read, so AMF is done
        // with the surface or about to release it: wait briefly, never `Busy`
        // (draining cannot free a slot that is not in flight).
        let deadline = Instant::now() + RELEASE_WAIT;
        while references(surface).map_err(|m| EngineError::Failed(m, -1))? > idle {
            if Instant::now() >= deadline {
                return Err(EngineError::Broken(
                    "AMF kept an input surface after its output",
                ));
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        // DIAG (temporary)
        let mem = unsafe { vcall!(surface, GetMemoryType) }.unwrap_or(999);
        eprintln!("[amf-diag] upload slot {slot} refs {:?} idle {idle} memtype {mem}", references(surface));
        let mut planes: [(*mut u8, usize, usize); 2] = [(ptr::null_mut(), 0, 0); 2];
        for (i, p) in planes.iter_mut().enumerate() {
            // SAFETY: a live surface; the plane belongs to it (no reference added)
            // and lives as long as it.
            let plane = unsafe { vcall!(surface, GetPlaneAt, i) }
                .map_err(|m| EngineError::Failed(m, -1))?;
            if plane.is_null() {
                return Err(EngineError::Broken("AMF surface without its planes"));
            }
            // SAFETY: a live plane.
            let (native, pitch, rows) = unsafe {
                (
                    vcall!(plane, GetNative).unwrap_or(ptr::null_mut()),
                    vcall!(plane, GetHPitch).unwrap_or(0),
                    vcall!(plane, GetVPitch).unwrap_or(0),
                )
            };
            eprintln!("[amf-diag]   plane {i} native {native:?} pitch {pitch} rows {rows}");
            *p = (
                native.cast(),
                usize::try_from(pitch).unwrap_or(0),
                usize::try_from(rows).unwrap_or(0),
            );
        }
        // Only the rows of the picture are written: the vertical pitch must cover
        // them, and the slices never reach past them (whatever `VPitch` a plane
        // reports).
        if planes
            .iter()
            .zip(self.rows)
            .any(|((p, pitch, rows), needed)| p.is_null() || *pitch == 0 || *rows < needed)
        {
            return Err(EngineError::Broken("AMF surface plane without memory"));
        }
        let [(y, y_pitch, _), (uv, uv_pitch, _)] = planes;
        let [y_rows, uv_rows] = self.rows;
        // SAFETY: host planes of at least `pitch * VPitch` bytes each, and `VPitch`
        // covers the rows written (checked above); owned by the surface, which AMF
        // no longer uses (checked above); the two planes do not overlap.
        let (luma, chroma) = unsafe {
            (
                std::slice::from_raw_parts_mut(y, y_pitch * y_rows),
                std::slice::from_raw_parts_mut(uv, uv_pitch * uv_rows),
            )
        };
        if !copy_420(
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
        ) {
            return Err(EngineError::Broken("frame planes do not match the session"));
        }
        Ok(())
    }

    fn encode(&mut self, slot: usize, token: u64, idr: bool) -> Result<(), EngineError> {
        let component = self.component();
        if self.drained {
            // SAFETY: an initialised component whose drained output was all read.
            let r = unsafe { vcall!(component, Flush) }.map_err(|m| EngineError::Failed(m, -1))?;
            if r != sys::AMF_OK {
                return Err(engine_error("Flush", r));
            }
            self.drained = false;
            self.drain_pending = false;
        }
        let keyframe = idr || self.since_idr + 1 >= self.gop;
        let surface = self.surfaces[slot].surface.ptr();
        let stamp = i64::try_from(token).unwrap_or(i64::MAX);
        self.set_surface(surface, sys::TOKEN, Value::Int(stamp))?;
        self.mark_idr(surface, keyframe)?;
        // SAFETY: a live surface owned by this engine.
        unsafe {
            let _ = vcall!(surface, SetPts, stamp.saturating_mul(self.duration));
            let _ = vcall!(surface, SetDuration, self.duration);
        }
        // SAFETY: an initialised component; a surface is an `AMFData` (its vtable
        // starts with the `AMFData` entries) and AMF adds its own reference.
        eprintln!("[amf-diag] SubmitInput slot {slot} refs before {:?}", references(surface));
        let r = unsafe { vcall!(component, SubmitInput, surface.cast::<sys::AMFData>()) }
            .map_err(|m| EngineError::Failed(m, -1))?;
        if r != sys::AMF_OK {
            return Err(engine_error("SubmitInput", r));
        }
        eprintln!("[amf-diag] SubmitInput -> {r}, refs after {:?}, memtype {:?}", references(surface), unsafe { vcall!(surface, GetMemoryType) });
        self.since_idr = if keyframe { 0 } else { self.since_idr + 1 };
        Ok(())
    }

    fn output(&mut self, _slot: usize, wait: bool) -> Result<Option<Output>, EngineError> {
        let component = self.component();
        let deadline = Instant::now() + WAIT;
        let data = loop {
            self.try_drain()?;
            let mut raw: *mut sys::AMFData = ptr::null_mut();
            // SAFETY: an initialised component and a valid out-pointer.
            let r = unsafe { vcall!(component, QueryOutput, &mut raw) }
                .map_err(|m| EngineError::Failed(m, -1))?;
            let data = Com::new(raw);
            match (r, data) {
                (sys::AMF_OK, Some(d)) => break d,
                (sys::AMF_OK | sys::AMF_REPEAT, None) if !wait => return Ok(None),
                (sys::AMF_OK | sys::AMF_REPEAT, None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                (sys::AMF_OK | sys::AMF_REPEAT, None) => {
                    return Err(EngineError::Broken(
                        "AMF did not finish a picture within 10 s",
                    ));
                }
                (sys::AMF_EOF, _) => {
                    return Err(EngineError::Broken(
                        "AMF drained without the expected picture",
                    ));
                }
                (r, _) => return Err(engine_error("QueryOutput", r)),
            }
        };
        // SAFETY: live output data from `QueryOutput`.
        let token = unsafe { int_property!(data.ptr(), sys::TOKEN) }
            .and_then(|t| u64::try_from(t).ok())
            .ok_or(EngineError::Broken("AMF output without its token"))?;
        // SAFETY: as above.
        let kind = unsafe { int_property!(data.ptr(), self.names.output_data_type) };
        // IDR is 0 for both encoders.
        let keyframe = kind == Some(i64::from(sys::AMF_VIDEO_ENCODER_OUTPUT_DATA_TYPE_IDR));
        let mut raw: *mut core::ffi::c_void = ptr::null_mut();
        // SAFETY: live data, a valid IID and out-pointer.
        let r = unsafe { vcall!(data.ptr(), QueryInterface, &sys::IID_AMF_BUFFER, &mut raw) }
            .map_err(|m| EngineError::Failed(m, -1))?;
        let buffer = Com::new(raw.cast::<sys::AMFBuffer>())
            .filter(|_| r == sys::AMF_OK)
            .ok_or(EngineError::Broken("AMF output is not a buffer"))?;
        let data = buffer_bytes(&buffer).map_err(|m| EngineError::Failed(m, -1))?;
        if data.is_empty() {
            return Err(EngineError::Broken("AMF returned an empty picture"));
        }
        Ok(Some(Output {
            data,
            keyframe,
            token,
        }))
    }

    fn end_of_stream(&mut self) -> Result<(), EngineError> {
        if self.drained {
            return Ok(());
        }
        self.drained = true;
        self.drain_pending = true;
        self.try_drain()
    }

    fn set_rate_control(&mut self, rc: Rc) -> Result<(), EngineError> {
        let component = self.component();
        for (name, value) in params::rate_properties(self.codec, rc, true) {
            // SAFETY: an initialised component and a NUL-terminated name; rate
            // properties are dynamic (changed while encoding).
            let r = unsafe { set_property!(component, name, variant(value)) }
                .map_err(|m| EngineError::Failed(m, -1))?;
            match r {
                sys::AMF_OK => {}
                sys::AMF_INVALID_ARG
                | sys::AMF_OUT_OF_RANGE
                | sys::AMF_NOT_SUPPORTED
                | sys::AMF_ACCESS_DENIED => {
                    return Err(EngineError::Unsupported);
                }
                other => return Err(engine_error("SetProperty", other)),
            }
        }
        Ok(())
    }
}

impl Drop for AmfEngine {
    fn drop(&mut self) {
        // Surfaces first (AMF keeps its own references to queued ones), then the
        // component, then (as a field) the context.
        self.surfaces.clear();
        if let Some(component) = self.component.take() {
            // SAFETY: an initialised component, terminated once; its queued work is
            // discarded and its references to surfaces released.
            let _ = unsafe { vcall!(component.ptr(), Terminate) };
            drop(component);
        }
    }
}
