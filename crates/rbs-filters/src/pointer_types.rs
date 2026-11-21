//! The built-in pointer filter types (ADR-0165 decision 3; spec `video-filters.md`,
//! "Pointer types"): click rings and the pointer halo. Both declare
//! [`Needs::POINTER`] and draw from the effect buffer.

use std::time::Duration;

use rbs_graph::node::NodeTypeId;
use rbs_input::Button;

use crate::pointer::{Click, Effect, Effects, Needs, PointerView, STILL};
use crate::types::{Field, FilterType, Pass, PassSource, Scale, Uniforms, Values};

/// Type id of click rings.
pub const CLICK_RINGS: &str = "org.rbs.filter.click-rings";
/// Type id of the pointer halo.
pub const POINTER_HALO: &str = "org.rbs.filter.pointer-halo";

/// Linear value of an sRGB-encoded 8-bit channel.
fn linear(c: u8) -> f32 {
    let e = f32::from(c) / 255.0;
    if e <= 0.040_45 {
        e / 12.92
    } else {
        ((e + 0.055) / 1.055).powf(2.4)
    }
}

/// Linear rgb and straight alpha of an sRGB colour.
fn linear_rgba(c: [u8; 4]) -> [f32; 4] {
    [
        linear(c[0]),
        linear(c[1]),
        linear(c[2]),
        f32::from(c[3]) / 255.0,
    ]
}

/// Whether a click has started on this tick: its time is at or before the frame's, so
/// the ring does not run ahead of the captured cursor, or the screen is still (no new
/// frame for `STILL`), so it does not wait for the screen to change (ADR-0165, "Time").
fn started(click: &Click, view: &PointerView<'_>) -> bool {
    click.time <= view.now
        && (click.time <= view.frame_time || view.now.saturating_since(view.frame_time) > STILL)
}

/// Click rings: a ring grows and fades out from each click.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClickRings;

const BUTTONS: [(&str, &str); 3] = [
    ("all", "All buttons"),
    ("left_right", "Left and right"),
    ("left", "Left only"),
];
const RING_FIELDS: [Field; 7] = [
    Field::color("left_color", "Left button", [255, 196, 0, 255]),
    Field::color("right_color", "Right button", [0, 160, 255, 255]),
    Field::color("middle_color", "Other buttons", [190, 90, 255, 255]),
    Field::number("width", "Ring size", 1.0, 16.0, 4.0, 1.0, "px")
        .with_help("How thick the ring's line is."),
    Field::number("radius", "Ring radius", 4.0, 200.0, 36.0, 1.0, "px")
        .with_help("How far the ring grows from the click."),
    Field::number("duration", "Duration", 0.1, 3.0, 0.6, 0.05, "s")
        .with_help("How long a ring stays before it has faded out."),
    Field::choice("buttons", "Show for", &BUTTONS, "all"),
];
const RING_PASSES: [Pass; 1] = [Pass {
    shader: include_str!("shaders/click_rings.wgsl"),
    target: Scale::Full,
    source: PassSource::Input,
}];

impl ClickRings {
    /// The colour field of `button`, or `None` when `buttons` (the choice) hides it.
    fn color_field(button: Button, buttons: u8) -> Option<usize> {
        match (button, buttons) {
            (Button::Left, _) => Some(0),
            (Button::Right, 0 | 1) => Some(1),
            (_, 0) => Some(2),
            _ => None,
        }
    }
}

impl FilterType for ClickRings {
    fn type_id(&self) -> NodeTypeId {
        NodeTypeId::new_static(CLICK_RINGS)
    }
    fn name(&self) -> &'static str {
        "Click rings"
    }
    fn fields(&self) -> &'static [Field] {
        &RING_FIELDS
    }
    fn passes(&self) -> &'static [Pass] {
        &RING_PASSES
    }
    fn uniforms(&self, values: &Values, _input: [u32; 2], out: &mut [Uniforms]) {
        let Some(u) = out.first_mut() else { return };
        u.0[0] = [values.number(3), 0.0, 0.0, 0.0];
    }
    fn needs(&self) -> Needs {
        Needs::POINTER
    }
    fn effects(&self, values: &Values, view: &PointerView<'_>, out: &mut Effects) {
        let width = values.number(3);
        let radius = values.number(4);
        let life = Duration::from_secs_f32(values.number(5).max(0.01));
        let buttons = values.choice(6);
        // Newest last, so with more live clicks than slots the oldest are left out.
        let skip = view
            .clicks
            .len()
            .saturating_sub(crate::pointer::MAX_EFFECTS);
        for click in &view.clicks[skip..] {
            if !started(click, view) {
                continue;
            }
            let age = view.now.saturating_since(click.time);
            if age >= life {
                continue;
            }
            let Some(field) = Self::color_field(click.button, buttons) else {
                continue;
            };
            let t = age.as_secs_f32() / life.as_secs_f32();
            let grow = 1.0 - (1.0 - t) * (1.0 - t);
            let [r, g, b, a] = linear_rgba(values.color(field));
            out.push(Effect([
                [
                    click.at[0],
                    click.at[1],
                    (radius * grow).max(width * 0.5),
                    a * (1.0 - t),
                ],
                [r, g, b, 0.0],
            ]));
        }
    }
}

/// Pointer halo: a circle that follows the pointer, with optional dimming outside a
/// spotlight.
#[derive(Debug, Clone, Copy, Default)]
pub struct PointerHalo;

