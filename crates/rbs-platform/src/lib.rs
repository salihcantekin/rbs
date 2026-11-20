//! The one list of RBS's built-in adapters per platform (ADR-0160). The composition
//! roots `rbs-app` and `rbs-cli` call it; no other crate may (P1, `check-layers`).
//!
//! Order is preference: a registry tries hardware adapters first, then registration
//! order. Building the list constructs factories only: nothing is probed, loaded or
//! opened until a registry probes or a source asks (P13).

use std::sync::Arc;

use rbs_codec::VideoEncoderFactory;
use rbs_core::registry::DuplicateAdapter;
use rbs_media_decode::MediaDecoderFactory;
use rbs_record::RecordAdapters;

mod text_clock;
mod vcam;

pub use text_clock::LocalTextClock;
pub use vcam::{vcam_link_file, virtual_camera, VcamUnavailable, VirtualCamera};

/// What a root asks for beyond the shared list.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct Options {
    /// Registers the deterministic reference adapters (synthetic displays, cameras and
    /// audio). Only with the crate's feature `demo-sources`; without it this field is
    /// ignored.
    pub demo_sources: bool,
}

impl Options {
    /// The default options with `demo_sources` set.
    pub fn with_demo_sources(mut self, on: bool) -> Self {
        self.demo_sources = on;
        self
    }
}

/// Adapters this build registers, not probed yet.
pub struct PlatformAdapters {
    /// Every port.
    pub adapters: RecordAdapters,
    /// The same video-encoder `Arc`s `adapters` holds, in registration order.
    /// Installing one of them alone (`rbs-cli --encoder`) does not probe it.
    pub video_encoders: Vec<Arc<dyn VideoEncoderFactory>>,
    /// Duplicate-id registrations: a bug in this list.
    pub errors: Vec<String>,
}

/// Every adapter this build has for the platform, not probed yet.
pub fn adapters(options: &Options) -> PlatformAdapters {
    let mut a = RecordAdapters::new();
    let mut errors = Vec::new();
    let mut note = |r: Result<(), DuplicateAdapter>| {
        if let Err(e) = r {
            errors.push(e.to_string());
        }
    };
    register_common(&mut a, &mut note);
    register_platform_io(&mut a, &mut note);
    let video_encoders = video_encoder_factories();
    for factory in &video_encoders {
        note(a.video_encoders.register(Arc::clone(factory)));
    }
    register_demo(&mut a, &mut note, options);
    for factory in media_factories() {
        note(a.sources.register_media(factory));
    }
    PlatformAdapters {
        adapters: a,
        video_encoders,
        errors,
    }
}

/// Muxers, audio encoders, transports and text on every platform (pure Rust).
fn register_common(a: &mut RecordAdapters, note: &mut impl FnMut(Result<(), DuplicateAdapter>)) {
    note(
        a.muxers
            .register(Arc::new(rbs_mux_matroska::MatroskaMuxerFactory)),
    );
    note(a.muxers.register(Arc::new(rbs_mux_fmp4::Fmp4MuxerFactory)));
    note(a.muxers.register(Arc::new(rbs_mux_wav::WavMuxerFactory)));
    note(a.muxers.register(Arc::new(rbs_mux_flac::FlacMuxerFactory)));
    note(
        a.audio_encoders
            .register(Arc::new(rbs_codec_pcm::PcmEncoderFactory)),
    );
    note(
        a.audio_encoders
            .register(Arc::new(rbs_codec_flac::FlacEncoderFactory)),
    );
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
    // Text sources (ADR-0129): parley on the system's fonts, loaded on first use.
    a.sources
        .register_text(Arc::new(rbs_text_parley::ParleyRenderer::system()));
    a.sources.set_text_clock(Arc::new(LocalTextClock));
}

/// Capture, audio input and output, and the platform's audio encoder.
fn register_platform_io(
    a: &mut RecordAdapters,
    note: &mut impl FnMut(Result<(), DuplicateAdapter>),
) {
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
            a.audio_encoders
                .register(Arc::new(rbs_codec_audiotoolbox::AacFactory::new())),
        );
    }
    let _ = (a, note);
}

/// Video encoders in preference order. Hardware adapters come first; a software
/// adapter is the last resort. The first that probes and opens wins.
fn video_encoder_factories() -> Vec<Arc<dyn VideoEncoderFactory>> {
    let mut factories: Vec<Arc<dyn VideoEncoderFactory>> = Vec::new();
    #[cfg(windows)]
    {
        factories.push(Arc::new(rbs_codec_nvenc::NvencFactory::new()));
        factories.push(Arc::new(rbs_codec_amf::AmfFactory::new()));
    }
    #[cfg(target_os = "linux")]
    {
        factories.push(Arc::new(rbs_codec_nvenc::NvencFactory::new()));
        factories.push(Arc::new(rbs_codec_amf::AmfFactory::new()));
        factories.push(Arc::new(rbs_codec_qsv::QsvFactory::new()));
        factories.push(Arc::new(rbs_codec_vaapi::VaapiFactory::new()));
    }
    #[cfg(target_os = "macos")]
    {
        factories.push(Arc::new(rbs_codec_videotoolbox::VideoToolboxFactory::new()));
        // Apple's software encoders: a last resort after the hardware adapter.
        factories.push(Arc::new(
            rbs_codec_videotoolbox::VideoToolboxFactory::with_software(),
        ));
    }
    factories
}

/// The reference capture and audio adapters, when asked for and built in.
fn register_demo(
    a: &mut RecordAdapters,
    note: &mut impl FnMut(Result<(), DuplicateAdapter>),
    options: &Options,
) {
    #[cfg(feature = "demo-sources")]
    if options.demo_sources {
        note(
            a.sources
                .register_capture(Arc::new(rbs_capture_test::TestCaptureFactory::new())),
        );
        note(
            a.sources
                .register_audio(Arc::new(rbs_audio_io_test::TestAudioBackend::new())),
        );
    }
    let _ = (a, note, options);
}

/// Media decoders in registration order (ADR-0108).
fn media_factories() -> Vec<Arc<dyn MediaDecoderFactory>> {
    let mut factories: Vec<Arc<dyn MediaDecoderFactory>> = Vec::new();
    // Animated and still images (GIF, APNG, WebP) in pure Rust. First, so it opens the
    // files it recognizes; it refuses the rest.
    factories.push(Arc::new(rbs_media_anim::AnimFactory::new()));
    // Video files through the platform, after the image decoder so an animated image
    // keeps its pure-Rust path.
    #[cfg(windows)]
    factories.push(Arc::new(rbs_media_mf::MfMediaFactory::new()));
    #[cfg(target_os = "macos")]
    factories.push(Arc::new(rbs_media_avf::AvfMediaFactory::new()));
    // Bright demo pattern for every other file until FFmpeg is linked. The file path is
    // ignored.
    factories.push(Arc::new(rbs_media_decode_test::ScriptFactory::new(
        rbs_media_decode_test::ScriptClip::demo_pattern(),
    )));
    factories
}

#[cfg(test)]
mod tests;
