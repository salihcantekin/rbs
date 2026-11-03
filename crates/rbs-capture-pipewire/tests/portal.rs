//! Screen and window capture through the desktop portal, against a test portal.
//!
//! The test starts a private session bus and a child process (this binary,
//! `mock_portal_process`) that owns `org.freedesktop.portal.Desktop` on it, serves the
//! ScreenCast interface like xdg-desktop-portal does (requests answered by `Response`
//! signals, single-use restore tokens, `OpenPipeWireRemote` handing out a daemon
//! connection) and publishes the PipeWire video node its casts point at. The adapter
//! then picks, restores, captures and loses targets as it would on a desktop, and the
//! capture conformance suite runs against picked displays.
//!
//! Needs a PipeWire daemon with WirePlumber and the `dbus-daemon` binary; without
//! them the test is skipped unless `RBS_REQUIRE_PIPEWIRE` is set (CI sets it). The
//! file holds one test that runs by default, so the process thread count is exact; it
//! runs the real checks in a child (`portal_checks`) that sees the private bus.

/// Environment variable naming the private bus for `portal_checks`.
const INNER: &str = "RBS_PORTAL_TEST_INNER";

#[test]
fn captures_through_the_portal() {
    #[cfg(target_os = "linux")]
    linux::run_outer();
}

/// The checks, run by `captures_through_the_portal` in a child process.
#[test]
#[ignore = "started by captures_through_the_portal with a private session bus"]
fn portal_checks() {
    #[cfg(target_os = "linux")]
    if std::env::var_os(INNER).is_some() {
        linux::run_checks();
    }
}

/// The test portal, started by `portal_checks`; does nothing on its own.
#[test]
#[ignore = "started by portal_checks as the test portal"]
fn mock_portal_process() {
    #[cfg(target_os = "linux")]
    if std::env::var_os(mock::SERVE).is_some() {
        mock::serve();
    }
}

#[cfg(target_os = "linux")]
#[path = "common/source.rs"]
mod source;

