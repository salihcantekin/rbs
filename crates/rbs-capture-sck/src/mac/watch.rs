//! The session's watcher thread (`rbs-sck-watch`): the cursor of window captures
//! (`crate::cursor`) and the target's desktop frame and covered test for the picture
//! mapping (ADR-0165 decision 5, `crate::geometry`).

use super::*;

/// The pointer in global display points, origin top left.
pub(super) fn pointer() -> Option<(f64, f64)> {
    let event = CGEvent::new(None)?;
    let point = CGEvent::location(Some(&event));
    Some((point.x, point.y))
}

/// Reads an `NSString` key of a CoreGraphics constant.
fn ns_key(key: &CFString) -> &NSString {
    // SAFETY: CFString is toll-free bridged to NSString.
    unsafe { &*(key as *const CFString).cast::<NSString>() }
}

/// The target window's frame and the on-screen windows stacked above it, from one
/// window-server query. The frame is `None` when the window is not on screen.
pub(super) fn window_list(window: u32) -> (Option<cursor::Rect>, Vec<Above>) {
    let options = CGWindowListOption::OptionOnScreenAboveWindow
        | CGWindowListOption::OptionIncludingWindow
        | CGWindowListOption::ExcludeDesktopElements;
    let Some(list) = CGWindowListCopyWindowInfo(options, window) else {
        return (None, Vec::new());
    };
    // SAFETY: CFArray is toll-free bridged to NSArray; the window list holds
    // dictionaries with string keys.
    let list = unsafe {
        &*(&*list as *const CFArray).cast::<NSArray<NSDictionary<NSString, AnyObject>>>()
    };
    // SAFETY: the keys are immutable framework constants.
    let (bounds_key, alpha_key, layer_key, number_key) = unsafe {
        (
            ns_key(kCGWindowBounds),
            ns_key(kCGWindowAlpha),
            ns_key(kCGWindowLayer),
            ns_key(kCGWindowNumber),
        )
    };
    let mut frame = None;
    let mut above = Vec::new();
    for info in list.iter() {
        let Some(bounds) = info
            .objectForKey(bounds_key)
            .and_then(|b| b.downcast::<NSDictionary>().ok())
        else {
            continue;
        };
        let mut rect = CGRect::default();
        // SAFETY: a dictionary of a CGRect (kCGWindowBounds), toll-free bridged; `rect`
        // is a valid out pointer.
        let ok = unsafe {
            let cf = &*(&*bounds as *const NSDictionary).cast::<CFDictionary>();
            CGRectMakeWithDictionaryRepresentation(Some(cf), &mut rect)
        };
        if !ok {
            continue;
        }
        let bounds = cursor::Rect {
            x: rect.origin.x,
            y: rect.origin.y,
            width: rect.size.width,
            height: rect.size.height,
        };
        let id = number(&info, number_key).map(|n| n.integerValue());
        if id == Some(window as isize) {
            frame = Some(bounds);
            continue;
        }
        above.push(Above {
            bounds,
            alpha: number(&info, alpha_key).map_or(1.0, |n| n.doubleValue()),
            layer: number(&info, layer_key).map_or(0, |n| n.integerValue() as i64),
        });
    }
    (frame, above)
}

/// Whether a window stacked above `window` is under the pointer now.
pub(super) fn cursor_covered(window: u32) -> bool {
    autoreleasepool(|_| {
        pointer().is_some_and(|(x, y)| cursor::covered(x, y, window_list(window).1))
    })
}

/// A display's frame in global points.
pub(super) fn display_frame(display: u32) -> cursor::Rect {
    let bounds = CGDisplayBounds(display);
    cursor::Rect {
        x: bounds.origin.x,
        y: bounds.origin.y,
        width: bounds.size.width,
        height: bounds.size.height,
    }
}

/// Where the target sits on the desktop, as of the watcher's last poll.
#[derive(Clone, Copy, Debug)]
pub(super) struct Geometry {
    /// The display's or window's frame in global points.
    pub(super) target: cursor::Rect,
    pub(super) cover: Cover,
}

/// State of the session's watcher thread (`rbs-sck-watch`).
///
/// The thread runs only while the session is Active and has a job: drawing the cursor
/// of a window capture only where nothing covers it (`crate::cursor`), or reading the
/// target's desktop frame and the covered test for the picture mapping while the
/// switch is on (P13). It ends itself within one poll when no job is left, and `stop`
/// joins it.
pub(super) struct Watch {
    pub(super) target: Parsed,
    pub(super) config: Arc<Mutex<Config>>,
    /// The Active stream, for the cursor's reconfiguration.
    pub(super) stream: Mutex<Option<Sendable<Retained<SCStream>>>>,
    pub(super) active: AtomicBool,
    /// A window capture that draws the cursor.
    pub(super) cursor: AtomicBool,
    /// The mapping switch is on.
    pub(super) mapping: AtomicBool,
    /// A thread runs the loop. Claimed with a compare-exchange by whoever starts one.
    pub(super) running: AtomicBool,
    /// Control path only (start, stop, reconfigure, the switch callback); never held
    /// while the session drops its mapping registration.
    pub(super) threads: Mutex<Vec<JoinHandle<()>>>,
    /// Read by the sample handler with `try_lock`.
    pub(super) geometry: Mutex<Option<Geometry>>,
}

