//! Runs the audio I/O port conformance suite against the PipeWire adapter.
//!
//! The test publishes one endpoint of each kind from its own child process (see
//! `common/fixture.rs`): a microphone, a null sink whose monitor is read, and an
//! application playing into an unlinked stream. It needs a running PipeWire daemon with
//! a session manager (WirePlumber) to link microphone and monitor streams; without a
//! daemon socket it only checks probing, unless `RBS_REQUIRE_PIPEWIRE` is set (CI
//! starts both and sets it, and then every case must run: strict mode).
//!
//! After the suite, the test checks the content of what the microphone and the
//! application deliver. The file holds a single test that runs by default, so the process's thread and fd
//! counts belong to the adapter alone.

#[cfg(target_os = "linux")]
#[path = "common/fixture.rs"]
mod fixture;

/// Allocations plus frees made by `f` on this thread.
#[cfg(target_os = "linux")]
fn measure(f: &mut dyn FnMut()) -> u64 {
    let info = allocation_counter::measure(f);
    info.count_total + info.count_current.unsigned_abs()
}

/// A fixture process started by `passes_conformance`; does nothing on its own.
#[test]
#[ignore = "started by passes_conformance as a fixture process"]
fn fixture_process() {
    #[cfg(target_os = "linux")]
    if let Some(role) = std::env::var_os(fixture::SERVE) {
        fixture::serve(&role.to_string_lossy());
    }
}

fn required() -> bool {
    std::env::var_os("RBS_REQUIRE_PIPEWIRE").is_some_and(|v| !v.is_empty())
}

#[test]
#[ignore = "slow: about 14 s of real audio through a real PipeWire daemon (the suite runs \
            300 ms runs, overflow and restart per endpoint, then graph changes and drifting clocks)"]
fn slow_passes_conformance() {
    use rbs_audio_io::conformance::{self, Options, RealTimePacer};
    use rbs_audio_io_pipewire::PipeWireAudioBackend;

    let backend = PipeWireAudioBackend::new();
    if !rbs_audio_io_pipewire::daemon_present() {
        assert!(
            !required(),
            "RBS_REQUIRE_PIPEWIRE is set but no PipeWire daemon socket was found"
        );
        eprintln!("no PipeWire daemon: only probing is checked");
        let report = conformance::run(&backend, &mut RealTimePacer::new(), &Options::default());
        eprintln!("{report}");
        return;
    }
    #[cfg(target_os = "linux")]
    {
        let hooks = Hooks {
            fixtures: ["mic", "sink", "app"]
                .map(|role| fixture::Fixture::start(role, "fixture_process")),
        };
        let options = Options {
            measure_allocations: Some(measure),
            hooks: Some(&hooks),
            strict: required(),
            ..Options::default()
        };
        // Only the fixture endpoints: a developer's daemon has real devices, which the
        // suite cannot remove.
        let fixtures = FixturesOnly(backend);
        let report = conformance::run(&fixtures, &mut RealTimePacer::new(), &options);
        eprintln!("{report}");
        report.assert_ran(&[
            "probe",
            "enumerate",
            "unknown endpoint",
            "format",
            "standby",
            "buffer mismatch",
            "delivery",
            "timestamps",
            "no allocation",
            "overflow",
            "accounting",
            "stop",
            "reads",
            "restart",
            "drop",
            "endpoint lost",
        ]);
        // The suite checks timing and accounting, not content: check that the audio
        // delivered is the audio played, which also proves the application's ports
        // were linked.
        delivers_what_was_played();
        survives_graph_changes();
        follows_drifting_devices();
    }
}

/// A master clock running `ppm` parts per million faster (positive) or slower than
/// `CLOCK_MONOTONIC`, so the daemon's device clock drifts against it like a real
/// device's crystal.
#[cfg(target_os = "linux")]
struct SkewedClock {
    base: rbs_core::time::MonotonicClock,
    ppm: i64,
}

#[cfg(target_os = "linux")]
impl rbs_core::time::Clock for SkewedClock {
    fn now(&self) -> rbs_core::time::Timestamp {
        let t = i128::from(self.base.now().as_nanos());
        let skewed = t + t * i128::from(self.ppm) / 1_000_000;
        rbs_core::time::Timestamp::from_nanos(u64::try_from(skewed.max(0)).unwrap_or(0))
    }
}

