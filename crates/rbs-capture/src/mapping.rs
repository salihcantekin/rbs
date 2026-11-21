//! Desktop to picture mapping for pointer effects (ADR-0165 decision 5).
//!
//! A capture adapter that knows where its target sits on the desktop can publish a
//! [`PictureMapping`] with every frame, so a pointer filter can turn a desktop point
//! (from the input port) into a pixel of the captured picture. The work is opt-in
//! through a [`MappingSwitch`] the host owns: off by default, and an adapter does none
//! of it (no thread, no OS query) while the switch is off (P13). Contract: "Picture
//! mapping" in `docs/specs/capture-port.md`.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering, fence};
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

/// Reads of a [`MappingCell`] that see a write in progress try again at most this
/// many times, then report no mapping for that read.
const CELL_READ_TRIES: usize = 4;

const COVER_NONE: u64 = 0;
const COVER_UNKNOWN: u64 = 1;
const COVER_CLEAR: u64 = 2;
const COVER_COVERED: u64 = 3;

/// The newest [`PictureMapping`] of one source, lent from the capture source node to
/// the pointer filters of that source's chain (ADR-0165 decision 5).
///
/// Owned by the host's source entry beside its [`MappingSwitch`], so a rebuilt capture
/// node keeps writing the cell the filters read. Lock-free: [`publish`](Self::publish)
/// and [`get`](Self::get) never wait, allocate or lock, so both run on real-time
/// threads. One writer at a time is the rule (the scheduled capture node); a publish
/// that finds another publish in progress is skipped, never torn. Clones share the
/// cell.
#[derive(Clone, Default)]
pub struct MappingCell {
    inner: Arc<CellInner>,
}

#[derive(Default)]
struct CellInner {
    /// Even while stable, odd while a publish is in progress.
    seq: AtomicU64,
    /// `COVER_NONE` for no mapping, else the cover plus one.
    cover: AtomicU64,
    time: AtomicU64,
    origin: [AtomicU64; 2],
    scale: [AtomicU64; 2],
    /// Width in the high 32 bits, height in the low.
    picture: AtomicU64,
}

impl fmt::Debug for MappingCell {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MappingCell")
            .field("mapping", &self.get())
            .finish()
    }
}

impl MappingCell {
    /// An empty cell.
    pub fn new() -> Self {
        Self::default()
    }

    /// Stores the newest frame's mapping, or `None` (writer thread; real-time).
    /// Skipped if another publish is in progress.
    pub fn publish(&self, mapping: Option<PictureMapping>) {
        let inner = &*self.inner;
        let seq = inner.seq.load(Ordering::Relaxed);
        if seq & 1 == 1
            || inner
                .seq
                .compare_exchange(seq, seq + 1, Ordering::Relaxed, Ordering::Relaxed)
                .is_err()
        {
            return;
        }
        fence(Ordering::Release);
        match mapping {
            None => inner.cover.store(COVER_NONE, Ordering::Relaxed),
            Some(m) => {
                let cover = match m.cover {
                    Cover::Clear => COVER_CLEAR,
                    Cover::Covered => COVER_COVERED,
                    Cover::Unknown => COVER_UNKNOWN,
                };
                inner.cover.store(cover, Ordering::Relaxed);
                inner.time.store(m.frame_time.as_nanos(), Ordering::Relaxed);
                for i in 0..2 {
                    inner.origin[i].store(m.origin[i].to_bits(), Ordering::Relaxed);
                    inner.scale[i].store(m.scale[i].to_bits(), Ordering::Relaxed);
                }
                inner.picture.store(
                    u64::from(m.picture.width) << 32 | u64::from(m.picture.height),
                    Ordering::Relaxed,
                );
            }
        }
        inner.seq.store(seq + 2, Ordering::Release);
    }

