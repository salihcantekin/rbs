//! Screenshots of Settings › Stream's Destinations card, in Dark and Light, for the
//! owner's approval of the visual change (CLAUDE.md rule 16). It writes raw RGBA files
//! to `RBS_SHOTS_DIR` with Skia's real pixels; it is not a check, so it is ignored and
//! run by hand:
//!
//! `RBS_SHOTS_DIR=/tmp/shots cargo test -p rbs-app --test destinations_shots -- --ignored`

#![allow(clippy::expect_used, clippy::unwrap_used)] // A tool run by hand.

use rbs_app_ui::tokens_apply::{apply_tokens, slot};
use rbs_app_ui::{DestinationRow, SettingsWindow, Studio, Theme, Tokens};
use rbs_ui::tokens::{Base, ThemeBody, Tokens as TokenFile};
use slint::ComponentHandle;

fn row(id: i32, name: &str, note: &str, on: bool, warn: bool) -> DestinationRow {
    DestinationRow {
        id,
        name: name.into(),
        note: note.into(),
        on,
        warn,
        removable: id != 0,
    }
}

fn shot(file: &TokenFile, base: Base, name: &str, dir: &std::path::Path) {
    let theme = file
        .resolve(&ThemeBody::new(base))
        .expect("the theme resolves");
    let s = slot(&theme, "").expect("a slot");
    let w = SettingsWindow::new().expect("the settings window");
    apply_tokens(
        &w.global::<Theme>(),
        &w.global::<Tokens>(),
        file,
        [s.clone(), s],
    )
    .expect("the tokens apply");
    let g = w.global::<Studio>();
    let rows = vec![
        row(
            0,
            "Stream",
            "RTMP · Key saved · Same as the stream",
            false,
            false,
        ),
        row(
            2,
            "Live Push",
            "RTMP · Key saved · Same as the stream",
            true,
            false,
        ),
        row(
            3,
            "YouTube",
            "YouTube · RTMP · No key yet · Same as the stream",
            true,
            true,
        ),
    ];
    g.set_st_dests(std::rc::Rc::new(slint::VecModel::from(rows)).into());
    g.set_st_dest_together("Go live sends to all 2 at the same time.".into());
    g.set_st_dest_count("3 of 6".into());
    g.set_st_dest_can_add(true);
    g.set_st_dest_editable(true);
    g.set_st_form_idle(true);
    g.set_st_dest_upload(
        "Upload needed: about 37 Mbit/s for 2 destinations (one encode shared by all).".into(),
    );
    // Settings › Stream (`main.rs` SECTION_STREAM).
    w.set_section(2);
    w.window().set_size(slint::LogicalSize::new(960.0, 520.0));
    w.show().expect("show");
    let px = w.window().take_snapshot().expect("a snapshot");
    let path = dir.join(format!("{name}-{}x{}.rgba", px.width(), px.height()));
    std::fs::write(&path, px.as_bytes()).expect("written");
    w.hide().expect("hide");
}

#[test]
#[ignore = "a tool: writes screenshots for the owner's approval"]
fn destinations_screenshots() {
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
        shot(&file, base, &format!("destinations-{b}"), &dir);
    }
}
