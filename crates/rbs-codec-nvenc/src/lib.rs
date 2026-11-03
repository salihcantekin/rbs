//! NVIDIA NVENC adapter for the RBS encoder port (`docs/specs/output-ports.md`): H.264
//! and HEVC on Windows and Linux (64-bit).
//!
//! # Loading
//!
//! The NVIDIA driver libraries are opened at run time (`nvEncodeAPI64.dll` and
//! `nvcuda.dll` from the Windows system directory, `libnvidia-encode.so.1` and
//! `libcuda.so.1` on Linux), so building needs no SDK and a machine without an NVIDIA
//! driver probes to an empty set. The bindings follow NVIDIA Video Codec SDK 12.1,
//! which needs driver 531 or newer (`NvEncodeAPIGetMaxSupportedVersion` is checked).
//!
//! # Sessions
//!
//! The session logic (ordering, keyframes, `Busy`, the capability set and the
//! configuration checks) is shared with the other hardware adapters in
//! `rbs-codec-hw`; this crate holds the loader, the bindings and the engine.
//!
//! Each session owns a CUDA context on device 0 and one NVENC session in synchronous
//! mode with [`FRAMES_IN_FLIGHT`] NVENC-allocated input and bitstream buffers.
//! `submit` copies the system-memory NV12 or P010 frame into the next input buffer and
//! queues it; `drain` reads finished pictures back in submission order without waiting
//! (`doNotWait`); `flush` sends end-of-stream and waits for the rest. Every call pushes
//! the session's CUDA context on the calling thread and pops it again.
//!
//! The encoder runs with picture type decision, no B-frames and no look-ahead, so each
//! frame yields one packet in order and none is dropped. Output is Annex B with the
//! parameter sets in band on every IDR picture and, as extradata, the sequence
//! parameters NVENC reports at creation (`nvEncGetSequenceParams`).
//!
//! Input is system memory only for now: native D3D11/D3D12/CUDA surfaces wait for the
//! `rbs-gpu` exporters on Windows and Linux (ADR-0025). The copy is counted in
//! [`SessionStats::copied`].
//!
//! Licence: the driver is NVIDIA's; the generated bindings come from FFmpeg's
//! `nv-codec-headers` (MIT, a redistribution of the SDK header), the loader is
//! `libloading` (ISC).
//!
//! On other platforms the factory compiles and probes to an empty set, so the
//! composition root can register it unconditionally.

/// Id of the adapter.
pub const ADAPTER_ID: &str = "org.rbs.codec.nvenc";

/// Frames a session accepts before `submit` returns `Busy`: frames NVENC is still
/// encoding plus pictures not yet drained. Also the number of NVENC input and
/// bitstream buffers per session.
pub const FRAMES_IN_FLIGHT: usize = 8;

/// Counters of one session, for metrics.
pub use rbs_codec_hw::SessionStats;

#[cfg(all(
    any(target_os = "windows", target_os = "linux"),
    target_pointer_width = "64"
))]
mod engine;
#[cfg(all(
    any(target_os = "windows", target_os = "linux"),
    target_pointer_width = "64"
))]
mod loader;
#[cfg(all(
    any(target_os = "windows", target_os = "linux"),
    target_pointer_width = "64"
))]
mod sys;

#[cfg(all(
    any(target_os = "windows", target_os = "linux"),
    target_pointer_width = "64"
))]
mod platform {
    use std::sync::OnceLock;

