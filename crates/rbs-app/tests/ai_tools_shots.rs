//! Screenshots of the Settings window's AI tools section (ADR-0134), in Dark and Light,
//! for the owner's approval of the visual change (CLAUDE.md rule 16). It writes raw RGBA
//! files to `RBS_SHOTS_DIR` with Skia's real pixels; it is not a check, so it is
//! ignored and run by hand:
//!
//! `RBS_SHOTS_DIR=/tmp/shots cargo test -p rbs-app --test ai_tools_shots -- --ignored`

#![allow(clippy::expect_used, clippy::unwrap_used)] // A tool run by hand.

use rbs_app_ui::tokens_apply::{apply_tokens, slot};
use rbs_app_ui::{AiActionRow, SettingsWindow, Studio, Theme, Tokens};
use rbs_ui::tokens::{Base, ThemeBody, Tokens as TokenFile};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

fn strings(v: &[&str]) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(
        v.iter().map(|s| SharedString::from(*s)).collect::<Vec<_>>(),
    ))
}

fn action(time: &str, method: &str, why: &str) -> AiActionRow {
    AiActionRow {
        time: time.into(),
        method: method.into(),
        allowed: why.is_empty(),
        why: why.into(),
    }
}

fn shot(
    file: &TokenFile,
    base: Base,
    name: &str,
    started: bool,
    missing: bool,
    dir: &std::path::Path,
) {
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
    g.set_ai_ready(true);
    g.set_ai_tool_names(strings(&[
        "Claude Desktop",
        "Claude Code",
        "Cursor",
        "VS Code",
        "Other",
    ]));
    g.set_ai_tool(0);
    // The line as the app makes it (`rbs_app::ai_tools::line`).
    let line = rbs_app::ai_tools::line(
        rbs_app::ai_tools::Tool::ClaudeDesktop,
        std::path::Path::new("/Applications/RBS.app/Contents/MacOS/rbs-mcp"),
        std::path::Path::new("/Users/me/Library/Application Support/RBS/agent-token"),
        rbs_app::ai_tools::Shell::Sh,
    );
    if missing {
        g.set_ai_line_note(
            "rbs-mcp is not built yet (/Users/me/rbs/target/debug/rbs-mcp). In the RBS source folder run: cargo build -p rbs-mcp"
                .into(),
        );
    } else {
        g.set_ai_where(line.place.into());
        g.set_ai_line_help(line.help.into());
        g.set_ai_line(line.text.into());
    }
    g.set_ai_config_name("claude_desktop_config.json".into());
    g.set_ai_config_folder("/Users/me/Library/Application Support/Claude".into());
    g.set_ai_config_exists(started);
    g.set_ai_access(1);
    if started {
        g.set_ai_state(1);
        g.set_ai_status("Started. 2 AI tools connected.".into());
        g.set_ai_clients(strings(&["Claude Desktop", "cursor-vscode"]));
        g.set_ai_autostart(true);
        let why = rbs_app::ai_tools::refusal_text;
        g.set_ai_actions(ModelRc::new(VecModel::from(vec![
            action("21:43:30", "scene.switch", why("on_air")),
            action("21:42:10", "record.start", why("irreversible")),
            action("21:40:02", "source.add", why("network")),
        ])));
    } else {
        g.set_ai_state(0);
        g.set_ai_status(
            "Stopped. AI tools cannot use RBS. Stopping is not a lock against other programs of your user account."
                .into(),
        );
    }
    w.set_section(10);
    w.window().set_size(slint::LogicalSize::new(960.0, 1480.0));
    w.show().expect("show");
    let px = w.window().take_snapshot().expect("a snapshot");
    let path = dir.join(format!("{name}-{}x{}.rgba", px.width(), px.height()));
    std::fs::write(&path, px.as_bytes()).expect("written");
    w.hide().expect("hide");
}

#[test]
#[ignore = "a tool: writes screenshots for the owner's approval"]
fn ai_tools_section_screenshots() {
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
            &format!("ai-tools-stopped-{b}"),
            false,
            false,
            &dir,
        );
        shot(
            &file,
            base,
            &format!("ai-tools-started-{b}"),
            true,
            false,
            &dir,
        );
        shot(
            &file,
            base,
            &format!("ai-tools-not-built-{b}"),
            false,
            true,
            &dir,
        );
    }
}
