//! Screenshots of a media file's Properties with the play range row (ADR-0108
//! decision 11), in Dark and Light, for the owner's approval of the visual change
//! (CLAUDE.md rule 16). It writes raw RGBA files to `RBS_SHOTS_DIR` with Skia's real
//! pixels; it is not a check, so it is ignored and run by hand:
//!
//! `RBS_SHOTS_DIR=/tmp/shots cargo test -p rbs-app --test media_range_shots -- --ignored`

#![allow(clippy::expect_used, clippy::unwrap_used)] // A tool run by hand.

use rbs_app_ui::tokens_apply::{apply_tokens, slot};
use rbs_app_ui::{PropertiesProbe, SourceData, Studio, Theme, Tokens};
use rbs_ui::tokens::{Base, ThemeBody, Tokens as TokenFile};
use slint::{ComponentHandle, ModelRc, VecModel};

fn media(start_ms: i32, end_ms: i32) -> SourceData {
    SourceData {
        id: 7,
        name: "Intro clip".into(),
        kind: "Media file".into(),
        icon: "media".into(),
        status: "Running".into(),
        ok: true,
        picture: true,
        audio: true,
        visible: true,
        kind_code: -1,
        places: 1,
        media: true,
        media_path: "/Users/me/Movies/intro.mp4".into(),
        media_loop: true,
        act_on: true,
        act_run: 0,
        act_return: 0,
        media_start_ms: start_ms,
        media_end_ms: end_ms,
        media_state: 1,
        media_pos_ms: 41_300,
        media_len_ms: 95_000,
        ..Default::default()
    }
}

fn shot(
    file: &TokenFile,
    base: Base,
    name: &str,
    data: SourceData,
    width: f32,
    dir: &std::path::Path,
) {
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
    let g = w.global::<Studio>();
    g.set_sources(ModelRc::new(VecModel::from(vec![data])));
    g.set_properties_source(7);
    g.set_properties_tab(3);
    w.window().set_size(slint::LogicalSize::new(width, 760.0));
    w.show().expect("show");
    let px = w.window().take_snapshot().expect("a snapshot");
    let path = dir.join(format!("{name}-{}x{}.rgba", px.width(), px.height()));
    std::fs::write(&path, px.as_bytes()).expect("written");
    w.hide().expect("hide");
}

#[test]
#[ignore = "a tool: writes screenshots for the owner's approval"]
fn media_range_screenshots() {
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
        let range = media(12_400, 65_000);
        shot(&file, base, &format!("range-{b}"), range, 380.0, &dir);
        shot(
            &file,
            base,
            &format!("whole-{b}"),
            media(0, -1),
            380.0,
            &dir,
        );
    }
}
