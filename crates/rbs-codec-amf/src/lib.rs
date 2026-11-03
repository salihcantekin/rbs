//! AMD Advanced Media Framework adapter for the RBS encoder port
//! (`docs/specs/output-ports.md`): H.264 and HEVC on Windows and Linux (x86-64).
//!
//! # Loading
//!
//! The AMF runtime ships with the AMD driver and is opened at run time
//! (`amfrt64.dll` from the system directory on Windows, `libamfrt64.so.1` on Linux,
//! which comes with AMD's Vulkan driver stack), so building needs no SDK and a
//! machine without an AMD GPU probes to an empty set. The runtime must be at least
//! the version of the headers the bindings come from (AMF SDK 1.4.33).
//!
//! # Sessions
//!
//! Each session opens an AMF context on the default device (Direct3D 11 on
//! Windows, Vulkan on Linux) and one encoder component. `submit` copies the NV12 or
//! P010 frame into a new host-memory surface from the context and submits it (AMF
//! converts a submitted host surface in place, so surfaces are never reused); `drain` takes the encoded data in submission order;
//! `flush` drains the encoder. The session logic (ordering, keyframes, `Busy`,
//! capabilities, configuration checks) is shared with the other hardware adapters
//! in `rbs-codec-hw`.
//!
//! The encoder runs with P frames only (no B-frames, no pre-analysis), so each
//! frame yields one packet in order. The adapter places the IDR frames itself at
//! the configured interval and on keyframe requests, with the parameter sets in band
//! on every IDR; the extradata holds the parameter sets of a black priming picture
//! the session encodes and drains when it is created (AMF's `ExtraData` property
//! leaves out the VUI the in-band ones carry), so the two are byte for byte equal. Each surface carries the session's frame token as a property, which
//! the output data echoes, so a reordered or lost packet is detected.
//!
//! Input is host memory only for now: native D3D11 and Vulkan surfaces wait for
//! the `rbs-gpu` exporters (ADR-0025). The copy is counted in
//! [`SessionStats::copied`].
//!
//! Licence: the runtime is AMD's; the generated bindings come from the AMF SDK
//! headers (MIT), the loader is `libloading` (ISC).
//!
//! Not verified on hardware yet: run `RBS_REQUIRE_AMF=1 cargo test -p
//! rbs-codec-amf` on a machine with an AMD GPU.
//!
//! On other platforms the factory compiles and probes to an empty set, so the
//! composition root can register it unconditionally.

/// Id of the adapter.
pub const ADAPTER_ID: &str = "org.rbs.codec.amf";

/// Frames a session accepts before `submit` returns `Busy`: frames the encoder is
/// still encoding plus pictures not yet drained.
pub const FRAMES_IN_FLIGHT: usize = 4;

/// Counters of one session, for metrics.
pub use rbs_codec_hw::SessionStats;

#[cfg(all(
    any(target_os = "windows", target_os = "linux"),
    target_arch = "x86_64"
))]
mod engine;
#[cfg(all(
    any(target_os = "windows", target_os = "linux"),
    target_arch = "x86_64"
))]
mod loader;
#[cfg(all(
    any(target_os = "windows", target_os = "linux"),
    target_arch = "x86_64"
))]
mod params;
#[cfg(all(
    any(target_os = "windows", target_os = "linux"),
    target_arch = "x86_64"
))]
mod sys;