    /// The newest published mapping (any thread; real-time). `None` when none was
    /// published, the last publish was `None`, or a publish kept the read from a
    /// stable copy [`CELL_READ_TRIES`] times in a row.
    pub fn get(&self) -> Option<PictureMapping> {
        let inner = &*self.inner;
        for _ in 0..CELL_READ_TRIES {
            let before = inner.seq.load(Ordering::Acquire);
            if before & 1 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let cover = inner.cover.load(Ordering::Relaxed);
            let time = inner.time.load(Ordering::Relaxed);
            let origin = [0, 1].map(|i| f64::from_bits(inner.origin[i].load(Ordering::Relaxed)));
            let scale = [0, 1].map(|i| f64::from_bits(inner.scale[i].load(Ordering::Relaxed)));
            let picture = inner.picture.load(Ordering::Relaxed);
            fence(Ordering::Acquire);
            if inner.seq.load(Ordering::Relaxed) != before {
                continue;
            }
            let cover = match cover {
                COVER_NONE => return None,
                COVER_CLEAR => Cover::Clear,
                COVER_COVERED => Cover::Covered,
                _ => Cover::Unknown,
            };
            // Truncation intended: the two halves were packed from two `u32`s.
            #[allow(clippy::cast_possible_truncation)]
            let size = Size::new((picture >> 32) as u32, picture as u32);
            return Some(
                PictureMapping::new(Timestamp::from_nanos(time), origin, scale, size)
                    .with_cover(cover),
            );
        }
        None
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
    #[test]
    fn a_cell_keeps_the_newest_publish() {
        let cell = MappingCell::new();
        assert_eq!(cell.get(), None);
        let m = PictureMapping::new(
            Timestamp::from_nanos(7),
            [-1920.5, 12.0],
            [2.0, 1.5],
            Size::new(3840, 2160),
        )
        .with_cover(Cover::Covered);
        cell.clone().publish(Some(m));
        assert_eq!(cell.get(), Some(m));
        cell.publish(Some(m.with_cover(Cover::Clear)));
        assert_eq!(cell.get().map(|m| m.cover), Some(Cover::Clear));
        cell.publish(None);
        assert_eq!(cell.get(), None);
    }

    #[test]
    fn a_cell_read_racing_a_writer_is_never_torn() {
        // Every published mapping has origin x == time and scale x == time + 1, so a
        // torn read shows as a mismatch. Progress-bounded (ADR-0032): the reader
        // stops after enough stable reads, the writer when told.
        let cell = MappingCell::new();
        let stop = Arc::new(AtomicBool::new(false));
        let writer = {
            let (cell, stop) = (cell.clone(), Arc::clone(&stop));
            std::thread::spawn(move || {
                let mut n = 1u64;
                while !stop.load(Ordering::Acquire) {
                    #[allow(clippy::cast_precision_loss)]
                    let f = n as f64;
                    cell.publish(Some(PictureMapping::new(
                        Timestamp::from_nanos(n),
                        [f, f],
                        [f + 1.0, 1.0],
                        Size::new(1, 1),
                    )));
                    n += 1;
                    std::thread::yield_now();
                }
            })
        };
        let mut seen = 0;
        let mut last = 0;
        while seen < 2000 {
            if let Some(m) = cell.get() {
                let t = m.frame_time.as_nanos();
                #[allow(clippy::cast_precision_loss)]
                let f = t as f64;
                assert_eq!(m.origin, [f, f], "torn read at {t}");
                assert_eq!(m.scale[0], f + 1.0, "torn read at {t}");
                assert!(t >= last, "went back from {last} to {t}");
                last = t;
                seen += 1;
            }
            std::thread::yield_now();
        }
        stop.store(true, Ordering::Release);
        writer.join().expect("writer thread panicked");
    }
    #[test]
    fn the_per_frame_calls_do_not_allocate() {
        let switch = MappingSwitch::new();
        let cell = MappingCell::new();
        let m = PictureMapping::new(
            Timestamp::from_nanos(1),
            [0.0; 2],
            [1.0; 2],
            Size::new(8, 8),
        );
        let info = allocation_counter::measure(|| {
            for _ in 0..100 {
                std::hint::black_box(switch.is_on());
                cell.publish(Some(m));
                std::hint::black_box(cell.get());
            }
            cell.publish(None);
        });
        assert_eq!(info.count_total, 0);
    }
}
