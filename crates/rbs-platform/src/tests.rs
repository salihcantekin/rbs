//! The list's contents and order (ADR-0160 decision 6). Nothing here probes.

use rbs_core::registry::{AdapterFactory, AdapterId, Registry};

use super::*;

fn ids<F: ?Sized + AdapterFactory>(registry: &Registry<F>) -> Vec<String> {
    registry.ids().map(|id| id.as_str().to_owned()).collect()
}

fn strings(ids: &[&str]) -> Vec<String> {
    ids.iter().map(|id| (*id).to_owned()).collect()
}

/// Capture adapters, audio adapters, video encoders and audio encoders of this build
/// platform, in registration order. Any other target has the common part only.
fn expected_platform() -> [Vec<&'static str>; 4] {
    #[cfg(windows)]
    {
        [
            vec![rbs_capture_wgc::ADAPTER_ID, rbs_capture_mf::ADAPTER_ID],
            vec![rbs_audio_io_wasapi::ADAPTER_ID],
            vec![rbs_codec_nvenc::ADAPTER_ID, rbs_codec_amf::ADAPTER_ID],
            vec![rbs_codec_mf::ADAPTER_ID],
        ]
    }
    #[cfg(target_os = "linux")]
    {
        [
            vec![rbs_capture_pipewire::ADAPTER_ID],
            vec![rbs_audio_io_pipewire::ADAPTER_ID],
            vec![
                rbs_codec_nvenc::ADAPTER_ID,
                rbs_codec_amf::ADAPTER_ID,
                rbs_codec_qsv::ADAPTER_ID,
                rbs_codec_vaapi::ADAPTER_ID,
            ],
            vec![rbs_codec_ffmpeg::ADAPTER_ID],
        ]
    }
    #[cfg(target_os = "macos")]
    {
        [
            vec![rbs_capture_sck::ADAPTER_ID, rbs_capture_avf::ADAPTER_ID],
            vec![rbs_audio_io_coreaudio::ADAPTER_ID],
            vec![
                rbs_codec_videotoolbox::ADAPTER_ID,
                rbs_codec_videotoolbox::SOFTWARE_ADAPTER_ID,
            ],
            vec![rbs_codec_audiotoolbox::ADAPTER_ID],
        ]
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        [vec![], vec![], vec![], vec![]]
    }
}

#[test]
fn every_registry_lists_its_adapters_in_order() {
    let mut registered = adapters(&Options::default());
    assert!(registered.errors.is_empty(), "{:?}", registered.errors);
    let [capture, audio, video, platform_audio] = expected_platform();
    let a = &mut registered.adapters;
    assert_eq!(
        ids(&a.muxers),
        strings(&[
            rbs_mux_matroska::ADAPTER_ID,
            rbs_mux_fmp4::ADAPTER_ID,
            rbs_mux_wav::ADAPTER_ID,
            rbs_mux_flac::ADAPTER_ID,
            rbs_mux_mpegts::ADAPTER_ID,
        ])
    );
    let mut audio_encoders = vec![rbs_codec_pcm::ADAPTER_ID, rbs_codec_flac::ADAPTER_ID];
    audio_encoders.extend(platform_audio);
    assert_eq!(ids(&a.audio_encoders), strings(&audio_encoders));
    assert_eq!(
        ids(&a.transports),
        strings(&[
            rbs_transport_file::ADAPTER_ID,
            rbs_transport_rtmp::ADAPTER_ID,
            rbs_transport_srt::ADAPTER_ID,
        ])
    );
    assert_eq!(ids(&a.video_encoders), strings(&video));
    assert_eq!(ids(a.sources.capture_adapters()), strings(&capture));
    assert_eq!(ids(a.sources.audio_adapters()), strings(&audio));
    assert_eq!(a.sources.text_renderers().len(), 1);
    assert!(a.sources.text_clock().is_some());
}

#[test]
fn the_video_encoder_list_is_the_registry_in_order() {
    let registered = adapters(&Options::default());
    let listed: Vec<AdapterId> = registered.video_encoders.iter().map(|f| f.id()).collect();
    let in_registry: Vec<AdapterId> = registered.adapters.video_encoders.ids().cloned().collect();
    assert_eq!(listed, in_registry);
}

#[test]
fn media_decoders_start_with_images_and_end_with_the_demo_pattern() {
    let ids: Vec<AdapterId> = media_factories().iter().map(|f| f.id()).collect();
    let demo = rbs_media_decode_test::ScriptFactory::new(
        rbs_media_decode_test::ScriptClip::demo_pattern(),
    )
    .id();
    let mut expected = vec![AdapterId::new(rbs_media_anim::ADAPTER_ID)];
    #[cfg(windows)]
    expected.push(AdapterId::new(rbs_media_mf::ADAPTER_ID));
    #[cfg(target_os = "macos")]
    expected.push(AdapterId::new(rbs_media_avf::ADAPTER_ID));
    expected.push(demo);
    assert_eq!(ids, expected);
}

#[cfg(feature = "demo-sources")]
#[test]
fn demo_sources_are_registered_only_when_asked() {
    let mut off = adapters(&Options::default());
    let mut on = adapters(&Options::default().with_demo_sources(true));
    assert!(on.errors.is_empty(), "{:?}", on.errors);
    let test_capture = rbs_capture_test::ADAPTER_ID.to_owned();
    let test_audio = rbs_audio_io_test::ADAPTER_ID.to_owned();
    assert!(!ids(off.adapters.sources.capture_adapters()).contains(&test_capture));
    assert!(!ids(off.adapters.sources.audio_adapters()).contains(&test_audio));
    assert!(ids(on.adapters.sources.capture_adapters()).contains(&test_capture));
    assert!(ids(on.adapters.sources.audio_adapters()).contains(&test_audio));
}

#[test]
fn the_link_file_is_in_the_settings_folder() {
    let config = std::path::Path::new("settings");
    assert_eq!(vcam_link_file(config), config.join("vcam-link.txt"));
}

#[cfg(not(windows))]
#[test]
fn no_virtual_camera_outside_windows() {
    let config = std::path::Path::new("settings");
    assert!(matches!(
        virtual_camera(config),
        Err(VcamUnavailable::NotOnThisPlatform)
    ));
}
