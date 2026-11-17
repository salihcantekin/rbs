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

#[test]
fn a_feature_section_keeps_its_settings_when_off_and_folds_by_id() {
    use rbs_app_ui::Sections;
    use std::cell::RefCell;
    use std::collections::HashSet;
    use std::rc::Rc;

    let p = setup();
    let shut: Rc<RefCell<HashSet<String>>> = Rc::default();
    let s = p.global::<Sections>();
    let read = shut.clone();
    s.on_closed(move |id, _| read.borrow().contains(id.as_str()));
    let write = shut.clone();
    s.on_set_closed(move |id, closed| {
        let mut set = write.borrow_mut();
        if closed {
            set.insert(id.to_string());
        } else {
            set.remove(id.as_str());
        }
    });
    settle();
    let open_height = p.get_feature_height();
    assert!(p.get_feature_on());
    // Switching it off leaves the settings in place.
    p.invoke_flip_feature();
    settle();
    assert!(!p.get_feature_on());
    assert_eq!(p.get_feature_height(), open_height);
    // Folding by id takes their room.
    s.invoke_set_closed("probe.feature".into(), true);
    s.set_rev(s.get_rev() + 1);
    settle();
    assert!(
        p.get_feature_height() < open_height,
        "{:?} vs {:?}",
        p.get_feature_height(),
        open_height
    );
}

#[test]
fn the_name_prompt_accepts_with_return_and_cancels_with_escape() {
    use std::cell::RefCell;
    use std::rc::Rc;

    let p = setup();
    let o = p.global::<Overlays>();
    let log: Rc<RefCell<Vec<String>>> = Rc::default();
    let accepted = log.clone();
    o.on_name_accepted(move |n| accepted.borrow_mut().push(format!("ok {n}")));
    let cancelled = log.clone();
    o.on_name_cancelled(move || cancelled.borrow_mut().push("cancel".to_owned()));
    o.set_name_value("Show".into());
    o.set_name_open(true);
    settle();
    // The field starts with the value and has the focus: Return accepts it.
    key(&p, Key::Return);
    assert_eq!(log.borrow().as_slice(), ["ok Show"]);
    key(&p, Key::Escape);
    assert_eq!(log.borrow().as_slice(), ["ok Show", "cancel"]);
}

fn picker_row(name: &str, index: i32) -> rbs_app_ui::PickerRow {
    rbs_app_ui::PickerRow {
        name: name.into(),
        note: "".into(),
        index,
    }
}

#[test]
fn the_picker_moves_with_the_arrows_and_chooses_by_the_rows_listing_index() {
    use std::cell::RefCell;
    use std::rc::Rc;

    let p = setup();
    let o = p.global::<Overlays>();
    let log: Rc<RefCell<Vec<String>>> = Rc::default();
    let chosen = log.clone();
    o.on_picker_chosen(move |i| chosen.borrow_mut().push(format!("chose {i}")));
    let filtered = log.clone();
    o.on_picker_filtered(move |t| filtered.borrow_mut().push(format!("filter {t}")));
    // A filter hid rows 0 and 1: the visible rows keep their listing indexes 2 and 5.
    o.set_picker_rows(slint::ModelRc::new(slint::VecModel::from(vec![
        picker_row("Capture card", 2),
        picker_row("Virtual cam", 5),
    ])));
    o.set_picker_open(true);
    settle();
    // The filter field has the focus: typing reaches the app, Down moves, Return chooses.
    p.window()
        .dispatch_event(WindowEvent::KeyPressed { text: "c".into() });
    key(&p, Key::DownArrow);
    key(&p, Key::Return);
    assert_eq!(log.borrow().last().map(String::as_str), Some("chose 5"));
    key(&p, Key::UpArrow);
    key(&p, Key::Return);
    assert_eq!(log.borrow().last().map(String::as_str), Some("chose 2"));
}

#[test]
fn escape_closes_the_picker_above_the_name_prompt_and_nothing_else() {
    use std::cell::RefCell;
    use std::rc::Rc;

    let p = setup();
    let o = p.global::<Overlays>();
    let log: Rc<RefCell<Vec<&'static str>>> = Rc::default();
    let name = log.clone();
    o.on_name_cancelled(move || name.borrow_mut().push("name"));
    let picker = log.clone();
    o.on_picker_cancelled(move || picker.borrow_mut().push("picker"));
    o.set_name_open(true);
    settle();
    o.set_picker_open(true);
    settle();
    key(&p, Key::Escape);
    assert_eq!(
        log.borrow().as_slice(),
        ["picker"],
        "only the topmost overlay"
    );
}

