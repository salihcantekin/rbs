//! Desktop to picture mapping for pointer effects (ADR-0165 decision 5).
//!
//! A capture adapter that knows where its target sits on the desktop can publish a
//! [`PictureMapping`] with every frame, so a pointer filter can turn a desktop point
//! (from the input port) into a pixel of the captured picture. The work is opt-in
//! through a [`MappingSwitch`] the host owns: off by default, and an adapter does none
//! of it (no thread, no OS query) while the switch is off (P13). Contract: "Picture
//! mapping" in `docs/specs/capture-port.md`.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use rbs_core::media::Size;
use rbs_core::time::Timestamp;

/// Whether the pointer was over an uncovered part of a captured window.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cover {
    /// Not checked: displays, or an adapter that cannot tell.
    Unknown,
    /// At the adapter's last check the pointer was over the target and nothing covered it.
    Clear,
    /// At the adapter's last check another window covered the target under the pointer,
    /// or the pointer was outside the target.
    Covered,
}

/// How desktop points map to the pixels of one captured frame.
///
/// `picture = (desktop - origin) * scale`, in the picture's pixels, for the frame
/// stamped `frame_time`. Desktop units are those of the input port: Windows physical
/// pixels of the virtual screen, macOS global points.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PictureMapping {
    /// Time of the frame this mapping belongs to (its `VideoFrame` timestamp).
    pub frame_time: Timestamp,
    /// Desktop point of the picture's top-left pixel corner, `[x, y]`.
    pub origin: [f64; 2],
    /// Picture pixels per desktop unit, `[x, y]`.
    pub scale: [f64; 2],
    /// The picture's size in pixels.
    pub picture: Size,
    /// The covered test for window captures, as of the adapter's last check (it answers
    /// for the pointer position then, not for each click).
    pub cover: Cover,
}

impl PictureMapping {
    /// A mapping with [`Cover::Unknown`].
    pub fn new(frame_time: Timestamp, origin: [f64; 2], scale: [f64; 2], picture: Size) -> Self {
        Self {
            frame_time,
            origin,
            scale,
            picture,
            cover: Cover::Unknown,
        }
    }

    /// Sets the covered state.
    pub fn with_cover(mut self, cover: Cover) -> Self {
        self.cover = cover;
        self
    }

    /// The picture pixel of a desktop point, even outside the picture (an effect near
    /// the edge is still partly visible).
    pub fn to_picture(&self, desktop: [f64; 2]) -> [f64; 2] {
        [
            (desktop[0] - self.origin[0]) * self.scale[0],
            (desktop[1] - self.origin[1]) * self.scale[1],
        ]
    }

    /// Whether a desktop point falls inside the picture.
    pub fn contains(&self, desktop: [f64; 2]) -> bool {
        let [x, y] = self.to_picture(desktop);
        x >= 0.0
            && y >= 0.0
            && x < f64::from(self.picture.width)
            && y < f64::from(self.picture.height)
    }
}

type Callback = Box<dyn Fn(bool) + Send>;

struct Registration {
    id: u64,
    callback: Callback,
}

struct SwitchInner {
    on: AtomicBool,
    next_id: AtomicU64,
    registered: Mutex<Option<Registration>>,
}

impl SwitchInner {
    /// Control path only; never taken on a real-time thread.
    fn lock(&self) -> MutexGuard<'_, Option<Registration>> {
        self.registered
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// The host's switch for a source's picture mapping (ADR-0165 decision 5).
///
/// Owned by the host's source entry (one per entry, kept across reopened sessions) and
/// set on the control thread to "any pointer filter of this source is scheduled". The
/// session's adapter registers one callback; [`set`](Self::set) calls it on the
/// caller's thread, under the switch's lock, so the adapter starts or stops its mapping
/// work there. Clones share the switch.
#[derive(Clone)]
pub struct MappingSwitch {
    inner: Arc<SwitchInner>,
}

impl Default for MappingSwitch {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for MappingSwitch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MappingSwitch")
            .field("on", &self.is_on())
            .finish_non_exhaustive()
    }
}

impl MappingSwitch {
    /// A switch that is off.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(SwitchInner {
                on: AtomicBool::new(false),
                next_id: AtomicU64::new(1),
                registered: Mutex::new(None),
            }),
        }
    }

    /// Whether the switch is on. Any thread; one atomic load.
    pub fn is_on(&self) -> bool {
        self.inner.on.load(Ordering::Acquire)
    }

    /// Turns the switch on or off (control thread). A change calls the registered
    /// callback with the new state; setting the current state again does nothing.
    pub fn set(&self, on: bool) {
        let registered = self.inner.lock();
        if self.inner.on.swap(on, Ordering::AcqRel) == on {
            return;
        }
        if let Some(registration) = registered.as_ref() {
            (registration.callback)(on);
        }
    }

    /// Registers the adapter's callback (control path, for example in `open` or
    /// `start`) and calls it once with the current state, under the switch's lock, so
    /// no change between reading the state and registering is lost.
    ///
    /// The callback runs under the switch's lock, so it only signals: it stores an
    /// atomic, unparks or posts to the adapter's mapping thread. It never joins a
    /// thread, never waits on a lock the adapter holds while it drops a registration,
    /// never calls back into the switch and never panics; the adapter joins its mapping
    /// thread itself, after dropping the registration. It holds only weak references to
    /// adapter state.
    ///
    /// A later registration replaces this one and first calls this callback with
    /// `false`, so a replaced session stops its mapping work. Dropping the returned guard
    /// (the session does, on `stop` or drop) unregisters it.
    pub fn register(&self, callback: impl Fn(bool) + Send + 'static) -> MappingRegistration {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let mut registered = self.inner.lock();
        let old = registered.take();
        if let Some(old) = old.as_ref() {
            (old.callback)(false);
        }
        callback(self.is_on());
        *registered = Some(Registration {
            id,
            callback: Box::new(callback),
        });
        drop(registered);
        // Dropped after the lock is released, so whatever the old callback owns never
        // drops under it.
        drop(old);
        MappingRegistration {
            inner: Arc::clone(&self.inner),
            id,
        }
    }
}

