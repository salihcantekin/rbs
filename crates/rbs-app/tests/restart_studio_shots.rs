//! Screenshots of Restart the studio in the Program panel (crash-resilience spec,
//! "Restart the studio"): a stopped studio with the button, and an old studio that did
//! not stop, in Dark and Light, for the owner's approval (CLAUDE.md rule 16). Writes raw
//! RGBA files to `RBS_SHOTS_DIR` with Skia's real pixels; it is not a check, so it is
//! ignored and run by hand:
//!
//! `RBS_SHOTS_DIR=/tmp/shots cargo test -p rbs-app --test restart_studio_shots -- --ignored`

#![allow(clippy::expect_used, clippy::unwrap_used)] // A tool run by hand.

use rbs_app_ui::tokens_apply::{apply_tokens, slot};
use rbs_app_ui::{MainWindow, StackData, Studio, TabData, Theme, Tokens};
use rbs_ui::tokens::{Base, ThemeBody, Tokens as TokenFile};
use slint::ComponentHandle;

fn shot(file: &TokenFile, base: Base, stuck: bool, name: &str, dir: &std::path::Path) {
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
    let tab = TabData {
        id: "rbs.program".into(),
        title: "Program".into(),
        known: true,
        icon: "".into(),
    };
    m.set_stacks(slint::ModelRc::new(slint::VecModel::from(vec![
        StackData {
            window: 0,
            stack: 0,
            x: 16.0,
            y: 48.0,
            w: 640.0,
            h: 300.0,
            tabs: slint::ModelRc::new(slint::VecModel::from(vec![tab])),
            active: 0,
            collapsed: false,
            can_collapse: true,
            can_hide: true,
            takes_drop: true,
        },
    ])));
    let g = m.global::<Studio>();
    g.set_picture_panel("rbs.program".into());
    g.set_state(2);
    g.set_preview_state(5);
    if stuck {
        g.set_failure("the studio did not stop. Quit RBS and open it again.".into());
        g.set_restart_busy(true);
    } else {
        g.set_failure("a studio node failed: the encoder stopped answering".into());
    }
    m.window()
        .dispatch_event(slint::platform::WindowEvent::ScaleFactorChanged { scale_factor: 2.0 });
    m.window().set_size(slint::PhysicalSize::new(1344, 840));
    m.show().expect("show");
    let px = m.window().take_snapshot().expect("a snapshot");
    let path = dir.join(format!("{name}-{}x{}.rgba", px.width(), px.height()));
    std::fs::write(&path, px.as_bytes()).expect("written");
    m.hide().expect("hide");
}

#[test]
#[ignore = "a tool: writes screenshots for the owner's approval"]
fn restart_studio_screenshots() {
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
        shot(&file, base, false, &format!("stopped-{b}"), &dir);
        shot(&file, base, true, &format!("stuck-{b}"), &dir);
    }
}