#[cfg(target_os = "linux")]
mod linux {
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Child, ChildStdin, Command, Stdio};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use rbs_capture::conformance::{self, Options, RealTimePacer, TestHooks};
    use rbs_capture::{
        CaptureError, CaptureFactory, CaptureRequest, CaptureSession, CaptureTarget, TargetId,
        TargetKind, caps,
    };
    use rbs_capture_pipewire::PipeWireCaptureFactory;
    use rbs_core::capability::{CapabilitySet, CapabilityValue};
    use rbs_core::media::{MemoryKind, PixelFormat};
    use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};
    use rbs_core::time::MonotonicClock;

    use super::{INNER, mock};

    /// Longest run of the checks (they take about 1.5 s).
    const WATCHDOG: Duration = Duration::from_secs(30);

    fn required() -> bool {
        std::env::var_os("RBS_REQUIRE_PIPEWIRE").is_some_and(|v| !v.is_empty())
    }

    fn exe() -> std::path::PathBuf {
        match std::env::current_exe() {
            Ok(exe) => exe,
            Err(e) => panic!("current_exe: {e}"),
        }
    }

    /// Starts a private session bus and runs the checks in a child that uses it.
    pub fn run_outer() {
        if !rbs_capture_pipewire::daemon_present() {
            assert!(!required(), "RBS_REQUIRE_PIPEWIRE is set but no daemon");
            eprintln!("no PipeWire daemon: portal capture not checked");
            return;
        }
        let bus = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let mut bus = match bus {
            Ok(bus) => bus,
            Err(e) if !required() => {
                eprintln!("no dbus-daemon ({e}): portal capture not checked");
                return;
            }
            Err(e) => panic!("dbus-daemon: {e}"),
        };
        let address = bus.stdout.take().and_then(|out| {
            let mut line = String::new();
            BufReader::new(out).read_line(&mut line).ok()?;
            Some(line.trim().to_owned())
        });
        let Some(address) = address.filter(|a| !a.is_empty()) else {
            let _ = bus.kill();
            panic!("dbus-daemon printed no address");
        };
        let checks = Command::new(exe())
            .args(["--exact", "portal_checks", "--ignored", "--nocapture"])
            .arg("--test-threads=1")
            .env("DBUS_SESSION_BUS_ADDRESS", &address)
            .env(INNER, "1")
            .spawn();
        let status = checks.map(|mut checks| {
            // Every call in the checks has its own limit; this guard turns a hang
            // nobody foresaw into a failure instead of a stuck CI job.
            let deadline = Instant::now() + WATCHDOG;
            loop {
                match checks.try_wait() {
                    Ok(Some(status)) => return Some(status),
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    _ => {
                        let _ = checks.kill();
                        let _ = checks.wait();
                        return None;
                    }
                }
            }
        });
        let _ = bus.kill();
        let _ = bus.wait();
        match status {
            Ok(Some(status)) => assert!(status.success(), "portal checks failed: {status}"),
            Ok(None) => panic!("portal checks did not finish within {WATCHDOG:?}"),
            Err(e) => panic!("run the portal checks: {e}"),
        }
    }

    /// The running test portal. Dropping it ends the process.
    struct Portal {
        child: Mutex<Option<Child>>,
        stdin: Mutex<Option<ChildStdin>>,
        lines: Mutex<std::io::Lines<BufReader<std::process::ChildStdout>>>,
    }

    impl Portal {
        fn start() -> Arc<Portal> {
            let child = Command::new(exe())
                .args(["--exact", "mock_portal_process", "--ignored", "--nocapture"])
                .arg("--test-threads=1")
                .env(mock::SERVE, "1")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn();
            let mut child = match child {
                Ok(child) => child,
                Err(e) => panic!("spawn the test portal: {e}"),
            };
            let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
                panic!("no pipes to the test portal");
            };
            let portal = Arc::new(Portal {
                child: Mutex::new(Some(child)),
                stdin: Mutex::new(Some(stdin)),
                lines: Mutex::new(BufReader::new(stdout).lines()),
            });
            assert_eq!(portal.reply(), "ready", "test portal did not start");
            portal
        }

        /// The next `mock:` line of the portal.
        fn reply(&self) -> String {
            let Ok(mut lines) = self.lines.lock() else {
                panic!("poisoned")
            };
            for line in lines.by_ref() {
                match line {
                    Ok(line) => {
                        // The test harness may print `test ... ` before it on one line.
                        if let Some((_, rest)) = line.trim_end().split_once("mock: ") {
                            return rest.to_owned();
                        }
                    }
                    Err(_) => break,
                }
            }
            String::new()
        }

        fn send(&self, command: &str) -> bool {
            let Ok(mut stdin) = self.stdin.lock() else {
                return false;
            };
            let Some(stdin) = stdin.as_mut() else {
                return false;
            };
            writeln!(stdin, "{command}")
                .and_then(|()| stdin.flush())
                .is_ok()
        }

        /// Sends a command the portal acknowledges, and returns the acknowledgement.
        fn ask(&self, command: &str) -> String {
            assert!(self.send(command), "{command}: not sent");
            self.reply()
        }

        /// `(open sessions, closed requests, Start calls)`.
        fn counts(&self) -> (u64, u64, u64) {
            let reply = self.ask("count");
            let n: Vec<u64> = reply
                .split_whitespace()
                .filter_map(|w| w.parse().ok())
                .collect();
            match n.as_slice() {
                [sessions, closed, starts] => (*sessions, *closed, *starts),
                _ => panic!("bad count reply: {reply}"),
            }
        }

        /// Removes the cast's node, as a compositor does when a window closes.
        fn remove(&self) -> bool {
            self.ask("remove") == "removed"
        }
    }

    impl Drop for Portal {
        fn drop(&mut self) {
            if let Ok(mut stdin) = self.stdin.lock() {
                drop(stdin.take());
            }
            if let Some(mut child) = self.child.lock().ok().and_then(|mut c| c.take()) {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    fn request(id: &TargetId) -> CaptureRequest {
        CaptureRequest::new(id.clone(), Arc::new(MonotonicClock::new()))
            .with_pixel_formats(&[PixelFormat::Bgra8])
            .with_memory(&[MemoryKind::System])
            .with_cursor(true)
    }

    fn pick(factory: &PipeWireCaptureFactory, kind: TargetKind) -> CaptureTarget {
        match factory.pick(kind) {
            Ok(target) => target,
            Err(e) => panic!("pick {kind}: {e}"),
        }
    }

    fn started(factory: &PipeWireCaptureFactory, id: &TargetId) -> Box<dyn CaptureSession> {
        let mut session = match factory.open(&request(id)) {
            Ok(session) => session,
            Err(e) => panic!("open {id}: {e}"),
        };
        if let Err(e) = session.start() {
            panic!("start {id}: {e}");
        }
        session
    }

    fn first_frame(session: &mut dyn CaptureSession) -> (u32, u32) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match session.latest_frame() {
                Ok(Some(frame)) => {
                    let format = frame.format();
                    assert_eq!(format.pixel_format, PixelFormat::Bgra8);
                    return (format.size.width, format.size.height);
                }
                Ok(None) if Instant::now() < deadline => std::thread::yield_now(),
                Ok(None) => panic!("no frame within 3 s"),
                Err(e) => panic!("latest_frame: {e}"),
            }
        }
    }

    pub fn run_checks() {
        let portal = Portal::start();
        let factory = PipeWireCaptureFactory::new();

        // The adapter declares the picker kinds once a session bus exists.
        let caps = match factory.probe() {
            Ok(caps) => caps,
            Err(e) => panic!("probe: {e}"),
        };
        let picker = caps.get(&caps::TARGET_PICKER);
        let expected = CapabilityValue::Set(vec!["display".into(), "window".into()]);
        assert_eq!(picker, Some(&expected));
        assert!(matches!(
            factory.enumerate(TargetKind::Display),
            Err(CaptureError::Unsupported(_))
        ));
        assert!(matches!(
            factory.pick(TargetKind::Camera),
            Err(CaptureError::Unsupported(_))
        ));

        restores_with_rotating_tokens(&portal, &factory);
        reports_the_users_answer(&portal, &factory);
        keeps_a_pick_without_token_for_one_start(&portal, &factory);
        refuses_a_used_token_without_waiting_for_the_user(&portal);
        refuses_rgba(&factory);
        passes_conformance(&portal);
    }

    /// Pick, start, stop and start again: each start restores with the newest token and
    /// shows no dialog; the session names the id to store.
    fn restores_with_rotating_tokens(portal: &Portal, factory: &PipeWireCaptureFactory) {
        for kind in [TargetKind::Display, TargetKind::Window] {
            let target = pick(factory, kind);
            assert_eq!(target.kind, kind);
            assert_eq!(target.size, Some((320, 240)));
            let prefix = format!("portal:{}:", kind.as_str());
            assert!(target.id.as_str().starts_with(&prefix), "{}", target.id);
            // The pick's own cast is closed: nothing runs in Standby.
            assert_eq!(portal.counts().0, 0, "pick left a session open");

            let mut session = started(factory, &target.id);
            assert_eq!(first_frame(session.as_mut()), (320, 240));
            assert_eq!(portal.counts().0, 1);
            let first = session.current_target();
            let Some(first) = first else {
                panic!("no new id after start")
            };
            assert_ne!(first, target.id, "restore tokens are single use");
            session.stop();
            assert_eq!(portal.counts().0, 0, "stop left the cast open");

            if let Err(e) = session.start() {
                panic!("restart: {e}");
            }
            first_frame(session.as_mut());
            let second = session.current_target();
            assert!(second.is_some() && second != Some(first.clone()));
            drop(session);
            assert_eq!(portal.counts().0, 0, "drop left the cast open");

            // The id from the pick still opens in this process (the factory follows
            // the tokens), and so does the newest id a scene would have stored.
            for id in [&target.id, &first] {
                let mut session = started(factory, id);
                first_frame(session.as_mut());
            }
        }
    }

    /// Cancel and deny in the dialog.
    fn reports_the_users_answer(portal: &Portal, factory: &PipeWireCaptureFactory) {
        assert_eq!(portal.ask("next cancel"), "ok");
        assert!(matches!(
            factory.pick(TargetKind::Display),
            Err(CaptureError::Cancelled)
        ));
        assert_eq!(portal.ask("next deny"), "ok");
        assert!(matches!(
            factory.pick(TargetKind::Window),
            Err(CaptureError::PermissionDenied)
        ));
        assert_eq!(portal.counts().0, 0);
    }

    /// Without a restore token the pick's cast is kept and used by the first start;
    /// after it stops, the target is gone.
    fn keeps_a_pick_without_token_for_one_start(portal: &Portal, factory: &PipeWireCaptureFactory) {
        assert_eq!(portal.ask("next notoken"), "ok");
        let target = pick(factory, TargetKind::Display);
        assert!(target.id.as_str().starts_with("portal:display:live-"));
        let (open, _, starts) = portal.counts();
        assert_eq!(open, 1, "the pick's cast is kept");
        let mut session = started(factory, &target.id);
        first_frame(session.as_mut());
        assert_eq!(portal.counts().2, starts, "start asked the portal again");
        assert_eq!(session.current_target(), None);
        session.stop();
        assert_eq!(portal.counts().0, 0);
        assert!(matches!(session.start(), Err(CaptureError::TargetNotFound)));
        assert!(matches!(
            factory.open(&request(&target.id)),
            Err(CaptureError::TargetNotFound)
        ));
    }

    /// A token that no longer restores makes the portal show its dialog; `start` closes
    /// it and reports the target as not found instead of waiting for the user.
    fn refuses_a_used_token_without_waiting_for_the_user(portal: &Portal) {
        const LIMIT: Duration = Duration::from_millis(100);
        let first = PipeWireCaptureFactory::new();
        let target = pick(&first, TargetKind::Display);
        drop(started(&first, &target.id));
        // Another process (a fresh factory) only has the used token from the scene.
        // The test portal never answers such a restore, so a short limit keeps the
        // test fast; the default (2 s) is for real portals.
        let fresh = PipeWireCaptureFactory::new().with_restore_timeout(LIMIT);
        let (_, closed, _) = portal.counts();
        let mut session = match fresh.open(&request(&target.id)) {
            Ok(session) => session,
            Err(e) => panic!("open: {e}"),
        };
        let begun = Instant::now();
        assert!(matches!(session.start(), Err(CaptureError::TargetNotFound)));
        assert!(begun.elapsed() < LIMIT * 5, "start waited too long");
        let (open, closed_after, _) = portal.counts();
        assert_eq!(closed_after, closed + 1, "the dialog was not closed");
        assert_eq!(open, 0, "the failed restore left a session open");
        // The used token is known to be dead now: no second wait.
        let begun = Instant::now();
        assert!(matches!(session.start(), Err(CaptureError::TargetNotFound)));
        assert!(begun.elapsed() < LIMIT, "the dead token was tried again");
    }

    fn refuses_rgba(factory: &PipeWireCaptureFactory) {
        let target = pick(factory, TargetKind::Display);
        let rgba = request(&target.id).with_pixel_formats(&[PixelFormat::Rgba8]);
        assert!(matches!(
            factory.open(&rgba),
            Err(CaptureError::FormatUnsupported)
        ));
    }

    /// The suite against picked displays: a wrapper lists one picked display as an
    /// enumerable target, so every session check runs on portal casts.
    fn passes_conformance(portal: &Arc<Portal>) {
        let picked = Picked(PipeWireCaptureFactory::new());
        let hooks = Hooks(Arc::clone(portal));
        let options = Options {
            measure_allocations: Some(measure),
            hooks: Some(&hooks),
            process_threads: Some(conformance::process_thread_count),
            ..Options::default()
        };
        match conformance::run(&picked, &mut RealTimePacer::new(), &options) {
            Ok(report) => {
                eprintln!("{report:?}");
                for check in [
                    "enumerate",
                    "formats",
                    "standby",
                    "timestamps",
                    "latest wins",
                    "no allocation",
                    "in flight",
                    "resize",
                    "stop",
                    "restart",
                    "drop",
                    "target lost",
                ] {
                    assert!(
                        report.passed.iter().any(|c| c == check),
                        "{check} did not run: {report:?}"
                    );
                }
                assert!(report.skipped.is_empty(), "{report:?}");
            }
            Err(failure) => panic!("conformance failed: {failure}"),
        }
    }

    fn measure(f: &mut dyn FnMut()) -> u64 {
        let info = allocation_counter::measure(f);
        info.count_total + info.count_current.unsigned_abs()
    }

    /// The adapter with picked displays listed by `enumerate`.
    struct Picked(PipeWireCaptureFactory);

    impl AdapterFactory for Picked {
        fn id(&self) -> AdapterId {
            self.0.id()
        }

        fn display_name(&self) -> &str {
            "PipeWire portal"
        }

        fn probe(&self) -> Result<CapabilitySet, ProbeError> {
            let set = |items: &[&str]| {
                CapabilityValue::Set(items.iter().map(|s| (*s).to_owned()).collect())
            };
            Ok(self
                .0
                .probe()?
                .with(caps::TARGET_KINDS, set(&["display"]))
                .with(caps::TARGET_ENUMERATE, set(&["display"]))
                .with(caps::TARGET_PICKER, set(&[]))
                .with(caps::PIXEL_FORMATS, set(&["bgra8"]))
                .with(caps::MEMORY, set(&["system"])))
        }
    }

    impl CaptureFactory for Picked {
        fn enumerate(&self, kind: TargetKind) -> Result<Vec<CaptureTarget>, CaptureError> {
            match kind {
                TargetKind::Display => Ok(vec![self.0.pick(kind)?]),
                _ => Err(CaptureError::Unsupported("not listed")),
            }
        }

        fn open(&self, request: &CaptureRequest) -> Result<Box<dyn CaptureSession>, CaptureError> {
            self.0.open(request)
        }
    }

    struct Hooks(Arc<Portal>);

    impl TestHooks for Hooks {
        /// The compositor renegotiates the stream size, as when a window is resized.
        fn resize(&self, target: &TargetId, width: u32, height: u32) -> bool {
            target.as_str().starts_with("portal:")
                && self.0.ask(&format!("resize {width} {height}")) == "resized"
        }

        fn remove(&self, target: &TargetId) -> bool {
            target.as_str().starts_with("portal:") && self.0.remove()
        }

        /// Open fds other than shared-memory buffers: daemon and bus connections,
        /// event and timer fds.
        fn resources(&self) -> Option<u64> {
            let entries = std::fs::read_dir("/proc/self/fd").ok()?;
            let count = entries
                .filter_map(Result::ok)
                .filter(|entry| {
                    std::fs::read_link(entry.path())
                        .is_ok_and(|target| !target.to_string_lossy().starts_with("/memfd:"))
                })
                .count();
            Some(count as u64)
        }
    }
}