#[cfg(all(
    any(target_os = "windows", target_os = "linux"),
    target_arch = "x86_64"
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
    use crate::engine::{AmfEngine, probe};

    /// The probed device, read once per process (opening a session and querying it
    /// takes longer than the probe budget; later probes are free). Only definitive
    /// results are kept: a probe that could not open a context now is
    /// repeated next time.
    fn device() -> Result<&'static DeviceCaps, ProbeError> {
        static DEVICE: OnceLock<DeviceCaps> = OnceLock::new();
        if let Some(d) = DEVICE.get() {
            return Ok(d);
        }
        let probed = probe().map_err(ProbeError::new)?;
        Ok(DEVICE.get_or_init(|| probed))
    }

    /// Factory of the AMF adapter.
    #[derive(Debug, Default)]
    pub struct AmfFactory {
        _private: (),
    }

    impl AmfFactory {
        /// The adapter on the default GPU, when it is an AMD GPU with AMF.
        pub fn new() -> Self {
            Self::default()
        }

        /// Creates a session, keeping its concrete type for [`AmfSession::stats`].
        pub fn create_session(
            &self,
            config: &VideoEncoderConfig,
        ) -> Result<AmfSession, CreateError> {
            // A probe failure is temporary (no context right now).
            let device = device().map_err(|_| CreateError::ResourceExhausted)?;
            let plan = plan::plan(config, device)?;
            let dynamic_bitrate = device
                .codec(config.codec)
                .is_some_and(|c| c.dynamic_bitrate);
            let (engine, extradata) = AmfEngine::open(&plan)?;
            let stream = StreamDescriptor::new(
                config.codec,
                BitstreamFormat::AnnexB,
                Bytes::from(extradata),
                StreamParams::video(config.format, config.frame_rate),
            );
            Ok(AmfSession(Session::new(
                engine,
                config.codec,
                config.format,
                config.rate_control,
                dynamic_bitrate,
                stream,
            )))
        }
    }

    impl AdapterFactory for AmfFactory {
        fn id(&self) -> AdapterId {
            AdapterId::new(crate::ADAPTER_ID)
        }

        fn display_name(&self) -> &str {
            "AMD Advanced Media Framework"
        }

        fn probe(&self) -> Result<CapabilitySet, ProbeError> {
            Ok(capability_set(device()?))
        }
    }

    impl VideoEncoderFactory for AmfFactory {
        fn create(
            &self,
            config: &VideoEncoderConfig,
        ) -> Result<Box<dyn VideoEncoder>, CreateError> {
            Ok(Box::new(self.create_session(config)?))
        }
    }

    /// One AMF encoder session.
    pub struct AmfSession(Session<AmfEngine>);

    impl AmfSession {
        /// Counters of this session.
        pub fn stats(&self) -> SessionStats {
            self.0.stats()
        }
    }

    impl std::fmt::Debug for AmfSession {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("AmfSession")
                .field("stats", &self.stats())
                .finish_non_exhaustive()
        }
    }

    impl VideoEncoder for AmfSession {
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
    target_arch = "x86_64"
)))]
mod platform {
    use rbs_codec::{CreateError, VideoEncoder, VideoEncoderConfig, VideoEncoderFactory};
    use rbs_core::capability::CapabilitySet;
    use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};

    /// Factory of the AMF adapter. Unavailable on this platform: it probes to an
    /// empty set and refuses every session.
    #[derive(Debug, Default)]
    pub struct AmfFactory {
        _private: (),
    }

    impl AmfFactory {
        /// The adapter on the default GPU, when it is an AMD GPU with AMF.
        pub fn new() -> Self {
            Self::default()
        }
    }

    impl AdapterFactory for AmfFactory {
        fn id(&self) -> AdapterId {
            AdapterId::new(crate::ADAPTER_ID)
        }

        fn display_name(&self) -> &str {
            "AMD Advanced Media Framework (unavailable on this platform)"
        }

        fn probe(&self) -> Result<CapabilitySet, ProbeError> {
            Ok(CapabilitySet::new())
        }
    }

    impl VideoEncoderFactory for AmfFactory {
        fn create(&self, _: &VideoEncoderConfig) -> Result<Box<dyn VideoEncoder>, CreateError> {
            Err(CreateError::Unsupported {
                what: "AMF is only available on x86-64 Windows and Linux".into(),
            })
        }
    }
}

pub use platform::AmfFactory;

#[cfg(all(
    any(target_os = "windows", target_os = "linux"),
    target_arch = "x86_64"
))]
pub use platform::AmfSession;
