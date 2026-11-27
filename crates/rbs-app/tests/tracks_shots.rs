//! Screenshots of the tracks (ADR-0170 decision 9) in Dark and Light, for the owner's
//! approval of the visual change (CLAUDE.md rule 16): the Mixer's Tracks group in
//! columns and in rows, and the Recording and Stream settings' track rows. It writes
//! raw RGBA files to `RBS_SHOTS_DIR` with Skia's real pixels; it is not a check, so it
//! is ignored and run by hand:
//!
//! `RBS_SHOTS_DIR=/tmp/shots cargo test -p rbs-app --test tracks_shots -- --ignored`

#![allow(clippy::expect_used, clippy::unwrap_used)] // A tool run by hand.

use rbs_app_ui::tokens_apply::{apply_tokens, slot};
use rbs_app_ui::{
    Experience, MeterRow, MixerProbe, ScaleMark, Sections, SettingsWindow, Studio, Theme, Tokens,
};
use rbs_ui::tokens::{Base, ThemeBody, Tokens as TokenFile};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

fn save(px: &slint::SharedPixelBuffer<slint::Rgba8Pixel>, dir: &std::path::Path, name: &str) {
    let path = dir.join(format!("{name}-{}x{}.rgba", px.width(), px.height()));
    std::fs::write(&path, px.as_bytes()).expect("written");
}

fn tokens(file: &TokenFile, base: Base, theme: &Theme<'_>, t: &Tokens<'_>) {
    let resolved = file
        .resolve(&ThemeBody::new(base))
        .expect("the theme resolves");
    let s = slot(&resolved, "").expect("a slot");
    apply_tokens(theme, t, file, [s.clone(), s]).expect("the tokens apply");
}

/// A strip lit to `level` (0 to 1 on the meter law), as the meter tick paints it.
fn strip(id: i32, name: &str, icon: &str, db: f32, level: f32) -> MeterRow {
    let lo = level.min(0.7);
    let mi = level.clamp(0.7, 0.9);
    MeterRow {
        id,
        name: name.into(),
        icon: icon.into(),
        volume: rbs_ui::fader::position_of(10f32.powf(db / 20.0)),
        volume_text: rbs_ui::fader::volume_text(10f32.powf(db / 20.0)).into(),
        lo_from: 0.0,
        lo_to: lo,
        mi_from: 0.7,
        mi_to: mi,
        hi_from: 0.9,
        hi_to: level.max(0.9),
        peak: (level + 0.04).min(1.0),
        peak_zone: if level > 0.7 { 1 } else { 0 },
        live: true,
        ..MeterRow::default()
    }
}

fn strings(items: &[&str]) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(
        items
            .iter()
            .map(|s| SharedString::from(*s))
            .collect::<Vec<_>>(),
    ))
}

fn mixer(file: &TokenFile, base: Base, columns: bool, name: &str, dir: &std::path::Path) {
    let w = MixerProbe::new().expect("the probe");
    tokens(file, base, &w.global::<Theme>(), &w.global::<Tokens>());
    w.global::<Experience>().set_advanced(true);
    let g = w.global::<Studio>();
    g.set_state(1);
    let sources = vec![
        strip(1, "Mic", "mic", 0.0, 0.62),
        strip(2, "Game audio", "speaker", -6.0, 0.55),
        strip(3, "Music", "speaker", -12.0, 0.4),
    ];
    let values = strings(&["-12 dB", "-16 dB", "-24 dB"]);
    g.set_meters(ModelRc::new(VecModel::from(sources)));
    g.set_meter_values(values);
    g.set_meter_scenes(3);
    g.set_master_meter(ModelRc::new(VecModel::from(vec![strip(
        -1, "Master", "speaker", 0.0, 0.68,
    )])));
    g.set_master_meter_values(strings(&["-10 dB"]));
    let mut muted = strip(1, "Voice only", "speaker", 0.0, 0.0);
    muted.muted = true;
    g.set_track_meters(ModelRc::new(VecModel::from(vec![
        strip(0, "No music", "speaker", 0.0, 0.64),
        muted,
    ])));
    g.set_track_meter_values(strings(&["-11 dB", "silent"]));
    let scale: Vec<ScaleMark> = [("-60", 0.0), ("-40", 0.33), ("-20", 0.67), ("0", 1.0)]
        .into_iter()
        .map(|(l, at)| ScaleMark {
            label: l.into(),
            at,
        })
        .collect();
    g.set_scale(ModelRc::new(VecModel::from(scale)));
    w.global::<Sections>()
        .set_mixer_orientation(if columns { 2 } else { 1 });
    let size = if columns {
        slint::LogicalSize::new(820.0, 320.0)
    } else {
        slint::LogicalSize::new(360.0, 620.0)
    };
    w.window().set_size(size);
    w.show().expect("show");
    save(&w.window().take_snapshot().expect("a snapshot"), dir, name);
    w.hide().expect("hide");
}

fn settings(file: &TokenFile, base: Base, section: i32, name: &str, dir: &std::path::Path) {
    let w = SettingsWindow::new().expect("the settings window");
    tokens(file, base, &w.global::<Theme>(), &w.global::<Tokens>());
    w.global::<Experience>().set_advanced(true);
    let g = w.global::<Studio>();
    g.set_state(1);
    g.set_rec_form_idle(true);
    g.set_st_form_idle(true);
    g.set_out_folder("~/Movies".into());
    g.set_out_containers(strings(&["MKV", "MP4", "MOV"]));
    g.set_out_container(0);
    g.set_out_tracks(strings(&["Master", "No music", "Voice only"]));
    g.set_out_tracks_on(ModelRc::new(VecModel::from(vec![true, true, false])));
    g.set_st_audio_buses(strings(&["Master", "No music", "Voice only"]));
    g.set_st_audio_bus(1);
    w.set_section(section);
    w.window().set_size(slint::LogicalSize::new(960.0, 900.0));
    w.show().expect("show");
    save(&w.window().take_snapshot().expect("a snapshot"), dir, name);
    w.hide().expect("hide");
}

#[test]
#[ignore = "a tool: writes screenshots for the owner's approval"]
fn tracks_screenshots() {
    let dir = std::path::PathBuf::from(std::env::var_os("RBS_SHOTS_DIR").expect("RBS_SHOTS_DIR"));
    std::fs::create_dir_all(&dir).unwrap();
    let backend = i_slint_backend_testing::TestingBackend::new(
        i_slint_backend_testing::TestingBackendOptions {
            renderer_name: Some("skia".into()),
            ..Default::default()
        },
    );
    slint::platform::set_platform(Box::new(backend)).expect("the testing platform");
    let file = TokenFile::built_in().unwrap();
    for (base, b) in [(Base::Dark, "dark"), (Base::Light, "light")] {
        mixer(&file, base, true, &format!("mixer-columns-{b}"), &dir);
        mixer(&file, base, false, &format!("mixer-rows-{b}"), &dir);
        settings(&file, base, 1, &format!("recording-{b}"), &dir);
        settings(&file, base, 2, &format!("stream-{b}"), &dir);
    }
}