#[cfg(target_os = "linux")]
mod mock {
    //! The test portal: `org.freedesktop.portal.ScreenCast` on the private bus, and the
    //! video node its casts point at.

    use std::collections::{HashMap, HashSet};
    use std::io::{BufRead, Write};
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    use dbus::arg::{PropMap, RefArg, Variant};
    use dbus::blocking::Connection;
    use dbus::message::MessageType;
    use dbus::{Message, Path};

    use super::source;

    pub const SERVE: &str = "RBS_PORTAL_TEST_MOCK";

    const OBJECT: &str = "/org/freedesktop/portal/desktop";
    const SIZE: (i32, i32) = (320, 240);

    fn say(line: &str) {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "mock: {line}");
        let _ = out.flush();
    }

    /// How the dialog answers the next pick.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Answer {
        Approve,
        NoToken,
        Cancel,
        Deny,
    }

    #[derive(Default)]
    struct Session {
        restore: Option<String>,
        started: bool,
    }

    struct State {
        node: Arc<AtomicU32>,
        sessions: HashMap<String, Session>,
        tokens: HashSet<String>,
        next_token: u64,
        next_answer: Answer,
        closed_requests: u64,
        starts: u64,
    }

    fn variant<T: RefArg + 'static>(value: T) -> Variant<Box<dyn RefArg>> {
        Variant(Box::new(value))
    }

    fn sender_part(message: &Message) -> String {
        message
            .sender()
            .map(|s| s.trim_start_matches(':').replace('.', "_"))
            .unwrap_or_default()
    }

    fn token_of(options: &PropMap, key: &str) -> Option<String> {
        options
            .get(key)
            .and_then(|v| v.0.as_str())
            .map(str::to_owned)
    }

    fn respond(conn: &Connection, message: &Message, path: &str, code: u32, results: PropMap) {
        let Ok(signal) = Message::new_signal(path, "org.freedesktop.portal.Request", "Response")
        else {
            return;
        };
        let mut signal = signal.append2(code, results);
        if let Some(sender) = message.sender() {
            signal.set_destination(Some(sender));
        }
        let _ = conn.channel().send(signal);
    }

    fn reply<A: dbus::arg::Append>(conn: &Connection, message: &Message, value: A) {
        let _ = conn.channel().send(message.method_return().append1(value));
    }

    fn pipewire_socket() -> Option<std::path::PathBuf> {
        let dir = std::env::var_os("PIPEWIRE_RUNTIME_DIR")
            .or_else(|| std::env::var_os("XDG_RUNTIME_DIR"))?;
        let name = std::env::var_os("PIPEWIRE_REMOTE").unwrap_or_else(|| "pipewire-0".into());
        Some(std::path::Path::new(&dir).join(name))
    }

    impl State {
        fn handle(&mut self, conn: &Connection, message: &Message) {
            let path = message.path().map(|p| p.to_string()).unwrap_or_default();
            let interface = message
                .interface()
                .map(|i| i.to_string())
                .unwrap_or_default();
            let member = message.member().map(|m| m.to_string()).unwrap_or_default();
            match (interface.as_str(), member.as_str()) {
                ("org.freedesktop.DBus.Properties", "Get") => {
                    let name: String = message
                        .read2::<String, String>()
                        .map(|(_, n)| n)
                        .unwrap_or_default();
                    let value: u32 = match name.as_str() {
                        "AvailableSourceTypes" => 3,
                        "AvailableCursorModes" => 3,
                        "version" => 5,
                        _ => 0,
                    };
                    reply(conn, message, variant(value));
                }
                ("org.freedesktop.portal.ScreenCast", "CreateSession") => {
                    let Ok(options) = message.read1::<PropMap>() else {
                        return;
                    };
                    let sender = sender_part(message);
                    let request = format!(
                        "{OBJECT}/request/{sender}/{}",
                        token_of(&options, "handle_token").unwrap_or_default()
                    );
                    let session = format!(
                        "{OBJECT}/session/{sender}/{}",
                        token_of(&options, "session_handle_token").unwrap_or_default()
                    );
                    self.sessions.insert(session.clone(), Session::default());
                    reply(conn, message, Path::from(request.clone()));
                    let mut results = PropMap::new();
                    results.insert("session_handle".into(), variant(session));
                    respond(conn, message, &request, 0, results);
                }
                ("org.freedesktop.portal.ScreenCast", "SelectSources") => {
                    let Ok((session, options)) = message.read2::<Path, PropMap>() else {
                        return;
                    };
                    let request = format!(
                        "{OBJECT}/request/{}/{}",
                        sender_part(message),
                        token_of(&options, "handle_token").unwrap_or_default()
                    );
                    if let Some(s) = self.sessions.get_mut(&session.to_string()) {
                        s.restore = token_of(&options, "restore_token");
                    }
                    reply(conn, message, Path::from(request.clone()));
                    respond(conn, message, &request, 0, PropMap::new());
                }
                ("org.freedesktop.portal.ScreenCast", "Start") => {
                    let Ok((session, _parent, options)) = message.read3::<Path, String, PropMap>()
                    else {
                        return;
                    };
                    self.starts += 1;
                    let request = format!(
                        "{OBJECT}/request/{}/{}",
                        sender_part(message),
                        token_of(&options, "handle_token").unwrap_or_default()
                    );
                    reply(conn, message, Path::from(request.clone()));
                    let restore = self
                        .sessions
                        .get(&session.to_string())
                        .and_then(|s| s.restore.clone());
                    let answer = match restore {
                        // A used or unknown token: the real portal shows its dialog and
                        // waits for the user. Never answer.
                        Some(token) if !self.tokens.remove(&token) => return,
                        Some(_) => Answer::Approve,
                        None => std::mem::replace(&mut self.next_answer, Answer::Approve),
                    };
                    let mut results = PropMap::new();
                    match answer {
                        Answer::Cancel => return respond(conn, message, &request, 1, results),
                        Answer::Deny => return respond(conn, message, &request, 2, results),
                        Answer::Approve | Answer::NoToken => {}
                    }
                    if let Some(s) = self.sessions.get_mut(&session.to_string()) {
                        s.started = true;
                    }
                    let mut props = PropMap::new();
                    props.insert("size".into(), variant(SIZE));
                    props.insert("source_type".into(), variant(1u32));
                    let streams: Vec<(u32, PropMap)> =
                        vec![(self.node.load(Ordering::Acquire), props)];
                    results.insert("streams".into(), variant(streams));
                    if answer == Answer::Approve {
                        self.next_token += 1;
                        let token = format!("token-{}", self.next_token);
                        self.tokens.insert(token.clone());
                        results.insert("restore_token".into(), variant(token));
                        results.insert("persist_mode".into(), variant(2u32));
                    }
                    respond(conn, message, &request, 0, results);
                }
                ("org.freedesktop.portal.ScreenCast", "OpenPipeWireRemote") => {
                    let started = message
                        .read1::<Path>()
                        .ok()
                        .and_then(|s| self.sessions.get(&s.to_string()))
                        .is_some_and(|s| s.started);
                    let stream = pipewire_socket().and_then(|p| UnixStream::connect(p).ok());
                    match stream {
                        Some(stream) if started => reply(conn, message, OwnedFd::from(stream)),
                        _ => {
                            let error = message.error(
                                &"org.freedesktop.portal.Error.Failed".into(),
                                c"not started",
                            );
                            let _ = conn.channel().send(error);
                        }
                    }
                }
                ("org.freedesktop.portal.Session", "Close") => {
                    self.sessions.remove(&path);
                    let _ = conn.channel().send(message.method_return());
                }
                ("org.freedesktop.portal.Request", "Close") => {
                    self.closed_requests += 1;
                    let _ = conn.channel().send(message.method_return());
                }
                _ => {
                    if message.msg_type() == MessageType::MethodCall {
                        let error = message.error(
                            &"org.freedesktop.DBus.Error.UnknownMethod".into(),
                            c"unknown",
                        );
                        let _ = conn.channel().send(error);
                    }
                }
            }
        }

        fn command(&mut self, line: &str) {
            match line.trim() {
                "next cancel" => self.next_answer = Answer::Cancel,
                "next deny" => self.next_answer = Answer::Deny,
                "next notoken" => self.next_answer = Answer::NoToken,
                "count" => {
                    return say(&format!(
                        "{} {} {}",
                        self.sessions.len(),
                        self.closed_requests,
                        self.starts
                    ));
                }
                other => return say(&format!("unknown {other}")),
            }
            say("ok");
        }
    }

    /// Serves the portal until stdin closes; the node runs on this thread.
    pub fn serve() {
        let node = Arc::new(AtomicU32::new(0));
        let (node_ready, node_ready_rx) = mpsc::channel();
        let (commands, receiver) = pipewire::channel::channel::<source::Message>();
        let bus = std::thread::spawn({
            let node = Arc::clone(&node);
            move || {
                if node_ready_rx.recv().is_err() {
                    return;
                }
                bus_loop(node, &commands);
                // The test ended (stdin closed): remove the node so the process exits.
                let _ = commands.send(source::Message::Quit);
            }
        });
        source::run(SIZE.0 as u32, SIZE.1 as u32, receiver, move |id| {
            node.store(id, Ordering::Release);
            let _ = node_ready.send(());
        });
        // The node is gone; keep serving the bus until the test ends.
        let _ = bus.join();
    }

    fn bus_loop(node: Arc<AtomicU32>, source: &pipewire::channel::Sender<source::Message>) {
        let Ok(conn) = Connection::new_session() else {
            panic!("test portal: no session bus")
        };
        if conn
            .request_name("org.freedesktop.portal.Desktop", false, true, true)
            .is_err()
        {
            panic!("test portal: name taken");
        }
        let (lines_tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                if lines_tx.send(line).is_err() {
                    break;
                }
            }
        });
        let mut state = State {
            node,
            sessions: HashMap::new(),
            tokens: HashSet::new(),
            next_token: 0,
            next_answer: Answer::Approve,
            closed_requests: 0,
            starts: 0,
        };
        say("ready");
        let mut removed = false;
        loop {
            match lines.try_recv() {
                Ok(line) if line.trim() == "remove" => {
                    if !removed {
                        removed = source.send(source::Message::Quit).is_ok();
                    }
                    say(if removed { "removed" } else { "not removed" });
                }
                Ok(line) if line.starts_with("resize ") => {
                    let size: Vec<u32> = line
                        .split_whitespace()
                        .filter_map(|w| w.parse().ok())
                        .collect();
                    let sent = match size.as_slice() {
                        [w, h] if !removed => source.send(source::Message::Resize(*w, *h)).is_ok(),
                        _ => false,
                    };
                    say(if sent { "resized" } else { "not resized" });
                }
                Ok(line) => state.command(&line),
                Err(mpsc::TryRecvError::Disconnected) => return,
                Err(mpsc::TryRecvError::Empty) => {}
            }
            match conn
                .channel()
                .blocking_pop_message(Duration::from_millis(5))
            {
                Ok(Some(message)) => state.handle(&conn, &message),
                Ok(None) => {}
                Err(_) => return,
            }
        }
    }
}