#[test]
fn the_colour_popover_is_a_layer_and_a_pick_reaches_only_the_swatch_that_opened_it() {
    let p = setup();
    let o = p.global::<Overlays>();
    // The swatch is at (200, 10), 28 px or so wide: a click opens the popover.
    p.window().dispatch_event(WindowEvent::PointerMoved {
        position: slint::LogicalPosition::new(210.0, 20.0),
    });
    p.window().dispatch_event(WindowEvent::PointerPressed {
        position: slint::LogicalPosition::new(210.0, 20.0),
        button: slint::platform::PointerEventButton::Left,
    });
    p.window().dispatch_event(WindowEvent::PointerReleased {
        position: slint::LogicalPosition::new(210.0, 20.0),
        button: slint::platform::PointerEventButton::Left,
    });
    settle();
    assert!(o.get_color_open(), "the swatch opens the popover");
    assert_eq!(o.get_color_label().as_str(), "Border colour");
    // A pick (as the popover publishes it) comes back through the swatch.
    o.set_color_result("#445566".into());
    o.set_color_serial(o.get_color_serial() + 1);
    settle();
    assert_eq!(p.get_last_pick().as_str(), "#445566");
    // Another owner's pick is not ours.
    o.set_color_owner(o.get_color_owner() + 100);
    o.set_color_result("#778899".into());
    o.set_color_serial(o.get_color_serial() + 1);
    settle();
    assert_eq!(p.get_last_pick().as_str(), "#445566");
    o.set_color_owner(o.get_color_owner() - 100);
    key(&p, Key::Escape);
    settle();
    assert!(!o.get_color_open());
    assert!(p.get_colour_focused(), "the focus is back on the swatch");
}

fn click(p: &Probe, x: f32, y: f32) {
    point(p, x, y);
    for pressed in [true, false] {
        let position = slint::LogicalPosition::new(x, y);
        let button = slint::platform::PointerEventButton::Left;
        p.window().dispatch_event(if pressed {
            WindowEvent::PointerPressed { position, button }
        } else {
            WindowEvent::PointerReleased { position, button }
        });
    }
}

#[test]
fn one_click_on_a_tab_selects_it_while_the_focus_is_elsewhere() {
    let p = setup();
    // The focus is on another control, as after a click in another panel.
    p.invoke_focus_button();
    settle();
    // The Tabs bar spans x 10..310 at y 240; three tabs of 100 each.
    click(&p, 160.0, 250.0);
    assert_eq!(p.get_tab_index(), 1, "the first click chooses Filters");
    click(&p, 260.0, 250.0);
    assert_eq!(p.get_tab_index(), 2);
}

/// Another window taking the focus closes the tooltip and keeps it closed until the
/// window is active again; coming back shows no tooltip for the focus alone.
#[test]
fn losing_the_window_activation_closes_the_tooltip() {
    let p = setup();
    rbs_app::window_events::install(&p);
    let o = p.global::<Overlays>();
    let activate = |on: bool| {
        p.window()
            .dispatch_event(WindowEvent::WindowActiveChanged(on));
        settle();
    };
    activate(true);

    // Keyboard focus shows it; the window losing the activation closes it.
    p.invoke_focus_button();
    settle();
    assert!(o.get_tip_shown());
    activate(false);
    assert!(!o.get_tip_shown() && !o.get_tip_waiting());
    // Back with the window: the focus returns, the tooltip does not.
    activate(true);
    assert!(!o.get_tip_shown() && !o.get_tip_waiting());

    // The pointer resting on the button while the window is inactive starts nothing.
    p.invoke_focus_hint();
    settle();
    point(&p, 390.0, 290.0);
    settle();
    activate(false);
    point(&p, 110.0, 20.0);
    settle();
    assert!(!o.get_tip_waiting());
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_secs(1));
    assert!(!o.get_tip_shown());

    // A tooltip waiting for the delay is dropped when the window loses the activation.
    activate(true);
    point(&p, 390.0, 290.0);
    point(&p, 110.0, 20.0);
    settle();
    assert!(o.get_tip_waiting());
    activate(false);
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_secs(1));
    assert!(!o.get_tip_shown());
}

