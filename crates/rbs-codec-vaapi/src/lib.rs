//! VA-API adapter for the RBS encoder port (`docs/specs/output-ports.md`): H.264
//! and HEVC (Main, Main 10) on Linux, on any GPU whose VA driver encodes (Intel
//! `iHD`/`i965`, AMD `radeonsi`).
//!
//! # Loading
//!
//! `libva.so.2` and `libva-drm.so.2` are opened at run time, so building needs no
//! SDK and a machine without them or without an encoding VA driver probes to an
//! empty set. The probe opens each DRM render node (`/dev/dri/renderD*`) in order
//! and keeps the first whose driver encodes H.264 or HEVC the way this adapter
//! needs: 4:2:0 input (10-bit for Main 10), a packed sequence header and CBR, VBR
//! or constant QP.
//!
//! # Sessions
//!
//! Each session opens its own VA display on that node, an encode context with
//! [`FRAMES_IN_FLIGHT`] input surfaces (written through a derived or a separate
//! image) and coded buffers, and `FRAMES_IN_FLIGHT + 1` reconstructed surfaces.
//! The adapter writes the parameter sets itself (H.264 SPS and PPS; HEVC VPS, SPS
//! and PPS, with the block sizes and coding tools the driver reports) and sends
//! them as a packed header with every IDR picture; the driver writes the slices.
//! Every other picture is a P picture referencing the previous one (HEVC drivers
//! without P slices get a B slice whose two lists hold that picture), so output is
//! in input order with one packet per frame. The session logic (ordering, keyframes, `Busy`, capabilities,
//! configuration checks) is shared with the other hardware adapters in
//! `rbs-codec-hw`.
//!
//! Input is system memory only for now: VA surfaces from DMA-BUF wait for the
//! `rbs-gpu` exporters (ADR-0025). The copy is counted in
//! [`SessionStats::copied`].
//!
//! Licence: the drivers are their vendors'; the generated bindings come from the
//! libva headers (MIT), the loader is `libloading` (ISC).
//!
//! Not verified on hardware yet: run `RBS_REQUIRE_VAAPI=1 cargo test -p
//! rbs-codec-vaapi` on Linux with an Intel or AMD GPU.
//!
//! On other platforms the factory compiles and probes to an empty set, so the
//! composition root can register it unconditionally.

/// Id of the adapter.
pub const ADAPTER_ID: &str = "org.rbs.codec.vaapi";

/// Frames a session accepts before `submit` returns `Busy`: frames the encoder is
/// still encoding plus pictures not yet drained. Also the number of input surfaces
/// per session.
pub const FRAMES_IN_FLIGHT: usize = 4;

/// Counters of one session, for metrics.
pub use rbs_codec_hw::SessionStats;

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod bits;
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod engine;
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod h264;
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod hevc;
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod loader;
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod sys;

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
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
    use crate::engine::{VaapiEngine, probe};

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

    /// Factory of the VA-API adapter.
    #[derive(Debug, Default)]
    pub struct VaapiFactory {
        _private: (),
    }

    impl VaapiFactory {
        /// The adapter on the first render node whose VA driver encodes.
        pub fn new() -> Self {
            Self::default()
        }

        /// Creates a session, keeping its concrete type for [`VaapiSession::stats`].
        pub fn create_session(
            &self,
            config: &VideoEncoderConfig,
        ) -> Result<VaapiSession, CreateError> {
            // A probe failure is temporary (no context right now).
            let device = device().map_err(|_| CreateError::ResourceExhausted)?;
            let plan = plan::plan(config, device)?;
            let dynamic_bitrate = device
                .codec(config.codec)
                .is_some_and(|c| c.dynamic_bitrate);
            let (engine, extradata) = VaapiEngine::open(&plan)?;
            let stream = StreamDescriptor::new(
                config.codec,
                BitstreamFormat::AnnexB,
                Bytes::from(extradata),
                StreamParams::video(config.format, config.frame_rate),
            );
            Ok(VaapiSession(Session::new(
                engine,
                config.codec,
                config.format,
                config.rate_control,
                dynamic_bitrate,
                stream,
            )))
        }
    }

    impl AdapterFactory for VaapiFactory {
        fn id(&self) -> AdapterId {
            AdapterId::new(crate::ADAPTER_ID)
        }

        fn display_name(&self) -> &str {
            "VA-API"
        }

        fn probe(&self) -> Result<CapabilitySet, ProbeError> {
            Ok(capability_set(device()?))
        }
    }

    impl VideoEncoderFactory for VaapiFactory {
        fn create(
            &self,
            config: &VideoEncoderConfig,
        ) -> Result<Box<dyn VideoEncoder>, CreateError> {
            Ok(Box::new(self.create_session(config)?))
        }
    }

    /// One VA-API encoder session.
    pub struct VaapiSession(Session<VaapiEngine>);

    impl VaapiSession {
        /// Counters of this session.
        pub fn stats(&self) -> SessionStats {
            self.0.stats()
        }
    }

    impl std::fmt::Debug for VaapiSession {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("VaapiSession")
                .field("stats", &self.stats())
                .finish_non_exhaustive()
        }
    }

    impl VideoEncoder for VaapiSession {
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
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
mod platform {
    use rbs_codec::{CreateError, VideoEncoder, VideoEncoderConfig, VideoEncoderFactory};
    use rbs_core::capability::CapabilitySet;
    use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};

    /// Factory of the VA-API adapter. Unavailable on this platform: it probes to an
    /// empty set and refuses every session.
    #[derive(Debug, Default)]
    pub struct VaapiFactory {
        _private: (),
    }

    impl VaapiFactory {
        /// The adapter on the first render node whose VA driver encodes.
        pub fn new() -> Self {
            Self::default()
        }
    }

    impl AdapterFactory for VaapiFactory {
        fn id(&self) -> AdapterId {
            AdapterId::new(crate::ADAPTER_ID)
        }

        fn display_name(&self) -> &str {
            "VA-API (unavailable on this platform)"
        }

        fn probe(&self) -> Result<CapabilitySet, ProbeError> {
            Ok(CapabilitySet::new())
        }
    }

    impl VideoEncoderFactory for VaapiFactory {
        fn create(&self, _: &VideoEncoderConfig) -> Result<Box<dyn VideoEncoder>, CreateError> {
            Err(CreateError::Unsupported {
                what: "VA-API is only available on Linux (x86-64 and AArch64)".into(),
            })
        }
    }
}

pub use platform::VaapiFactory;

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub use platform::VaapiSession;