/// Devices whose clock runs fast or slow against the master clock lose no audio and
/// keep one run: their drift shows as small steps. 1000 ppm (ten times a poor
/// crystal) so a few seconds show what minutes would at real drift.
#[cfg(target_os = "linux")]
fn follows_drifting_devices() {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use rbs_audio_io::{AudioBackend, EndpointId, InputRequest};
    use rbs_core::time::MonotonicClock;

    let _sink = fixture::Fixture::start("sink", "fixture_process");
    let backend = rbs_audio_io_pipewire::PipeWireAudioBackend::new();
    for ppm in [1_000_i64, -1_000] {
        let clock = Arc::new(SkewedClock {
            base: MonotonicClock::new(),
            ppm,
        });
        let request =
            InputRequest::new(EndpointId::new(format!("monitor:{}", fixture::SINK)), clock);
        let mut stream = match backend.open_input(&request) {
            Ok(stream) => stream,
            Err(e) => panic!("open: {e}"),
        };
        if let Err(e) = stream.start() {
            panic!("start: {e}");
        }
        let rate = u64::from(stream.format().sample_rate.hz());
        let mut buffers = vec![vec![0.0_f32; 4_096]; 2];
        let mut delivered = 0_u64;
        // The clock starts at zero now, so the first packets end before it and are
        // dropped; count drops from the first delivered audio on.
        let mut dropped_before = None;
        // Three seconds of audio; the guard only catches a hang (ADR-0032).
        let guard = Instant::now() + Duration::from_secs(30);
        while delivered < rate * 3 {
            assert!(Instant::now() < guard, "{ppm} ppm: only {delivered} frames");
            let mut slices: Vec<&mut [f32]> = buffers.iter_mut().map(Vec::as_mut_slice).collect();
            match stream.read(&mut slices) {
                Ok(read) if read.frames > 0 => {
                    dropped_before.get_or_insert(stream.stats().dropped);
                    delivered += read.frames as u64;
                }
                Ok(_) => std::thread::sleep(Duration::from_millis(5)),
                Err(e) => panic!("{ppm} ppm: read: {e}"),
            }
        }
        let stats = stream.stats();
        assert_eq!(Some(stats.dropped), dropped_before, "{ppm} ppm: {stats:?}");
        assert_eq!(stats.discontinuities, 1, "{ppm} ppm: {stats:?}");
        stream.stop();
    }
}

/// The ids of the fixture endpoints.
#[cfg(target_os = "linux")]
fn fixture_ids() -> [String; 3] {
    [
        format!("source:{}", fixture::MIC),
        format!("monitor:{}", fixture::SINK),
        format!("app:{}", fixture::APP),
    ]
}

/// The PipeWire backend, listing only the fixture endpoints.
#[cfg(target_os = "linux")]
struct FixturesOnly(rbs_audio_io_pipewire::PipeWireAudioBackend);

#[cfg(target_os = "linux")]
impl rbs_core::registry::AdapterFactory for FixturesOnly {
    fn id(&self) -> rbs_core::registry::AdapterId {
        self.0.id()
    }

    fn display_name(&self) -> &str {
        self.0.display_name()
    }

    fn probe(&self) -> Result<rbs_core::capability::CapabilitySet, rbs_core::registry::ProbeError> {
        self.0.probe()
    }
}

#[cfg(target_os = "linux")]
impl rbs_audio_io::AudioBackend for FixturesOnly {
    fn enumerate(
        &self,
        kind: rbs_audio_io::EndpointKind,
    ) -> Result<Vec<rbs_audio_io::AudioEndpoint>, rbs_audio_io::AudioIoError> {
        let ids = fixture_ids();
        let mut list = self.0.enumerate(kind)?;
        list.retain(|e| ids.iter().any(|id| id == e.id.as_str()));
        Ok(list)
    }

    fn open_input(
        &self,
        request: &rbs_audio_io::InputRequest,
    ) -> Result<Box<dyn rbs_audio_io::AudioInputStream>, rbs_audio_io::AudioIoError> {
        self.0.open_input(request)
    }
}

/// Sets a key of the daemon's `settings` metadata (0 resets it) with `pw-metadata`.
#[cfg(target_os = "linux")]
fn set_setting(key: &str, value: u32) {
    let status = std::process::Command::new("pw-metadata")
        .args(["-n", "settings", "0", key, &value.to_string()])
        .stdout(std::process::Stdio::null())
        .status();
    assert!(
        status.is_ok_and(|s| s.success()),
        "pw-metadata {key} {value}"
    );
}

/// Resets the graph settings this test changes, also when it fails.
#[cfg(target_os = "linux")]
struct ResetSettings;