/// The pointer moving off the control (onto a control without a tooltip, or where a
/// removed control was) closes the tooltip; on the control or the tooltip it stays.
#[test]
fn the_pointer_off_the_control_closes_the_tooltip() {
    let p = setup();
    let o = p.global::<Overlays>();
    p.invoke_focus_hint();
    settle();
    point(&p, 390.0, 290.0);
    point(&p, 110.0, 20.0);
    settle();
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_secs(1));
    assert!(o.get_tip_shown());
    // Still on the control, then on the tooltip itself: it stays.
    o.invoke_pointer_at(112.0, 22.0);
    assert!(o.get_tip_shown());
    let cover = o.get_tip_cover();
    o.invoke_pointer_at(cover.x + cover.width / 2.0, cover.y + cover.height / 2.0);
    assert!(o.get_tip_shown());
    // Elsewhere in the window: closed, without the control hearing the pointer leave.
    o.invoke_pointer_at(390.0, 290.0);
    assert!(!o.get_tip_shown() && !o.get_tip_waiting());
}

/// A button clicked with the pointer keeps the focus, but not its tooltip once the
/// pointer moves away.
#[test]
fn a_clicked_button_does_not_keep_its_tooltip() {
    let p = setup();
    let o = p.global::<Overlays>();
    p.invoke_focus_hint();
    settle();
    click(&p, 110.0, 20.0);
    settle();
    point(&p, 390.0, 290.0);
    settle();
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_secs(1));
    assert!(!o.get_tip_shown() && !o.get_tip_waiting());
}

#[test]
fn tab_into_a_row_below_the_view_shows_the_whole_row() {
    i_slint_backend_testing::init_no_event_loop();
    let p = rbs_app_ui::FocusRevealProbe::new().expect("the probe window");
    p.show().expect("show");
    p.invoke_focus_first();
    settle();
    assert_eq!(p.get_offset(), 0.0, "the first row is in view");
    let s: slint::SharedString = Key::Tab.into();
    p.window()
        .dispatch_event(WindowEvent::KeyPressed { text: s.clone() });
    p.window()
        .dispatch_event(WindowEvent::KeyReleased { text: s });
    settle();
    let shown_to = p.get_offset() + p.get_view_height();
    assert!(
        shown_to >= p.get_row_bottom(),
        "the row's help is in view: shown to {shown_to}, row ends at {}",
        p.get_row_bottom()
    );
}

#[test]
fn a_setting_rows_hint_shows_only_while_the_pointer_is_over_the_row_or_it_has_focus() {
    i_slint_backend_testing::init_no_event_loop();
    let p = rbs_app_ui::SettingRowProbe::new().expect("the probe window");
    p.show().expect("show");
    let at = |x: f32, y: f32| {
        p.window().dispatch_event(WindowEvent::PointerMoved {
            position: slint::LogicalPosition::new(x, y),
        });
        settle();
    };
    assert!(!p.get_hint_shown(), "no (i) before the pointer comes");
    at(20.0, 20.0);
    assert!(p.get_hint_shown(), "over the name");
    at(385.0, 28.0);
    assert!(p.get_hint_shown(), "over the switch at the other end");
    at(200.0, 180.0);
    assert!(!p.get_hint_shown(), "gone once the pointer leaves the row");
    // Tab reaches the (i) though it is not drawn, and then shows it.
    let s: slint::SharedString = Key::Tab.into();
    p.window()
        .dispatch_event(WindowEvent::KeyPressed { text: s.clone() });
    p.window()
        .dispatch_event(WindowEvent::KeyReleased { text: s });
    settle();
    assert!(p.get_hint_shown(), "keyboard focus on the (i)");
}

