//! The theme editor carries neighbours along (themes spec, "Editing"): one edit that
//! breaks an order rule of "Across tokens" still gives a theme that resolves, with the
//! edited value kept.

#![allow(clippy::unwrap_used)] // Tests fail by panicking.

use rbs_ui::tokens::{Base, ThemeBody, Tokens, TypeStyle};

fn file() -> Tokens {
    Tokens::built_in().unwrap()
}

fn length(name: &str, v: f32) -> ThemeBody {
    let f = file();
    let before = ThemeBody::new(Base::Dark);
    let mut b = before.clone();
    b.length.insert(name.into(), v);
    f.carry(&before, &mut b, name);
    b
}

fn style(name: &str, st: TypeStyle) -> ThemeBody {
    let f = file();
    let before = ThemeBody::new(Base::Dark);
    let mut b = before.clone();
    b.type_styles.insert(name.into(), st);
    f.carry(&before, &mut b, name);
    b
}

#[test]
fn one_length_edit_always_resolves_and_keeps_its_value() {
    for (name, v) in [
        ("radius.md", 16.0),
        ("radius.md", 0.0),
        ("radius.sm", 8.0),
        ("radius.lg", 0.0),
        ("space.2", 16.0),
        ("space.4", 2.0),
        ("space.1", 2.0),
        ("size.control.sm", 32.0),
        ("size.control.lg", 24.0),
        ("size.field.gap", 20.0),
        ("size.section.gap", 12.0),
        ("size.icon.box", 24.0),
        ("size.focus-ring", 6.0),
    ] {
        let b = length(name, v);
        assert!(
            file().resolve(&b).is_ok(),
            "{name} = {v}: {:?}",
            file().resolve(&b).err()
        );
        assert_eq!(
            b.length.get(name).copied().unwrap_or(v),
            v,
            "{name} keeps {v}"
        );
    }
}

#[test]
fn carried_neighbours_move_to_the_edited_value() {
    let b = length("radius.md", 16.0);
    assert_eq!(b.length.get("radius.lg"), Some(&16.0));
    assert_eq!(
        b.length.get("radius.sm"),
        None,
        "radius.sm (4) was already in order"
    );
    let b = length("radius.md", 2.0);
    assert_eq!(b.length.get("radius.sm"), Some(&2.0));
    assert_eq!(b.length.get("radius.lg"), None);
    let b = length("space.2", 20.0);
    assert_eq!(b.length.get("space.3"), Some(&20.0));
    assert_eq!(b.length.get("space.4"), Some(&20.0));
    assert_eq!(b.length.get("space.6"), None, "24 is already above 20");
}

#[test]
fn a_value_under_a_floor_still_fails() {
    // The focus ring cannot go under 2 px, so space.1 = 1 cannot hold it.
    let b = length("space.1", 1.0);
    assert_eq!(
        b.length.get("size.focus-ring"),
        None,
        "held at its floor (2, the base)"
    );
    assert!(file().resolve(&b).is_err());
}

#[test]
fn a_button_radius_stays_a_pill_at_most() {
    let b = length("radius.button", 16.0);
    assert_eq!(b.length.get("radius.button"), Some(&12.0));
    assert!(file().resolve(&b).is_ok());
}

#[test]
fn one_text_edit_always_resolves() {
    for (name, size, line) in [
        ("type.body", 14, 20),
        ("type.body", 20, 28),
        ("type.caption", 14, 18),
        ("type.title", 12, 16),
        ("type.heading", 30, 40),
        ("type.label", 11, 14),
    ] {
        let b = style(
            name,
            TypeStyle {
                size,
                line,
                weight: 400,
            },
        );
        assert!(
            file().resolve(&b).is_ok(),
            "{name} {size}: {:?}",
            file().resolve(&b).err()
        );
        assert_eq!(b.type_styles[name].size, size);
    }
    // A body line of 28 makes small controls 28 high, and the larger ones follow.
    let b = style(
        "type.body",
        TypeStyle {
            size: 20,
            line: 28,
            weight: 400,
        },
    );
    assert_eq!(b.length.get("size.control.sm"), Some(&28.0));
    assert_eq!(
        b.length.get("size.control.md"),
        None,
        "28 is the base value: no override"
    );
}