#[cfg(target_os = "linux")]
impl Drop for ResetSettings {
    fn drop(&mut self) {
        set_setting("clock.force-quantum", 0);
        set_setting("clock.force-rate", 0);
    }
}

/// The graph's quantum and rate change while a monitor stream is read (any client
/// asking for another latency does this on a desktop). Every read keeps invariants 2
/// and 3: strictly increasing, never ahead of the clock, and a read that continues a
/// run starts where the previous one ended. Audio that cannot be placed is counted
/// as dropped, so accounting still holds.
#[cfg(target_os = "linux")]
fn survives_graph_changes() {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use rbs_audio_io::{AudioBackend, EndpointId, InputRequest};
    use rbs_core::time::{Clock, MonotonicClock};

    const TOLERANCE: u64 = 2_000_000;

    let _sink = fixture::Fixture::start("sink", "fixture_process");
    let _reset = ResetSettings;
    let clock = Arc::new(MonotonicClock::new());
    let backend = rbs_audio_io_pipewire::PipeWireAudioBackend::new();
    let request = InputRequest::new(
        EndpointId::new(format!("monitor:{}", fixture::SINK)),
        clock.clone(),
    );
    let mut stream = match backend.open_input(&request) {
        Ok(stream) => stream,
        Err(e) => panic!("open: {e}"),
    };
    if let Err(e) = stream.start() {
        panic!("start: {e}");
    }
    let rate = u64::from(stream.format().sample_rate.hz());
    let mut buffers = vec![vec![0.0_f32; 4_096]; 2];
    let mut last: Option<(u64, u64)> = None;
    let mut delivered = 0_u64;
    let steps: [(&str, u32); 8] = [
        ("clock.force-quantum", 256),
        ("clock.force-quantum", 2_048),
        ("clock.force-quantum", 64),
        ("clock.force-quantum", 1_024),
        ("clock.force-quantum", 0),
        ("clock.force-rate", 44_100),
        ("clock.force-rate", 96_000),
        ("clock.force-rate", 0),
    ];
    for (key, value) in steps {
        set_setting(key, value);
        // 250 ms of audio after each change; the guard only catches a hang (ADR-0032).
        let wanted = delivered + rate / 4;
        let guard = Instant::now() + Duration::from_secs(5);
        while delivered < wanted {
            assert!(Instant::now() < guard, "no audio after {key} = {value}");
            let mut slices: Vec<&mut [f32]> = buffers.iter_mut().map(Vec::as_mut_slice).collect();
            let read = match stream.read(&mut slices) {
                Ok(read) => read,
                Err(e) => panic!("read after {key} = {value}: {e}"),
            };
            let now = clock.now().as_nanos();
            if read.frames == 0 {
                std::thread::sleep(Duration::from_millis(2));
                continue;
            }
            let start = read.timestamp.as_nanos();
            let end = start + read.frames as u64 * 1_000_000_000 / rate;
            assert!(
                end <= now + TOLERANCE,
                "after {key} = {value}: read ends {} us after it was made",
                (end - now) / 1_000
            );
            if let Some((previous_start, previous_end)) = last {
                assert!(
                    start > previous_start,
                    "after {key} = {value}: not increasing"
                );
                assert!(
                    start + TOLERANCE >= previous_end,
                    "after {key} = {value}: overlaps the previous read by {} us",
                    (previous_end - start) / 1_000
                );
                if !read.discontinuity {
                    assert!(
                        start.abs_diff(previous_end) <= TOLERANCE,
                        "after {key} = {value}: a continued read stepped {} us",
                        (start as i64 - previous_end as i64) / 1_000
                    );
                }
            }
            last = Some((start, end));
            delivered += read.frames as u64;
        }
    }
    stream.stop();
    let stats = stream.stats();
    assert_eq!(stats.delivered, delivered);
    assert_eq!(stats.captured, stats.delivered + stats.dropped, "{stats:?}");
    eprintln!("graph changes: {stats:?}");
}

/// The fixtures play a 0.25 amplitude sine on every channel; the microphone is mono
/// and applications are captured in stereo.
#[cfg(target_os = "linux")]
fn delivers_what_was_played() {
    let _mic = fixture::Fixture::start("mic", "fixture_process");
    let _app = fixture::Fixture::start("app", "fixture_process");
    let mic = peaks(&format!("source:{}", fixture::MIC));
    assert_eq!(mic.len(), 1, "the microphone is mono");
    assert!(mic[0] > 0.2 && mic[0] <= 0.26, "microphone peaks {mic:?}");
    let app = peaks(&format!("app:{}", fixture::APP));
    assert_eq!(app.len(), 2, "applications are captured in stereo");
    assert!(
        app.iter().all(|p| *p > 0.2 && *p <= 0.26),
        "application peaks {app:?}"
    );
}

