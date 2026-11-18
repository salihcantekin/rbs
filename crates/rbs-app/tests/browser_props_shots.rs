//! Screenshots of a browser source's Page tab in Properties and in the Source settings
//! window (ADR-0109, ADR-0132, ADR-0149), in Dark and Light, for the owner's approval of the visual change
//! (CLAUDE.md rule 16). It writes raw RGBA files to `RBS_SHOTS_DIR` with Skia's real
//! pixels; it is not a check, so it is ignored and run by hand:
//!
//! `RBS_SHOTS_DIR=/tmp/shots cargo test -p rbs-app --test browser_props_shots -- --ignored`

#![allow(clippy::expect_used, clippy::unwrap_used)] // A tool run by hand.

use rbs_app_ui::tokens_apply::{apply_tokens, slot};
use rbs_app_ui::{
    BrowserData, PropertiesProbe, SourceData, SourceSettingsData, SourceSettingsWindow, Studio,
    Theme, Tokens,
};
use rbs_ui::tokens::{Base, ThemeBody, Tokens as TokenFile};
use slint::{ComponentHandle, ModelRc, VecModel};

fn page(state: i32) -> BrowserData {
    BrowserData {
        on: true,
        id: 7,
        url: "https://streamelements.com/overlay/5f2c/chat".into(),
        width: 1280,
        height: 720,
        canvas_w: 1920,
        canvas_h: 1080,
        fps: 30,
        zoom: 125,
        transparent: true,
        css: "body { background-color: rgba(0, 0, 0, 0); overflow: hidden; }".into(),
        reload_every: 600,
        script: "document.querySelector('.cookie-banner')?.remove();".into(),
        script_on_load: true,
        script_every: 0,
        keep_running: true,
        retry_on_error: true,
        keep_signed_in: state == 0,
        running: state == 0,
        state,
        status: "The page did not answer (connection timed out). Trying again in 8 s.".into(),
        ..Default::default()
    }
}

fn shot(file: &TokenFile, base: Base, name: &str, data: BrowserData, dir: &std::path::Path) {
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
        icon: "globe".into(),
        ok: data.state == 0,
        picture: true,
        audio: true,
        visible: true,
        kind_code: -1,
        places: 1,
        ..Default::default()
    }])));
    g.set_properties_source(7);
    g.set_properties_tab(5);
    g.set_browser_props(data);
    w.window().set_size(slint::LogicalSize::new(380.0, 1500.0));
    w.show().expect("show");
    let px = w.window().take_snapshot().expect("a snapshot");
    let path = dir.join(format!("{name}-{}x{}.rgba", px.width(), px.height()));
    std::fs::write(&path, px.as_bytes()).expect("written");
    w.hide().expect("hide");
}

fn window_shot(file: &TokenFile, base: Base, name: &str, data: BrowserData, dir: &std::path::Path) {
    let theme = file
        .resolve(&ThemeBody::new(base))
        .expect("the theme resolves");
    let s = slot(&theme, "").expect("a slot");
    let w = SourceSettingsWindow::new().expect("the window");
    apply_tokens(
        &w.global::<Theme>(),
        &w.global::<Tokens>(),
        file,
        [s.clone(), s],
    )
    .expect("the tokens apply");
    let g = w.global::<Studio>();
    g.set_source_settings(ModelRc::new(VecModel::from(vec![SourceSettingsData {
        id: 7,
        name: "Chat".into(),
        kind_code: -1,
        mode_index: -1,
        rate_index: -1,
        ..Default::default()
    }])));
    g.set_browser_props(data);
    w.window().set_size(slint::LogicalSize::new(440.0, 1460.0));
    w.show().expect("show");
    let px = w.window().take_snapshot().expect("a snapshot");
    let path = dir.join(format!("{name}-{}x{}.rgba", px.width(), px.height()));
    std::fs::write(&path, px.as_bytes()).expect("written");
    w.hide().expect("hide");
}

#[test]
#[ignore = "a tool: writes screenshots for the owner's approval"]
fn page_tab_screenshots() {
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
        shot(&file, base, &format!("page-{b}"), page(0), &dir);
        shot(&file, base, &format!("page-error-{b}"), page(3), &dir);
        let mut local = page(1);
        local.file = true;
        local.url = "file:///Users/me/overlays/scoreboard.html".into();
        local.file_name = "scoreboard.html".into();
        local.reload_on_file_change = true;
        local.width = 1920;
        local.height = 1080;
        shot(&file, base, &format!("page-file-{b}"), local, &dir);
        window_shot(&file, base, &format!("window-{b}"), page(0), &dir);
    }
}
