//! Screenshots of the Sources list with a picture's sound under it (ADR-0173), in Dark
//! and Light, for the owner's approval of the visual change (CLAUDE.md rule 16). It
//! writes raw RGBA files to `RBS_SHOTS_DIR` with Skia's real pixels; it is not a check,
//! so it is ignored and run by hand:
//!
//! `RBS_SHOTS_DIR=/tmp/shots cargo test -p rbs-app --test sound_nest_shots -- --ignored`

#![allow(clippy::expect_used, clippy::unwrap_used)] // A tool run by hand.

use rbs_app_ui::tokens_apply::{apply_tokens, slot};
use rbs_app_ui::{SourceData, SourcesProbe, Studio, Theme, Tokens};
use rbs_ui::tokens::{Base, ThemeBody, Tokens as TokenFile};
use slint::{ComponentHandle, ModelRc, VecModel};

fn save(px: &slint::SharedPixelBuffer<slint::Rgba8Pixel>, dir: &std::path::Path, name: &str) {
    let path = dir.join(format!("{name}-{}x{}.rgba", px.width(), px.height()));
    std::fs::write(&path, px.as_bytes()).expect("written");
}

/// One row as the app marks it (`studio_ui/sound_nest.rs`).
fn row(id: i32, name: &str, icon: &str, picture: bool) -> SourceData {
    SourceData {
        id,
        name: name.into(),
        kind: "Window".into(),
        icon: icon.into(),
        status: "Running".into(),
        ok: true,
        picture,
        visible: true,
        audio: !picture,
        volume_text: "-6.0 dB".into(),
        arrow_room: true,
        ..SourceData::default()
    }
}

fn rows(open: bool) -> Vec<SourceData> {
    let mut youtube = row(1, "YouTube", "window", true);
    youtube.expandable = true;
    youtube.sound_muted = true;
    youtube.companion = true;
    let mut yt_sound = row(2, "YouTube sound", "speaker", false);
    yt_sound.nested = true;
    yt_sound.parent = 1;
    yt_sound.muted = true;
    yt_sound.volume_text = "Silent".into();
    let mut discord = row(3, "Discord", "window", true);
    discord.expandable = true;
    discord.open = open;
    discord.companion = true;
    let mut ds_sound = row(4, "Discord sound", "speaker", false);
    ds_sound.nested = true;
    ds_sound.parent = 3;
    ds_sound.open = open;
    let camera = row(5, "Camera", "camera", true);
    let mut mic = row(6, "Mic", "mic", false);
    mic.volume_text = "0.0 dB".into();
    vec![youtube, yt_sound, discord, ds_sound, camera, mic]
}

fn shot(file: &TokenFile, base: Base, open: bool, name: &str, dir: &std::path::Path) {
    let theme = file
        .resolve(&ThemeBody::new(base))
        .expect("the theme resolves");
    let s = slot(&theme, "").expect("a slot");
    let w = SourcesProbe::new().expect("the probe");
    apply_tokens(
        &w.global::<Theme>(),
        &w.global::<Tokens>(),
        file,
        [s.clone(), s],
    )
    .expect("the tokens apply");
    let g = w.global::<Studio>();
    g.set_state(1);
    g.set_selected_source(if open { 4 } else { 1 });
    g.set_sources(ModelRc::new(VecModel::from(rows(open))));
    w.window().set_size(slint::LogicalSize::new(340.0, 300.0));
    w.show().expect("show");
    save(&w.window().take_snapshot().expect("a snapshot"), dir, name);
    w.hide().expect("hide");
}

#[test]
#[ignore = "a tool: writes screenshots for the owner's approval"]
fn sound_nest_screenshots() {
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
        shot(&file, base, false, &format!("closed-{b}"), &dir);
        shot(&file, base, true, &format!("open-{b}"), &dir);
    }
}
