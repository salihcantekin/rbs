//! Platform-neutral helpers of the Raw Input handler: what a mouse report's button
//! flags mean, the wheel's notches, the mask bits and the QPC conversion. Pure, so
//! their tests run on every OS.

use rbs_input::{Button, EventMask};

/// `RAWMOUSE.usButtonFlags` bits (`RI_MOUSE_*`, stable since Windows XP).
pub(crate) mod ri {
    pub const LEFT_DOWN: u16 = 0x0001;
    pub const LEFT_UP: u16 = 0x0002;
    pub const RIGHT_DOWN: u16 = 0x0004;
    pub const RIGHT_UP: u16 = 0x0008;
    pub const MIDDLE_DOWN: u16 = 0x0010;
    pub const MIDDLE_UP: u16 = 0x0020;
    pub const BUTTON_4_DOWN: u16 = 0x0040;
    pub const BUTTON_4_UP: u16 = 0x0080;
    pub const BUTTON_5_DOWN: u16 = 0x0100;
    pub const BUTTON_5_UP: u16 = 0x0200;
    pub const WHEEL: u16 = 0x0400;
    pub const HWHEEL: u16 = 0x0800;
}

/// One notch of the wheel (`WHEEL_DELTA`).
pub(crate) const WHEEL_DELTA: f32 = 120.0;

/// The button transitions of one report, in a fixed order: every flag set gives one.
pub(crate) const TRANSITIONS: [(u16, Button, bool); 10] = [
    (ri::LEFT_DOWN, Button::Left, true),
    (ri::LEFT_UP, Button::Left, false),
    (ri::RIGHT_DOWN, Button::Right, true),
    (ri::RIGHT_UP, Button::Right, false),
    (ri::MIDDLE_DOWN, Button::Middle, true),
    (ri::MIDDLE_UP, Button::Middle, false),
    (ri::BUTTON_4_DOWN, Button::Back, true),
    (ri::BUTTON_4_UP, Button::Back, false),
    (ri::BUTTON_5_DOWN, Button::Forward, true),
    (ri::BUTTON_5_UP, Button::Forward, false),
];

/// Every button flag.
pub(crate) const BUTTON_FLAGS: u16 = 0x03ff;

/// The wheel's `(dx, dy)` notches of a report, `None` without a wheel flag.
/// `usButtonData` is a signed count of `WHEEL_DELTA` units: positive is away from
/// the user for the vertical wheel and to the right for the horizontal one, the
/// port's signs.
pub(crate) fn wheel(flags: u16, data: u16) -> Option<(f32, f32)> {
    let notches = f32::from(data as i16) / WHEEL_DELTA;
    if flags & ri::WHEEL != 0 {
        Some((0.0, notches))
    } else if flags & ri::HWHEEL != 0 {
        Some((notches, 0.0))
    } else {
        None
    }
}

/// The mouse classes of `mask` as bits, for an atomic.
pub(crate) fn mask_bits(mask: EventMask) -> u8 {
    let mut bits = 0;
    if mask.contains(EventMask::POINTER) {
        bits |= 1;
    }
    if mask.contains(EventMask::BUTTONS) {
        bits |= 2;
    }
    if mask.contains(EventMask::WHEEL) {
        bits |= 4;
    }
    bits
}

/// The inverse of [`mask_bits`].
pub(crate) fn mask_from_bits(bits: u8) -> EventMask {
    let mut mask = EventMask::NONE;
    if bits & 1 != 0 {
        mask = mask.union(EventMask::POINTER);
    }
    if bits & 2 != 0 {
        mask = mask.union(EventMask::BUTTONS);
    }
    if bits & 4 != 0 {
        mask = mask.union(EventMask::WHEEL);
    }
    mask
}

/// QPC ticks to nanoseconds without overflow; a zero frequency reads as 1 Hz.
pub(crate) fn qpc_to_nanos(counter: i64, frequency: i64) -> u64 {
    let counter = u128::from(counter.unsigned_abs());
    let frequency = u128::from(frequency.unsigned_abs().max(1));
    u64::try_from(counter * 1_000_000_000 / frequency).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_button_flag_has_one_transition() {
        let mut all = 0u16;
        for (flag, _, _) in TRANSITIONS {
            assert_eq!(flag.count_ones(), 1, "{flag:#x}");
            assert_eq!(all & flag, 0, "{flag:#x} twice");
            all |= flag;
        }
        assert_eq!(all, BUTTON_FLAGS);
        assert_eq!(BUTTON_FLAGS & (ri::WHEEL | ri::HWHEEL), 0);
    }

    #[test]
    fn side_buttons_are_back_then_forward() {
        let find = |flag| TRANSITIONS.iter().find(|t| t.0 == flag).map(|t| (t.1, t.2));
        assert_eq!(find(ri::BUTTON_4_DOWN), Some((Button::Back, true)));
        assert_eq!(find(ri::BUTTON_5_UP), Some((Button::Forward, false)));
        assert_eq!(find(ri::LEFT_DOWN), Some((Button::Left, true)));
        assert_eq!(find(ri::RIGHT_UP), Some((Button::Right, false)));
    }

    #[test]
    fn wheel_data_is_signed_notches() {
        assert_eq!(wheel(ri::WHEEL, 120), Some((0.0, 1.0)));
        assert_eq!(wheel(ri::WHEEL, (-240i16) as u16), Some((0.0, -2.0)));
        assert_eq!(wheel(ri::HWHEEL, 60), Some((0.5, 0.0)));
        assert_eq!(wheel(ri::HWHEEL, (-120i16) as u16), Some((-1.0, 0.0)));
        assert_eq!(wheel(ri::LEFT_DOWN, 120), None);
        assert_eq!(wheel(0, 0), None);
    }

    #[test]
    fn mask_bits_round_trip() {
        for bits in 0..8u8 {
            assert_eq!(mask_bits(mask_from_bits(bits)), bits);
        }
        assert_eq!(
            mask_from_bits(mask_bits(EventMask::MOUSE)),
            EventMask::MOUSE
        );
        assert_eq!(mask_bits(EventMask::KEYS), 0);
    }

    #[test]
    fn qpc_converts_without_overflow() {
        assert_eq!(qpc_to_nanos(10_000_000, 10_000_000), 1_000_000_000);
        assert_eq!(qpc_to_nanos(3, 3_000_000_000), 1);
        // A year of 10 MHz ticks.
        let year = 10_000_000i64 * 3600 * 24 * 365;
        assert_eq!(qpc_to_nanos(year, 10_000_000), 31_536_000_000_000_000);
        assert_eq!(qpc_to_nanos(5, 0), 5_000_000_000);
    }
}
