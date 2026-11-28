//! The only `unsafe` code of the crate: thin owners of libopus encoder and decoder
//! states. Everything else in the crate is safe Rust.
//!
//! libopus is thread-compatible: a state may move between threads but must not be used
//! from two at once, which `&mut self` and `Send` (not `Sync`) express. The states are
//! allocated with `malloc` by `opus_*_create` (cold, once per session) and freed in
//! `Drop`. `encode` and `decode` allocate nothing: libopus keeps its scratch on the
//! stack (`USE_ALLOCA`).

#![allow(unsafe_code)]

use std::ffi::{CStr, c_char, c_int, c_uchar};
use std::ptr::{self, NonNull};

#[repr(C)]
struct OpusEncoder {
    _private: [u8; 0],
}

#[repr(C)]
struct OpusDecoder {
    _private: [u8; 0],
}

unsafe extern "C" {
    fn opus_encoder_create(
        fs: i32,
        channels: c_int,
        application: c_int,
        error: *mut c_int,
    ) -> *mut OpusEncoder;
    fn opus_encoder_destroy(st: *mut OpusEncoder);
    fn opus_encoder_ctl(st: *mut OpusEncoder, request: c_int, ...) -> c_int;
    fn opus_encode_float(
        st: *mut OpusEncoder,
        pcm: *const f32,
        frame_size: c_int,
        data: *mut c_uchar,
        max_data_bytes: i32,
    ) -> i32;
    fn opus_decoder_create(fs: i32, channels: c_int, error: *mut c_int) -> *mut OpusDecoder;
    fn opus_decoder_destroy(st: *mut OpusDecoder);
    fn opus_decoder_ctl(st: *mut OpusDecoder, request: c_int, ...) -> c_int;
    fn opus_decode_float(
        st: *mut OpusDecoder,
        data: *const c_uchar,
        len: i32,
        pcm: *mut f32,
        frame_size: c_int,
        decode_fec: c_int,
    ) -> c_int;
    fn opus_packet_get_nb_samples(packet: *const c_uchar, len: i32, fs: i32) -> c_int;
    fn opus_get_version_string() -> *const c_char;
}

/// `OPUS_APPLICATION_*` values of `opus_defines.h`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Application {
    Voip = 2048,
    Audio = 2049,
    LowDelay = 2051,
}

const OPUS_SET_BITRATE: c_int = 4002;
const OPUS_SET_COMPLEXITY: c_int = 4010;
const OPUS_SET_VBR: c_int = 4006;
const OPUS_SET_INBAND_FEC: c_int = 4012;
const OPUS_SET_PACKET_LOSS_PERC: c_int = 4014;
const OPUS_GET_LOOKAHEAD: c_int = 4027;
#[cfg(test)]
const OPUS_GET_FINAL_RANGE: c_int = 4031;
const OPUS_RESET_STATE: c_int = 4028;

/// libopus error code (negative `OPUS_*`).
pub type OpusCode = i32;

/// An owned libopus encoder state.
pub struct Encoder {
    raw: NonNull<OpusEncoder>,
    channels: usize,
}

// SAFETY: the state is plain heap memory without thread affinity; libopus functions only
// need exclusive access, which `&mut self` provides. Not `Sync`.
unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new(rate_hz: i32, channels: usize, app: Application) -> Result<Self, OpusCode> {
        let mut error: c_int = 0;
        // SAFETY: `error` is a valid out pointer; libopus validates the other arguments.
        let raw =
            unsafe { opus_encoder_create(rate_hz, channels as c_int, app as c_int, &mut error) };
        match NonNull::new(raw) {
            Some(raw) if error == 0 => Ok(Self { raw, channels }),
            Some(raw) => {
                // SAFETY: `raw` came from `opus_encoder_create` and is not used again.
                unsafe { opus_encoder_destroy(raw.as_ptr()) };
                Err(error)
            }
            None => Err(if error == 0 { -7 } else { error }),
        }
    }

    fn set(&mut self, request: c_int, value: i32) -> Result<(), OpusCode> {
        // SAFETY: `raw` is a live encoder; every request used here takes one `opus_int32`.
        let code = unsafe { opus_encoder_ctl(self.raw.as_ptr(), request, value) };
        if code == 0 { Ok(()) } else { Err(code) }
    }

    pub fn set_bitrate(&mut self, bps: i32) -> Result<(), OpusCode> {
        self.set(OPUS_SET_BITRATE, bps)
    }

    pub fn set_complexity(&mut self, level: i32) -> Result<(), OpusCode> {
        self.set(OPUS_SET_COMPLEXITY, level)
    }

    pub fn set_vbr(&mut self, on: bool) -> Result<(), OpusCode> {
        self.set(OPUS_SET_VBR, i32::from(on))
    }

    pub fn set_inband_fec(&mut self, on: bool) -> Result<(), OpusCode> {
        self.set(OPUS_SET_INBAND_FEC, i32::from(on))
    }

    pub fn set_packet_loss_percent(&mut self, percent: i32) -> Result<(), OpusCode> {
        self.set(OPUS_SET_PACKET_LOSS_PERC, percent)
    }

    /// Input samples per channel the encoder delays the signal by (the Ogg Opus pre-skip).
    pub fn lookahead(&mut self) -> Result<i32, OpusCode> {
        let mut value: i32 = 0;
        // SAFETY: `raw` is a live encoder; OPUS_GET_LOOKAHEAD writes one `opus_int32`
        // through the pointer, which refers to `value`.
        let code = unsafe {
            opus_encoder_ctl(
                self.raw.as_ptr(),
                OPUS_GET_LOOKAHEAD,
                &mut value as *mut i32,
            )
        };
        if code == 0 { Ok(value) } else { Err(code) }
    }

    /// Encodes `frame_size` sample frames of interleaved `pcm` into `out`; returns the
    /// packet length in bytes.
    pub fn encode(
        &mut self,
        pcm: &[f32],
        frame_size: usize,
        out: &mut [u8],
    ) -> Result<usize, OpusCode> {
        let needed = frame_size.checked_mul(self.channels).ok_or(-1)?;
        let frames = c_int::try_from(frame_size).map_err(|_| -1)?;
        if pcm.len() < needed {
            return Err(-1);
        }
        let max = i32::try_from(out.len()).unwrap_or(i32::MAX);
        // SAFETY: `raw` is live; `pcm` holds at least `frame_size * channels` floats
        // (checked above); `out` is valid for `max` bytes.
        let n = unsafe {
            opus_encode_float(
                self.raw.as_ptr(),
                pcm.as_ptr(),
                frames,
                out.as_mut_ptr(),
                max,
            )
        };
        usize::try_from(n).map_err(|_| n)
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: `raw` came from `opus_encoder_create` and is freed exactly once here.
        unsafe { opus_encoder_destroy(self.raw.as_ptr()) };
    }
}

