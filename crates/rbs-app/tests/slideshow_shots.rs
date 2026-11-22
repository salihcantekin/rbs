//! Screenshots of a slide show's Source tab in Properties (ADR-0171): the folder row,
//! the playback row and the settings drawn from the type's schema, in Dark and Light,
//! for the owner's approval of the visual change (CLAUDE.md rule 16). It writes raw
//! RGBA files to `RBS_SHOTS_DIR`; it is not a check, so it is ignored and run by hand:
//!
//! `RBS_SHOTS_DIR=/tmp/shots cargo test -p rbs-app --test slideshow_shots -- --ignored`

#![allow(clippy::expect_used, clippy::unwrap_used)] // A tool run by hand.

use rbs_app_ui::tokens_apply::{apply_tokens, slot};
use rbs_app_ui::{FilterFieldData, PropertiesProbe, SourceData, Studio, Theme, Tokens};
use rbs_ui::tokens::{Base, ThemeBody, Tokens as TokenFile};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

fn options(names: &[&str]) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(
        names
            .iter()
            .map(|n| SharedString::from(*n))
            .collect::<Vec<_>>(),
    ))
}

fn choice(label: &str, names: &[&str], choice: i32, help: &str) -> FilterFieldData {
    choice_from(label, names, choice, 0, help)
}

fn choice_from(
    label: &str,
    names: &[&str],
    choice: i32,
    default: usize,
    help: &str,
) -> FilterFieldData {
    FilterFieldData {
        label: label.into(),
        help: help.into(),
        kind: 4,
        options: options(names),
        choice,
        changed: choice != default as i32,
        default_text: names[default].into(),
        ..Default::default()
    }
}

fn toggle(label: &str, on: bool, help: &str) -> FilterFieldData {
    FilterFieldData {
        label: label.into(),
        help: help.into(),
        kind: 5,
        choice: i32::from(on),
        ..Default::default()
    }
}

fn number(label: &str, position: f32, text: &str, changed: bool) -> FilterFieldData {
    FilterFieldData {
        label: label.into(),
        kind: 0,
        position,
        step: 0.01,
        value_text: text.into(),
        changed,
        default_text: "default".into(),
        ..Default::default()
    }
}

fn slides(paused: bool) -> SourceData {
    let fields = vec![
        choice(
            "Advance",
            &["Automatic", "Manual"],
            0,
            "Automatic changes the picture on a timer; Manual waits for Next or Previous.",
        ),
        number("Show each", 0.2, "5 s", false),
        choice("Order", &["In order", "Shuffle"], 0, ""),
        toggle("Loop", true, "Start again after the last picture."),
        choice_from("Transition", &["Cut", "Fade", "Slide", "Swipe"], 1, 1, ""),
        number("Transition duration", 0.14, "700 ms", false),
        choice("Fit", &["Fit inside", "Fill", "Stretch"], 0, ""),
        toggle(
            "Watch folder",
            false,
            "Pick up pictures added to or removed from the folder.",
        ),
    ];
    SourceData {
        id: 7,
        name: "Sponsors".into(),
        kind: "Slide show".into(),
        icon: "image".into(),
        status: "Running".into(),
        ok: true,
        picture: true,
        visible: true,
        kind_code: -1,
        places: 1,
        slides: true,
        slides_folder: "/Users/me/Pictures/Sponsors".into(),
        slides_note: "24 pictures".into(),
        slides_position: "3 / 24".into(),
        slides_paused: paused,
        slides_fields: ModelRc::new(VecModel::from(fields)),
        ..Default::default()
    }
}

fn shot(file: &TokenFile, base: Base, name: &str, data: SourceData, dir: &std::path::Path) {
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
    w.window().set_size(slint::LogicalSize::new(380.0, 900.0));
    w.show().expect("show");
    let px = w.window().take_snapshot().expect("a snapshot");
    let path = dir.join(format!("{name}-{}x{}.rgba", px.width(), px.height()));
    std::fs::write(&path, px.as_bytes()).expect("written");
    w.hide().expect("hide");
}

#[test]
#[ignore = "a tool: writes screenshots for the owner's approval"]
fn slideshow_screenshots() {
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
        shot(&file, base, &format!("slideshow-{b}"), slides(false), &dir);
    }
}
