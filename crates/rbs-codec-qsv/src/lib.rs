//! Intel Quick Sync Video adapter for the RBS encoder port
//! (`docs/specs/output-ports.md`): H.264 and HEVC on Windows and Linux (x86-64),
//! through oneVPL.
//!
//! # Loading
//!
//! The oneVPL dispatcher is opened at run time (`libvpl.dll` from the application or
//! system directory on Windows, `libvpl.so.2` on Linux, plus `libva.so.2` and
//! `libva-drm.so.2`), so building needs no SDK and a machine without an Intel GPU or
//! runtime probes to an empty set. The dispatcher picks the runtime (`libmfx-gen`
//! for current GPUs, `libmfxhw64` for older ones). Intel drivers install the runtime
//! but not the dispatcher on Windows: applications ship `libvpl.dll` (MIT) next to
//! the executable. The bindings follow oneVPL API 2.13.
//!
//! # Sessions
//!
//! Each session opens the first hardware implementation, with (on Linux) a VA
//! display on the render node the dispatcher reports, and one encoder with
//! [`FRAMES_IN_FLIGHT`] system-memory input surfaces and bitstreams. `submit`
//! copies the NV12 or P010 frame into the next surface and queues it; `drain` polls
//! the sync points in submission order; `flush` waits for the rest. The session
//! logic (ordering, keyframes, `Busy`, capabilities, configuration checks) is shared
//! with the other hardware adapters in `rbs-codec-hw`.
//!
//! The encoder runs with a closed, strict GOP of P frames (no B-frames, no
//! look-ahead), so each frame yields one packet in order. Output is Annex B with the
//! parameter sets in band on every IDR and, as extradata, the parameter sets the
//! runtime reports at creation.
//!
//! Input is system memory only for now: native D3D11 and VA surfaces wait for the
//! `rbs-gpu` exporters (ADR-0025). The copy is counted in [`SessionStats::copied`].
//!
//! Licence: the runtime is Intel's; the generated bindings come from the libvpl API
//! headers (MIT), the loader is `libloading` (ISC).
//!
//! Not verified on hardware yet: run `RBS_REQUIRE_QSV=1 cargo test -p
//! rbs-codec-qsv` on a machine with an Intel GPU.
//!
//! On other platforms the factory compiles and probes to an empty set, so the
//! composition root can register it unconditionally.

/// Id of the adapter.
pub const ADAPTER_ID: &str = "org.rbs.codec.qsv";

/// Frames a session accepts before `submit` returns `Busy`: frames the encoder is
/// still encoding plus pictures not yet drained. Also the encoder's `AsyncDepth`
/// and the number of input surfaces and bitstreams per session.
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
    use crate::engine::{QsvEngine, probe};

    /// The probed device, read once per process (opening a session and querying it
    /// takes longer than the probe budget; later probes are free). Only definitive
    /// results are kept: a probe that could not open a session or a display now is
    /// repeated next time.
    fn device() -> Result<&'static DeviceCaps, ProbeError> {
        static DEVICE: OnceLock<DeviceCaps> = OnceLock::new();
        if let Some(d) = DEVICE.get() {
            return Ok(d);
        }
        let probed = probe().map_err(ProbeError::new)?;
        Ok(DEVICE.get_or_init(|| probed))
    }

    /// Factory of the QSV adapter.
    #[derive(Debug, Default)]
    pub struct QsvFactory {
        _private: (),
    }

    impl QsvFactory {
        /// The adapter on the first Intel GPU the oneVPL dispatcher finds.
        pub fn new() -> Self {
            Self::default()
        }

        /// Creates a session, keeping its concrete type for [`QsvSession::stats`].
        pub fn create_session(
            &self,
            config: &VideoEncoderConfig,
        ) -> Result<QsvSession, CreateError> {
            // A probe failure is temporary (no session or display right now).
            let device = device().map_err(|_| CreateError::ResourceExhausted)?;
            let plan = plan::plan(config, device)?;
            let dynamic_bitrate = device
                .codec(config.codec)
                .is_some_and(|c| c.dynamic_bitrate);
            let (engine, extradata) = QsvEngine::open(&plan)?;
            let stream = StreamDescriptor::new(
                config.codec,
                BitstreamFormat::AnnexB,
                Bytes::from(extradata),
                StreamParams::video(config.format, config.frame_rate),
            );
            Ok(QsvSession(Session::new(
                engine,
                config.codec,
                config.format,
                config.rate_control,
                dynamic_bitrate,
                stream,
            )))
        }
    }

    impl AdapterFactory for QsvFactory {
        fn id(&self) -> AdapterId {
            AdapterId::new(crate::ADAPTER_ID)
        }

        fn display_name(&self) -> &str {
            "Intel Quick Sync Video"
        }

        fn probe(&self) -> Result<CapabilitySet, ProbeError> {
            Ok(capability_set(device()?))
        }
    }

    impl VideoEncoderFactory for QsvFactory {
        fn create(
            &self,
            config: &VideoEncoderConfig,
        ) -> Result<Box<dyn VideoEncoder>, CreateError> {
            Ok(Box::new(self.create_session(config)?))
        }
    }

    /// One QSV encoder session.
    pub struct QsvSession(Session<QsvEngine>);

    impl QsvSession {
        /// Counters of this session.
        pub fn stats(&self) -> SessionStats {
            self.0.stats()
        }
    }

    impl std::fmt::Debug for QsvSession {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("QsvSession")
                .field("stats", &self.stats())
                .finish_non_exhaustive()
        }
    }

    impl VideoEncoder for QsvSession {
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

    /// Factory of the QSV adapter. Unavailable on this platform: it probes to an
    /// empty set and refuses every session.
    #[derive(Debug, Default)]
    pub struct QsvFactory {
        _private: (),
    }

    impl QsvFactory {
        /// The adapter on the first Intel GPU the oneVPL dispatcher finds.
        pub fn new() -> Self {
            Self::default()
        }
    }

    impl AdapterFactory for QsvFactory {
        fn id(&self) -> AdapterId {
            AdapterId::new(crate::ADAPTER_ID)
        }

        fn display_name(&self) -> &str {
            "Intel Quick Sync Video (unavailable on this platform)"
        }

        fn probe(&self) -> Result<CapabilitySet, ProbeError> {
            Ok(CapabilitySet::new())
        }
    }

    impl VideoEncoderFactory for QsvFactory {
        fn create(&self, _: &VideoEncoderConfig) -> Result<Box<dyn VideoEncoder>, CreateError> {
            Err(CreateError::Unsupported {
                what: "QSV is only available on x86-64 Windows and Linux".into(),
            })
        }
    }
}

pub use platform::QsvFactory;

#[cfg(all(
    any(target_os = "windows", target_os = "linux"),
    target_arch = "x86_64"
))]
pub use platform::QsvSession;
