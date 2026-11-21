//! Tests of the output profile's recording section.

use super::*;

/// ADR-0089 decisions 3 and 4: a file without `[markers]` takes the defaults, the
/// section round-trips, and an offset past 30 s is a problem.
#[test]
fn markers_default_round_trip_and_cap_the_lead() {
    let mut p = RecordProfile::new();
    let text = toml::to_string(&p).unwrap();
    let without: String = text.split("\n[markers]").next().unwrap().to_owned();
    let old: RecordProfile = toml::from_str(&without).unwrap();
    assert_eq!(old.markers, MarkerSettings::default());
    assert!(old.markers.scene_changes && !old.markers.stream_events);
    p.markers = MarkerSettings {
        lead_ms: 3_000,
        scene_changes: false,
        stream_events: true,
        pause_markers: false,
    };
    let back: RecordProfile = toml::from_str(&toml::to_string(&p).unwrap()).unwrap();
    assert_eq!(back.markers, p.markers);
    assert_eq!(MarkerSettings::from_api(p.markers.to_api()), p.markers);
    assert_eq!(p.problem(), None);
    p.markers.lead_ms = MAX_MARKER_LEAD_MS + 1;
    assert!(p.problem().is_some_and(|w| w.contains("30 seconds")));
}
use rbs_core::media::ColorSpace;

#[test]
fn the_hardware_limit_policy_defaults_to_ask_and_keeps_values_it_does_not_know() {
    let old: SeparateFiles = toml::from_str("format_version = 1\nenabled = false\n").unwrap();
    assert_eq!(old.on_hardware_limit, "ask");
    assert_eq!(old.limit_policy(), (LimitPolicy::Ask, true));
    let skip: SeparateFiles =
        toml::from_str("format_version = 1\nenabled = true\non_hardware_limit = \"skip\"\n")
            .unwrap();
    assert_eq!(skip.limit_policy(), (LimitPolicy::Skip, true));
    // A later build's value behaves as `ask` and is saved back as read.
    let later: SeparateFiles =
        toml::from_str("format_version = 1\nenabled = true\non_hardware_limit = \"cpu\"\n")
            .unwrap();
    assert_eq!(later.limit_policy(), (LimitPolicy::Ask, false));
    let text = toml::to_string(&later).unwrap();
    assert!(text.contains("on_hardware_limit = \"cpu\""), "{text}");
}

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rbs-app-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

#[test]
fn the_api_profile_round_trips() {
    for (format, rate, preset) in [
        (Format::Mkv, RateMode::Cbr, Preset::Quality),
        (Format::Mp4, RateMode::Vbr, Preset::Balanced),
        (Format::Mkv, RateMode::ConstantQuality, Preset::Speed),
    ] {
        let mut p = RecordProfile::new();
        p.folder = PathBuf::from("rec");
        p.use_format(format);
        p.rate_control = rate;
        p.preset = preset;
        p.encoder = Some("org.rbs.codec.nvenc".into());
        p.max_video_kbps = Some(20_000);
        p.profile = Some("high".into());
        p.b_frames = 2;
        p.cbr_padding = true;
        let api = p.to_api();
        assert_eq!(api.id, API_PROFILE_ID);
        assert_eq!(RecordProfile::from_api(&api), p);
        let listing = p.to_api_listing();
        assert_eq!(listing.video_kbps, p.video_kbps);
        assert_eq!(listing.encoder, p.encoder);
    }
}

#[test]
fn the_recommended_bitrate_follows_the_output_picture() {
    let h264 = VideoCodec::H264;
    assert_eq!(recommended_video_kbps(1920, 1080, 30, h264), Some(6_000));
    assert_eq!(recommended_video_kbps(2560, 1440, 30, h264), Some(9_000));
    assert_eq!(recommended_video_kbps(1920, 1080, 60, h264), Some(9_000));
    assert_eq!(recommended_video_kbps(3840, 2160, 30, h264), Some(20_000));
    assert_eq!(recommended_video_kbps(1280, 720, 30, h264), Some(2_500));
    // 24 fps stays on the 30 fps figure. 50 fps is (30 + 50) / 60.
    assert_eq!(recommended_video_kbps(1920, 1080, 24, h264), Some(6_000));
    assert_eq!(recommended_video_kbps(1920, 1080, 50, h264), Some(8_000));
    // A vertical 1080p picture has the same pixel count as 1920 x 1080.
    assert_eq!(recommended_video_kbps(1080, 1920, 30, h264), Some(6_000));
    assert_eq!(
        recommended_video_kbps(2560, 1440, 30, VideoCodec::Hevc),
        Some(6_000)
    );
    assert_eq!(
        recommended_video_kbps(2560, 1440, 30, VideoCodec::Av1),
        Some(4_500)
    );
    assert_eq!(recommended_video_kbps(2560, 1440, 60, h264), Some(13_500));
    assert_eq!(recommended_video_kbps(0, 1080, 30, h264), None);
    assert_eq!(recommended_video_kbps(1920, 1080, 0, h264), None);
}

