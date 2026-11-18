//! Screenshots of the Interact window's reasons and of Forget sign-in's question on
//! the Page tab (ADR-0132 decisions 6 and 10), in Dark and Light, for the owner's
//! approval of the visual change (CLAUDE.md rule 16). It writes raw RGBA files to
//! `RBS_SHOTS_DIR` with Skia's real pixels; it is not a check, so it is ignored and run
//! by hand (the question needs the element tree):
//!
//! `SLINT_EMIT_DEBUG_INFO=1 RBS_SHOTS_DIR=/tmp/shots cargo test -p rbs-app --test interact_shots -- --ignored`

#![allow(clippy::expect_used, clippy::unwrap_used)] // A tool run by hand.

use rbs_app_ui::tokens_apply::{apply_tokens, slot};
use rbs_app_ui::{BrowserData, InteractWindow, PropertiesProbe, SourceData, Studio, Theme, Tokens};
use rbs_ui::tokens::{Base, ThemeBody, Tokens as TokenFile};
use slint::{ComponentHandle, ModelRc, VecModel};

fn save(px: &slint::SharedPixelBuffer<slint::Rgba8Pixel>, dir: &std::path::Path, name: &str) {
    let path = dir.join(format!("{name}-{}x{}.rgba", px.width(), px.height()));
    std::fs::write(&path, px.as_bytes()).expect("written");
}

fn interact(file: &TokenFile, base: Base, name: &str, state: i32, dir: &std::path::Path) {
    let theme = file
        .resolve(&ThemeBody::new(base))
        .expect("the theme resolves");
    let s = slot(&theme, "").expect("a slot");
    let w = InteractWindow::new().expect("the window");
    apply_tokens(
        &w.global::<Theme>(),
        &w.global::<Tokens>(),
        file,
        [s.clone(), s],
    )
    .expect("the tokens apply");
    w.set_window_title("Interact – Chat".into());
    w.set_state(state);
    w.set_status("The page did not answer (connection timed out). Trying again in 8 s.".into());
    w.window().set_size(slint::LogicalSize::new(640.0, 360.0));
    w.show().expect("show");
    save(&w.window().take_snapshot().expect("a snapshot"), dir, name);
    w.hide().expect("hide");
}

fn forget(file: &TokenFile, base: Base, name: &str, dir: &std::path::Path) {
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
    g.set_sources(ModelRc::new(VecModel::from(vec![SourceData {
        id: 7,
        name: "Chat".into(),
        kind: "Browser".into(),
        browser: true,
        icon: "globe".into(),
        ok: true,
        picture: true,
        audio: true,
        visible: true,
        kind_code: -1,
        places: 1,
        ..Default::default()
    }])));
    g.set_properties_source(7);
    g.set_properties_tab(5);
    g.set_browser_props(BrowserData {
        on: true,
        id: 7,
        url: "https://streamelements.com/overlay/5f2c/chat".into(),
        width: 1280,
        height: 720,
        canvas_w: 1920,
        canvas_h: 1080,
        fps: 30,
        zoom: 100,
        retry_on_error: true,
        keep_signed_in: true,
        running: true,
        ..Default::default()
    });
    w.window().set_size(slint::LogicalSize::new(380.0, 1700.0));
    w.show().expect("show");
    // A first frame makes the tab's rows.
    let _ = w.window().take_snapshot();
    let button = i_slint_backend_testing::ElementQuery::from_root(&w)
        .match_descendants()
        .match_accessible_role(i_slint_backend_testing::AccessibleRole::Button)
        .find_all()
        .into_iter()
        .find(|e| {
            e.accessible_label()
                .is_some_and(|l| l == "Forget the sign-in of Chat")
        })
        .expect("Forget sign-in (build with SLINT_EMIT_DEBUG_INFO=1)");
    button.invoke_accessible_default_action();
    save(&w.window().take_snapshot().expect("a snapshot"), dir, name);
    w.hide().expect("hide");
}

#[test]
#[ignore = "a tool: writes screenshots for the owner's approval"]
fn interact_screenshots() {
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
        interact(&file, base, &format!("interact-engine-{b}"), 1, &dir);
        interact(&file, base, &format!("interact-slot-{b}"), 2, &dir);
        interact(&file, base, &format!("interact-failed-{b}"), 3, &dir);
        forget(&file, base, &format!("forget-{b}"), &dir);
    }
}