/// Unregisters an adapter's callback from a [`MappingSwitch`] when dropped.
#[must_use = "dropping the registration unregisters the callback at once"]
pub struct MappingRegistration {
    inner: Arc<SwitchInner>,
    id: u64,
}

impl fmt::Debug for MappingRegistration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MappingRegistration")
            .field("id", &self.id)
            .finish()
    }
}

impl Drop for MappingRegistration {
    fn drop(&mut self) {
        let mut registered = self.inner.lock();
        let removed = if registered.as_ref().is_some_and(|r| r.id == self.id) {
            registered.take()
        } else {
            None
        };
        drop(registered);
        // The callback is dropped after the lock is released.
        drop(removed);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    #[test]
    fn mapping_converts_and_contains() {
        let m = PictureMapping::new(
            Timestamp::from_nanos(5),
            [-1920.0, 0.0],
            [2.0, 2.0],
            Size::new(3840, 2160),
        );
        assert_eq!(m.to_picture([-1920.0, 0.0]), [0.0, 0.0]);
        assert_eq!(m.to_picture([-960.0, 540.0]), [1920.0, 1080.0]);
        assert!(m.contains([-1.0, 1079.0]));
        assert!(!m.contains([0.0, 0.0]));
        assert!(!m.contains([-1921.0, 0.0]));
        assert_eq!(m.cover, Cover::Unknown);
        assert_eq!(m.with_cover(Cover::Covered).cover, Cover::Covered);
    }

    #[test]
    fn register_sees_the_current_state_and_later_changes() {
        let switch = MappingSwitch::new();
        switch.set(true);
        let (tx, rx) = mpsc::channel();
        let reg = switch.register(move |on| {
            let _ = tx.send(on);
        });
        assert_eq!(rx.try_recv(), Ok(true));
        switch.set(true);
        assert!(rx.try_recv().is_err(), "an unchanged state calls nothing");
        switch.set(false);
        assert_eq!(rx.try_recv(), Ok(false));
        drop(reg);
        switch.set(true);
        assert!(rx.try_recv().is_err(), "unregistered");
        assert!(switch.is_on());
    }

    #[test]
    fn a_replaced_registration_is_told_to_stop() {
        let switch = MappingSwitch::new();
        switch.set(true);
        let (tx, rx) = mpsc::channel();
        let _old = switch.register(move |on| {
            let _ = tx.send(on);
        });
        assert_eq!(rx.try_recv(), Ok(true));
        let _new = switch.register(|_| {});
        assert_eq!(rx.try_recv(), Ok(false));
        switch.set(false);
        assert!(rx.try_recv().is_err(), "the replaced callback is gone");
    }

    #[test]
    fn an_old_registration_does_not_remove_a_newer_one() {
        let switch = MappingSwitch::new();
        let old = switch.register(|_| {});
        let (tx, rx) = mpsc::channel();
        let _new = switch.register(move |on| {
            let _ = tx.send(on);
        });
        assert_eq!(rx.try_recv(), Ok(false));
        drop(old);
        switch.set(true);
        assert_eq!(rx.try_recv(), Ok(true));
    }

    #[test]
    fn flips_during_an_open_on_another_thread_are_not_lost() {
        // The session registers on its own thread while the control thread flips the
        // switch; the callback's last call always matches the final state.
        for _ in 0..200 {
            let switch = MappingSwitch::new();
            let state = Arc::new(Mutex::new(None));
            let opener = {
                let switch = switch.clone();
                let state = Arc::clone(&state);
                std::thread::spawn(move || {
                    switch.register(move |on| {
                        *state.lock().unwrap_or_else(PoisonError::into_inner) = Some(on);
                    })
                })
            };
            for i in 0..11 {
                switch.set(i % 2 == 0);
            }
            let reg = opener.join().expect("opener thread panicked");
            // No further `set`: the callback's last call must already match the state,
            // whichever came first, the registration or any of the flips.
            assert_eq!(
                *state.lock().unwrap_or_else(PoisonError::into_inner),
                Some(switch.is_on())
            );
            assert!(switch.is_on());
            drop(reg);
        }
    }
}
