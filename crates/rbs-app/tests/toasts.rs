//! The notice cards' timing (ui-design-system spec, Toast): the undo toast after a
//! delete closes after `delay.toast-undo`, the other notices after `delay.toast`.
//! Mock time, so nothing waits. The error bar's copy is shown by `toast_shots`.

#![allow(clippy::expect_used, clippy::unwrap_used)] // Test code.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use rbs_app_ui::tokens_apply::{apply_tokens, slot};
use rbs_app_ui::{MainWindow, Studio, Theme, Tokens};
use rbs_ui::tokens::{Base, ThemeBody, Tokens as TokenFile};
use slint::ComponentHandle;

fn window() -> MainWindow {
    let file = TokenFile::built_in().unwrap();
    let theme = file.resolve(&ThemeBody::new(Base::Dark)).unwrap();
    let s = slot(&theme, "").unwrap();
    let w = MainWindow::new().unwrap();
    apply_tokens(
        &w.global::<Theme>(),
        &w.global::<Tokens>(),
        &file,
        [s.clone(), s],
    )
    .unwrap();
    w.window().set_size(slint::LogicalSize::new(1280.0, 720.0));
    w.show().unwrap();
    w
}

fn ms(w: &MainWindow, name: impl Fn(&Tokens) -> i64) -> u64 {
    u64::try_from(name(&w.global::<Tokens>())).unwrap()
}

// One test: the testing platform is set once per process.
#[test]
fn delete_undo_toast_closes_after_its_own_shorter_time() {
    i_slint_backend_testing::init_no_event_loop();
    let w = window();
    let undo = ms(&w, |t| t.get_delay_toast_undo());
    assert_eq!(undo, 3000);
    assert!(undo < ms(&w, |t| t.get_delay_toast()));
    let closed = Rc::new(Cell::new(false));
    let c = closed.clone();
    w.global::<Studio>().on_dismiss_deleted(move || c.set(true));
    w.global::<Studio>().set_deleted("Camera".into());
    w.global::<Studio>().set_deleted_generation(1);
    // Builds the card, which starts its countdown; then step one tick at a time,
    // because each step starts the next one.
    i_slint_backend_testing::mock_elapsed_time(Duration::ZERO);
    let tick = ms(&w, |t| t.get_delay_toast_tick());
    for _ in 1..undo / tick {
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(tick));
    }
    assert!(
        !closed.get(),
        "still showing one tick before the time is up"
    );
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(tick));
    assert!(closed.get(), "closed once the time is up");
}
