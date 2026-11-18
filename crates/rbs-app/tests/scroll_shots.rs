//! Screenshots of Scroll in Properties (Transform tab) and One line on the Text tab
//! (ADR-0150 decision 12), in Dark and Light, for the owner's approval of the visual
//! change (CLAUDE.md rule 16). It writes raw RGBA files to `RBS_SHOTS_DIR` with Skia's
//! real pixels; it is not a check, so it is ignored and run by hand:
//!
//! `RBS_SHOTS_DIR=/tmp/shots cargo test -p rbs-app --test scroll_shots -- --ignored`

#![allow(clippy::expect_used, clippy::unwrap_used)] // A tool run by hand.

use rbs_app_ui::tokens_apply::{apply_tokens, slot};
use rbs_app_ui::{PropertiesProbe, Sections, SourceData, Studio, TextData, Theme, Tokens};
use rbs_ui::tokens::{Base, ThemeBody, Tokens as TokenFile};
use slint::{ComponentHandle, ModelRc, VecModel};

fn colour(hex: u32) -> slint::Color {
    slint::Color::from_argb_encoded(0xff00_0000 | hex)
}

fn source(scroll_on: bool, paused: bool) -> SourceData {
    SourceData {
        id: 7,
        name: "Ticker".into(),
        kind: "Text".into(),
        icon: "text".into(),
        ok: true,
        picture: true,
        visible: true,
        kind_code: -1,
        places: 1,
        x: 0.0,
        y: 980.0,
        w: 1920.0,
        h: 100.0,
        pic_w: 5200,
        pic_h: 100,
        scroll_on,
        scroll_dir: 0,
        scroll_speed: 120.0,
        scroll_gap: 80.0,
        scroll_repeat: 0,
        scroll_start: 1,
        scroll_fade: 24.0,
        scroll_paused: paused,
        border_color: "#ffffff".into(),
        border_swatch: colour(0xffffff),
        border_width: 4.0,
        ..Default::default()
    }
}

fn text() -> TextData {
    TextData {
        on: true,
        id: 7,
        kind: 0,
        text: "Breaking news: the match starts at 20:00 · Tickets are sold out".into(),
        size: 64.0,
        color: "#ffffff".into(),
        swatch: colour(0xffffff),
        align: 0,
        single_line: true,
        outline_color: "#000000".into(),
        outline_swatch: colour(0x000000),
        shadow_color: "#00000099".into(),
        background_color: "#00000099".into(),
        padding: 16.0,
        ..Default::default()
    }
}

/// `tab`: 0 Transform, 4 Text.
fn shot(
    file: &TokenFile,
    base: Base,
    name: &str,
    data: SourceData,
    tab: i32,
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
    // Scroll starts closed in the app; the shots open it and fold the sections above
    // it so it shows in full. The text effects stay closed.
    let closed = [
        "properties.arrange",
        "properties.crop",
        "properties.shape",
        "properties.text-outline",
        "properties.text-shadow",
        "properties.text-background",
        "properties.timed-show",
        "properties.scenes",
        "properties.picture-recording",
    ];
    w.global::<Sections>()
        .on_closed(move |id, _| closed.contains(&id.as_str()));
    let g = w.global::<Studio>();
    g.set_sources(ModelRc::new(VecModel::from(vec![data])));
    g.set_properties_source(7);
    g.set_properties_tab(tab);
    g.set_text_props(text());
    w.window().set_size(slint::LogicalSize::new(380.0, 1000.0));
    w.show().expect("show");
    let px = w.window().take_snapshot().expect("a snapshot");
    let path = dir.join(format!("{name}-{}x{}.rgba", px.width(), px.height()));
    std::fs::write(&path, px.as_bytes()).expect("written");
    w.hide().expect("hide");
}

#[test]
#[ignore = "a tool: writes screenshots for the owner's approval"]
fn scroll_screenshots() {
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
        shot(
            &file,
            base,
            &format!("scroll-on-{b}"),
            source(true, false),
            0,
            &dir,
        );
        shot(
            &file,
            base,
            &format!("scroll-paused-{b}"),
            source(true, true),
            0,
            &dir,
        );
        shot(
            &file,
            base,
            &format!("scroll-off-{b}"),
            source(false, false),
            0,
            &dir,
        );
        shot(
            &file,
            base,
            &format!("one-line-{b}"),
            source(true, false),
            4,
            &dir,
        );
    }
}