#[test]
fn a_setting_rows_switch_and_value_sit_at_its_right_edge_without_fill_false() {
    i_slint_backend_testing::init_no_event_loop();
    let p = rbs_app_ui::SettingRowAlignProbe::new().expect("the probe window");
    p.show().expect("show");
    settle();
    let close = |a: f32, b: f32| (a - b).abs() < 0.5;
    let right = p.get_row_right();
    assert!(
        !p.get_stacked(),
        "a 600 px row puts the control beside the name"
    );
    // The switch keeps its own width, at the right edge (not the column's left edge).
    assert!(
        close(p.get_switch_right(), right),
        "switch ends at {} of {right}",
        p.get_switch_right()
    );
    assert!(
        close(
            p.get_switch_right() - p.get_switch_left(),
            p.get_switch_own()
        ),
        "switch at its own width"
    );
    // So does a shown value.
    assert!(
        close(p.get_value_right(), right),
        "value ends at {} of {right}",
        p.get_value_right()
    );
    assert!(
        close(p.get_value_right() - p.get_value_left(), p.get_value_own()),
        "value at its own width"
    );
    // A drop-down still fills the column, up to the same edge.
    assert!(
        close(p.get_pick_right(), right),
        "drop-down ends at {} of {right}",
        p.get_pick_right()
    );
    assert!(
        p.get_pick_right() - p.get_pick_left() > p.get_switch_own(),
        "drop-down fills the column"
    );
    // Stacked (narrow), both go under the name at the left edge.
    p.set_row_width(200.0);
    settle();
    assert!(p.get_stacked(), "a 200 px row stacks");
    assert!(
        close(p.get_switch_left(), 0.0),
        "stacked switch at the left edge"
    );
    assert!(
        close(p.get_value_left(), 0.0),
        "stacked value at the left edge"
    );
}

fn number_probe() -> rbs_app_ui::NumberFieldProbe {
    i_slint_backend_testing::init_no_event_loop();
    let p = rbs_app_ui::NumberFieldProbe::new().expect("the probe window");
    p.show().expect("show");
    p
}

fn press(w: &slint::Window, x: f32, y: f32) {
    w.dispatch_event(WindowEvent::PointerMoved {
        position: slint::LogicalPosition::new(x, y),
    });
    w.dispatch_event(WindowEvent::PointerPressed {
        position: slint::LogicalPosition::new(x, y),
        button: slint::platform::PointerEventButton::Left,
    });
}

fn drag_to(w: &slint::Window, x: f32, y: f32) {
    w.dispatch_event(WindowEvent::PointerMoved {
        position: slint::LogicalPosition::new(x, y),
    });
}

fn release(w: &slint::Window, x: f32, y: f32) {
    w.dispatch_event(WindowEvent::PointerReleased {
        position: slint::LogicalPosition::new(x, y),
        button: slint::platform::PointerEventButton::Left,
    });
    settle();
}

/// Lets the field's live-edit timer (`delay.live-edit`, 16 ms) fire.
fn next_live_edit() {
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(20));
    settle();
}

#[test]
fn dragging_a_number_field_sends_each_value_in_one_gesture() {
    let p = number_probe();
    let w = p.window();
    press(w, 50.0, 14.0);
    // 10 px right and 2 px up at 4 px a step: three steps.
    drag_to(w, 60.0, 12.0);
    next_live_edit();
    assert_eq!(
        p.get_edits(),
        1,
        "the value in between applies during the drag"
    );
    assert_eq!(p.get_value(), 13.0);
    let gesture = p.get_edit_gesture();
    assert_ne!(gesture, 0, "a drag's edits carry its gesture");
    // 20 px right and 4 px up: six steps.
    drag_to(w, 70.0, 10.0);
    next_live_edit();
    assert_eq!(p.get_edits(), 2);
    assert_eq!(p.get_value(), 16.0);
    assert_eq!(
        p.get_edit_gesture(),
        gesture,
        "the same drag, the same gesture"
    );
    release(w, 70.0, 10.0);
    assert_eq!(p.get_edits(), 2, "release sends nothing already sent");
    assert_eq!(p.get_gesture(), 0, "no gesture outside a drag");
    // Left lowers it, in a gesture of its own; release sends a value still waiting.
    press(w, 50.0, 14.0);
    drag_to(w, 42.0, 14.0);
    release(w, 42.0, 14.0);
    assert_eq!(p.get_value(), 14.0);
    assert_eq!(p.get_edits(), 3);
    assert_ne!(p.get_edit_gesture(), gesture);
}

