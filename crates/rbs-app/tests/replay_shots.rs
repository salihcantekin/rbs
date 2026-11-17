//! Screenshots of the replay buffer in Settings and in the Live controls (ADR-0088), in
//! Dark and Light, for the owner's approval of the visual change (CLAUDE.md rule 16).
//! It writes raw RGBA files to `RBS_SHOTS_DIR` with Skia's real pixels; it is not a
//! check, so it is ignored and run by hand:
//!
//! `RBS_SHOTS_DIR=/tmp/shots cargo test -p rbs-app --test replay_shots -- --ignored`

#![allow(clippy::expect_used, clippy::unwrap_used)] // A tool run by hand.

use rbs_app_ui::tokens_apply::{apply_tokens, slot};
use rbs_app_ui::{ReplayBuffer, ReplayProbe, SettingsWindow, Theme, Tokens};
use rbs_ui::tokens::{Base, ThemeBody, Tokens as TokenFile};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

/// The states the shots show.
#[derive(Clone, Copy)]
enum State {
    Off,
    Armed,
    Saved,
}

fn fill(g: &ReplayBuffer<'_>, state: State) {
    g.set_can_arm(true);
    g.set_audio(true);
    g.set_length(0.42);
    g.set_length_text("2 min".into());
    g.set_length_default_text("2 min".into());
    g.set_memories(ModelRc::new(VecModel::from(
        ["256 MiB", "512 MiB", "1 GiB", "2 GiB"]
            .iter()
            .map(|s| SharedString::from(*s))
            .collect::<Vec<_>>(),
    )));
    g.set_memory(2);
    g.set_memory_default(2);
    g.set_folder("/Users/me/Movies/Replays".into());
    g.set_estimate(
        "About 113 MiB for 2 min at your recording's bit rate; the limit caps it.".into(),
    );
    match state {
        State::Off => {
            g.set_status("Not armed. Nothing is kept.".into());
            g.set_tone(0);
        }
        State::Armed | State::Saved => {
            g.set_armed(true);
            g.set_can_save(true);
            g.set_status("Holds the last 1:32".into());
            g.set_tone(1);
        }
    }
    if matches!(state, State::Saved) {
        g.set_saved_name("RBS Replay 2026-11-17 12-40-05.mp4".into());
    }
}

/// Applies a built-in theme to a window's `Theme` and `Tokens` globals.
macro_rules! tokens {
    ($w:expr, $file:expr, $base:expr) => {{
        let theme = $file
            .resolve(&ThemeBody::new($base))
            .expect("the theme resolves");
        let s = slot(&theme, "").expect("a slot");
        apply_tokens(
            &$w.global::<Theme>(),
            &$w.global::<Tokens>(),
            $file,
            [s.clone(), s],
        )
        .expect("the tokens apply");
    }};
}

fn write(px: &slint::SharedPixelBuffer<slint::Rgba8Pixel>, dir: &std::path::Path, name: &str) {
    let path = dir.join(format!("{name}-{}x{}.rgba", px.width(), px.height()));
    std::fs::write(&path, px.as_bytes()).expect("written");
}

#[test]
#[ignore = "a tool: writes screenshots for the owner's approval"]
fn replay_screenshots() {
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
        for (state, s) in [
            (State::Off, "off"),
            (State::Armed, "armed"),
            (State::Saved, "saved"),
        ] {
            let w = ReplayProbe::new().expect("the probe");
            tokens!(w, &file, base);
            fill(&w.global::<ReplayBuffer>(), state);
            w.show().expect("show");
            write(
                &w.window().take_snapshot().expect("a snapshot"),
                &dir,
                &format!("live-{s}-{b}"),
            );
            w.hide().expect("hide");

            let w = SettingsWindow::new().expect("the settings window");
            tokens!(w, &file, base);
            fill(&w.global::<ReplayBuffer>(), state);
            w.set_section(1);
            w.window().set_size(slint::LogicalSize::new(960.0, 5200.0));
            w.show().expect("show");
            write(
                &w.window().take_snapshot().expect("a snapshot"),
                &dir,
                &format!("settings-{s}-{b}"),
            );
            w.hide().expect("hide");
        }
    }
}
