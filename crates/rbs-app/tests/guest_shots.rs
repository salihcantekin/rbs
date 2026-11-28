//! Screenshots of the guest window (ADR-0139 phase 1d), Dark and Light, for the owner's
//! approval of the visual change (CLAUDE.md rule 16). It writes raw RGBA files to
//! `RBS_SHOTS_DIR` with Skia's real pixels; it is not a check, so it is ignored and run
//! by hand:
//!
//! `RBS_SHOTS_DIR=/tmp/shots cargo test -p rbs-app --test guest_shots -- --ignored`

#![allow(clippy::expect_used, clippy::unwrap_used)] // A tool run by hand.

use rbs_app::guest_view::{Flow, Session, State, Status};
use rbs_app_ui::tokens_apply::{apply_tokens, slot};
use rbs_app_ui::{GuestWindow, Theme, Tokens};
use rbs_ui::tokens::{Base, ThemeBody, Tokens as TokenFile};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

const CODE: &str = "rbs-guest:1:eJyVVNtu2zAMfZ98BUH0JUBQSL5E8h5aZEA3rFvRDtsDJFmODQiSDcmo87f9j3zRSGRb0CIdZlJH5CHhy8sXV3lmGm4BpR9dKwXrC8bZ0o1pQqy3AqxD";

fn strings(v: &[String]) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(
        v.iter()
            .map(|s| SharedString::from(s.as_str()))
            .collect::<Vec<_>>(),
    ))
}

/// Pushes what `guest_view` computes for a flow and a status into the window.
fn fill(w: &GuestWindow, flow: Flow, state: State, problem: &str) {
    let words: Vec<String> = ["amber", "velvet", "harbor", "quartz", "meadow", "lantern"]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    let status = Status {
        state,
        phrase: words.clone(),
        reason: (state == State::Failed).then(|| "guest_unreachable".to_string()),
        expires_in_s: Some(462),
    };
    let v = rbs_app::guest_view::view(flow, &status);
    let _ = Session::new(flow);
    w.set_sharing(flow == Flow::Share);
    w.set_page(v.page.index());
    w.set_heading(v.title.as_str().into());
    w.set_hint(v.hint.as_str().into());
    let marks: Vec<i32> = v.steps.iter().map(|m| m.index()).collect();
    w.set_steps(ModelRc::new(VecModel::from(marks)));
    w.set_badge(v.badge.as_str().into());
    w.set_tone(v.tone);
    w.set_expires(v.expires.as_str().into());
    w.set_failure(v.failure.as_str().into());
    w.set_code(CODE.into());
    w.set_code_ready(v.code_ready);
    w.set_paste_enabled(v.paste_enabled);
    w.set_words_top(strings(&words[..3]));
    w.set_words_bottom(strings(&words[3..]));
    w.set_problem(problem.into());
}

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

#[test]
#[ignore = "a tool: writes screenshots for the owner's approval"]
fn guest_screenshots() {
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
    let shots: [(&str, Flow, State, &str); 9] = [
        ("share-invite", Flow::Share, State::WaitingForAnswer, ""),
        (
            "share-bad-answer",
            Flow::Share,
            State::WaitingForAnswer,
            "That code belongs to another invite.",
        ),
        ("share-compare", Flow::Share, State::CheckPhrase, ""),
        ("share-live", Flow::Share, State::Live, ""),
        ("share-failed", Flow::Share, State::Failed, ""),
        ("host-invite", Flow::Host, State::Gathering, ""),
        ("host-answer", Flow::Host, State::Connecting, ""),
        ("host-compare", Flow::Host, State::CheckPhrase, ""),
        ("host-live", Flow::Host, State::Live, ""),
    ];
    for (base, b) in [(Base::Dark, "dark"), (Base::Light, "light")] {
        for (name, flow, state, problem) in shots {
            let w = GuestWindow::new().expect("the window");
            tokens!(w, &file, base);
            fill(&w, flow, state, problem);
            w.show().expect("show");
            let px = w.window().take_snapshot().expect("a snapshot");
            let path = dir.join(format!("{name}-{b}-{}x{}.rgba", px.width(), px.height()));
            std::fs::write(path, px.as_bytes()).expect("written");
            w.hide().expect("hide");
        }
    }
}