#[test]
fn a_field_that_is_not_live_changes_once_on_release() {
    let p = number_probe();
    p.set_live(false);
    let w = p.window();
    press(w, 50.0, 14.0);
    drag_to(w, 60.0, 12.0);
    next_live_edit();
    drag_to(w, 70.0, 10.0);
    next_live_edit();
    assert_eq!(p.get_edits(), 0, "nothing applies during the drag");
    release(w, 70.0, 10.0);
    assert_eq!(p.get_edits(), 1, "one change for the whole drag");
    assert_eq!(p.get_value(), 16.0);
}

#[test]
fn escape_during_a_drag_puts_the_value_back() {
    let p = number_probe();
    let w = p.window();
    press(w, 50.0, 14.0);
    drag_to(w, 90.0, 14.0);
    next_live_edit();
    assert_eq!(p.get_value(), 20.0, "applied live");
    key_on(w, Key::Escape);
    assert_eq!(p.get_value(), 10.0, "back where the drag started");
    let edits = p.get_edits();
    drag_to(w, 120.0, 14.0);
    next_live_edit();
    release(w, 120.0, 14.0);
    assert_eq!(p.get_edits(), edits, "the cancelled drag sends no more");
    assert_eq!(p.get_value(), 10.0);
    // Not live: Escape sends nothing at all.
    p.set_live(false);
    press(w, 50.0, 14.0);
    drag_to(w, 90.0, 14.0);
    key_on(w, Key::Escape);
    release(w, 90.0, 14.0);
    assert_eq!(p.get_edits(), edits);
    assert_eq!(p.get_value(), 10.0);
}

#[test]
fn a_field_being_typed_in_still_drags() {
    let p = number_probe();
    let w = p.window();
    // A click types.
    press(w, 50.0, 14.0);
    release(w, 50.0, 14.0);
    assert!(p.get_typing());
    // A drag past the swipe threshold changes the value live, from what is typed.
    w.dispatch_event(WindowEvent::KeyPressed { text: "2".into() });
    w.dispatch_event(WindowEvent::KeyReleased { text: "2".into() });
    press(w, 50.0, 14.0);
    drag_to(w, 62.0, 14.0);
    next_live_edit();
    assert_eq!(p.get_edits(), 1);
    assert_eq!(p.get_value(), 5.0, "2 and three steps");
    assert_ne!(p.get_edit_gesture(), 0);
    release(w, 62.0, 14.0);
    assert!(p.get_typing(), "the field keeps the focus");
    assert_eq!(p.get_gesture(), 0);
    // Escape during such a drag puts the value back.
    press(w, 50.0, 14.0);
    drag_to(w, 70.0, 14.0);
    next_live_edit();
    assert_eq!(p.get_value(), 10.0);
    key_on(w, Key::Escape);
    assert_eq!(p.get_value(), 5.0);
    release(w, 70.0, 14.0);
}

#[test]
fn a_click_without_moving_types_instead_of_dragging() {
    let p = number_probe();
    let w = p.window();
    press(w, 50.0, 14.0);
    drag_to(w, 51.0, 14.0);
    release(w, 51.0, 14.0);
    assert_eq!(p.get_edits(), 0);
    // The value is selected: typing replaces it, Return applies it.
    w.dispatch_event(WindowEvent::KeyPressed { text: "7".into() });
    w.dispatch_event(WindowEvent::KeyReleased { text: "7".into() });
    key_on(w, Key::Return);
    assert_eq!(p.get_edits(), 1);
    assert_eq!(p.get_value(), 7.0);
}

fn key_on(w: &slint::Window, k: Key) {
    let s: slint::SharedString = k.into();
    w.dispatch_event(WindowEvent::KeyPressed { text: s.clone() });
    w.dispatch_event(WindowEvent::KeyReleased { text: s });
    settle();
}

