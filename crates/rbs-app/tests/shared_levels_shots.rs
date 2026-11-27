//! Screenshots for the owner's approval of the visual change (CLAUDE.md rule 16):
//! turning a scene's Own levels off asks in a small dialog (ADR-0091), then an undo
//! toast shows, in Dark and Light. It writes raw RGBA files to `RBS_SHOTS_DIR` with
//! Skia's real pixels; it is not a check, so it is ignored and run by hand:
//!
//! `RBS_SHOTS_DIR=/tmp/shots cargo test -p rbs-app --test shared_levels_shots -- --ignored`

#![allow(clippy::expect_used, clippy::unwrap_used)] // A tool run by hand.

use rbs_app_ui::tokens_apply::{apply_tokens, slot};
use rbs_app_ui::{MainWindow, Overlays, Studio, Theme, Tokens};
use rbs_ui::tokens::{Base, ThemeBody, Tokens as TokenFile};
use slint::ComponentHandle;

fn shot(file: &TokenFile, base: Base, dialog: bool, name: &str, dir: &std::path::Path) {
    let theme = file.resolve(&ThemeBody::new(base)).unwrap();
    let s = slot(&theme, "").unwrap();
    let w = MainWindow::new().unwrap();
    apply_tokens(
        &w.global::<Theme>(),
        &w.global::<Tokens>(),
        file,
        [s.clone(), s],
    )
    .unwrap();
    if dialog {
        // The words `shared_levels.rs` sets.
        let o = w.global::<Overlays>();
        o.set_name_title("Stop using own levels in Interview?".into());
        o.set_name_detail(
            "The volumes and mutes set only for this scene are removed. Its sources play \
             at their shared levels again. You can undo this."
                .into(),
        );
        o.set_name_confirm("Use shared levels".into());
        o.set_name_cancel("Keep own levels".into());
        o.set_name_ask(false);
        o.set_name_open(true);
    } else {
        let st = w.global::<Studio>();
        st.set_levels_undo("Interview".into());
        st.set_levels_undo_generation(1);
    }
    w.window().set_size(slint::LogicalSize::new(1280.0, 720.0));
    w.show().unwrap();
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::ZERO);
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(1000));
    let px = w.window().take_snapshot().unwrap();
    let path = dir.join(format!("{name}-{}x{}.rgba", px.width(), px.height()));
    std::fs::write(&path, px.as_bytes()).unwrap();
    w.hide().unwrap();
}

#[test]
#[ignore = "a tool: writes screenshots for the owner's approval"]
fn shared_levels_screenshots() {
    let dir = std::path::PathBuf::from(std::env::var_os("RBS_SHOTS_DIR").expect("RBS_SHOTS_DIR"));
    std::fs::create_dir_all(&dir).unwrap();
    let backend = i_slint_backend_testing::TestingBackend::new(
        i_slint_backend_testing::TestingBackendOptions {
            renderer_name: Some("skia".into()),
            mock_time: true,
            ..Default::default()
        },
    );
    slint::platform::set_platform(Box::new(backend)).expect("the testing platform");
    let file = TokenFile::built_in().unwrap();
    for (base, b) in [(Base::Dark, "dark"), (Base::Light, "light")] {
        shot(&file, base, true, &format!("dialog-{b}"), &dir);
        shot(&file, base, false, &format!("toast-{b}"), &dir);
    }
}