impl Watch {
    pub(super) fn new(target: Parsed, config: Arc<Mutex<Config>>) -> Self {
        Self {
            target,
            config,
            stream: Mutex::new(None),
            active: AtomicBool::new(false),
            cursor: AtomicBool::new(false),
            mapping: AtomicBool::new(false),
            running: AtomicBool::new(false),
            threads: Mutex::new(Vec::new()),
            geometry: Mutex::new(None),
        }
    }

    fn needed(&self) -> bool {
        self.active.load(Ordering::SeqCst)
            && (self.cursor.load(Ordering::SeqCst) || self.mapping.load(Ordering::SeqCst))
    }

    /// Starts the thread if it has a job and none runs (control path), and joins
    /// threads that already ended on their own (joining them does not wait). If the
    /// thread cannot start, the cursor is drawn as ScreenCaptureKit does and frames
    /// carry no mapping.
    pub(super) fn ensure(self: &Arc<Self>) {
        let done: Vec<_> = {
            let mut threads = lock(&self.threads);
            let (done, running): (Vec<_>, Vec<_>) =
                threads.drain(..).partition(JoinHandle::is_finished);
            *threads = running;
            done
        };
        for thread in done {
            let _ = thread.join();
        }
        self.spawn_if_needed();
    }

    /// [`ensure`](Self::ensure) for the mapping switch's callback, which runs under
    /// the switch's lock: it only spawns, never joins or waits (capture-port.md,
    /// picture mapping rule 2). Ended threads are joined by the next `ensure` or by
    /// `stop`.
    pub(super) fn signal(self: &Arc<Self>) {
        self.spawn_if_needed();
    }

    fn spawn_if_needed(self: &Arc<Self>) {
        if !self.needed()
            || self
                .running
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
        {
            return;
        }
        let watch = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("rbs-sck-watch".into())
            .spawn(move || watch.run());
        match spawned {
            Ok(thread) => lock(&self.threads).push(thread),
            Err(_) => self.running.store(false, Ordering::SeqCst),
        }
    }

    /// Ends every watcher thread and waits for them (control path).
    pub(super) fn stop(&self) {
        self.active.store(false, Ordering::SeqCst);
        *lock(&self.stream) = None;
        let threads = std::mem::take(&mut *lock(&self.threads));
        for thread in threads {
            thread.thread().unpark();
            let _ = thread.join();
        }
        *lock(&self.geometry) = None;
    }

    fn run(self: Arc<Self>) {
        // A panic in a poll (an OS call) must not leave `running` claimed, or no
        // watcher would start again in this session.
        struct Unclaim<'a>(&'a AtomicBool);
        impl Drop for Unclaim<'_> {
            fn drop(&mut self) {
                if std::thread::panicking() {
                    self.0.store(false, Ordering::SeqCst);
                }
            }
        }
        let _unclaim = Unclaim(&self.running);
        let mut last_point = None;
        let mut last_query = None;
        loop {
            if !self.needed() {
                *lock(&self.geometry) = None;
                self.running.store(false, Ordering::SeqCst);
                // A job given between the check and the store is taken by this thread
                // if no other one claimed it.
                if !self.needed()
                    || self
                        .running
                        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                        .is_err()
                {
                    return;
                }
            }
            autoreleasepool(|_| self.poll(&mut last_point, &mut last_query));
            std::thread::park_timeout(cursor::POLL);
        }
    }

    fn poll(&self, last_point: &mut Option<(f64, f64)>, last_query: &mut Option<Instant>) {
        let now = Instant::now();
        // A display's cover is always `Unknown`: no pointer read for it.
        let point = match self.target {
            Parsed::Window(_) => pointer(),
            Parsed::Display(_) => None,
        };
        let moved = point != *last_point;
        *last_point = point;
        let mapping = self.mapping.load(Ordering::SeqCst);
        let geometry = match self.target {
            Parsed::Display(id) => mapping.then(|| Geometry {
                target: display_frame(id),
                cover: Cover::Unknown,
            }),
            Parsed::Window(id) => {
                let cursor = self.cursor.load(Ordering::SeqCst);
                // The mapping follows the window's frame on every poll; the cursor alone
                // skips the query while the pointer is still and the last one is young.
                if !mapping && !(cursor && cursor::should_query(moved, *last_query, now)) {
                    return;
                }
                *last_query = Some(now);
                let (frame, above) = window_list(id);
                if cursor {
                    let covered =
                        point.is_some_and(|(x, y)| cursor::covered(x, y, above.iter().copied()));
                    let mut config = lock(&self.config);
                    if config.cursor && config.cursor_covered != covered {
                        config.cursor_covered = covered;
                        if let Some(stream) = lock(&self.stream).as_ref() {
                            send_configuration(&stream.0, &config);
                        }
                    }
                }
                match frame {
                    Some(frame) if mapping => Some(Geometry {
                        target: frame,
                        cover: geometry::window_cover(point, frame, above),
                    }),
                    _ => None,
                }
            }
        };
        *lock(&self.geometry) = geometry;
    }
}