/// An owned libopus decoder state.
pub struct Decoder {
    raw: NonNull<OpusDecoder>,
    channels: usize,
}

// SAFETY: as for `Encoder`.
unsafe impl Send for Decoder {}

impl Decoder {
    pub fn new(rate_hz: i32, channels: usize) -> Result<Self, OpusCode> {
        let mut error: c_int = 0;
        // SAFETY: `error` is a valid out pointer; libopus validates the other arguments.
        let raw = unsafe { opus_decoder_create(rate_hz, channels as c_int, &mut error) };
        match NonNull::new(raw) {
            Some(raw) if error == 0 => Ok(Self { raw, channels }),
            Some(raw) => {
                // SAFETY: `raw` came from `opus_decoder_create` and is not used again.
                unsafe { opus_decoder_destroy(raw.as_ptr()) };
                Err(error)
            }
            None => Err(if error == 0 { -7 } else { error }),
        }
    }

    pub fn reset(&mut self) -> Result<(), OpusCode> {
        // SAFETY: `raw` is a live decoder; OPUS_RESET_STATE takes no argument.
        let code = unsafe { opus_decoder_ctl(self.raw.as_ptr(), OPUS_RESET_STATE) };
        if code == 0 { Ok(()) } else { Err(code) }
    }

    /// Final range coder state of the last packet: equals the encoder's value when the
    /// entropy decoding was bit-exact (the RFC 8251 test vectors record it).
    #[cfg(test)]
    pub fn final_range(&mut self) -> Result<u32, OpusCode> {
        let mut value: u32 = 0;
        // SAFETY: `raw` is a live decoder; OPUS_GET_FINAL_RANGE writes one `opus_uint32`
        // through the pointer, which refers to `value`.
        let code = unsafe {
            opus_decoder_ctl(
                self.raw.as_ptr(),
                OPUS_GET_FINAL_RANGE,
                &mut value as *mut u32,
            )
        };
        if code == 0 { Ok(value) } else { Err(code) }
    }

    /// Decodes `packet` into `pcm` (room for `frame_size * channels` floats); `None`
    /// conceals a lost packet of `frame_size` frames. Returns the frames produced.
    pub fn decode(
        &mut self,
        packet: Option<&[u8]>,
        pcm: &mut [f32],
        frame_size: usize,
    ) -> Result<usize, OpusCode> {
        let needed = frame_size.checked_mul(self.channels).ok_or(-2)?;
        let frames = c_int::try_from(frame_size).map_err(|_| -2)?;
        if pcm.len() < needed {
            return Err(-2);
        }
        let (data, len) = match packet {
            Some(p) => (p.as_ptr(), i32::try_from(p.len()).map_err(|_| -1)?),
            None => (ptr::null(), 0),
        };
        // SAFETY: `raw` is live; `data` is null with length 0 (loss concealment) or valid
        // for `len` bytes; `pcm` holds `frame_size * channels` floats (checked above).
        let n =
            unsafe { opus_decode_float(self.raw.as_ptr(), data, len, pcm.as_mut_ptr(), frames, 0) };
        usize::try_from(n).map_err(|_| n)
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: `raw` came from `opus_decoder_create` and is freed exactly once here.
        unsafe { opus_decoder_destroy(self.raw.as_ptr()) };
    }
}

/// Sample frames per channel `packet` decodes to at `rate_hz`.
pub fn packet_samples(packet: &[u8], rate_hz: i32) -> Result<usize, OpusCode> {
    let len = i32::try_from(packet.len()).map_err(|_| -1)?;
    if len == 0 {
        return Err(-4);
    }
    // SAFETY: `packet` is valid for `len` bytes and non-empty.
    let n = unsafe { opus_packet_get_nb_samples(packet.as_ptr(), len, rate_hz) };
    usize::try_from(n).map_err(|_| n)
}

/// The libopus version string, for diagnostics and tests.
pub fn version() -> String {
    // SAFETY: returns a pointer to a static NUL-terminated string.
    unsafe { CStr::from_ptr(opus_get_version_string()) }
        .to_string_lossy()
        .into_owned()
}
