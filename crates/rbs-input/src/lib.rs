//! The input port of RBS: what the user does with the mouse anywhere on the desktop
//! (ADR-0165; keys arrive in phase 2).
//!
//! Every OS input API is an adapter implementing [`InputBackend`]. The port owns the
//! shared [`InputMonitor`]: one OS listener per process, started by the first
//! [`InputMonitor::subscribe`] and stopped when the last [`Subscription`] is dropped.
//! Readers get events and the pointer history through an [`InputReader`], lock-free.
//! Contract: `docs/specs/input-port.md`.
//!
//! This crate is a port: it depends only on `rbs-core` and never on an OS API or
//! adapter (principle P1).

pub mod caps;
mod error;
mod event;
mod monitor;
mod ring;

pub use error::InputError;
pub use event::{
    Button, DesktopPoint, EventMask, InputEvent, Permission, PointerSample, TimedEvent,
};
pub use monitor::{
    InputBackend, InputMonitor, InputReader, InputSink, MAX_SUBSCRIPTIONS, RunningInput,
    Subscription,
};
pub use ring::{EVENT_RING, POINTER_HISTORY};