#[test]
fn defaults_are_valid() {
    let p = RecordProfile::new();
    assert_eq!(p.problem(), None, "{p:?}");
    assert!(VIDEO_KBPS.contains(&p.video_kbps));
    assert!(AUDIO_KBPS.contains(&p.audio_kbps));
}

#[test]
fn a_saved_profile_loads_back() {
    let d = dir("profile");
    assert_eq!(load(&d), (RecordProfile::new(), None));
    let mut p = RecordProfile::new();
    p.folder = d.join("rec");
    p.use_format(Format::Mp4);
    p.encoder = Some("org.rbs.codec.nvenc".into());
    p.video_kbps = 8_000;
    p.audio_kbps = 192;
    p.rate_control = RateMode::Vbr;
    p.max_video_kbps = Some(20_000);
    p.quality = 18;
    p.keyframe_seconds = 4;
    p.profile = Some("main".into());
    p.preset = Preset::Quality;
    p.b_frames = 2;
    p.cbr_padding = true;
    save(&d, &p).unwrap();
    assert_eq!(load(&d), (p, None));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_bad_file_gives_the_defaults_and_why() {
    let d = dir("bad-profile");
    std::fs::create_dir_all(&d).unwrap();
    let mut p = RecordProfile::new();
    p.folder = d.clone();
    p.audio_kbps = 100;
    std::fs::write(file(&d), toml::to_string(&p).unwrap()).unwrap();
    let (got, why) = load(&d);
    assert_eq!(got, RecordProfile::new());
    assert!(why.unwrap().contains("96, 128, 160 or 192"));
    std::fs::write(file(&d), "not toml [").unwrap();
    assert!(load(&d).1.is_some());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_file_from_before_the_encoder_settings_loads_with_their_defaults() {
    let d = dir("v1-profile");
    std::fs::create_dir_all(&d).unwrap();
    let folder = d.join("rec");
    let text = format!(
        "format_version = 1\nfolder = {:?}\nformat = \"mp4\"\nvideo_kbps = 8000\naudio_kbps = 128\n",
        folder.display().to_string()
    );
    std::fs::write(file(&d), text).unwrap();
    let (p, why) = load(&d);
    assert_eq!(why, None);
    let mut want = RecordProfile::new();
    want.folder = folder;
    want.use_format(Format::Mp4);
    want.video_kbps = 8_000;
    want.audio_kbps = 128;
    assert_eq!(p, want);
    assert_eq!(p.delivery, Delivery::Video);
    assert_eq!(p.output_size, None, "an old file encodes at its canvas");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn screen_delivery_is_saved_and_video_is_omitted() {
    let d = dir("delivery");
    let mut p = RecordProfile::new();
    p.folder = d.join("rec");
    p.delivery = Delivery::Screen;
    save(&d, &p).unwrap();
    let text = std::fs::read_to_string(file(&d)).unwrap();
    assert!(text.contains("delivery = \"screen\""));
    assert_eq!(load(&d).0.delivery, Delivery::Screen);
    assert_eq!(p.video_settings().color, Delivery::Screen.color_space());
    p.delivery = Delivery::Video;
    save(&d, &p).unwrap();
    let text = std::fs::read_to_string(file(&d)).unwrap();
    assert!(
        !text.contains("delivery ="),
        "Video is the default and is left out of the file: {text}"
    );
    assert_eq!(load(&d).0.delivery, Delivery::Video);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn open_shadows_is_saved_when_on_and_applies_only_to_video() {
    let d = dir("open-shadows");
    let mut p = RecordProfile::new();
    p.folder = d.join("rec");
    assert!(!p.video_settings().open_shadows);
    p.open_shadows = true;
    save(&d, &p).unwrap();
    let text = std::fs::read_to_string(file(&d)).unwrap();
    assert!(text.contains("open_shadows = true"));
    assert!(load(&d).0.open_shadows);
    let v = p.video_settings();
    assert!(v.open_shadows);
    assert_eq!(v.color, ColorSpace::BT709_LIMITED);
    p.delivery = Delivery::Screen;
    assert!(
        !p.video_settings().open_shadows,
        "Screen already matches the display, so the grade stays off"
    );
    save(&d, &p).unwrap();
    let text = std::fs::read_to_string(file(&d)).unwrap();
    assert!(
        text.contains("open_shadows = true"),
        "the switch is kept while Screen is selected: {text}"
    );
    p.open_shadows = false;
    p.delivery = Delivery::Video;
    save(&d, &p).unwrap();
    let text = std::fs::read_to_string(file(&d)).unwrap();
    assert!(
        !text.contains("open_shadows"),
        "off is the default and is left out of the file: {text}"
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn video_settings_follow_the_profile() {
    let mut p = RecordProfile::new();
    let v = p.video_settings();
    assert_eq!(
        v.rate_control,
        RateControl::Cbr {
            bitrate_bps: 12_000_000
        }
    );
    assert_eq!(
        v.keyframe_interval,
        KeyframeInterval::Duration(Duration::from_secs(2))
    );
    assert_eq!(v.preset, EncoderPreset::Balanced);
    assert_eq!((v.max_b_frames, v.profile.as_deref()), (0, None));
    assert_eq!(v.color, ColorSpace::BT709_LIMITED);
    assert!(!v.open_shadows);
    assert_eq!(p.delivery, Delivery::Video);

    p.cbr_padding = true;
    p.keyframe_seconds = 5;
    p.profile = Some("high".into());
    p.preset = Preset::Speed;
    p.b_frames = 2;
    let v = p.video_settings();
    assert_eq!(
        v.rate_control,
        RateControl::CbrPadded {
            bitrate_bps: 12_000_000
        }
    );
    assert_eq!(
        v.keyframe_interval,
        KeyframeInterval::Duration(Duration::from_secs(5))
    );
    assert_eq!(v.profile.as_deref(), Some("high"));
    assert_eq!(v.preset, EncoderPreset::Speed);
    assert_eq!(v.max_b_frames, 2);

    p.rate_control = RateMode::Vbr;
    let v = p.video_settings();
    assert_eq!(
        v.rate_control,
        RateControl::Vbr {
            target_bps: 12_000_000,
            max_bps: 18_000_000
        }
    );
    p.max_video_kbps = Some(40_000);
    assert_eq!(
        p.video_settings().rate_control,
        RateControl::Vbr {
            target_bps: 12_000_000,
            max_bps: 40_000_000
        }
    );

    p.rate_control = RateMode::ConstantQuality;
    p.quality = 18;
    assert_eq!(
        p.video_settings().rate_control,
        RateControl::ConstantQuality { level: 18 }
    );
    p.video_codec = VideoCodec::Hevc;
    assert_eq!(p.video_settings().codec, CodecId::Hevc);
}

#[test]
fn problems_are_found() {
    let mut p = RecordProfile::new();
    p.folder = PathBuf::from("relative");
    assert!(p.problem().is_some());
    let mut p = RecordProfile::new();
    p.video_kbps = 100;
    assert!(p.problem().is_some());
    let mut p = RecordProfile::new();
    p.rate_control = RateMode::Vbr;
    p.max_video_kbps = Some(6_000);
    assert!(p.problem().unwrap().contains("below the bitrate"));
    p.rate_control = RateMode::Cbr;
    assert_eq!(p.problem(), None, "the peak only matters with VBR");
    let mut p = RecordProfile::new();
    p.quality = 52;
    assert!(p.problem().is_some());
    let mut p = RecordProfile::new();
    p.keyframe_seconds = 0;
    assert!(p.problem().is_some());
    let mut p = RecordProfile::new();
    p.b_frames = 5;
    assert!(p.problem().is_some());
}

#[test]
fn file_names_carry_the_local_start_time_and_extension() {
    let mut p = RecordProfile::new();
    p.folder = PathBuf::from("/videos");
    p.format = Format::Mp4;
    let t = chrono::NaiveDate::from_ymd_opt(2026, 11, 3)
        .unwrap()
        .and_hms_opt(21, 30, 5)
        .unwrap();
    assert_eq!(
        p.file_for(t),
        PathBuf::from("/videos").join("RBS 2026-11-03 21-30-05.mp4")
    );
    assert_eq!(Format::Mkv.container(), ContainerId::Matroska);
    assert_eq!(Format::Mp4.container(), ContainerId::Mp4Fragmented);
    assert_eq!(Format::Mov.container(), ContainerId::Mov);
    assert_eq!(Format::Mov.extension(), "mov");
    assert!(!Format::Mov.is_audio_only());
    assert_eq!(Format::Mov.category(), &Format::VIDEO);
    assert_eq!(Format::Mov.category_index(), 2);
    p.format = Format::Wav;
    p.bit_depth = 24;
    p.video_kbps = 1;
    assert_eq!(
        p.file_for(t),
        PathBuf::from("/videos").join("RBS 2026-11-03 21-30-05.wav")
    );
    p.folder = std::env::temp_dir();
    assert_eq!(
        p.problem(),
        None,
        "a WAV profile does not use the video bitrate"
    );
    let start = p.recording_start(Some("nvenc".into()), Some((1280, 720)));
    assert_eq!(start.container, ContainerId::Wav);
    assert_eq!(start.video, None);
    assert_eq!(start.bit_depth, Some(24));
    assert_eq!(start.encoder, None);
    assert_eq!(start.size, None);
    assert_eq!(start.audio_bitrate_bps, 0);
    let api = p.to_api();
    assert_eq!(api.container, rbs_api::types::Container::Wav);
    assert_eq!(api.bit_depth, 24);
    let back = RecordProfile::from_api(&api);
    assert_eq!(back.format, Format::Wav);
    assert_eq!(back.bit_depth, 24);
    p.bit_depth = 12;
    assert!(p.problem().is_some());
    p.bit_depth = 32;
    p.use_format(Format::Flac);
    assert_eq!(p.bit_depth, 16, "FLAC has no 32-bit float");
    assert_eq!(p.format, Format::Flac);
    p.bit_depth = 24;
    p.folder = std::env::temp_dir();
    assert_eq!(p.problem(), None);
    let start = p.recording_start(Some("nvenc".into()), Some((1280, 720)));
    assert_eq!(start.container, ContainerId::Flac);
    assert_eq!(start.video, None);
    assert_eq!(start.bit_depth, Some(24));
    assert_eq!(p.format.audio_codec(), CodecId::Flac);
    assert_eq!(
        p.file_for(t),
        std::env::temp_dir().join("RBS 2026-11-03 21-30-05.flac")
    );
    let api = p.to_api();
    assert_eq!(api.container, rbs_api::types::Container::Flac);
    let back = RecordProfile::from_api(&api);
    assert_eq!(back.format, Format::Flac);
    assert_eq!(back.bit_depth, 24);
    p.bit_depth = 32;
    assert!(p.problem().is_some());
}

#[test]
fn the_media_choice_keeps_each_category() {
    let mut p = RecordProfile::new();
    p.folder = std::env::temp_dir();
    p.use_format(Format::Mp4);
    p.use_media(true);
    assert_eq!(p.format, Format::Wav);
    p.use_format(Format::Flac);
    p.use_media(false);
    assert_eq!(p.format, Format::Mp4);
    p.use_media(true);
    assert_eq!(p.format, Format::Flac);
    assert_eq!(p.format.category_index(), 1);
}

#[test]
fn an_explicit_output_size_is_saved_and_an_absent_one_is_not() {
    let d = dir("output-size");
    let mut p = RecordProfile::new();
    p.folder = d.join("rec");
    save(&d, &p).unwrap();
    let text = std::fs::read_to_string(file(&d)).unwrap();
    assert!(
        !text.contains("output_size"),
        "same as canvas is left out of the file: {text}"
    );
    assert_eq!(load(&d).0.output_size, None);
    p.output_size = Some(OutputSize {
        width: 1280,
        height: 720,
    });
    save(&d, &p).unwrap();
    let text = std::fs::read_to_string(file(&d)).unwrap();
    assert!(text.contains("width = 1280"), "{text}");
    assert!(text.contains("height = 720"), "{text}");
    assert_eq!(load(&d).0.output_size, p.output_size);
    let api = p.to_api();
    assert_eq!(
        api.output_size.map(|s| (s.width, s.height)),
        Some((1280, 720))
    );
    assert_eq!(RecordProfile::from_api(&api).output_size, p.output_size);
    // One side only is not a size.
    let half = text.replace("height = 720\n", "");
    std::fs::write(file(&d), half).unwrap();
    let (got, why) = load(&d);
    assert_eq!(got.output_size, None);
    assert!(why.is_some(), "a half-specified size is refused");
    p.output_size = Some(OutputSize {
        width: 1921,
        height: 1080,
    });
    assert!(p.problem().unwrap().contains("even sides"));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn output_size_presets_keep_the_canvas_aspect() {
    let canvas = (1920, 1080);
    let options = output_size_options(canvas, None);
    assert_eq!(options[0].size, None);
    assert_eq!(options[0].label, "Same as canvas");
    let sizes: Vec<_> = options.iter().filter_map(|o| o.size).collect();
    assert!(!sizes.iter().any(|s| s.pair() == canvas));
    assert!(
        sizes
            .iter()
            .all(|s| s.width.is_multiple_of(2) && s.height.is_multiple_of(2))
    );
    assert!(sizes.iter().all(|s| s.width * 9 == s.height * 16));
    let down = options
        .iter()
        .find(|o| o.label == "1280 x 720 (downscale)")
        .and_then(|o| o.size)
        .unwrap();
    assert_eq!(
        output_choice_copy(down, canvas),
        ("720p".to_owned(), "1280 × 720 · downscale".to_owned())
    );
    let custom = fit_output_size(1600, canvas).unwrap();
    assert_eq!(
        output_choice_copy(custom, canvas),
        ("1600 × 900".to_owned(), "downscale".to_owned())
    );
    assert!(options.iter().any(|o| o.label == "1280 x 720 (downscale)"));
    assert!(options.iter().any(|o| o.label == "2560 x 1440 (upscale)"));
    assert!(options.iter().any(|o| o.label == "3840 x 2160 (upscale)"));
    assert!(!options.iter().any(|o| o.label.contains("1920 x 1080")));

    // An odd nearest short side takes the closer even neighbour (562.5 → 562).
    assert_eq!(
        fit_output_size(1000, canvas).map(OutputSize::pair),
        Some((1000, 562))
    );
    // Exact odd integer: the two even neighbours tie, and the smaller one wins.
    assert_eq!(
        fit_output_size(22, (200, 100)).map(OutputSize::pair),
        Some((22, 10))
    );

    let vertical = (1080, 1920);
    let options = output_size_options(vertical, None);
    assert!(options.iter().any(|o| {
        o.size.map(OutputSize::pair) == Some((720, 1280)) && o.label.contains("downscale")
    }));
    assert!(options.iter().any(|o| {
        o.size.map(OutputSize::pair) == Some((1440, 2560)) && o.label.contains("upscale")
    }));
    assert!(
        !options
            .iter()
            .any(|o| o.size.map(OutputSize::pair) == Some(vertical))
    );
    assert!(options.iter().filter_map(|o| o.size).all(|s| {
        s.height > s.width && s.width.is_multiple_of(2) && s.height.is_multiple_of(2)
    }));

    // A long side that cannot fit inside 2..=7680 is not offered.
    assert_eq!(fit_output_size(1280, (7680, 2)), None);
    assert!(
        output_size_options((7680, 2), None)
            .iter()
            .all(|o| o.size.is_none())
    );

    let saved = fit_output_size(1600, canvas).unwrap();
    assert_eq!(saved.pair(), (1600, 900));
    let options = output_size_options(canvas, Some(saved));
    assert!(options.iter().any(|o| o.size == Some(saved)));
    assert!(output_size_matches(saved.pair(), canvas));
    assert!(!output_size_matches((1280, 800), canvas));
    // A stored size equal to the canvas is the default row, not a second one.
    let same = OutputSize {
        width: 1920,
        height: 1080,
    };
    assert!(
        !output_size_options(canvas, Some(same))
            .iter()
            .any(|o| o.size == Some(same))
    );
}

#[test]
fn the_first_vertical_recording_omits_the_main_output_size() {
    let mut main = RecordProfile::new();
    main.output_size = Some(OutputSize {
        width: 1280,
        height: 720,
    });
    main.video_kbps = 8_000;
    let vertical = vertical_recording_from_main(&main);
    assert_eq!(vertical.output_size, None);
    assert_eq!(vertical.video_kbps, main.video_kbps);
    assert_eq!(vertical.folder, main.folder);
    assert_eq!(vertical.format, main.format);
}