#[test]
fn a_changed_setting_resets_from_its_button_or_a_double_click_on_the_name() {
    i_slint_backend_testing::init_no_event_loop();
    let p = rbs_app_ui::SettingRowProbe::new().expect("the probe window");
    p.show().expect("show");
    let w = p.window();
    assert!(!p.get_reset_shown(), "no reset at the default");
    p.set_changed(true);
    settle();
    assert!(p.get_reset_shown(), "a changed value shows it");
    // The button sits just before the switch at the trailing end.
    let click = |x: f32, y: f32| {
        press(w, x, y);
        release(w, x, y);
    };
    click(345.0, 28.0);
    assert_eq!(p.get_resets(), 1, "the reset button");
    assert!(!p.get_reset_shown());
    // A double-click on the name resets too.
    p.set_changed(true);
    settle();
    click(20.0, 20.0);
    click(20.0, 20.0);
    assert_eq!(p.get_resets(), 2, "a double-click on the name");
    // Too narrow for the button: none, the double-click still works.
    p.set_row_width(200.0);
    p.set_changed(true);
    settle();
    assert!(!p.get_reset_shown(), "no room in a stacked row");
}

/// ADR-0113 decisions 1 and 4: a section above the level is not instantiated, the shell
/// says so, and Show draws it.
#[test]
fn a_hidden_level_is_not_made_until_show() {
    i_slint_backend_testing::init_no_event_loop();
    let p = rbs_app_ui::ExperienceProbe::new().expect("the probe window");
    p.global::<rbs_app_ui::Experience>().set_expert(false);
    p.show().expect("show");
    settle();
    // The window was built at the default level; what counts is what Show makes.
    let before = p.get_made();
    assert!(!p.get_show_all());
    // Show on "More options are hidden" is the only Tab stop in the window.
    for k in [Key::Tab, Key::Return] {
        let s: slint::SharedString = k.into();
        p.window()
            .dispatch_event(WindowEvent::KeyPressed { text: s.clone() });
        p.window()
            .dispatch_event(WindowEvent::KeyReleased { text: s });
    }
    settle();
    assert!(p.get_show_all(), "Show sets the shell's show-all");
    assert_eq!(
        p.get_made(),
        before + 1,
        "the section is made by Show, so it did not exist below Pro"
    );
    // Pro draws it without asking.
    let q = rbs_app_ui::ExperienceProbe::new().expect("a second probe window");
    q.show().expect("show");
    settle();
    assert_eq!(q.get_made(), 1, "drawn in Pro");
    assert!(!q.get_show_all());
}

#[test]
fn a_long_select_opens_a_list_that_stays_in_the_window_and_chooses_by_keyboard() {
    let p = setup();
    let o = p.global::<Overlays>();
    let options: Vec<slint::SharedString> = (0..40).map(|i| format!("Mode {i}").into()).collect();
    p.set_select_options(slint::ModelRc::new(slint::VecModel::from(options)));
    p.invoke_focus_select();
    key(&p, Key::Return);
    settle();
    assert!(
        o.get_select_open(),
        "a long list opens the scrolling list, not a menu"
    );
    assert_eq!(o.get_select_label().as_str(), "Mode");
    let cover = o.get_select_cover();
    assert!(
        cover.y >= 0.0 && cover.y + cover.height <= 300.0,
        "inside the window: {cover:?}"
    );
    assert!(cover.height < 40.0 * 20.0, "shorter than the 40 rows");
    // Nothing chosen yet: the highlight starts on the first row.
    key(&p, Key::DownArrow);
    key(&p, Key::DownArrow);
    key(&p, Key::End);
    key(&p, Key::UpArrow);
    key(&p, Key::Return);
    settle();
    assert!(!o.get_select_open());
    assert_eq!(p.get_select_index(), 38);
    // Opening again starts on the chosen row; Escape closes without a choice.
    key(&p, Key::Return);
    settle();
    assert!(o.get_select_open());
    assert_eq!(o.get_select_current(), 38);
    key(&p, Key::Home);
    key(&p, Key::Escape);
    settle();
    assert!(!o.get_select_open());
    assert_eq!(p.get_select_index(), 38);
}

fn wheel(p: &Probe, x: f32, y: f32, delta_y: f32) {
    point(p, x, y);
    p.window().dispatch_event(WindowEvent::PointerScrolled {
        position: slint::LogicalPosition::new(x, y),
        delta_x: 0.0,
        delta_y,
    });
    settle();
}

