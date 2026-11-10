//! The info hint popover and the tooltip (ADR-0094): keyboard and pointer behaviour of
//! the real library components in a probe window, with a mocked clock (no sleeping).

#![allow(clippy::expect_used)] // Tests fail by panicking.

use slint::ComponentHandle;
use slint::platform::{Key, WindowEvent};

use rbs_app_ui::{OverlayProbe as Probe, Overlays};

fn key(p: &Probe, k: Key) {
    let s: slint::SharedString = k.into();
    p.window()
        .dispatch_event(WindowEvent::KeyPressed { text: s.clone() });
    p.window()
        .dispatch_event(WindowEvent::KeyReleased { text: s });
}

fn point(p: &Probe, x: f32, y: f32) {
    p.window().dispatch_event(WindowEvent::PointerMoved {
        position: slint::LogicalPosition::new(x, y),
    });
}

/// Runs what Slint defers to the next frame: `changed` handlers and timers.
fn settle() {
    slint::platform::update_timers_and_animations();
}

fn setup() -> Probe {
    i_slint_backend_testing::init_no_event_loop();
    let p = Probe::new().expect("the probe window");
    p.show().expect("show");
    p
}

#[test]
fn the_hint_opens_with_return_and_escape_returns_the_focus() {
    let p = setup();
    let o = p.global::<Overlays>();
    p.invoke_focus_hint();
    key(&p, Key::Return);
    assert!(o.get_hint_open());
    assert_eq!(
        o.get_hint_text().as_str(),
        "Sound below this level is muted."
    );
    key(&p, Key::Escape);
    assert!(!o.get_hint_open());
    assert!(p.get_hint_focused(), "the focus is back on the info button");
}

#[test]
fn opening_a_hint_shows_no_tooltip_and_a_click_outside_closes_it() {
    let p = setup();
    let o = p.global::<Overlays>();
    p.invoke_focus_hint();
    key(&p, Key::Return);
    assert!(o.get_hint_open() && !o.get_tip_shown());
    // A click far from the popover closes it.
    point(&p, 380.0, 280.0);
    p.window().dispatch_event(WindowEvent::PointerPressed {
        position: slint::LogicalPosition::new(380.0, 280.0),
        button: slint::platform::PointerEventButton::Left,
    });
    p.window().dispatch_event(WindowEvent::PointerReleased {
        position: slint::LogicalPosition::new(380.0, 280.0),
        button: slint::platform::PointerEventButton::Left,
    });
    assert!(!o.get_hint_open());
}

#[test]
fn a_tooltip_shows_at_once_for_the_keyboard_and_after_the_delay_for_the_pointer() {
    let p = setup();
    let o = p.global::<Overlays>();
    // Keyboard focus: at once, with the button's name; Escape dismisses it.
    p.invoke_focus_button();
    settle();
    assert!(o.get_tip_shown(), "keyboard focus shows it at once");
    assert_eq!(o.get_tip_text().as_str(), "Flip horizontally");
    key(&p, Key::Escape);
    settle();
    assert!(!o.get_tip_shown());

    // Pointer: waits for the delay, then shows; leaving hides it. The keyboard focus
    // moves away first, since a focused button counts as pointed at.
    p.invoke_focus_hint();
    settle();
    point(&p, 300.0, 250.0);
    point(&p, 110.0, 20.0);
    settle();
    assert!(!o.get_tip_shown() && o.get_tip_waiting());
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_secs(1));
    assert!(o.get_tip_shown());
    point(&p, 300.0, 250.0);
    settle();
    assert!(!o.get_tip_shown());
}
