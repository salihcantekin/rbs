//! Platform-neutral helpers of the tap: which Core Graphics event types a mask needs,
//! what an event type means, the other-button numbers, and which clock an event
//! timestamp is in. Pure, so their tests run on every OS.

use rbs_input::{Button, EventMask, Permission};

/// Core Graphics event type numbers (`CGEventType`, stable since macOS 10.4).
pub(crate) mod cg {
    pub const LEFT_DOWN: u32 = 1;
    pub const LEFT_UP: u32 = 2;
    pub const RIGHT_DOWN: u32 = 3;
    pub const RIGHT_UP: u32 = 4;
    pub const MOVED: u32 = 5;
    pub const LEFT_DRAGGED: u32 = 6;
    pub const RIGHT_DRAGGED: u32 = 7;
    pub const SCROLL_WHEEL: u32 = 22;
    pub const OTHER_DOWN: u32 = 25;
    pub const OTHER_UP: u32 = 26;
    pub const OTHER_DRAGGED: u32 = 27;
    /// `kCGEventTapDisabledByTimeout`.
    pub const DISABLED_BY_TIMEOUT: u32 = 0xFFFF_FFFE;
    /// `kCGEventTapDisabledByUserInput`.
    pub const DISABLED_BY_USER_INPUT: u32 = 0xFFFF_FFFF;
}

const fn bit(ty: u32) -> u64 {
    1 << ty
}

/// The `CGEventMask` a tap needs for `mask`: moves and drags for the pointer, downs
/// and ups for buttons, the scroll wheel for the wheel. Never asks for keys.
pub(crate) fn cg_event_mask(mask: EventMask) -> u64 {
    let mut out = 0;
    if mask.contains(EventMask::POINTER) {
        out |= bit(cg::MOVED)
            | bit(cg::LEFT_DRAGGED)
            | bit(cg::RIGHT_DRAGGED)
            | bit(cg::OTHER_DRAGGED);
    }
    if mask.contains(EventMask::BUTTONS) {
        out |= bit(cg::LEFT_DOWN)
            | bit(cg::LEFT_UP)
            | bit(cg::RIGHT_DOWN)
            | bit(cg::RIGHT_UP)
            | bit(cg::OTHER_DOWN)
            | bit(cg::OTHER_UP);
    }
    if mask.contains(EventMask::WHEEL) {
        out |= bit(cg::SCROLL_WHEEL);
    }
    out
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

/// Which physical button a Core Graphics event type names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Side {
    Left,
    Right,
    /// The button number field says which one.
    Other,
}

/// What a tapped event means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// A move or a drag: only the position.
    Move,
    Down(Side),
    Up(Side),
    Wheel,
    /// The system disabled the tap; re-enable it.
    Disabled,
}

/// The meaning of event type `ty`, or `None` for a type the tap does not use.
pub(crate) fn classify(ty: u32) -> Option<Kind> {
    Some(match ty {
        cg::MOVED | cg::LEFT_DRAGGED | cg::RIGHT_DRAGGED | cg::OTHER_DRAGGED => Kind::Move,
        cg::LEFT_DOWN => Kind::Down(Side::Left),
        cg::RIGHT_DOWN => Kind::Down(Side::Right),
        cg::OTHER_DOWN => Kind::Down(Side::Other),
        cg::LEFT_UP => Kind::Up(Side::Left),
        cg::RIGHT_UP => Kind::Up(Side::Right),
        cg::OTHER_UP => Kind::Up(Side::Other),
        cg::SCROLL_WHEEL => Kind::Wheel,
        cg::DISABLED_BY_TIMEOUT | cg::DISABLED_BY_USER_INPUT => Kind::Disabled,
        _ => return None,
    })
}

/// The port's button for `side`; `number` is `kCGMouseEventButtonNumber`, read only
/// for [`Side::Other`]: 2 middle, 3 back, 4 forward, anything else ignored.
pub(crate) fn button(side: Side, number: i64) -> Option<Button> {
    match side {
        Side::Left => Some(Button::Left),
        Side::Right => Some(Button::Right),
        Side::Other => match number {
            2 => Some(Button::Middle),
            3 => Some(Button::Back),
            4 => Some(Button::Forward),
            _ => None,
        },
    }
}

/// How far an event timestamp may be from now and still be believed.
pub(crate) const PLAUSIBLE_NANOS: u64 = 1_000_000_000;

/// The host time in nanoseconds (`mach_absolute_time` scaled) of an event whose
/// `CGEventGetTimestamp` is `raw`.
///
/// The documented unit is nanoseconds, but on Apple silicon the value is in mach
/// ticks. Of the two readings, raw as nanoseconds and `raw * numer / denom`, this picks
/// the one nearer `now_nanos` (host time when the event arrived); when both are more
/// than [`PLAUSIBLE_NANOS`] away, the event is stamped as arriving now.
pub(crate) fn event_host_nanos(raw: u64, numer: u32, denom: u32, now_nanos: u64) -> u64 {
    let scaled = if denom == 0 {
        raw
    } else {
        let wide = u128::from(raw) * u128::from(numer) / u128::from(denom);
        u64::try_from(wide).unwrap_or(u64::MAX)
    };
    let (a, b) = (raw.abs_diff(now_nanos), scaled.abs_diff(now_nanos));
    let (best, gap) = if b < a { (scaled, b) } else { (raw, a) };
    if gap > PLAUSIBLE_NANOS {
        now_nanos
    } else {
        best
    }
}

