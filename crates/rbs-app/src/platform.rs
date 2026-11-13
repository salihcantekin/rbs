//! The platform's adapters: with `rbs-cli`, the only place that names them (P1). Order
//! is preference: a registry tries hardware adapters first, then registration order.

use std::sync::Arc;

use rbs_record::RecordAdapters;

/// Every adapter this build has for the platform, not probed yet. Registration errors
/// (a duplicate id) are returned as messages; they mean a bug in this list.
pub fn adapters() -> (RecordAdapters, Vec<String>) {
    let mut a = RecordAdapters::new();
    let mut errors = Vec::new();
    let mut note = |r: Result<(), rbs_core::registry::DuplicateAdapter>| {
        if let Err(e) = r {
            errors.push(e.to_string());
        }
    };
    note(
        a.muxers
            .register(Arc::new(rbs_mux_matroska::MatroskaMuxerFactory)),
    );
    note(a.muxers.register(Arc::new(rbs_mux_fmp4::Fmp4MuxerFactory)));
    note(
        a.transports
            .register(Arc::new(rbs_transport_file::FileTransportFactory)),
    );
    // Go live: RTMP takes packets; SRT takes MPEG-TS (pure Rust, every platform).
    note(
        a.muxers
            .register(Arc::new(rbs_mux_mpegts::MpegTsMuxerFactory)),
    );
    note(
        a.transports
            .register(Arc::new(rbs_transport_rtmp::RtmpTransportFactory)),
    );
    note(
        a.transports
            .register(Arc::new(rbs_transport_srt::SrtTransportFactory)),
    );
    #[cfg(windows)]
    {
        note(
            a.sources
                .register_capture(Arc::new(rbs_capture_wgc::WgcCaptureFactory::new())),
        );
        note(
            a.sources
                .register_capture(Arc::new(rbs_capture_mf::MfCaptureFactory::new())),
        );
        note(
            a.sources
                .register_audio(Arc::new(rbs_audio_io_wasapi::WasapiAudioBackend::new())),
        );
        note(
            a.video_encoders
                .register(Arc::new(rbs_codec_nvenc::NvencFactory::new())),
        );
        note(
            a.video_encoders
                .register(Arc::new(rbs_codec_amf::AmfFactory::new())),
        );
        note(
            a.audio_encoders
                .register(Arc::new(rbs_codec_mf::MfAacFactory::new())),
        );
    }
    #[cfg(target_os = "linux")]
    {
        // Cameras (PipeWire video nodes) and, through the screen-cast portal's
        // picker, displays and windows. System memory only until DMA-BUF import is
        // checked on hardware (rbs_nodes::capture::import_preferences).
        note(
            a.sources
                .register_capture(Arc::new(rbs_capture_pipewire::PipeWireCaptureFactory::new())),
        );
        note(
            a.sources
                .register_audio(Arc::new(rbs_audio_io_pipewire::PipeWireAudioBackend::new())),
        );
        // Hardware encoders; the first that probes and opens wins.
        note(
            a.video_encoders
                .register(Arc::new(rbs_codec_nvenc::NvencFactory::new())),
        );
        note(
            a.video_encoders
                .register(Arc::new(rbs_codec_amf::AmfFactory::new())),
        );
        note(
            a.video_encoders
                .register(Arc::new(rbs_codec_qsv::QsvFactory::new())),
        );
        note(
            a.video_encoders
                .register(Arc::new(rbs_codec_vaapi::VaapiFactory::new())),
        );
        // The system's FFmpeg (libavcodec), loaded at run time (ADR-0007).
        note(
            a.audio_encoders
                .register(Arc::new(rbs_codec_ffmpeg::FfmpegAacFactory::new())),
        );
    }
    #[cfg(target_os = "macos")]
    {
        note(
            a.sources
                .register_capture(Arc::new(rbs_capture_sck::SckCaptureFactory::new())),
        );
        note(
            a.sources
                .register_capture(Arc::new(rbs_capture_avf::AvfCaptureFactory::new())),
        );
        note(
            a.sources
                .register_audio(Arc::new(rbs_audio_io_coreaudio::CoreAudioBackend::new())),
        );
        note(
            a.video_encoders
                .register(Arc::new(rbs_codec_videotoolbox::VideoToolboxFactory::new())),
        );
        // Apple's software encoders: a last resort after the hardware adapter.
        note(a.video_encoders.register(Arc::new(
            rbs_codec_videotoolbox::VideoToolboxFactory::with_software(),
        )));
        note(
            a.audio_encoders
                .register(Arc::new(rbs_codec_audiotoolbox::AacFactory::new())),
        );
    }
    #[cfg(feature = "demo-sources")]
    if std::env::var_os("RBS_DEMO_SOURCES").is_some_and(|v| v == "1") {
        note(
            a.sources
                .register_capture(Arc::new(rbs_capture_test::TestCaptureFactory::new())),
        );
        note(
            a.sources
                .register_audio(Arc::new(rbs_audio_io_test::TestAudioBackend::new())),
        );
    }
    // Bright demo pattern until FFmpeg is linked (ADR-0108). The file path is ignored.
    note(a.sources.register_media(Arc::new(
        rbs_media_decode_test::ScriptFactory::new(
            rbs_media_decode_test::ScriptClip::demo_pattern(),
        ),
    )));
    (a, errors)
}

/// The platform's virtual camera (ADR-0066: Windows only).
#[derive(Clone)]
pub struct VirtualCamera {
    /// The adapter.
    pub factory: Arc<dyn rbs_vcam::VirtualCameraFactory>,
    /// Settings › Virtual camera's "Remove completely": removes the camera and RBS's
    /// part from the OS, which may ask for administrator approval again. Blocks until
    /// the prompt is answered.
    pub remove_completely:
        Arc<dyn Fn() -> Result<rbs_vcam::InstallState, rbs_vcam::VcamError> + Send + Sync>,
}

/// The virtual camera adapter of this platform, or why there is none. `config` is the
/// RBS settings folder, where the camera's symbolic link is kept in the file
/// `rbs-cli vcam` uses too.
pub fn virtual_camera(config: &std::path::Path) -> Result<VirtualCamera, &'static str> {
    #[cfg(windows)]
    {
        // A missing folder only means the link is not kept; `status` then says not added.
        let _ = std::fs::create_dir_all(config);
        let links = rbs_vcam_mf::FileLinkStore::new(crate::vcam_settings::link_file(config));
        let mf = Arc::new(rbs_vcam_mf::MfVcamFactory::new(Box::new(links)));
        let remove = Arc::clone(&mf);
        Ok(VirtualCamera {
            factory: mf,
            remove_completely: Arc::new(move || remove.remove_completely()),
        })
    }
    #[cfg(target_os = "macos")]
    {
        let _ = config;
        Err("Not available on macOS yet")
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = config;
        Err("Not available on Linux yet")
    }
}