/// Picture mapping on real hardware (ADR-0165 decision 5). Gated by `RBS_CAPTURE_HW=1`
/// on a Mac with screen recording permission; ScreenCaptureKit only delivers frames
/// when content changes, so keep something moving on screen.
#[cfg(test)]
mod mapping_tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::super::*;
    use super::*;
    use rbs_capture::GpuImportTarget;
    use rbs_core::time::MonotonicClock;

    fn hardware() -> bool {
        if std::env::var_os("RBS_CAPTURE_HW").is_none() || !permission_granted() {
            eprintln!("skipped: set RBS_CAPTURE_HW=1 on a Mac with screen recording permission");
            return false;
        }
        true
    }

    /// Opens `id` with the switch on and returns the first frame's size and mapping.
    fn first_mapping(id: TargetId) -> (Size, PictureMapping, Box<dyn CaptureSession>) {
        let switch = MappingSwitch::new();
        switch.set(true);
        let gpu = GpuImportTarget::new(GpuDeviceId::MetalRegistryId(0), &[SyncKind::Ready]);
        let request = CaptureRequest::new(id.clone(), Arc::new(MonotonicClock::new()))
            .with_pixel_formats(&[PixelFormat::Bgra8])
            .with_memory(&[MemoryKind::Metal])
            .with_gpu(gpu)
            .with_mapping(switch);
        let mut session = SckCaptureFactory::new().open(&request).expect("open");
        session.start().expect("start");
        // A hang guard, not a timing assumption: wait for frames, at most 5 s.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Some(frame) = session.latest_frame().expect("frame") {
                if let Some(mapping) = session.latest_mapping() {
                    assert_eq!(mapping.frame_time, frame.timestamp());
                    return (session.format().size, mapping, session);
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("{id}: no mapped frame within 5 s");
    }

    fn near(a: [f64; 2], b: [f64; 2], tolerance: f64) -> bool {
        (a[0] - b[0]).abs() <= tolerance && (a[1] - b[1]).abs() <= tolerance
    }

    #[test]
    fn display_centre_maps_to_the_picture_centre() {
        if !hardware() {
            return;
        }
        let id = CGMainDisplayID();
        let (size, mapping, _session) =
            first_mapping(TargetId::new(format!("{DISPLAY_PREFIX}{id}")));
        let frame = display_frame(id);
        let centre = [frame.x + frame.width / 2.0, frame.y + frame.height / 2.0];
        let picture = mapping.to_picture(centre);
        let expected = [f64::from(size.width) / 2.0, f64::from(size.height) / 2.0];
        eprintln!(
            "display {id}: frame {frame:?}, picture {size:?}, mapping {mapping:?}, centre -> {picture:?}"
        );
        assert_eq!(mapping.cover, Cover::Unknown);
        assert!(
            near(picture, expected, 1.0),
            "centre {picture:?}, expected {expected:?}"
        );
    }

    #[test]
    fn window_frame_corners_map_to_the_picture_corners() {
        if !hardware() {
            return;
        }
        let windows = SckCaptureFactory::new()
            .enumerate(TargetKind::Window)
            .expect("windows");
        let target = windows.first().expect("an on-screen window");
        let Some(Parsed::Window(number)) = parse(&target.id) else {
            panic!("not a window id: {}", target.id);
        };
        let (size, mapping, _session) = first_mapping(target.id.clone());
        let frame = window_list(number).0.expect("window frame");
        let top_left = mapping.to_picture([frame.x, frame.y]);
        let bottom_right = mapping.to_picture([frame.x + frame.width, frame.y + frame.height]);
        let corner = [f64::from(size.width), f64::from(size.height)];
        eprintln!(
            "window {} ({:?}): frame {frame:?}, picture {size:?}, mapping {mapping:?}, corners -> {top_left:?} {bottom_right:?}",
            target.id, target.name
        );
        assert!(near(top_left, [0.0, 0.0], 2.0), "top left {top_left:?}");
        assert!(
            near(bottom_right, corner, 2.0),
            "bottom right {bottom_right:?}, picture {corner:?}"
        );
        // The window's pixels per point match its display's (no fit scaling).
        let scale = window_scale_of(frame);
        assert!(
            (mapping.scale[0] - scale).abs() < 0.01 && (mapping.scale[1] - scale).abs() < 0.01,
            "scale {:?}, display {scale}",
            mapping.scale
        );
    }

    /// Pixels per point of the display that holds the centre of `frame`.
    fn window_scale_of(frame: cursor::Rect) -> f64 {
        let content = shareable_content().expect("content");
        let (cx, cy) = (frame.x + frame.width / 2.0, frame.y + frame.height / 2.0);
        // SAFETY: plain property reads of immutable shareable-content objects.
        unsafe {
            content
                .displays()
                .iter()
                .find(|d| {
                    let r = d.frame();
                    cx >= r.origin.x
                        && cx < r.origin.x + r.size.width
                        && cy >= r.origin.y
                        && cy < r.origin.y + r.size.height
                })
                .map_or(1.0, |d| display_scale(&d))
        }
    }
}