/// The Input Monitoring status (`input-port.md`, "macOS adapter"). `granted`:
/// `CGPreflightListenEventAccess`. `asked`: a request already ran in this process.
/// The preflight cannot tell "not asked" from "refused", and the system shows its
/// prompt only once, so after one request a missing grant reads `Denied` (the same
/// rule as ScreenCaptureKit, ADR-0141 decision 5).
pub(crate) fn permission(granted: bool, asked: bool) -> Permission {
    match (granted, asked) {
        (true, _) => Permission::Granted,
        (false, true) => Permission::Denied,
        (false, false) => Permission::NotDetermined,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointer_asks_for_moves_and_drags_only() {
        let m = cg_event_mask(EventMask::POINTER);
        for ty in [
            cg::MOVED,
            cg::LEFT_DRAGGED,
            cg::RIGHT_DRAGGED,
            cg::OTHER_DRAGGED,
        ] {
            assert_ne!(m & bit(ty), 0, "{ty}");
        }
        assert_eq!(m.count_ones(), 4);
    }

    #[test]
    fn buttons_ask_for_downs_and_ups_only() {
        let m = cg_event_mask(EventMask::BUTTONS);
        for ty in [
            cg::LEFT_DOWN,
            cg::LEFT_UP,
            cg::RIGHT_DOWN,
            cg::RIGHT_UP,
            cg::OTHER_DOWN,
            cg::OTHER_UP,
        ] {
            assert_ne!(m & bit(ty), 0, "{ty}");
        }
        assert_eq!(m.count_ones(), 6);
    }

    #[test]
    fn wheel_asks_for_the_scroll_wheel_and_keys_for_nothing() {
        assert_eq!(cg_event_mask(EventMask::WHEEL), bit(cg::SCROLL_WHEEL));
        assert_eq!(cg_event_mask(EventMask::KEYS), 0);
        assert_eq!(cg_event_mask(EventMask::NONE), 0);
        assert_eq!(cg_event_mask(EventMask::MOUSE).count_ones(), 11);
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
    fn types_classify() {
        assert_eq!(classify(cg::MOVED), Some(Kind::Move));
        assert_eq!(classify(cg::OTHER_DRAGGED), Some(Kind::Move));
        assert_eq!(classify(cg::LEFT_DOWN), Some(Kind::Down(Side::Left)));
        assert_eq!(classify(cg::RIGHT_UP), Some(Kind::Up(Side::Right)));
        assert_eq!(classify(cg::OTHER_DOWN), Some(Kind::Down(Side::Other)));
        assert_eq!(classify(cg::SCROLL_WHEEL), Some(Kind::Wheel));
        assert_eq!(classify(cg::DISABLED_BY_TIMEOUT), Some(Kind::Disabled));
        assert_eq!(classify(cg::DISABLED_BY_USER_INPUT), Some(Kind::Disabled));
        assert_eq!(classify(10), None, "key down");
        assert_eq!(classify(0), None, "null");
    }

    #[test]
    fn other_buttons_map_by_number() {
        assert_eq!(button(Side::Left, 99), Some(Button::Left));
        assert_eq!(button(Side::Right, 0), Some(Button::Right));
        assert_eq!(button(Side::Other, 2), Some(Button::Middle));
        assert_eq!(button(Side::Other, 3), Some(Button::Back));
        assert_eq!(button(Side::Other, 4), Some(Button::Forward));
        assert_eq!(button(Side::Other, 5), None);
        assert_eq!(button(Side::Other, 0), None);
        assert_eq!(button(Side::Other, -1), None);
    }

    const NOW: u64 = 3_600_000_000_000; // an hour after boot

    #[test]
    fn nanosecond_timestamps_are_kept() {
        // Intel: numer = denom = 1, both readings agree.
        assert_eq!(
            event_host_nanos(NOW - 2_000_000, 1, 1, NOW),
            NOW - 2_000_000
        );
        // Apple silicon timebase, but the value already in nanoseconds.
        assert_eq!(
            event_host_nanos(NOW - 2_000_000, 125, 3, NOW),
            NOW - 2_000_000
        );
    }

    #[test]
    fn tick_timestamps_are_scaled() {
        // 24 MHz ticks: 125/3 ns per tick.
        let ticks = (NOW - 3_000_000) * 3 / 125;
        let got = event_host_nanos(ticks, 125, 3, NOW);
        assert!(got.abs_diff(NOW - 3_000_000) < 100, "{got}");
    }

    #[test]
    fn implausible_timestamps_fall_back_to_arrival() {
        assert_eq!(event_host_nanos(0, 125, 3, NOW), NOW);
        assert_eq!(event_host_nanos(NOW + 5_000_000_000, 1, 1, NOW), NOW);
        assert_eq!(event_host_nanos(u64::MAX, 125, 3, NOW), NOW);
        // Exactly at the bound is still believed.
        assert_eq!(
            event_host_nanos(NOW - PLAUSIBLE_NANOS, 1, 1, NOW),
            NOW - PLAUSIBLE_NANOS
        );
    }

    #[test]
    fn a_zero_denominator_reads_raw() {
        assert_eq!(event_host_nanos(NOW - 10, 125, 0, NOW), NOW - 10);
    }

    #[test]
    fn permission_reads_denied_only_after_a_request() {
        assert_eq!(permission(true, false), Permission::Granted);
        assert_eq!(permission(true, true), Permission::Granted);
        assert_eq!(permission(false, false), Permission::NotDetermined);
        assert_eq!(permission(false, true), Permission::Denied);
    }
}
