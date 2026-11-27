//! Screenshots of the Mixer's master strip with loudness on (columns and rows) and of a
//! camera's Transform tab at the right dock's default width, in Dark and Light, for the
//! owner's approval of the layout fixes (CLAUDE.md rule 16). Writes raw RGBA files to
//! `RBS_SHOTS_DIR` with Skia's real pixels; it is not a check, so it is ignored and run
//! by hand:
//!
//! `RBS_SHOTS_DIR=/tmp/shots cargo test -p rbs-app --test mixer_layout_shots -- --ignored`
//!
//! `RBS_SHOTS_WIDTH` sets the Properties width (default 260, the dock's default).

#![allow(clippy::expect_used, clippy::unwrap_used)] // A tool run by hand.

use rbs_app_ui::tokens_apply::{apply_tokens, slot};
use rbs_app_ui::{
    LoudnessData, MainWindow, MeterRow, PropertiesProbe, Sections, SourceData, StackData, Studio,
    TabData, Theme, Tokens,
};
use rbs_ui::tokens::{Base, ThemeBody, Tokens as TokenFile};
use slint::{ComponentHandle, ModelRc, VecModel};

fn strip(id: i32, name: &str, icon: &str, volume_text: &str) -> MeterRow {
    MeterRow {
        id,
        name: name.into(),
        icon: icon.into(),
        volume: 0.8,
        volume_text: volume_text.into(),
        lo_from: 0.0,
        lo_to: 0.55,
        mi_from: 0.55,
        mi_to: 0.62,
        peak: 0.66,
        peak_zone: 1,
        live: true,
        ..Default::default()
    }
}

fn loudness() -> LoudnessData {
    LoudnessData {
        on: true,
        has: true,
        short_text: "\u{2212}14.2".into(),
        at: 0.6,
        target_at: 0.62,
        target_text: "\u{2212}14 LUFS".into(),
        over: false,
        momentary: "\u{2212}13.8 LUFS".into(),
        integrated: "\u{2212}15.1 LUFS".into(),
        range: "6.3 LU".into(),
        true_peak: "\u{2212}1.2 dBTP".into(),
        limit_text: "\u{2212}1 dBTP".into(),
    }
}

fn mixer(
    file: &TokenFile,
    base: Base,
    name: &str,
    orientation: i32,
    w: f32,
    h: f32,
    dir: &std::path::Path,
) {
    let theme = file
        .resolve(&ThemeBody::new(base))
        .expect("the theme resolves");
    let s = slot(&theme, "").expect("a slot");
    let m = MainWindow::new().expect("the main window");
    apply_tokens(
        &m.global::<Theme>(),
        &m.global::<Tokens>(),
        file,
        [s.clone(), s],
    )
    .expect("the tokens apply");
    m.global::<Sections>().set_mixer_orientation(orientation);
    let g = m.global::<Studio>();
    g.set_state(1);
    g.set_meters(ModelRc::new(VecModel::from(vec![
        strip(1, "Microphone", "mic", "\u{2212}3.2 dB"),
        strip(2, "Desktop audio", "speaker", "0 dB"),
    ])));
    g.set_meter_scenes(2);
    g.set_master_meter(ModelRc::new(VecModel::from(vec![strip(
        -1,
        "Master",
        "speaker",
        "\u{2212}1.5 dB",
    )])));
    g.set_loudness_rows(ModelRc::new(VecModel::from(vec![loudness()])));
    let tab = TabData {
        id: "rbs.mixer".into(),
        title: "Mixer".into(),
        known: true,
        icon: "".into(),
    };
    m.set_stacks(ModelRc::new(VecModel::from(vec![StackData {
        window: 0,
        stack: 0,
        x: 16.0,
        y: 48.0,
        w,
        h,
        tabs: ModelRc::new(VecModel::from(vec![tab])),
        active: 0,
        collapsed: false,
        can_collapse: true,
        can_hide: true,
        takes_drop: true,
    }])));
    m.window()
        .set_size(slint::LogicalSize::new(w + 32.0, h + 120.0));
    m.show().expect("show");
    let px = m.window().take_snapshot().expect("a snapshot");
    let path = dir.join(format!("{name}-{}x{}.rgba", px.width(), px.height()));
    std::fs::write(&path, px.as_bytes()).expect("written");
    m.hide().expect("hide");
}

fn camera() -> SourceData {
    SourceData {
        id: 7,
        name: "Camera".into(),
        kind: "Camera".into(),
        icon: "camera".into(),
        ok: true,
        picture: true,
        audio: true,
        visible: true,
        kind_code: -1,
        places: 1,
        x: 1280.0,
        y: 720.0,
        w: 640.0,
        h: 360.0,
        pic_w: 1920,
        pic_h: 1080,
        border_color: "#ffffff".into(),
        ..Default::default()
    }
}

fn transform(file: &TokenFile, base: Base, name: &str, scroll_on: bool, dir: &std::path::Path) {
    let theme = file
        .resolve(&ThemeBody::new(base))
        .expect("the theme resolves");
    let s = slot(&theme, "").expect("a slot");
    let w = PropertiesProbe::new().expect("the probe window");
    apply_tokens(
        &w.global::<Theme>(),
        &w.global::<Tokens>(),
        file,
        [s.clone(), s],
    )
    .expect("the tokens apply");
    w.global::<Sections>().on_closed(|_, _| false);
    let g = w.global::<Studio>();
    let mut source = camera();
    source.scroll_on = scroll_on;
    source.scroll_speed = 120.0;
    source.scroll_gap = 80.0;
    source.scroll_fade = 24.0;
    g.set_sources(ModelRc::new(VecModel::from(vec![source])));
    g.set_properties_source(7);
    g.set_properties_tab(0);
    let width = std::env::var("RBS_SHOTS_WIDTH")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(260.0);
    w.window().set_size(slint::LogicalSize::new(width, 1800.0));
    w.show().expect("show");
    let px = w.window().take_snapshot().expect("a snapshot");
    let path = dir.join(format!("{name}-{}x{}.rgba", px.width(), px.height()));
    std::fs::write(&path, px.as_bytes()).expect("written");
    w.hide().expect("hide");
}

#[test]
#[ignore = "a tool: writes screenshots for the owner's approval"]
fn mixer_layout_screenshots() {
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
        mixer(
            &file,
            base,
            &format!("mixer-columns-{b}"),
            2,
            760.0,
            300.0,
            &dir,
        );
        mixer(
            &file,
            base,
            &format!("mixer-columns-short-{b}"),
            2,
            760.0,
            220.0,
            &dir,
        );
        mixer(
            &file,
            base,
            &format!("mixer-rows-{b}"),
            1,
            460.0,
            640.0,
            &dir,
        );
        transform(&file, base, &format!("transform-{b}"), false, &dir);
        transform(&file, base, &format!("transform-scroll-{b}"), true, &dir);
    }
}