const HALO_FIELDS: [Field; 5] = [
    Field::number("size", "Size", 8.0, 200.0, 40.0, 1.0, "px").with_help("The circle's radius."),
    Field::color("color", "Colour", [255, 220, 0, 96]),
    Field::toggle("spotlight", "Spotlight", false)
        .with_help("Darkens the picture outside a circle around the pointer."),
    Field::number("dim", "Dimming", 0.0, 1.0, 0.5, 0.01, ""),
    Field::number(
        "spot_radius",
        "Spotlight radius",
        20.0,
        600.0,
        160.0,
        1.0,
        "px",
    ),
];
const HALO_PASSES: [Pass; 1] = [Pass {
    shader: include_str!("shaders/pointer_halo.wgsl"),
    target: Scale::Full,
    source: PassSource::Input,
}];

impl FilterType for PointerHalo {
    fn type_id(&self) -> NodeTypeId {
        NodeTypeId::new_static(POINTER_HALO)
    }
    fn name(&self) -> &'static str {
        "Pointer halo"
    }
    fn fields(&self) -> &'static [Field] {
        &HALO_FIELDS
    }
    fn passes(&self) -> &'static [Pass] {
        &HALO_PASSES
    }
    fn uniforms(&self, values: &Values, _input: [u32; 2], out: &mut [Uniforms]) {
        let Some(u) = out.first_mut() else { return };
        let dim = if values.toggle(2) {
            values.number(3)
        } else {
            0.0
        };
        u.0[0] = [values.number(0), dim, values.number(4), 0.0];
        u.0[1] = linear_rgba(values.color(1));
    }
    fn needs(&self) -> Needs {
        Needs::POINTER
    }
    fn effects(&self, _values: &Values, view: &PointerView<'_>, out: &mut Effects) {
        if let Some([x, y]) = view.pointer {
            out.push(Effect([[x, y, 0.0, 0.0], [0.0; 4]]));
        }
    }
}

#[cfg(test)]
mod tests {
    use rbs_core::time::Timestamp;

    use super::*;

    const MS: u64 = 1_000_000;

    fn click(ms: u64, button: Button) -> Click {
        Click {
            time: Timestamp::from_nanos(ms * MS),
            at: [10.0, 20.0],
            button,
        }
    }

    fn view(now_ms: u64, frame_ms: u64, clicks: &[Click]) -> PointerView<'_> {
        PointerView {
            now: Timestamp::from_nanos(now_ms * MS),
            frame_time: Timestamp::from_nanos(frame_ms * MS),
            size: [64, 64],
            pointer: Some([5.0, 6.0]),
            clicks,
        }
    }

    fn rings(values: &Values, view: &PointerView<'_>) -> Effects {
        let mut out = Effects::default();
        ClickRings.effects(values, view, &mut out);
        out
    }

    #[test]
    fn a_ring_grows_fades_and_dies_after_its_duration() {
        let v = Values::defaults(&ClickRings as &dyn FilterType);
        let clicks = [click(100, Button::Left)];
        let early = rings(&v, &view(110, 110, &clicks));
        let late = rings(&v, &view(600, 600, &clicks));
        let [e, l] = [early.as_slice()[0].0[0], late.as_slice()[0].0[0]];
        assert!(l[2] > e[2], "grows: {} then {}", e[2], l[2]);
        assert!(l[3] < e[3], "fades: {} then {}", e[3], l[3]);
        assert!(rings(&v, &view(710, 710, &clicks)).is_empty(), "past 0.6 s");
    }

    #[test]
    fn a_ring_waits_for_the_frame_unless_the_screen_is_still() {
        let v = Values::defaults(&ClickRings as &dyn FilterType);
        let clicks = [click(100, Button::Left)];
        assert!(
            rings(&v, &view(110, 90, &clicks)).is_empty(),
            "ahead of the frame"
        );
        assert_eq!(rings(&v, &view(160, 90, &clicks)).len(), 1, "still screen");
        assert!(
            rings(&v, &view(90, 90, &clicks)).is_empty(),
            "in the future"
        );
    }

    #[test]
    fn buttons_pick_their_colour_and_the_choice_hides_some() {
        let all = Values::defaults(&ClickRings as &dyn FilterType);
        let clicks = [
            click(100, Button::Left),
            click(100, Button::Right),
            click(100, Button::Middle),
        ];
        let fx = rings(&all, &view(110, 110, &clicks));
        assert_eq!(fx.len(), 3);
        let right = linear_rgba([0, 160, 255, 255]);
        assert_eq!(fx.as_slice()[1].0[1][..3], right[..3]);
        let left = all
            .with(
                &ClickRings as &dyn FilterType,
                [("buttons", rbs_params::Value::Choice(2))],
            )
            .unwrap_or(all);
        assert_eq!(rings(&left, &view(110, 110, &clicks)).len(), 1);
    }

    #[test]
    fn the_halo_follows_the_pointer_and_is_gone_without_one() {
        let v = Values::defaults(&PointerHalo as &dyn FilterType);
        let mut out = Effects::default();
        PointerHalo.effects(&v, &view(10, 10, &[]), &mut out);
        assert_eq!(out.as_slice()[0].0[0][..2], [5.0, 6.0]);
        let mut gone = Effects::default();
        let mut none = view(10, 10, &[]);
        none.pointer = None;
        PointerHalo.effects(&v, &none, &mut gone);
        assert!(gone.is_empty());
    }

    #[test]
    fn the_types_pass_the_type_checks() {
        for ty in [&ClickRings as &dyn FilterType, &PointerHalo] {
            let problems = crate::conformance::check_type(ty);
            assert!(problems.is_empty(), "{}: {problems:?}", ty.name());
        }
    }
}
