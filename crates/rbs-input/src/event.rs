//! Event types of the input port.

use rbs_core::time::Timestamp;

/// A point in the OS desktop space.
///
/// Windows: physical pixels of the virtual screen (the process is per-monitor DPI aware
/// v2), negative left of or above the primary monitor. macOS: points in the global
/// display space, origin at the top left of the main display. The capture port maps it
/// to a captured picture (ADR-0165 decision 5).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct DesktopPoint {
    /// Horizontal, growing to the right.
    pub x: f64,
    /// Vertical, growing downwards.
    pub y: f64,
}

impl DesktopPoint {
    /// A point at `(x, y)`.
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

/// A mouse button.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Button {
    /// The primary button.
    Left,
    /// The secondary button.
    Right,
    /// The wheel button.
    Middle,
    /// The first side button (back).
    Back,
    /// The second side button (forward).
    Forward,
}

impl Button {
    /// Every button known to this version, in a fixed order.
    pub const ALL: [Button; 5] = [
        Button::Left,
        Button::Right,
        Button::Middle,
        Button::Back,
        Button::Forward,
    ];

    pub(crate) const fn index(self) -> u8 {
        match self {
            Button::Left => 0,
            Button::Right => 1,
            Button::Middle => 2,
            Button::Back => 3,
            Button::Forward => 4,
        }
    }

    pub(crate) const fn from_index(index: u8) -> Option<Self> {
        match index {
            0 => Some(Button::Left),
            1 => Some(Button::Right),
            2 => Some(Button::Middle),
            3 => Some(Button::Back),
            4 => Some(Button::Forward),
            _ => None,
        }
    }
}

/// A discrete input event. Pointer moves are not events; they go to the pointer history.
///
/// Non-exhaustive: keys are added in ADR-0165 phase 2.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InputEvent {
    /// A button was pressed at a point.
    ButtonDown {
        /// Which button.
        button: Button,
        /// Where the pointer was.
        at: DesktopPoint,
    },
    /// A button was released at a point.
    ButtonUp {
        /// Which button.
        button: Button,
        /// Where the pointer was.
        at: DesktopPoint,
    },
    /// The wheel turned.
    Wheel {
        /// Where the pointer was.
        at: DesktopPoint,
        /// Horizontal notches, positive to the right.
        dx: f32,
        /// Vertical notches, positive away from the user.
        dy: f32,
    },
}

impl InputEvent {
    /// The mask class this event belongs to.
    pub const fn class(&self) -> EventMask {
        match self {
            InputEvent::ButtonDown { .. } | InputEvent::ButtonUp { .. } => EventMask::BUTTONS,
            InputEvent::Wheel { .. } => EventMask::WHEEL,
        }
    }
}

/// An event with its master time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TimedEvent {
    /// When the OS saw the event, in master time.
    pub time: Timestamp,
    /// The event.
    pub event: InputEvent,
}

/// One pointer position with its master time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PointerSample {
    /// When the pointer was there, in master time.
    pub time: Timestamp,
    /// Where it was.
    pub at: DesktopPoint,
}

/// Whether the OS lets this process read input (ADR-0165 decision 7, ADR-0141).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    /// Input may be read (or the OS has no permission for it).
    Granted,
    /// The user or a policy refused it; only the OS settings can change it.
    Denied,
    /// The user has not been asked yet; [`InputMonitor::request_permission`] asks.
    ///
    /// [`InputMonitor::request_permission`]: crate::InputMonitor::request_permission
    NotDetermined,
    /// An administrator forbids it and the user cannot change it.
    Restricted,
    /// The OS has a permission but no way to read it before use.
    Unknown,
}

/// Which event classes a subscription wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct EventMask(u8);

impl EventMask {
    /// No class.
    pub const NONE: EventMask = EventMask(0);
    /// The pointer history.
    pub const POINTER: EventMask = EventMask(1);
    /// Button down and up.
    pub const BUTTONS: EventMask = EventMask(1 << 1);
    /// Wheel.
    pub const WHEEL: EventMask = EventMask(1 << 2);
    /// Keys. Named now, refused until ADR-0165 phase 2.
    pub const KEYS: EventMask = EventMask(1 << 3);
    /// Every mouse class.
    pub const MOUSE: EventMask = EventMask(1 | (1 << 1) | (1 << 2));

    /// Both masks together.
    pub const fn union(self, other: EventMask) -> EventMask {
        EventMask(self.0 | other.0)
    }

    /// The classes of `self` not in `other`.
    pub const fn without(self, other: EventMask) -> EventMask {
        EventMask(self.0 & !other.0)
    }

    /// Whether every class of `other` is in `self`.
    pub const fn contains(self, other: EventMask) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether some class of `other` is in `self`.
    pub const fn intersects(self, other: EventMask) -> bool {
        self.0 & other.0 != 0
    }

    /// Whether the mask has no class.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub(crate) const fn bits(self) -> u8 {
        self.0
    }
}

impl std::ops::BitOr for EventMask {
    type Output = EventMask;
    fn bitor(self, rhs: EventMask) -> EventMask {
        self.union(rhs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_algebra() {
        let m = EventMask::BUTTONS | EventMask::WHEEL;
        assert!(m.contains(EventMask::BUTTONS));
        assert!(!m.contains(EventMask::POINTER));
        assert!(m.intersects(EventMask::MOUSE));
        assert!(!m.intersects(EventMask::KEYS));
        assert_eq!(m.without(EventMask::WHEEL), EventMask::BUTTONS);
        assert!(EventMask::NONE.is_empty());
        assert!(EventMask::MOUSE.contains(m | EventMask::POINTER));
    }

    #[test]
    fn button_index_round_trips() {
        for b in Button::ALL {
            assert_eq!(Button::from_index(b.index()), Some(b));
        }
        assert_eq!(Button::from_index(9), None);
    }

    #[test]
    fn event_classes() {
        let at = DesktopPoint::new(1.0, 2.0);
        let down = InputEvent::ButtonDown {
            button: Button::Left,
            at,
        };
        assert_eq!(down.class(), EventMask::BUTTONS);
        let wheel = InputEvent::Wheel {
            at,
            dx: 0.0,
            dy: 1.0,
        };
        assert_eq!(wheel.class(), EventMask::WHEEL);
    }
}