    use rbs_codec::{CreateError, VideoEncoder, VideoEncoderConfig, VideoEncoderFactory};
    use rbs_core::capability::CapabilitySet;
    use rbs_core::media::{BitstreamFormat, Bytes, EncodedPacket, StreamDescriptor, StreamParams};
    use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};

    use rbs_codec_hw::device::{DeviceCaps, capability_set};
    use rbs_codec_hw::plan;
    use rbs_codec_hw::session::Session;

    use crate::SessionStats;
    use crate::engine::{NvencEngine, probe};

    /// The probed device, read once per process (opening CUDA and an NVENC session
    /// takes longer than the probe budget; later probes are free). Only definitive
    /// results are kept: a probe that found no free session or could not create a
    /// context is repeated next time.
    fn device() -> Result<&'static DeviceCaps, ProbeError> {
        static DEVICE: OnceLock<DeviceCaps> = OnceLock::new();
        if let Some(d) = DEVICE.get() {
            return Ok(d);
        }
        let probed = probe(0).map_err(ProbeError::new)?;
        Ok(DEVICE.get_or_init(|| probed))
    }

    /// Factory of the NVENC adapter.
    #[derive(Debug, Default)]
    pub struct NvencFactory {
        _private: (),
    }

    impl NvencFactory {
        /// The adapter on the first CUDA device.
        pub fn new() -> Self {
            Self::default()
        }

        /// Creates a session, keeping its concrete type for [`NvencSession::stats`].
        pub fn create_session(
            &self,
            config: &VideoEncoderConfig,
        ) -> Result<NvencSession, CreateError> {
            // A probe failure is temporary (no free session or context right now).
            let device = device().map_err(|_| CreateError::ResourceExhausted)?;
            let plan = plan::plan(config, device)?;
            let dynamic_bitrate = device
                .codec(config.codec)
                .is_some_and(|c| c.dynamic_bitrate);
            let (engine, extradata) = NvencEngine::open(0, &plan)?;
            let stream = StreamDescriptor::new(
                config.codec,
                BitstreamFormat::AnnexB,
                Bytes::from(extradata),
                StreamParams::video(config.format, config.frame_rate),
            );
            Ok(NvencSession(Session::new(
                engine,
                config.codec,
                config.format,
                config.rate_control,
                dynamic_bitrate,
                stream,
            )))
        }
    }

    impl AdapterFactory for NvencFactory {
        fn id(&self) -> AdapterId {
            AdapterId::new(crate::ADAPTER_ID)
        }

        fn display_name(&self) -> &str {
            "NVIDIA NVENC"
        }

        fn probe(&self) -> Result<CapabilitySet, ProbeError> {
            Ok(capability_set(device()?))
        }
    }

    impl VideoEncoderFactory for NvencFactory {
        fn create(
            &self,
            config: &VideoEncoderConfig,
        ) -> Result<Box<dyn VideoEncoder>, CreateError> {
            Ok(Box::new(self.create_session(config)?))
        }
    }

    /// One NVENC encoder session.
    pub struct NvencSession(Session<NvencEngine>);

    impl NvencSession {
        /// Counters of this session.
        pub fn stats(&self) -> SessionStats {
            self.0.stats()
        }
    }

    impl std::fmt::Debug for NvencSession {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("NvencSession")
                .field("stats", &self.stats())
                .finish_non_exhaustive()
        }
    }

    impl VideoEncoder for NvencSession {
        fn stream(&self) -> &StreamDescriptor {
            self.0.stream()
        }

        fn submit(
            &mut self,
            frame: &rbs_core::media::VideoFrame,
        ) -> Result<(), rbs_codec::EncodeError> {
            self.0.submit(frame)
        }

        fn drain(&mut self, out: &mut Vec<EncodedPacket>) -> Result<(), rbs_codec::EncodeError> {
            self.0.drain(out)
        }

        fn reconfigure(
            &mut self,
            change: &rbs_codec::EncoderChange,
        ) -> Result<(), rbs_codec::EncodeError> {
            self.0.reconfigure(change)
        }

        fn flush(&mut self, out: &mut Vec<EncodedPacket>) -> Result<(), rbs_codec::EncodeError> {
            self.0.flush(out)
        }
    }
}

#[cfg(not(all(
    any(target_os = "windows", target_os = "linux"),
    target_pointer_width = "64"
)))]
mod platform {
    use rbs_codec::{CreateError, VideoEncoder, VideoEncoderConfig, VideoEncoderFactory};
    use rbs_core::capability::CapabilitySet;
    use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};

    /// Factory of the NVENC adapter. Unavailable on this platform: it probes to an
    /// empty set and refuses every session.
    #[derive(Debug, Default)]
    pub struct NvencFactory {
        _private: (),
    }

    impl NvencFactory {
        /// The adapter on the first CUDA device.
        pub fn new() -> Self {
            Self::default()
        }
    }

    impl AdapterFactory for NvencFactory {
        fn id(&self) -> AdapterId {
            AdapterId::new(crate::ADAPTER_ID)
        }

        fn display_name(&self) -> &str {
            "NVIDIA NVENC (unavailable on this platform)"
        }

        fn probe(&self) -> Result<CapabilitySet, ProbeError> {
            Ok(CapabilitySet::new())
        }
    }

    impl VideoEncoderFactory for NvencFactory {
        fn create(&self, _: &VideoEncoderConfig) -> Result<Box<dyn VideoEncoder>, CreateError> {
            Err(CreateError::Unsupported {
                what: "NVENC is only available on 64-bit Windows and Linux".into(),
            })
        }
    }
}

pub use platform::NvencFactory;

#[cfg(all(
    any(target_os = "windows", target_os = "linux"),
    target_pointer_width = "64"
))]
pub use platform::NvencSession;