/// The peak of each channel over 200 ms of audio from `endpoint`.
#[cfg(target_os = "linux")]
fn peaks(endpoint: &str) -> Vec<f32> {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use rbs_audio_io::{AudioBackend, EndpointId, InputRequest};
    use rbs_core::time::MonotonicClock;

    let backend = rbs_audio_io_pipewire::PipeWireAudioBackend::new();
    let request = InputRequest::new(EndpointId::new(endpoint), Arc::new(MonotonicClock::new()));
    let mut stream = match backend.open_input(&request) {
        Ok(stream) => stream,
        Err(e) => panic!("open {endpoint}: {e}"),
    };
    if let Err(e) = stream.start() {
        panic!("start {endpoint}: {e}");
    }
    let format = stream.format();
    let channels = format.channels.channels();
    let wanted = format.sample_rate.hz() as usize / 5;
    let mut buffers = vec![vec![0.0_f32; 1_024]; channels];
    let mut peaks = vec![0.0_f32; channels];
    let mut frames = 0;
    // Hang guard only (ADR-0032): audio normally arrives within one graph cycle.
    let guard = Instant::now() + Duration::from_secs(5);
    while frames < wanted {
        assert!(
            Instant::now() < guard,
            "{endpoint}: only {frames} frames in 5 s"
        );
        let mut slices: Vec<&mut [f32]> = buffers.iter_mut().map(Vec::as_mut_slice).collect();
        let read = match stream.read(&mut slices) {
            Ok(read) => read,
            Err(e) => panic!("read {endpoint}: {e}"),
        };
        if read.frames == 0 {
            std::thread::yield_now();
            continue;
        }
        for (peak, buffer) in peaks.iter_mut().zip(&buffers) {
            *peak = buffer[..read.frames]
                .iter()
                .fold(*peak, |p, s| p.max(s.abs()));
        }
        frames += read.frames;
    }
    stream.stop();
    peaks
}

#[cfg(target_os = "linux")]
struct Hooks {
    fixtures: [fixture::Fixture; 3],
}

#[cfg(target_os = "linux")]
impl rbs_audio_io::conformance::TestHooks for Hooks {
    fn remove(&self, endpoint: &rbs_audio_io::EndpointId) -> bool {
        let role = match endpoint.as_str() {
            id if id == format!("source:{}", fixture::MIC) => "mic",
            id if id == format!("monitor:{}", fixture::SINK) => "sink",
            id if id == format!("app:{}", fixture::APP) => "app",
            _ => return false,
        };
        self.fixtures
            .iter()
            .find(|f| f.role() == role)
            .is_some_and(fixture::Fixture::remove)
    }

    /// Threads (other than exiting ones) plus open file descriptors other than
    /// shared-memory buffers: daemon connections, event and timer fds. PipeWire maps
    /// buffer memory as memfds that the daemon may recycle, so those are not counted.
    fn resources(&self) -> Option<u64> {
        let fds = std::fs::read_dir("/proc/self/fd")
            .ok()?
            .filter_map(Result::ok)
            .filter(|entry| {
                std::fs::read_link(entry.path())
                    .is_ok_and(|target| !target.to_string_lossy().starts_with("/memfd:"))
            })
            .count();
        let threads = std::fs::read_dir("/proc/self/task")
            .ok()?
            .filter_map(Result::ok)
            .filter(|entry| !thread_exiting(&entry.path()))
            .count();
        Some((fds + threads) as u64)
    }
}

/// Whether a thread is exiting (`PF_EXITING` in its `stat` flags) or gone: a joined
/// thread can stay listed for a moment after `pthread_join` returns.
#[cfg(target_os = "linux")]
fn thread_exiting(task: &std::path::Path) -> bool {
    const PF_EXITING: u64 = 0x4;
    let Ok(stat) = std::fs::read_to_string(task.join("stat")) else {
        return true;
    };
    stat.rsplit_once(')')
        .and_then(|(_, rest)| rest.split_whitespace().nth(6))
        .and_then(|flags| flags.parse::<u64>().ok())
        .is_some_and(|flags| flags & PF_EXITING != 0)
}
