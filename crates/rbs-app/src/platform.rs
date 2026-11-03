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
    (a, errors)
}