fn open_long_select(p: &Probe, count: usize, chosen: i32) {
    let options: Vec<slint::SharedString> =
        (0..count).map(|i| format!("Mode {i}").into()).collect();
    p.set_select_options(slint::ModelRc::new(slint::VecModel::from(options)));
    p.set_select_index(chosen);
    p.invoke_focus_select();
    key(p, Key::Return);
    settle();
    assert!(p.global::<Overlays>().get_select_open());
}

#[test]
fn the_wheel_scrolls_a_long_select_to_its_last_option() {
    let p = setup();
    let o = p.global::<Overlays>();
    // The highlight sits on a chosen row near the top: scrolling must not be pulled
    // back to it when the rows in view are rebuilt.
    open_long_select(&p, 60, 3);
    let c = o.get_select_cover();
    let (x, top) = (c.x + c.width / 2.0, c.y);
    for _ in 0..200 {
        wheel(&p, x, top + c.height / 2.0, -40.0);
    }
    // The bottom row in view is the last option.
    click(&p, x - 10.0, top + c.height - 12.0);
    settle();
    assert!(!o.get_select_open());
    assert_eq!(p.get_select_index(), 59, "the list reached its end");
}

#[test]
fn dragging_the_scrollbar_reaches_the_last_option() {
    let p = setup();
    let o = p.global::<Overlays>();
    open_long_select(&p, 60, 3);
    let c = o.get_select_cover();
    // The bar is the trailing strip of the list; its thumb starts at the top.
    let bar_x = c.x + c.width - 8.0;
    let top = c.y + 12.0;
    point(&p, bar_x, top);
    let button = slint::platform::PointerEventButton::Left;
    p.window().dispatch_event(WindowEvent::PointerPressed {
        position: slint::LogicalPosition::new(bar_x, top),
        button,
    });
    // Small steps, as a hand moves: each one rebuilds only a few rows.
    for step in 1..=400 {
        point(&p, bar_x, top + step as f32);
        settle();
    }
    p.window().dispatch_event(WindowEvent::PointerReleased {
        position: slint::LogicalPosition::new(bar_x, top + 400.0),
        button,
    });
    settle();
    click(&p, c.x + c.width / 2.0, c.y + c.height - 12.0);
    settle();
    assert_eq!(p.get_select_index(), 59, "the thumb reached the end");
}

#[test]
fn the_wheel_outside_an_open_select_list_closes_it() {
    let p = setup();
    let o = p.global::<Overlays>();
    open_long_select(&p, 60, 3);
    let c = o.get_select_cover();
    // Over the list itself the wheel scrolls the list and keeps it open.
    wheel(&p, c.x + c.width / 2.0, c.y + c.height / 2.0, -40.0);
    assert!(o.get_select_open());
    // Outside it the page would move under the list: the list closes instead of
    // staying where the field was.
    wheel(&p, 380.0, 20.0, -40.0);
    assert!(!o.get_select_open());
    assert_eq!(p.get_select_index(), 3, "no choice was made");
}

#[test]
fn the_wheel_outside_an_open_hint_closes_it() {
    let p = setup();
    let o = p.global::<Overlays>();
    p.invoke_focus_hint();
    key(&p, Key::Return);
    assert!(o.get_hint_open());
    wheel(&p, 380.0, 280.0, -40.0);
    assert!(!o.get_hint_open());
}

#[test]
fn a_fast_spinning_wheel_moves_a_long_select_while_it_spins() {
    let p = setup();
    let o = p.global::<Overlays>();
    open_long_select(&p, 60, 3);
    let c = o.get_select_cover();
    let (x, mid) = (c.x + c.width / 2.0, c.y + c.height / 2.0);
    // A free-spinning wheel: many wheel steps a few milliseconds apart, each with
    // the animated scrolling of a real mouse wheel (the public event has no phase).
    for _ in 0..60 {
        let step = i_slint_core::input::BackendMouseEvent::Wheel {
            position: i_slint_core::lengths::LogicalPoint::new(x, mid),
            delta_x: 0.0,
            delta_y: -40.0,
            phase: i_slint_core::input::TouchPhase::Moved,
        };
        p.window().dispatch_event(WindowEvent::internal(step));
        i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(8));
        settle();
    }
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_secs(1));
    settle();
    click(&p, x - 10.0, c.y + c.height - 12.0);
    settle();
    assert_eq!(p.get_select_index(), 59, "the list reached its end");
}
