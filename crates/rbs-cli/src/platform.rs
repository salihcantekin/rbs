//! The platform's adapters: the only place in the headless path that names them (P1).
//! Order is preference: a registry tries hardware adapters first, then registration
//! order, unless the user picked one (`--encoder`).

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
    (a, errors)
}
